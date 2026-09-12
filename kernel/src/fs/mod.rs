//! In-memory filesystem and VFS.
//!
//! Paths are normalised lexically against the task's cwd and then looked up
//! from the root, so directories do not need parent back-pointers.

pub mod cpio;
pub mod dev;
pub mod pipe;
pub mod procfs;

use crate::abi::*;
use crate::sync::Spinlock;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub type NodeRef = Arc<Node>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    Device(dev::DeviceKind),
    /// A file whose contents the kernel produces on each read.
    Generated(procfs::Generated),
    Fifo,
}

pub struct NodeInner {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// File contents, or the target of a symlink.
    pub data: Vec<u8>,
    pub children: BTreeMap<String, NodeRef>,
    pub mtime: i64,
}

pub struct Node {
    pub ino: u64,
    pub kind: NodeKind,
    pub inner: Spinlock<NodeInner>,
}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// Make room for `target` bytes of file contents.
///
/// Files live in RAM, so a write has to be refused before the allocator runs
/// the machine out of memory. Growing a vector copies the old contents into a
/// new allocation, so the peak is the old capacity plus the new one; this
/// grows by half again when that peak fits and falls back to an exact fit when
/// memory is tight.
fn reserve_for(data: &mut Vec<u8>, target: usize) -> Result<(), Errno> {
    if target <= data.capacity() {
        return Ok(());
    }
    let available = crate::mm::file_available_bytes();
    let current = data.capacity();
    let roomy = target.max(current + current / 2);

    if current.saturating_add(roomy) <= available {
        data.reserve_exact(roomy - data.len());
        return Ok(());
    }
    if current.saturating_add(target) <= available {
        data.reserve_exact(target - data.len());
        return Ok(());
    }
    Err(Errno::ENOSPC)
}

impl Node {
    pub fn new(kind: NodeKind, mode: u32) -> NodeRef {
        Arc::new(Node {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            kind,
            inner: Spinlock::new(NodeInner {
                mode,
                uid: 0,
                gid: 0,
                data: Vec::new(),
                children: BTreeMap::new(),
                mtime: 0,
            }),
        })
    }

    pub fn new_dir() -> NodeRef {
        Node::new(NodeKind::Dir, S_IFDIR | 0o755)
    }

    pub fn new_file(mode: u32) -> NodeRef {
        Node::new(NodeKind::File, S_IFREG | (mode & 0o7777))
    }

    pub fn new_symlink(target: &str) -> NodeRef {
        let node = Node::new(NodeKind::Symlink, S_IFLNK | 0o777);
        node.inner.lock().data = target.as_bytes().to_vec();
        node
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Dir)
    }

    pub fn size(&self) -> u64 {
        match self.kind {
            NodeKind::Dir => 4096,
            NodeKind::Device(_) => 0,
            NodeKind::Generated(kind) => procfs::size(kind),
            _ => self.inner.lock().data.len() as u64,
        }
    }

    pub fn mode(&self) -> u32 {
        self.inner.lock().mode
    }

    pub fn dirent_type(&self) -> u8 {
        match self.kind {
            NodeKind::Dir => DT_DIR,
            NodeKind::File => DT_REG,
            NodeKind::Symlink => DT_LNK,
            NodeKind::Device(_) => DT_CHR,
            NodeKind::Generated(_) => DT_REG,
            NodeKind::Fifo => DT_FIFO,
        }
    }

    pub fn stat(&self) -> Stat {
        // Generated files report a length that has to be computed without the
        // node lock held, so do it first.
        let size = self.size() as i64;
        let inner = self.inner.lock();
        Stat {
            st_dev: 1,
            st_ino: self.ino,
            st_nlink: 1,
            st_mode: inner.mode,
            st_uid: inner.uid,
            st_gid: inner.gid,
            __pad0: 0,
            st_rdev: match self.kind {
                NodeKind::Device(kind) => kind.rdev(),
                _ => 0,
            },
            st_size: size,
            st_blksize: 4096,
            st_blocks: (size + 511) / 512,
            st_mtime: inner.mtime,
            st_atime: inner.mtime,
            st_ctime: inner.mtime,
            ..Default::default()
        }
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        match self.kind {
            NodeKind::Device(kind) => dev::read(kind, buf),
            NodeKind::Generated(kind) => procfs::read(kind, offset, buf),
            NodeKind::Dir => Err(Errno::EISDIR),
            _ => {
                let inner = self.inner.lock();
                let start = offset as usize;
                if start >= inner.data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(inner.data.len() - start);
                buf[..n].copy_from_slice(&inner.data[start..start + n]);
                Ok(n)
            }
        }
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, Errno> {
        match self.kind {
            NodeKind::Device(kind) => dev::write(kind, buf),
            NodeKind::Generated(_) => Err(Errno::EACCES),
            NodeKind::Dir => Err(Errno::EISDIR),
            _ => {
                let mut inner = self.inner.lock();
                let start = offset as usize;
                let end = start + buf.len();
                if end > inner.data.len() {
                    reserve_for(&mut inner.data, end)?;
                    inner.data.resize(end, 0);
                }
                inner.data[start..end].copy_from_slice(buf);
                Ok(buf.len())
            }
        }
    }

    pub fn truncate(&self, len: u64) -> Result<(), Errno> {
        if self.is_dir() {
            return Err(Errno::EISDIR);
        }
        let mut inner = self.inner.lock();
        let target = len as usize;
        if target > inner.data.len() {
            reserve_for(&mut inner.data, target)?;
        }
        inner.data.resize(target, 0);
        Ok(())
    }

    pub fn symlink_target(&self) -> Option<String> {
        if self.kind != NodeKind::Symlink {
            return None;
        }
        String::from_utf8(self.inner.lock().data.clone()).ok()
    }
}

