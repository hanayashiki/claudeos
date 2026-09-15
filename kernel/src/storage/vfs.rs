//! /data as nodes of the kernel's filesystem tree.
//!
//! A directory or file on the volume is a node of kind `DataDir` or `DataFile`
//! whose `stored` field holds its path inside the volume and, for a file, its
//! length. Nothing of its contents is in the node: reads, writes, lookups and
//! listings all go to the volume. A node is made when a lookup reaches an
//! entry, and a table of weak references hands the same node to every lookup
//! of the same path while anything holds it, so two descriptors on one file
//! see one length.
//!
//! **One lock, and a sleeping one.** The card is driven by polling, from
//! whichever task asked, with interrupts on. A spinlock would hold interrupts
//! off for the whole of a card command, and the timer, the watchdog's feed
//! and every other task with them, so the volume is behind a lock that puts a
//! waiting task to sleep instead.
//!
//! **Identity.** A node's inode number is a hash of its path, so a program
//! that looks at a file twice sees one number even when no node lived in
//! between. A node keeps the number it was made with when it is renamed.
//!
//! **Removed while open.** An unlink or a rename onto a file marks its node
//! gone, and a read or write through a descriptor still open on it answers
//! ESTALE. FAT frees a file's clusters when its entry goes, so there is no file
//! left to read.

use crate::abi::*;
use crate::fs::{DirEntry, Node, NodeRef, Offset};
use crate::sched::WaitQueue;
use crate::storage::card::Partition;
use crate::storage::fat::{join, Entry, FsError, Volume};
use crate::sync::Spinlock;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// The lock
// ---------------------------------------------------------------------------

/// A lock whose waiters sleep on a queue rather than spin with interrupts off.
pub struct SleepLock<T> {
    held: AtomicBool,
    queue: WaitQueue,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SleepLock<T> {}

pub struct SleepGuard<'a, T> {
    lock: &'a SleepLock<T>,
}

impl<T> SleepLock<T> {
    pub const fn new(value: T) -> SleepLock<T> {
        SleepLock { held: AtomicBool::new(false), queue: WaitQueue::new(), value: UnsafeCell::new(value) }
    }

    /// Take the lock, sleeping until it is free. Only from a task.
    pub fn lock(&self) -> SleepGuard<'_, T> {
        // The test runs inside the queue's check, with interrupts off, so a
        // release between a failed try and the sleep still wakes this task.
        self.queue.wait_until(|| self.held.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok());
        SleepGuard { lock: self }
    }
}

impl<T> core::ops::Deref for SleepGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SleepGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        self.lock.queue.wake_all();
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

static VOLUME: SleepLock<Option<Volume<Partition>>> = SleepLock::new(None);
/// Every live node on the volume, by path.
static NODES: Spinlock<BTreeMap<String, Weak<Node>>> = Spinlock::new(BTreeMap::new());
/// The volume's root, which the tree holds as /data.
static ROOT: Spinlock<Option<NodeRef>> = Spinlock::new(None);
/// The line /proc/mounts gives the volume.
static MOUNT_LINE: Spinlock<Option<String>> = Spinlock::new(None);
/// Dead entries in `NODES` are swept out when it grows past this.
const SWEEP_AT: usize = 256;

fn errno(error: FsError) -> Errno {
    match error {
        FsError::NotFound => Errno::ENOENT,
        FsError::Exists => Errno::EEXIST,
        FsError::NotDir => Errno::ENOTDIR,
        FsError::IsDir => Errno::EISDIR,
        FsError::NotEmpty => Errno::ENOTEMPTY,
        FsError::NoSpace => Errno::ENOSPC,
        FsError::NameTooLong => Errno::ENAMETOOLONG,
        FsError::BadName | FsError::Invalid => Errno::EINVAL,
        FsError::TooBig => Errno::EFBIG,
        FsError::Io => Errno::EIO,
        FsError::Corrupt => Errno::EUCLEAN,
        FsError::ReadOnly => Errno::EROFS,
    }
}

