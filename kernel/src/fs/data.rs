//! Nodes whose contents are on the data volume rather than in memory.
//!
//! Everywhere else in the tree a file's bytes are in its node. A node of kind
//! `DataFile` or `DataDir` holds only where its entry is on the FAT32 volume
//! mounted at /data, and the functions below are where the tree hands such a
//! node to `storage::vfs`. The volume exists only on the Raspberry Pi 4. On
//! x86-64 no such node is ever made, and these answer as for a name that is
//! not there.

use alloc::string::String;

/// Where a node on the data volume is, and what the kernel knows of it
/// without asking the card.
pub struct Stored {
    /// Its path inside the volume: empty for the root, `www/index.html` below
    /// it.
    pub path: String,
    /// A file's length in bytes. The volume is changed only through this
    /// kernel while it is mounted, so the copy stays current.
    pub len: u64,
    /// The entry was removed, or replaced by a rename. A descriptor still open
    /// on the node gets ESTALE.
    pub gone: bool,
}

#[cfg(target_arch = "aarch64")]
pub use crate::storage::vfs::{create, fsync, lookup, mkdir, mounts, read, readdir, rename, statfs, sync, truncate, unlink, write};

#[cfg(not(target_arch = "aarch64"))]
pub use absent::*;

#[cfg(not(target_arch = "aarch64"))]
mod absent {
    use super::super::{DirEntry, Node, NodeRef, Offset};
    use crate::abi::Errno;
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn lookup(_dir: &NodeRef, _name: &str) -> Result<NodeRef, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn readdir(_dir: &NodeRef) -> Result<Vec<DirEntry>, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn read(_node: &Node, _offset: Offset, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn write(_node: &Node, _offset: Offset, _buf: &[u8]) -> Result<usize, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn truncate(_node: &Node, _len: Offset) -> Result<(), Errno> {
        Err(Errno::ENOENT)
    }
    pub fn create(_dir: &NodeRef, _name: &str) -> Result<NodeRef, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn mkdir(_dir: &NodeRef, _name: &str) -> Result<NodeRef, Errno> {
        Err(Errno::ENOENT)
    }
    pub fn unlink(_dir: &NodeRef, _name: &str, _want_dir: bool) -> Result<(), Errno> {
        Err(Errno::ENOENT)
    }
    pub fn rename(_from: &NodeRef, _from_name: &str, _to: &NodeRef, _to_name: &str) -> Result<(), Errno> {
        Err(Errno::ENOENT)
    }
    pub fn fsync(_node: &Node) -> Result<(), Errno> {
        Ok(())
    }
    pub fn sync() -> Result<(), Errno> {
        Ok(())
    }
    pub fn statfs(_node: &Node) -> Result<(u64, u64, u64), Errno> {
        Err(Errno::ENOENT)
    }
    pub fn mounts() -> Option<String> {
        None
    }
}