static ROOT: Spinlock<Option<NodeRef>> = Spinlock::new(None);

pub fn root() -> NodeRef {
    ROOT.lock().as_ref().expect("filesystem not mounted").clone()
}

pub fn init() {
    let root = Node::new_dir();
    *ROOT.lock() = Some(root);
    // A minimal skeleton; the initramfs fills in the rest.
    for dir in ["dev", "proc", "tmp", "bin", "etc", "sys", "root"] {
        let _ = mkdir(&alloc::format!("/{}", dir), 0o755);
    }
    dev::populate();
    procfs::populate();
}

/// Collapse `.`/`..` and make `path` absolute relative to `cwd`.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let combined: String;
    let source = if path.starts_with('/') {
        path
    } else {
        combined = alloc::format!("{}/{}", cwd, path);
        &combined
    };
    for part in source.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

const SYMLINK_DEPTH: usize = 16;

/// Look up an absolute, normalised path.
pub fn lookup(path: &str) -> Result<NodeRef, Errno> {
    lookup_inner(path, true, 0)
}

/// Look up a path without following a final symlink.
pub fn lookup_nofollow(path: &str) -> Result<NodeRef, Errno> {
    lookup_inner(path, false, 0)
}

fn lookup_inner(path: &str, follow_final: bool, depth: usize) -> Result<NodeRef, Errno> {
    if depth > SYMLINK_DEPTH {
        return Err(Errno::ELOOP);
    }
    let mut node = root();
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();

    for (i, name) in components.iter().enumerate() {
        let is_final = i + 1 == components.len();
        if !node.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let child = {
            let inner = node.inner.lock();
            inner.children.get(*name).cloned()
        };
        let child = child.ok_or(Errno::ENOENT)?;

        if child.kind == NodeKind::Symlink && (!is_final || follow_final) {
            let target = child.symlink_target().ok_or(Errno::EIO)?;
            // Resolve the link against the directory that contains it.
            let parent_path = join_components(&components[..i]);
            let resolved = normalize(&parent_path, &target);
            let intermediate = lookup_inner(&resolved, true, depth + 1)?;
            node = intermediate;
        } else {
            node = child;
        }
    }
    Ok(node)
}

fn join_components(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Split an absolute path into (parent directory node, final component).
pub fn split_parent(path: &str) -> Result<(NodeRef, String), Errno> {
    let trimmed = path.trim_end_matches('/');
    let (dir, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        None => ("/", trimmed),
    };
    if name.is_empty() {
        return Err(Errno::EINVAL);
    }
    let parent = lookup(dir)?;
    if !parent.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    Ok((parent, name.to_string()))
}

pub fn create(path: &str, mode: u32) -> Result<NodeRef, Errno> {
    if path == "/" {
        return Err(Errno::EISDIR);
    }
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    if let Some(existing) = inner.children.get(&name) {
        return Ok(existing.clone());
    }
    let node = Node::new_file(mode);
    inner.children.insert(name, node.clone());
    Ok(node)
}

pub fn mkdir(path: &str, mode: u32) -> Result<NodeRef, Errno> {
    // The root always exists; `mkdir -p` walks down from it and expects the
    // "already there" answer rather than a malformed-path one.
    if path == "/" {
        return Err(Errno::EEXIST);
    }
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    if inner.children.contains_key(&name) {
        return Err(Errno::EEXIST);
    }
    let node = Node::new(NodeKind::Dir, S_IFDIR | (mode & 0o7777));
    inner.children.insert(name, node.clone());
    Ok(node)
}

/// Create every missing directory along `path`.
pub fn mkdir_p(path: &str) -> Result<NodeRef, Errno> {
    let mut current = String::new();
    let mut node = root();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        current.push('/');
        current.push_str(part);
        node = match mkdir(&current, 0o755) {
            Ok(node) => node,
            Err(Errno::EEXIST) => lookup(&current)?,
            Err(e) => return Err(e),
        };
    }
    Ok(node)
}