fn with_volume<T>(op: impl FnOnce(&mut Volume<Partition>) -> Result<T, FsError>) -> Result<T, Errno> {
    let mut guard = VOLUME.lock();
    match guard.as_mut() {
        Some(volume) => op(volume).map_err(errno),
        None => Err(Errno::EIO),
    }
}

/// The node's path inside the volume.
fn path_of(node: &Node) -> Result<String, Errno> {
    match &node.inner.lock().stored {
        Some(stored) if !stored.gone => Ok(stored.path.clone()),
        Some(_) => Err(Errno::ESTALE),
        None => Err(Errno::EIO),
    }
}

/// FNV-1a over the path, with bit 62 set so the number cannot be one the
/// in-memory tree hands out, which count up from 1.
fn ino_for(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash | (1 << 62)
}

/// The live node for `path`, or a new one made from `entry`.
fn node_for(path: String, entry: &Entry) -> NodeRef {
    let mut nodes = NODES.lock();
    if let Some(node) = nodes.get(&path).and_then(Weak::upgrade) {
        return node;
    }
    if nodes.len() >= SWEEP_AT {
        nodes.retain(|_, node| node.strong_count() > 0);
    }
    let node = Node::new_stored(ino_for(&path), entry.is_dir, path.clone(), entry.len as u64, entry.mtime);
    nodes.insert(path, Arc::downgrade(&node));
    node
}

/// Paths in the table at or under `path`.
fn under(nodes: &BTreeMap<String, Weak<Node>>, path: &str) -> Vec<String> {
    let prefix = format!("{}/", path);
    nodes.keys().filter(|key| key.as_str() == path || key.starts_with(&prefix)).cloned().collect()
}

/// The entry at `path` is gone: mark its node, and any under it, and forget
/// them.
fn detach(path: &str) {
    let mut nodes = NODES.lock();
    for key in under(&nodes, path) {
        if let Some(node) = nodes.remove(&key).and_then(|weak| weak.upgrade()) {
            if let Some(stored) = node.inner.lock().stored.as_mut() {
                stored.gone = true;
            }
        }
    }
}

/// The entry at `old` is now at `new`, and so is everything under it.
fn rekey(old: &str, new: &str) {
    let mut nodes = NODES.lock();
    for key in under(&nodes, old) {
        let Some(weak) = nodes.remove(&key) else { continue };
        let moved = format!("{}{}", new, &key[old.len()..]);
        if let Some(node) = weak.upgrade() {
            if let Some(stored) = node.inner.lock().stored.as_mut() {
                stored.path = moved.clone();
            }
            nodes.insert(moved, weak);
        }
    }
}

// ---------------------------------------------------------------------------
// What the tree calls
// ---------------------------------------------------------------------------

/// Put the mounted volume at /data.
pub fn publish(volume: Volume<Partition>, device: String) {
    *VOLUME.lock() = Some(volume);
    let root = Node::new_stored(ino_for(""), true, String::new(), 0, crate::time::unix_time());
    NODES.lock().insert(String::new(), Arc::downgrade(&root));
    *ROOT.lock() = Some(root.clone());
    *MOUNT_LINE.lock() = Some(format!("{} /data vfat rw 0 0\n", device));
    crate::fs::root().inner.lock().children.insert(String::from("data"), root);
}

pub fn mounted() -> bool {
    ROOT.lock().is_some()
}

pub fn lookup(dir: &NodeRef, name: &str) -> Result<NodeRef, Errno> {
    let dir_path = path_of(dir)?;
    let entry = with_volume(|volume| volume.lookup(&dir_path, name))?;
    Ok(node_for(join(&dir_path, &entry.name), &entry))
}

