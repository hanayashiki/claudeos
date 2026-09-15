//! The volume's blocks, and the cache in front of them.
//!
//! The FAT code addresses the volume as bytes: four for a FAT entry, thirty-two
//! for a directory entry, a cluster for file contents. A card moves 512-byte
//! blocks, one command at a time, and a command costs milliseconds. This file
//! sits between the two.
//!
//! **What is kept in memory.** A block read for a request smaller than a block
//! is kept in a cache of at most `CACHE_BLOCKS` blocks, 4 MiB, so directory
//! scans and FAT lookups after the first are memory reads. When the cache is
//! full, a block that nothing has written is evicted. A request aligned to whole
//! blocks, which is what file contents are, goes to the device directly and is
//! not cached, so reading a large file does not push the metadata out.
//!
//! **What is written when.** A write to part of a block changes the cached copy
//! and marks it dirty; a write of whole blocks goes to the device at once.
//! Dirty blocks go to the device in `flush`, in block order. The volume decides
//! when to flush: its write ordering (see `volume.rs`) is a sequence of phases,
//! each ended by a flush, and one flush writes only the blocks its own phase
//! changed. A cache full of dirty blocks flushes them to make room, which also
//! writes only the current phase's blocks, because the phase before it was
//! flushed when it ended.
//!
//! **Bounds.** Every request is checked against the length of the blocks given,
//! and a position past it is `OutOfRange`: positions are computed from numbers
//! on the card, and a damaged number must not become a block that belongs to
//! something else. `Partition` in the kernel checks the same bound again before
//! a command reaches the card.
//!
//! This file uses nothing from the kernel, so `tools/fatdisk` compiles it on
//! the Mac.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Bytes in a block. SD cards and the FAT volumes on them use 512.
pub const BLOCK: usize = 512;
const BLOCK_U64: u64 = BLOCK as u64;

/// The most blocks kept in memory: 4 MiB.
pub const CACHE_BLOCKS: usize = 8192;

/// The most blocks one call to the device moves. A PIO transfer of 128 KiB is
/// tens of milliseconds, which keeps one command well inside the controller's
/// ten-second request timeout.
pub const DIRECT_MAX: usize = 256;

/// Where the blocks come from: the chosen partition of the card in the kernel,
/// memory or a region of an image file in `tools/fatdisk`.
pub trait Blocks {
    /// How many blocks there are. Nothing asks for a block at or past this.
    fn count(&self) -> u64;
    /// Fill `buf`, a whole number of blocks, starting at `block`.
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError>;
    /// Write `data`, a whole number of blocks, starting at `block`.
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError>;
}

/// The device failed. What went wrong is its own to report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceError;

/// Why a read, write or flush failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheError {
    /// The device failed a read or a write.
    Device,
    /// A position at or past the end of the blocks.
    OutOfRange,
    /// A write while writes are refused.
    ReadOnly,
}

struct Slot {
    block: u64,
    data: [u8; BLOCK],
    /// Written in memory and not yet on the device.
    dirty: bool,
    /// Used since the eviction hand last passed.
    used: bool,
}

pub struct Cache<B: Blocks> {
    blocks: B,
    slots: Vec<Slot>,
    /// Block number to its slot.
    index: BTreeMap<u64, usize>,
    hand: usize,
    writable: bool,
}

impl<B: Blocks> Cache<B> {
    /// A cache over `blocks`, refusing writes until `set_writable`.
    pub fn new(blocks: B) -> Cache<B> {
        Cache { blocks, slots: Vec::new(), index: BTreeMap::new(), hand: 0, writable: false }
    }

    pub fn device(&self) -> &B {
        &self.blocks
    }

    pub fn device_mut(&mut self) -> &mut B {
        &mut self.blocks
    }

    /// The blocks back, with whatever is cached and not flushed dropped.
    pub fn into_device(self) -> B {
        self.blocks
    }

    /// The length of the volume in bytes.
    pub fn bytes(&self) -> u64 {
        self.blocks.count().saturating_mul(BLOCK_U64)
    }

