//! The file allocation table (fatgen103, "FAT Data Structure").
//!
//! A FAT32 entry is 32 bits on the card, of which the low 28 are the value:
//! 0 for a free cluster, the next cluster's number, 0x0FFFFFF7 for a bad
//! cluster, and 0x0FFFFFF8 or more for the end of a chain. The high 4 bits are
//! reserved and kept as found when an entry is written. Anything else, 1 or a
//! number past the last cluster, is a damaged entry, and a chain that reaches
//! one, or a free or bad cluster, is an error.
//!
//! **Copies.** BPB_ExtFlags bit 7 clear means every FAT copy is kept the same:
//! the first is read, and a change is written to all. Bit 7 set means only the
//! copy bits 0 to 3 name is in use, and it is the only one read or written.
//!
//! **Bounds.** A chain has at most as many links as the volume has clusters,
//! so a walk that goes further has visited a cluster twice, which is a loop,
//! and is an error. Scans for free clusters cover each entry once.
//!
//! **What is kept in memory.** The count of free clusters, once the FAT has
//! been counted, and where the last search for a free cluster found one.
//! FSInfo's copies of both are hints (fatgen103 says they may be wrong), so
//! the count is taken from the FAT itself the first time it is needed, and
//! the hint only decides where a search starts.

use super::boot::Layout;
use super::cache::{Blocks, Cache, BLOCK};
use super::{le32, FsError};
use alloc::vec;
use alloc::vec::Vec;

const MASK: u32 = 0x0FFF_FFFF;
const RESERVED_BITS: u32 = 0xF000_0000;
pub const FREE: u32 = 0;
pub const BAD: u32 = 0x0FFF_FFF7;
const END_MIN: u32 = 0x0FFF_FFF8;
pub const END: u32 = 0x0FFF_FFFF;

/// FAT[1]'s two flags on FAT32 (fatgen103, "FAT Data Structure"): set when
/// the volume was dismounted cleanly (ClnShutBitMask), and set when no disk
/// I/O error was met (HrdErrBitMask). Windows and macOS read the first to
/// decide whether a volume needs checking; this code clears it before its
/// first write and sets it again at sync and unmount, and leaves the second as
/// found.
pub const CLEAN_SHUTDOWN: u32 = 0x0800_0000;
pub const NO_HARD_ERROR: u32 = 0x0400_0000;

const ENTRIES_PER_BLOCK: u32 = (BLOCK / 4) as u32;
/// FAT sectors read in one device call while counting or searching.
const SCAN_BLOCKS: u32 = 256;

/// What a FAT entry says about the cluster after its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Free,
    Next(u32),
    End,
    Bad,
    Damaged(u32),
}

pub struct Fat {
    pub layout: Layout,
    /// Free clusters, once counted.
    free: Option<u32>,
    /// Where the next search for a free cluster starts: always a cluster
    /// number of the volume.
    cursor: u32,
}

impl Fat {
    /// The table of a volume with `layout`, searching for free clusters from
    /// `hint` when it names a cluster.
    pub fn new(layout: Layout, hint: u32) -> Fat {
        let cursor = if layout.is_cluster(hint) { hint } else { 2 };
        Fat { layout, free: None, cursor }
    }