pub fn readdir(dir: &NodeRef) -> Result<Vec<DirEntry>, Errno> {
    let dir_path = path_of(dir)?;
    let entries = with_volume(|volume| volume.list(&dir_path))?;
    let mut out = Vec::with_capacity(entries.len() + 2);
    out.push(DirEntry { ino: dir.ino, kind: DT_DIR, name: String::from(".") });
    out.push(DirEntry { ino: dir.ino, kind: DT_DIR, name: String::from("..") });
    for entry in entries {
        out.push(DirEntry {
            ino: ino_for(&join(&dir_path, &entry.name)),
            kind: if entry.is_dir { DT_DIR } else { DT_REG },
            name: entry.name,
        });
    }
    Ok(out)
}

pub fn read(node: &Node, offset: Offset, buf: &mut [u8]) -> Result<usize, Errno> {
    let path = path_of(node)?;
    let want = offset.range(buf.len())?;
    with_volume(|volume| volume.read(&path, want.start as u64, buf))
}

pub fn write(node: &Node, offset: Offset, buf: &[u8]) -> Result<usize, Errno> {
    let path = path_of(node)?;
    let want = offset.range(buf.len())?;
    let written = with_volume(|volume| volume.write(&path, want.start as u64, buf))?;
    let mut inner = node.inner.lock();
    if let Some(stored) = inner.stored.as_mut() {
        stored.len = stored.len.max((want.start + written) as u64);
    }
    inner.mtime = crate::time::unix_time();
    Ok(written)
}

pub fn truncate(node: &Node, len: Offset) -> Result<(), Errno> {
    let path = path_of(node)?;
    let len = len.raw();
    with_volume(|volume| volume.truncate(&path, len))?;
    let mut inner = node.inner.lock();
    if let Some(stored) = inner.stored.as_mut() {
        stored.len = len;
    }
    inner.mtime = crate::time::unix_time();
    Ok(())
}

pub fn create(dir: &NodeRef, name: &str) -> Result<NodeRef, Errno> {
    let dir_path = path_of(dir)?;
    let entry = with_volume(|volume| volume.create(&dir_path, name))?;
    Ok(node_for(join(&dir_path, &entry.name), &entry))
}

pub fn mkdir(dir: &NodeRef, name: &str) -> Result<NodeRef, Errno> {
    let dir_path = path_of(dir)?;
    let entry = with_volume(|volume| volume.mkdir(&dir_path, name))?;
    Ok(node_for(join(&dir_path, &entry.name), &entry))
}

pub fn unlink(dir: &NodeRef, name: &str, want_dir: bool) -> Result<(), Errno> {
    let dir_path = path_of(dir)?;
    let removed = with_volume(|volume| {
        let entry = volume.lookup(&dir_path, name)?;
        volume.remove(&dir_path, name, want_dir)?;
        Ok(join(&dir_path, &entry.name))
    })?;
    detach(&removed);
    Ok(())
}

pub fn rename(from_dir: &NodeRef, from_name: &str, to_dir: &NodeRef, to_name: &str) -> Result<(), Errno> {
    let from = path_of(from_dir)?;
    let to = path_of(to_dir)?;
    let (old, new, replaced) = with_volume(|volume| {
        let source = volume.lookup(&from, from_name)?;
        let existing = volume.lookup(&to, to_name).ok();
        let name = volume.rename(&from, from_name, &to, to_name)?;
        Ok((join(&from, &source.name), join(&to, &name), existing.map(|entry| join(&to, &entry.name))))
    })?;
    if let Some(replaced) = replaced {
        if replaced != old {
            detach(&replaced);
        }
    }
    rekey(&old, &new);
    Ok(())
}

pub fn fsync(node: &Node) -> Result<(), Errno> {
    let path = path_of(node)?;
    with_volume(|volume| volume.fsync(&path))
}

pub fn sync() -> Result<(), Errno> {
    if !mounted() {
        return Ok(());
    }
    with_volume(|volume| volume.sync())
}

/// Cluster size, total clusters and free clusters.
pub fn statfs(_node: &Node) -> Result<(u64, u64, u64), Errno> {
    let (cluster, total, free) = with_volume(|volume| volume.stats())?;
    Ok((cluster as u64, total as u64, free as u64))
}

pub fn mounts() -> Option<String> {
    MOUNT_LINE.lock().clone()
}