    pub fn set_writable(&mut self, on: bool) {
        self.writable = on;
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Whether any block is written in memory and not on the device.
    pub fn has_dirty(&self) -> bool {
        self.slots.iter().any(|slot| slot.dirty)
    }

    /// Throw away every cached block, written or not. After a failed device
    /// command what the device holds is not known, and after an operation that
    /// stopped between two flushes its unflushed blocks must not be written.
    pub fn discard(&mut self) {
        self.slots.clear();
        self.index.clear();
        self.hand = 0;
    }

    fn check(&self, offset: u64, len: usize) -> Result<(), CacheError> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.bytes() => Ok(()),
            _ => Err(CacheError::OutOfRange),
        }
    }

    fn device_read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), CacheError> {
        let count = (buf.len() / BLOCK) as u64;
        if block.checked_add(count).map_or(true, |end| end > self.blocks.count()) {
            return Err(CacheError::OutOfRange);
        }
        self.blocks.read(block, buf).map_err(|_| CacheError::Device)
    }

    fn device_write(&mut self, block: u64, data: &[u8]) -> Result<(), CacheError> {
        let count = (data.len() / BLOCK) as u64;
        if block.checked_add(count).map_or(true, |end| end > self.blocks.count()) {
            return Err(CacheError::OutOfRange);
        }
        self.blocks.write(block, data).map_err(|_| CacheError::Device)
    }

    /// The slot holding `block`, read from the device if it is not cached.
    fn slot(&mut self, block: u64) -> Result<usize, CacheError> {
        if let Some(&at) = self.index.get(&block) {
            if let Some(slot) = self.slots.get_mut(at) {
                slot.used = true;
                return Ok(at);
            }
        }
        let mut data = [0u8; BLOCK];
        self.device_read(block, &mut data)?;
        let fresh = Slot { block, data, dirty: false, used: true };
        let at = if self.slots.len() < CACHE_BLOCKS {
            self.slots.push(fresh);
            self.slots.len() - 1
        } else {
            let victim = match self.victim() {
                Some(victim) => victim,
                None => {
                    // Every cached block is written and not yet on the
                    // device, so put them there and take any of them.
                    self.flush()?;
                    self.victim().ok_or(CacheError::Device)?
                }
            };
            let old = self.slots.get(victim).map(|slot| slot.block).ok_or(CacheError::Device)?;
            self.index.remove(&old);
            *self.slots.get_mut(victim).ok_or(CacheError::Device)? = fresh;
            victim
        };
        self.index.insert(block, at);
        Ok(at)
    }

    /// A clean slot not used since the hand last passed it, clearing the used
    /// marks on the way. Two passes find one if any slot is clean.
    fn victim(&mut self) -> Option<usize> {
        let len = self.slots.len();
        if len == 0 {
            return None;
        }
        for _ in 0..2 * len {
            let at = self.hand % len;
            self.hand = (at + 1) % len;
            let slot = self.slots.get_mut(at)?;
            if slot.dirty {
                continue;
            }
            if slot.used {
                slot.used = false;
                continue;
            }
            return Some(at);
        }
        None
    }

    /// Read `buf.len()` bytes at `offset`.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), CacheError> {
        self.check(offset, buf.len())?;
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let block = at / BLOCK_U64;
            let within = (at % BLOCK_U64) as usize;
            let left = buf.len() - done;
            if within == 0 && left >= BLOCK {
                let count = (left / BLOCK).min(DIRECT_MAX);
                let bytes = count * BLOCK;
                let part = buf.get_mut(done..done + bytes).ok_or(CacheError::OutOfRange)?;
                self.device_read(block, part)?;
                // A cached block written in memory is newer than the device.
                for (&cached, &slot) in self.index.range(block..block + count as u64) {
                    let Some(slot) = self.slots.get(slot) else { continue };
                    if slot.dirty {
                        let start = ((cached - block) as usize) * BLOCK;
                        if let Some(target) = part.get_mut(start..start + BLOCK) {
                            target.copy_from_slice(&slot.data);
                        }
                    }
                }
                done += bytes;
            } else {
                let slot = self.slot(block)?;
                let n = (BLOCK - within).min(left);
                let source = self.slots.get(slot).and_then(|slot| slot.data.get(within..within + n)).ok_or(CacheError::OutOfRange)?;
                buf.get_mut(done..done + n).ok_or(CacheError::OutOfRange)?.copy_from_slice(source);
                done += n;
            }
        }
        Ok(())
    }

    /// Write `data` at `offset`.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), CacheError> {
        if !self.writable {
            return Err(CacheError::ReadOnly);
        }
        self.check(offset, data.len())?;
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let block = at / BLOCK_U64;
            let within = (at % BLOCK_U64) as usize;
            let left = data.len() - done;
            if within == 0 && left >= BLOCK {
                let count = (left / BLOCK).min(DIRECT_MAX);
                let bytes = count * BLOCK;
                let part = data.get(done..done + bytes).ok_or(CacheError::OutOfRange)?;
                self.device_write(block, part)?;
                // What the device now holds replaces any cached copy, written
                // in memory or not.
                for (&cached, &slot) in self.index.range(block..block + count as u64) {
                    let start = ((cached - block) as usize) * BLOCK;
                    if let (Some(slot), Some(source)) = (self.slots.get_mut(slot), part.get(start..start + BLOCK)) {
                        slot.data.copy_from_slice(source);
                        slot.dirty = false;
                    }
                }
                done += bytes;
            } else {
                let slot = self.slot(block)?;
                let n = (BLOCK - within).min(left);
                let source = data.get(done..done + n).ok_or(CacheError::OutOfRange)?;
                let slot = self.slots.get_mut(slot).ok_or(CacheError::OutOfRange)?;
                slot.data.get_mut(within..within + n).ok_or(CacheError::OutOfRange)?.copy_from_slice(source);
                slot.dirty = true;
                done += n;
            }
        }
        Ok(())
    }

    /// Put every block written in memory on the device, in block order, in as
    /// few commands as the runs of adjacent blocks allow.
    pub fn flush(&mut self) -> Result<(), CacheError> {
        let dirty: Vec<(u64, usize)> = self
            .index
            .iter()
            .filter(|(_, &slot)| self.slots.get(slot).is_some_and(|slot| slot.dirty))
            .map(|(&block, &slot)| (block, slot))
            .collect();
        let mut run: Vec<u8> = Vec::new();
        let mut i = 0;
        while let Some(&(first, _)) = dirty.get(i) {
            let mut j = i;
            run.clear();
            while let Some(&(block, slot)) = dirty.get(j) {
                if block != first + (j - i) as u64 || j - i >= DIRECT_MAX {
                    break;
                }
                if let Some(slot) = self.slots.get(slot) {
                    run.extend_from_slice(&slot.data);
                }
                j += 1;
            }
            self.device_write(first, &run)?;
            for &(_, slot) in dirty.get(i..j).unwrap_or(&[]) {
                if let Some(slot) = self.slots.get_mut(slot) {
                    slot.dirty = false;
                }
            }
            i = j;
        }
        Ok(())
    }
}
