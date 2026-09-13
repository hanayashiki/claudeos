//! In-memory filesystem and VFS.
//!
//! Paths are normalised lexically against the task's cwd and then looked up
//! from the root, so directories do not need parent back-pointers.

pub mod cpio;
pub mod dev;
pub mod chan;
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
    /// An entry in /proc/<pid>/fd. Opening it is opening that descriptor,
    /// whatever it is attached to; reading the link gives the name it carries.
    Fd(u32, i32),
}

pub struct NodeInner {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// File contents, or the target of a symlink.
    pub data: Vec<u8>,
    pub children: BTreeMap<String, NodeRef>,
    pub mtime: i64,
    /// How many directory entries name this node.
    pub nlink: u32,
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
                mtime: crate::time::unix_time(),
                nlink: 1,
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
            NodeKind::Fd(..) => DT_LNK,
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
            st_nlink: inner.nlink as u64,
            st_mode: inner.mode,
            st_uid: inner.uid,
            st_gid: inner.gid,
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
                inner.mtime = crate::time::unix_time();
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
        inner.mtime = crate::time::unix_time();
        Ok(())
    }

    pub fn symlink_target(&self) -> Option<String> {
        if self.kind != NodeKind::Symlink && !matches!(self.kind, NodeKind::Fd(..)) {
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

pub const SYMLINK_DEPTH: usize = 16;

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
        procfs::refresh_dir(&node);
        // "self" inside /proc is the calling process's own directory. Doing
        // this here rather than in the syscalls means a symbolic link into
        // /proc/self resolves too, which is what /dev/stdout is.
        let own;
        let mut key: &str = name;
        if *name == "self" && procfs::is_proc_root(node.ino) {
            own = alloc::format!("{}", crate::sched::current_pid());
            key = &own;
        }
        let child = {
            let inner = node.inner.lock();
            inner.children.get(key).cloned()
        };
        let child = child.ok_or(Errno::ENOENT)?;

        // A descriptor entry is followed to the file it names only when that
        // file has a name; opening it is handled where the descriptor can be
        // reached, which path resolution cannot do.
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

pub fn mkfifo(path: &str, mode: u32) -> Result<NodeRef, Errno> {
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    if inner.children.contains_key(&name) {
        return Err(Errno::EEXIST);
    }
    let node = Node::new(NodeKind::Fifo, S_IFIFO | (mode & 0o7777));
    inner.children.insert(name, node.clone());
    Ok(node)
}

pub fn link_node(path: &str, node: NodeRef) -> Result<(), Errno> {
    let (parent, name) = split_parent(path)?;
    parent.inner.lock().children.insert(name, node);
    Ok(())
}

/// A second directory entry for a file that already has one.
pub fn hard_link(path: &str, node: NodeRef) -> Result<(), Errno> {
    let (parent, name) = split_parent(path)?;
    let mut inner = parent.inner.lock();
    if inner.children.contains_key(&name) {
        return Err(Errno::EEXIST);
    }
    node.inner.lock().nlink += 1;
    inner.children.insert(name, node);
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
    drop(inner);
    {
        let mut node_inner = node.inner.lock();
        node_inner.nlink = node_inner.nlink.saturating_sub(1);
    }
    Ok(())
}

/// Whether `node` is `ancestor` or sits somewhere beneath it.
///
/// Directories carry no parent back-pointers, so the question is answered by
/// walking down from `ancestor` rather than up from `node`. The lock on a
/// directory is dropped before its children are looked at, so nothing here
/// holds two at once.
fn is_within(ancestor: &NodeRef, node: &NodeRef) -> bool {
    let mut pending = alloc::vec![ancestor.clone()];
    while let Some(current) = pending.pop() {
        if Arc::ptr_eq(&current, node) {
            return true;
        }
        if !current.is_dir() {
            continue;
        }
        let children: Vec<NodeRef> = current.inner.lock().children.values().cloned().collect();
        pending.extend(children);
    }
    false
}

pub fn rename(from: &str, to: &str) -> Result<(), Errno> {
    if from == "/" || to == "/" {
        return Err(Errno::EBUSY);
    }
    let node = lookup_nofollow(from)?;
    let (from_parent, from_name) = split_parent(from)?;
    let (to_parent, to_name) = split_parent(to)?;
    let existing = to_parent.inner.lock().children.get(&to_name).cloned();

    // The same file under both names, which includes a path renamed onto
    // itself. Inserting the entry and then removing it is what loses the file.
    if let Some(existing) = &existing {
        if Arc::ptr_eq(existing, &node) {
            return Ok(());
        }
    }

    // A directory moved into its own subtree ends up holding a reference to
    // itself and unreachable from the root, so nothing ever frees it.
    if node.is_dir() && is_within(&node, &to_parent) {
        return Err(Errno::EINVAL);
    }

    if let Some(existing) = &existing {
        if existing.is_dir() != node.is_dir() {
            return Err(if existing.is_dir() { Errno::EISDIR } else { Errno::ENOTDIR });
        }
        if existing.is_dir() && !existing.inner.lock().children.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
    }

    // Both parents may be the same directory, so the two edits take its lock
    // one after the other rather than one inside the other.
    to_parent.inner.lock().children.insert(to_name, node);
    from_parent.inner.lock().children.remove(&from_name);
    // The moved file keeps the one name it had. What the move landed on has
    // lost one, and may still have others.
    if let Some(existing) = existing {
        let mut inner = existing.inner.lock();
        inner.nlink = inner.nlink.saturating_sub(1);
    }
    Ok(())
}

/// One entry as `getdents64` reports it.
pub struct DirEntry {
    pub ino: u64,
    pub kind: u8,
    pub name: String,
}

pub fn readdir(node: &NodeRef) -> Vec<DirEntry> {
    procfs::refresh_dir(node);
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

/// A descriptor's hold on an internet socket.
///
/// The socket outlives the descriptor: a connection that has been closed still
/// owes the other end a FIN and has to see it acknowledged, and then sits in
/// TIME-WAIT. So the descriptor holds this instead, and its drop is what tells
/// the stack the user's end has gone.
pub struct InetHandle {
    pub socket: Arc<crate::net::socket::InetSocket>,
}

impl InetHandle {
    pub fn new(socket: Arc<crate::net::socket::InetSocket>) -> Arc<InetHandle> {
        Arc::new(InetHandle { socket })
    }
}

impl Drop for InetHandle {
    fn drop(&mut self) {
        crate::net::socket::close(&self.socket);
    }
}

/// An entry in a task's file descriptor table.
pub enum FileBacking {
    Node(NodeRef),
    Pipe(Arc<pipe::Pipe>, pipe::PipeEnd),
    EventFd(Arc<chan::EventFd>),
    /// One end of a connected pair, which is what `socketpair` returns.
    Socket(Arc<chan::Socket>),
    /// An internet socket: AF_INET over TCP or UDP.
    Inet(Arc<InetHandle>),
    Epoll(Arc<chan::Epoll>),
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
            FileBacking::Pipe(_, end) => end.reads(),
            FileBacking::EventFd(_) | FileBacking::Socket(_) | FileBacking::Inet(_) => true,
            FileBacking::Epoll(_) => false,
            _ => {
                let access = self.flags() & O_ACCMODE;
                access == O_RDONLY || access == O_RDWR
            }
        }
    }

    pub fn writable(&self) -> bool {
        match &self.backing {
            FileBacking::Pipe(_, end) => end.writes(),
            FileBacking::EventFd(_) | FileBacking::Socket(_) | FileBacking::Inet(_) => true,
            FileBacking::Epoll(_) => false,
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
            FileBacking::EventFd(event) => event.read(buf, self.flags() & O_NONBLOCK != 0),
            FileBacking::Socket(socket) => socket.read(buf, self.flags() & O_NONBLOCK != 0),
            FileBacking::Inet(handle) => handle
                .socket
                .read_blocking(buf, self.flags() & O_NONBLOCK != 0, false)
                .map(|(n, _)| n),
            FileBacking::Epoll(_) => Err(Errno::EINVAL),
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
            FileBacking::EventFd(event) => event.write(buf, self.flags() & O_NONBLOCK != 0),
            FileBacking::Socket(socket) => socket.write(buf, self.flags() & O_NONBLOCK != 0),
            FileBacking::Inet(handle) => handle.socket.write_blocking(
                buf,
                self.flags() & O_NONBLOCK != 0,
                None,
                true,
            ),
            FileBacking::Epoll(_) => Err(Errno::EINVAL),
        }
    }

    pub fn seek(&self, pos: i64, whence: u32) -> Result<u64, Errno> {
        let node = match &self.backing {
            FileBacking::Node(node) => node,
            _ => return Err(Errno::ESPIPE),
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
            _ => Stat {
                st_dev: 0,
                st_ino: 0,
                st_nlink: 1,
                st_mode: S_IFSOCK | 0o600,
                st_blksize: 4096,
                ..Default::default()
            },
        }
    }
}

/// A task's open descriptors.
///
/// The table sits behind a shared handle rather than in the task itself, so
/// that threads can hold the same one. A `clone` with `CLONE_FILES` gives the
/// child this value, and then a descriptor either of them opens is one both of
/// them have; a fork takes a copy with `clone_table` instead, and the two
/// tables go their separate ways from there.
#[derive(Clone)]
pub struct FdTable {
    inner: Arc<Spinlock<FdInner>>,
}

struct FdInner {
    entries: Vec<Option<Arc<OpenFile>>>,
    cloexec: Vec<bool>,
}

pub const MAX_FDS: usize = 256;

impl FdInner {
    fn ensure(&mut self, index: usize) {
        while self.entries.len() <= index {
            self.entries.push(None);
            self.cloexec.push(false);
        }
    }
}

impl FdTable {
    pub fn new() -> Self {
        FdTable {
            inner: Arc::new(Spinlock::new(FdInner { entries: Vec::new(), cloexec: Vec::new() })),
        }
    }

    /// A second handle on the same table, for a thread that shares it.
    pub fn share(&self) -> FdTable {
        FdTable { inner: self.inner.clone() }
    }

    /// True when no other task holds this table.
    pub fn is_last_reference(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    pub fn get(&self, fd: i32) -> Result<Arc<OpenFile>, Errno> {
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        self.inner
            .lock()
            .entries
            .get(fd as usize)
            .and_then(|slot| slot.clone())
            .ok_or(Errno::EBADF)
    }

    pub fn insert_at(&self, fd: usize, file: Arc<OpenFile>, cloexec: bool) {
        let mut inner = self.inner.lock();
        inner.ensure(fd);
        inner.entries[fd] = Some(file);
        inner.cloexec[fd] = cloexec;
    }

    /// Lowest free descriptor at or above `min`.
    pub fn alloc_at_least(&self, min: usize, file: Arc<OpenFile>, cloexec: bool) -> Result<i32, Errno> {
        let mut inner = self.inner.lock();
        let mut fd = min;
        loop {
            if fd >= MAX_FDS {
                return Err(Errno::EMFILE);
            }
            if fd >= inner.entries.len() {
                inner.ensure(fd);
            }
            if inner.entries[fd].is_none() {
                inner.entries[fd] = Some(file);
                inner.cloexec[fd] = cloexec;
                return Ok(fd as i32);
            }
            fd += 1;
        }
    }

    pub fn alloc(&self, file: Arc<OpenFile>, cloexec: bool) -> Result<i32, Errno> {
        self.alloc_at_least(0, file, cloexec)
    }

    pub fn close(&self, fd: i32) -> Result<(), Errno> {
        // The last reference to the file is let go after the lock is, because
        // what happens then is a whole pipe or socket being taken down and
        // that has no business running with this table held and interrupts
        // off.
        let closed = {
            let mut inner = self.inner.lock();
            if fd < 0 || fd as usize >= inner.entries.len() {
                return Err(Errno::EBADF);
            }
            inner.entries[fd as usize].take()
        };
        match closed {
            Some(file) => {
                drop(file);
                Ok(())
            }
            None => Err(Errno::EBADF),
        }
    }

    pub fn is_cloexec(&self, fd: i32) -> bool {
        if fd < 0 {
            return false;
        }
        self.inner.lock().cloexec.get(fd as usize).copied().unwrap_or(false)
    }

    pub fn set_cloexec(&self, fd: i32, value: bool) {
        if fd < 0 {
            return;
        }
        let mut inner = self.inner.lock();
        if (fd as usize) < inner.cloexec.len() {
            inner.cloexec[fd as usize] = value;
        }
    }

    /// The descriptors open right now, for anything that has to walk them.
    pub fn snapshot(&self) -> Vec<(i32, Arc<OpenFile>)> {
        let inner = self.inner.lock();
        inner
            .entries
            .iter()
            .enumerate()
            .filter_map(|(fd, slot)| slot.clone().map(|file| (fd as i32, file)))
            .collect()
    }

    /// A table of its own holding the same descriptors, for a fork.
    pub fn clone_table(&self) -> FdTable {
        let inner = self.inner.lock();
        FdTable {
            inner: Arc::new(Spinlock::new(FdInner {
                entries: inner.entries.clone(),
                cloexec: inner.cloexec.clone(),
            })),
        }
    }

    /// Drop descriptors marked close-on-exec.
    pub fn close_on_exec(&self) {
        let dropped = self.take_matching(|inner, fd| inner.cloexec[fd]);
        drop(dropped);
    }

    /// Drop every descriptor.
    pub fn clear(&self) {
        let dropped = self.take_matching(|_, _| true);
        drop(dropped);
    }

    fn take_matching(&self, wanted: impl Fn(&FdInner, usize) -> bool) -> Vec<Arc<OpenFile>> {
        let mut out = Vec::new();
        let mut inner = self.inner.lock();
        for fd in 0..inner.entries.len() {
            if !wanted(&inner, fd) {
                continue;
            }
            if let Some(file) = inner.entries[fd].take() {
                out.push(file);
            }
            inner.cloexec[fd] = false;
        }
        out
    }
}