    fn read_raw<B: Blocks>(&self, cache: &mut Cache<B>, copy: u32, entry: u32) -> Result<u32, FsError> {
        let mut bytes = [0u8; 4];
        cache.read_at(self.layout.fat_offset(copy, entry), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn write_raw<B: Blocks>(&self, cache: &mut Cache<B>, copy: u32, entry: u32, value: u32) -> Result<(), FsError> {
        cache.write_at(self.layout.fat_offset(copy, entry), &value.to_le_bytes())?;
        Ok(())
    }

    fn classify(&self, value: u32) -> Link {
        match value & MASK {
            FREE => Link::Free,
            next if self.layout.is_cluster(next) => Link::Next(next),
            BAD => Link::Bad,
            end if end >= END_MIN => Link::End,
            other => Link::Damaged(other),
        }
    }

    /// The entry of `cluster`, which must be a cluster of the volume.
    pub fn link<B: Blocks>(&self, cache: &mut Cache<B>, cluster: u32) -> Result<Link, FsError> {
        if !self.layout.is_cluster(cluster) {
            return Err(FsError::Corrupt);
        }
        let value = self.read_raw(cache, self.layout.read_copy(), cluster)?;
        Ok(self.classify(value))
    }

    /// The cluster after `cluster` in a chain, or nothing at the chain's end.
    /// A free, bad or damaged entry inside a chain is corruption.
    pub fn next<B: Blocks>(&self, cache: &mut Cache<B>, cluster: u32) -> Result<Option<u32>, FsError> {
        match self.link(cache, cluster)? {
            Link::Next(next) => Ok(Some(next)),
            Link::End => Ok(None),
            Link::Free | Link::Bad | Link::Damaged(_) => Err(FsError::Corrupt),
        }
    }

    /// Set the entry of `cluster` to `value`, in every copy that is written,
    /// keeping each copy's reserved high bits.
    pub fn set<B: Blocks>(&self, cache: &mut Cache<B>, cluster: u32, value: u32) -> Result<(), FsError> {
        if !self.layout.is_cluster(cluster) {
            return Err(FsError::Corrupt);
        }
        for copy in self.layout.written_copies() {
            let old = self.read_raw(cache, copy, cluster)?;
            self.write_raw(cache, copy, cluster, (old & RESERVED_BITS) | (value & MASK))?;
        }
        Ok(())
    }

    /// The length of the chain from `first` and its last cluster. A loop, or a
    /// link to a free, bad or damaged entry, is corruption.
    pub fn walk<B: Blocks>(&self, cache: &mut Cache<B>, first: u32) -> Result<(u32, u32), FsError> {
        if !self.layout.is_cluster(first) {
            return Err(FsError::Corrupt);
        }
        let (mut count, mut cluster) = (1u32, first);
        loop {
            match self.link(cache, cluster)? {
                Link::End => return Ok((count, cluster)),
                Link::Next(next) => {
                    if count >= self.layout.clusters {
                        return Err(FsError::Corrupt);
                    }
                    count += 1;
                    cluster = next;
                }
                Link::Free | Link::Bad | Link::Damaged(_) => return Err(FsError::Corrupt),
            }
        }
    }

    /// Free the `count` clusters of the chain from `first`, which `walk` has
    /// measured.
    pub fn free_chain<B: Blocks>(&mut self, cache: &mut Cache<B>, first: u32, count: u32) -> Result<(), FsError> {
        let mut cluster = first;
        for i in 0..count {
            let link = self.link(cache, cluster)?;
            self.set(cache, cluster, FREE)?;
            if i + 1 < count {
                match link {
                    Link::Next(next) => cluster = next,
                    _ => return Err(FsError::Corrupt),
                }
            }
        }
        self.gave(count);
        Ok(())
    }

    /// Whether FAT[1] says the volume was dismounted cleanly, in the copy that
    /// is read.
    pub fn clean<B: Blocks>(&self, cache: &mut Cache<B>) -> Result<bool, FsError> {
        Ok(self.read_raw(cache, self.layout.read_copy(), 1)? & CLEAN_SHUTDOWN != 0)
    }

    /// Set or clear FAT[1]'s clean-shutdown flag in every copy that is
    /// written, leaving the rest of the entry, the hard-error flag with it, as
    /// found. Returns whether anything changed.
    pub fn set_clean<B: Blocks>(&self, cache: &mut Cache<B>, clean: bool) -> Result<bool, FsError> {
        let mut changed = false;
        for copy in self.layout.written_copies() {
            let old = self.read_raw(cache, copy, 1)?;
            let new = if clean { old | CLEAN_SHUTDOWN } else { old & !CLEAN_SHUTDOWN };
            if new != old {
                self.write_raw(cache, copy, 1, new)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Visit the entries from `from` up to, not including, `to`, reading whole
    /// FAT sectors, until `visit` returns false. `to` is at most
    /// `clusters + 2`, so every sector read is inside the FAT.
    fn scan<B: Blocks>(&self, cache: &mut Cache<B>, from: u32, to: u32, mut visit: impl FnMut(u32, Link) -> bool) -> Result<(), FsError> {
        let to = to.min(self.layout.clusters + 2);
        let copy = self.layout.read_copy();
        let mut buf = vec![0u8; SCAN_BLOCKS as usize * BLOCK];
        let mut entry = from;
        while entry < to {
            let block = entry / ENTRIES_PER_BLOCK;
            let blocks = ((to - 1) / ENTRIES_PER_BLOCK - block + 1).min(SCAN_BLOCKS);
            let first = block * ENTRIES_PER_BLOCK;
            let bytes = buf.get_mut(..blocks as usize * BLOCK).ok_or(FsError::Corrupt)?;
            cache.read_at(self.layout.fat_offset(copy, first), bytes)?;
            for (i, raw) in bytes.chunks_exact(4).enumerate() {
                let at = first + i as u32;
                if at < entry {
                    continue;
                }
                if at >= to {
                    break;
                }
                if !visit(at, self.classify(le32(raw, 0))) {
                    return Ok(());
                }
            }
            entry = first + blocks * ENTRIES_PER_BLOCK;
        }
        Ok(())
    }

    /// The count of free clusters, from the FAT the first time it is asked
    /// for and kept up to date after that.
    pub fn free_count<B: Blocks>(&mut self, cache: &mut Cache<B>) -> Result<u32, FsError> {
        if let Some(free) = self.free {
            return Ok(free);
        }
        let mut free = 0u32;
        self.scan(cache, 2, self.layout.clusters + 2, |_, link| {
            if link == Link::Free {
                free += 1;
            }
            true
        })?;
        self.free = Some(free);
        Ok(free)
    }

    pub fn known_free(&self) -> Option<u32> {
        self.free
    }

    /// Forget the count, after an operation stopped between its writes.
    pub fn forget_free(&mut self) {
        self.free = None;
    }

    /// `count` clusters are now in use.
    pub fn took(&mut self, count: u32) {
        if let Some(free) = self.free.as_mut() {
            *free = free.saturating_sub(count);
        }
    }

    /// `count` clusters are now free.
    pub fn gave(&mut self, count: u32) {
        if let Some(free) = self.free.as_mut() {
            *free = free.saturating_add(count).min(self.layout.clusters);
        }
    }

    /// Append to `out` up to `want` free clusters, searching from the cursor
    /// to the last cluster and then from cluster 2. Nothing is marked: the
    /// caller writes what goes in them and then links them. A search that
    /// covered the whole FAT without finding `want` has counted every free
    /// cluster, and the count is kept.
    pub fn find_free<B: Blocks>(&mut self, cache: &mut Cache<B>, want: u32, out: &mut Vec<u32>) -> Result<(), FsError> {
        if want == 0 || self.free == Some(0) {
            return Ok(());
        }
        let start = self.cursor;
        let mut found = 0u32;
        for (from, to) in [(start, self.layout.clusters + 2), (2, start)] {
            if found >= want {
                break;
            }
            self.scan(cache, from, to, |cluster, link| {
                if link == Link::Free {
                    out.push(cluster);
                    found += 1;
                }
                found < want
            })?;
        }
        if found < want {
            self.free = Some(found);
        }
        if let Some(&first) = out.first() {
            if self.layout.is_cluster(first) {
                self.cursor = first;
            }
        }
        Ok(())
    }

    /// FSInfo's next-free hint: a cluster that is free now, or the value that
    /// means nothing is known. The macOS fsck_msdos reports a hint that names
    /// a cluster in use.
    pub fn next_free_hint<B: Blocks>(&mut self, cache: &mut Cache<B>) -> Result<u32, FsError> {
        let mut one = Vec::new();
        self.find_free(cache, 1, &mut one)?;
        Ok(one.first().copied().unwrap_or(super::boot::FSI_UNKNOWN))
    }
}