pub fn symlink(path: &str, target: &str) -> Result<(), Errno> {
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    if inner.children.contains_key(&name) {
        return Err(Errno::EEXIST);
    }
    inner.children.insert(name, Node::new_symlink(target));
    Ok(())
}

pub fn link_node(path: &str, node: NodeRef) -> Result<(), Errno> {
    let (parent, name) = split_parent(path)?;
    parent.inner.lock().children.insert(name, node);
    Ok(())
}

pub fn unlink(path: &str, want_dir: bool) -> Result<(), Errno> {
    if path == "/" {
        return Err(Errno::EBUSY);
    }
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    let node = inner.children.get(&name).ok_or(Errno::ENOENT)?.clone();
    if node.is_dir() != want_dir {
        return Err(if want_dir { Errno::ENOTDIR } else { Errno::EISDIR });
    }
    if want_dir && !node.inner.lock().children.is_empty() {
        return Err(Errno::ENOTEMPTY);
    }
    inner.children.remove(&name);
    Ok(())
}

pub fn rename(from: &str, to: &str) -> Result<(), Errno> {
    let node = lookup_nofollow(from)?;
    let (from_parent, from_name) = split_parent(from)?;
    let (to_parent, to_name) = split_parent(to)?;
    to_parent.inner.lock().children.insert(to_name, node);
    from_parent.inner.lock().children.remove(&from_name);
    Ok(())
}

/// One entry as `getdents64` reports it.
pub struct DirEntry {
    pub ino: u64,
    pub kind: u8,
    pub name: String,
}

pub fn readdir(node: &NodeRef) -> Vec<DirEntry> {
    let mut out = Vec::new();
    out.push(DirEntry { ino: node.ino, kind: DT_DIR, name: ".".to_string() });
    out.push(DirEntry { ino: node.ino, kind: DT_DIR, name: "..".to_string() });
    for (name, child) in node.inner.lock().children.iter() {
        out.push(DirEntry {
            ino: child.ino,
            kind: child.dirent_type(),
            name: name.clone(),
        });
    }
    out
}

/// An entry in a task's file descriptor table.
pub enum FileBacking {
    Node(NodeRef),
    Pipe(Arc<pipe::Pipe>, bool),
}

pub struct OpenFile {
    pub backing: FileBacking,
    pub offset: Spinlock<u64>,
    pub flags: Spinlock<u32>,
    /// Path this descriptor was opened with, for *at() resolution and
    /// /proc/self/fd.
    pub path: String,
}

impl OpenFile {
    pub fn from_node(node: NodeRef, flags: u32) -> Arc<OpenFile> {
        Self::from_node_at(node, flags, "")
    }

