//! FAT32 for /data, written for this kernel.
//!
//! The references are Microsoft's "FAT32 File System Specification"
//! (fatgen103), cited by section, and Linux's fs/fat, cited by function.
//! Nothing here uses the kernel: `tools/fatdisk` compiles these same files on
//! the Mac to test them against macOS's own FAT tools and against damaged
//! images.
//!
//! - `cache`: the volume's blocks and the cache in front of them.
//! - `boot`: the boot sector, the BPB and FSInfo, checked.
//! - `table`: the FAT, its copies, chains, free clusters, and FAT[1]'s flags.
//! - `name`: which names are allowed, comparing them, short names and long
//!   names.
//! - `dir`: directory entries, scanning them, and finding room.
//! - `time`: FAT timestamps.
//! - `volume`: the operations, and the order their writes reach the card in.
//!
//! **What it does.** FAT32 on 512-byte sectors, and nothing else: FAT12,
//! FAT16 and exFAT are refused when the volume is looked at, and nothing here
//! formats a volume. Lookup, listing, read, write, truncate and extend,
//! create, mkdir, unlink, rmdir of an empty directory, rename within and
//! across directories, stat through the entries, statfs, fsync and sync.
//!
//! **No panics.** A panic restarts the board, which then reads the same card
//! again, so nothing read from the card can cause one:
//!
//! - Every number read off the card is checked before it is used for
//!   anything: `boot::parse` checks the BPB so that a cluster number from 2 to
//!   `clusters + 1` gives a position inside the volume, `Layout::is_cluster` is
//!   asked before any cluster number is turned into a position, and the cache
//!   refuses a position past the end of the volume.
//! - Bytes are taken out of buffers with `get`, the helpers below, or
//!   destructuring, never with an index that can be out of range; arithmetic on
//!   card numbers is done after a bound check or with a checked operation.
//! - Every walk along a cluster chain is bounded by the volume's cluster count,
//!   and a directory by its 65536 entries, and going past either is an error.
//! - Every failure is an `FsError`: `Io` (EIO) when the card failed a command,
//!   `Corrupt` (EUCLEAN) when what is on the card contradicts itself.
//!
//! None of these files calls `unwrap`, `expect`, `panic!` or `unreachable!`,
//! or indexes a buffer with `[]`. What else in Rust can panic is used only
//! where the line before it rules the panic out: a subtraction after a
//! comparison that keeps it from going below zero, a division by a constant
//! or by a cluster size `boot::parse` has held to 512 bytes or more,
//! `copy_from_slice` between two ranges of the same length, and `Vec::remove`
//! at an index `position` has just found.
//!
//! `tools/fatdisk fuzz` runs these files, built with overflow checks, against
//! images with random bytes over the boot sector, the FATs and the
//! directories.

pub mod boot;
pub mod cache;
pub mod dir;
pub mod name;
pub mod table;
pub mod time;
pub mod volume;

pub use boot::{looks_like_boot_sector, probe, Probe};
pub use cache::{Blocks, DeviceError, BLOCK};
pub use volume::{Entry, Volume};

use alloc::format;
use alloc::string::String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NoSpace,
    NameTooLong,
    /// A name FAT cannot hold.
    BadName,
    /// Past FAT's limit of 4 GiB less one byte on a file.
    TooBig,
    /// A directory moved into itself or below itself.
    Invalid,
    /// The card failed a command.
    Io,
    /// The volume's structures contradict themselves: a chain that loops or
    /// leads to a free, bad or damaged FAT entry, a cluster number outside the
    /// volume, a chain shorter than its file, a directory without `..`.
    Corrupt,
    /// A write before the volume was allowed writes.
    ReadOnly,
}

impl From<cache::CacheError> for FsError {
    fn from(error: cache::CacheError) -> FsError {
        match error {
            cache::CacheError::Device => FsError::Io,
            // Every position comes from a number on the card, so one outside
            // the volume is a damaged number.
            cache::CacheError::OutOfRange => FsError::Corrupt,
            cache::CacheError::ReadOnly => FsError::ReadOnly,
        }
    }
}

/// `path` with `name` under it; paths inside the volume are `""` for its root
/// and `www/index.html` below it.
pub fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        String::from(name)
    } else {
        format!("{}/{}", path, name)
    }
}

/// Little-endian fields of an on-card structure. A field that does not fit in
/// `bytes` reads as zero and is not written; callers pass fixed offsets inside
/// fixed-size structures, so neither happens.
pub fn le16(bytes: &[u8], at: usize) -> u16 {
    match at.checked_add(2).and_then(|end| bytes.get(at..end)) {
        Some(&[a, b]) => u16::from_le_bytes([a, b]),
        _ => 0,
    }
}

pub fn le32(bytes: &[u8], at: usize) -> u32 {
    match at.checked_add(4).and_then(|end| bytes.get(at..end)) {
        Some(&[a, b, c, d]) => u32::from_le_bytes([a, b, c, d]),
        _ => 0,
    }
}

pub fn put8(bytes: &mut [u8], at: usize, value: u8) {
    if let Some(slot) = bytes.get_mut(at) {
        *slot = value;
    }
}

pub fn put16(bytes: &mut [u8], at: usize, value: u16) {
    for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
        put8(bytes, at.saturating_add(i), byte);
    }
}

pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
        put8(bytes, at.saturating_add(i), byte);
    }
}