    pub fn from_node_at(node: NodeRef, flags: u32, path: &str) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            backing: FileBacking::Node(node),
            offset: Spinlock::new(0),
            flags: Spinlock::new(flags),
            path: path.to_string(),
        })
    }

    pub fn node(&self) -> Option<&NodeRef> {
        match &self.backing {
            FileBacking::Node(node) => Some(node),
            _ => None,
        }
    }

    pub fn flags(&self) -> u32 {
        *self.flags.lock()
    }

    pub fn readable(&self) -> bool {
        match &self.backing {
            FileBacking::Pipe(_, is_write) => !is_write,
            _ => {
                let access = self.flags() & O_ACCMODE;
                access == O_RDONLY || access == O_RDWR
            }
        }
    }

    pub fn writable(&self) -> bool {
        match &self.backing {
            FileBacking::Pipe(_, is_write) => *is_write,
            _ => {
                let access = self.flags() & O_ACCMODE;
                access == O_WRONLY || access == O_RDWR
            }
        }
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if !self.readable() {
            return Err(Errno::EBADF);
        }
        match &self.backing {
            FileBacking::Node(node) => {
                // A device read can block for an arbitrary time. Holding the
                // offset lock across it would also hold interrupts off, which
                // would stop the very device the read is waiting on.
                if matches!(node.kind, NodeKind::Device(_)) {
                    return node.read_at(0, buf);
                }
                let offset = *self.offset.lock();
                let n = node.read_at(offset, buf)?;
                *self.offset.lock() = offset + n as u64;
                Ok(n)
            }
            FileBacking::Pipe(pipe, _) => pipe.read(buf, self.flags() & O_NONBLOCK != 0),
        }
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        if !self.writable() {
            return Err(Errno::EBADF);
        }
        match &self.backing {
            FileBacking::Node(node) => {
                if matches!(node.kind, NodeKind::Device(_)) {
                    return node.write_at(0, buf);
                }
                let offset = if self.flags() & O_APPEND != 0 {
                    node.size()
                } else {
                    *self.offset.lock()
                };
                let n = node.write_at(offset, buf)?;
                *self.offset.lock() = offset + n as u64;
                Ok(n)
            }
            FileBacking::Pipe(pipe, _) => pipe.write(buf, self.flags() & O_NONBLOCK != 0),
        }
    }

    pub fn seek(&self, pos: i64, whence: u32) -> Result<u64, Errno> {
        let node = match &self.backing {
            FileBacking::Node(node) => node,
            FileBacking::Pipe(..) => return Err(Errno::ESPIPE),
        };
        if matches!(node.kind, NodeKind::Device(_)) {
            return Ok(0);
        }
        let mut offset = self.offset.lock();
        let base = match whence {
            SEEK_SET => 0i64,
            SEEK_CUR => *offset as i64,
            SEEK_END => node.size() as i64,
            _ => return Err(Errno::EINVAL),
        };
        let new = base.checked_add(pos).ok_or(Errno::EINVAL)?;
        if new < 0 {
            return Err(Errno::EINVAL);
        }
        *offset = new as u64;
        Ok(new as u64)
    }

    pub fn stat(&self) -> Stat {
        match &self.backing {
            FileBacking::Node(node) => node.stat(),
            FileBacking::Pipe(..) => Stat {
                st_dev: 0,
                st_ino: 0,
                st_nlink: 1,
                st_mode: S_IFIFO | 0o600,
                st_blksize: 4096,
                ..Default::default()
            },
        }
    }
}

/// A process's file descriptor table.
#[derive(Default)]
pub struct FdTable {
    pub entries: Vec<Option<Arc<OpenFile>>>,
    pub cloexec: Vec<bool>,
}

pub const MAX_FDS: usize = 256;

impl FdTable {
    pub fn new() -> Self {
        FdTable { entries: Vec::new(), cloexec: Vec::new() }
    }

    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        self.entries
            .get(fd as usize)
            .and_then(|slot| slot.clone())
            .ok_or(Errno::EBADF)
    }

    fn ensure(&mut self, index: usize) {
        while self.entries.len() <= index {
            self.entries.push(None);
            self.cloexec.push(false);
        }
    }

    pub fn insert_at(&mut self, fd: usize, file: Arc<OpenFile>, cloexec: bool) {
        self.ensure(fd);
        self.entries[fd] = Some(file);
        self.cloexec[fd] = cloexec;
    }

    /// Lowest free descriptor at or above `min`.
    pub fn alloc_at_least(&mut self, min: usize, file: Arc<OpenFile>, cloexec: bool) -> Result<i32, Errno> {
        let mut fd = min;
        loop {
            if fd >= MAX_FDS {
                return Err(Errno::EMFILE);
            }
            if fd >= self.entries.len() {
                self.ensure(fd);
            }
            if self.entries[fd].is_none() {
                self.entries[fd] = Some(file);
                self.cloexec[fd] = cloexec;
                return Ok(fd as i32);
            }
            fd += 1;
        }
    }

    pub fn alloc(&mut self, file: Arc<OpenFile>, cloexec: bool) -> Result<i32, Errno> {
        self.alloc_at_least(0, file, cloexec)
    }

    pub fn close(&mut self, fd: i32) -> Result<(), Errno> {
        if fd < 0 || fd as usize >= self.entries.len() {
            return Err(Errno::EBADF);
        }
        if self.entries[fd as usize].take().is_none() {
            return Err(Errno::EBADF);
        }
        Ok(())
    }

    pub fn clone_table(&self) -> FdTable {
        FdTable { entries: self.entries.clone(), cloexec: self.cloexec.clone() }
    }

    /// Drop descriptors marked close-on-exec.
    pub fn close_on_exec(&mut self) {
        for i in 0..self.entries.len() {
            if self.cloexec[i] {
                self.entries[i] = None;
                self.cloexec[i] = false;
            }
        }
    }
}
