//! The byte stream rust-fatfs reads and writes, made of whole blocks.
//!
//! rust-fatfs addresses its volume as one long run of bytes and moves a few
//! at a time: four for a FAT entry, thirty-two for a directory entry, a
//! cluster for file contents. A card moves 512-byte blocks, one command at a
//! time, and a command costs milliseconds. This file sits between the two.
//!
//! **What is kept in memory.** A block read for a small request is kept in a
//! cache of at most `CACHE_BLOCKS` blocks, 4 MiB, so directory walks and FAT
//! lookups after the first are memory reads. When the cache is full a block
//! that nothing has written is evicted. A request aligned to whole blocks,
//! which is what file contents are, goes to the card directly and is not
//! cached, so reading a large file does not push the metadata out.
//!
//! **What is written when.** A write to part of a block changes the cached
//! copy and marks it dirty; a write of whole blocks goes to the card at once.
//! Dirty blocks go to the card in `flush`, which the volume calls at the end of
//! every operation, before the system call that asked for it returns. So a
//! program is told about a failed write by the call that made it, and a FAT
//! sector that one write updates for every cluster it allocates reaches the
//! card once rather than once per cluster. Nothing is left dirty between
//! operations, which is what lets `fsync` and `sync` mean what they say.
//!
//! **Bounds.** Everything the stream is asked is checked against the length of
//! the blocks it was given, and a position past it is an error: rust-fatfs
//! computes positions from numbers on the card, and a damaged number must not
//! turn into a block that belongs to something else. Each operation also has a
//! budget of calls and a deadline, because rust-fatfs follows cluster chains
//! until they end and a damaged chain can loop. Once either runs out every
//! call fails, rust-fatfs returns the error, and the operation gives up.
//!
//! This file uses nothing from the kernel, so `tools/fatdisk` compiles the
//! same code to test it against damaged images on the Mac.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

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
/// a region of an image file in `tools/fatdisk`.
pub trait Blocks {
    /// How many blocks there are. Nothing asks for a block at or past this.
    fn count(&self) -> u64;
    /// Fill `buf`, a whole number of blocks, starting at `block`.
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError>;
    /// Write `data`, a whole number of blocks, starting at `block`.
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError>;
    /// A clock in milliseconds, for the deadline on one operation.
    fn now_ms(&self) -> u64;
}

/// The device failed. What went wrong is its own to report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceError;

/// Why a read, write or seek of the stream failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    /// The device failed a read or a write.
    Device,
    /// A position at or past the end of the blocks.
    OutOfRange,
    /// A write while the volume is only being looked at.
    ReadOnly,
    /// The operation made more calls than any valid one could need.
    Budget,
    /// The operation ran past its deadline.
    Deadline,
    UnexpectedEof,
    WriteZero,
}

impl fatfs::IoError for DiskError {
    fn is_interrupted(&self) -> bool {
        false
    }
    fn new_unexpected_eof_error() -> Self {
        DiskError::UnexpectedEof
    }
    fn new_write_zero_error() -> Self {
        DiskError::WriteZero
    }
}

struct Slot {
    block: u64,
    data: [u8; BLOCK],
    /// Written in memory and not yet on the device.
    dirty: bool,
    /// Used since the eviction hand last passed.
    used: bool,
}

/// The blocks, the cache in front of them, and the current operation's limits.
pub struct State<B: Blocks> {
    blocks: B,
    slots: Vec<Slot>,
    index: BTreeMap<u64, usize>,
    hand: usize,
    writable: bool,
    calls_left: u64,
    deadline_ms: u64,
    /// Calls since the clock was last looked at.
    since_clock: u32,
    /// The device failed since `begin` was last called.
    pub failed: bool,
}

/// The state, shared between the stream rust-fatfs owns and the volume.
///
/// rust-fatfs takes its stream by value and has no way to hand it back, so the
/// stream is a handle and the volume keeps another. That is what lets the
/// volume flush after an operation, patch a directory entry rust-fatfs has no
/// call for, and mount the same blocks again after unmounting.
pub type Shared<B> = Rc<RefCell<State<B>>>;

pub fn share<B: Blocks>(blocks: B) -> Shared<B> {
    Rc::new(RefCell::new(State {
        blocks,
        slots: Vec::new(),
        index: BTreeMap::new(),
        hand: 0,
        writable: false,
        calls_left: u64::MAX,
        deadline_ms: u64::MAX,
        since_clock: 0,
        failed: false,
    }))
}

impl<B: Blocks> State<B> {
    /// The length of the stream in bytes.
    pub fn bytes(&self) -> u64 {
        self.blocks.count().saturating_mul(BLOCK_U64)
    }

    /// Allow writes. Until this is called every write fails, which is how the
    /// volume is looked at before it is known to be the one to use.
    pub fn allow_writes(&mut self) {
        self.writable = true;
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Refuse or allow writes for a while, as recovery does while it lets
    /// rust-fatfs drop what it holds without writing any of it.
    pub fn set_writable(&mut self, on: bool) {
        self.writable = on;
    }

    /// Whether the current operation's budget or deadline ran out.
    pub fn exhausted(&self) -> bool {
        self.calls_left == 0
    }

    /// Start an operation that may make `calls` calls and must be done
    /// `millis` milliseconds from now.
    pub fn begin(&mut self, calls: u64, millis: u64) {
        self.calls_left = calls;
        self.deadline_ms = self.blocks.now_ms().saturating_add(millis);
        self.since_clock = 0;
        self.failed = false;
    }

    fn charge(&mut self) -> Result<(), DiskError> {
        if self.calls_left == 0 {
            return Err(DiskError::Budget);
        }
        self.calls_left -= 1;
        self.since_clock += 1;
        if self.since_clock >= 64 {
            self.since_clock = 0;
            if self.blocks.now_ms() >= self.deadline_ms {
                self.calls_left = 0;
                return Err(DiskError::Deadline);
            }
        }
        Ok(())
    }

    /// Throw away every cached block, written or not. After the device has
    /// failed, what it holds is not known, and a cached copy is not evidence.
    pub fn discard(&mut self) {
        self.slots.clear();
        self.index.clear();
        self.hand = 0;
    }

    fn device_read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DiskError> {
        let count = (buf.len() / BLOCK) as u64;
        if block.checked_add(count).map_or(true, |end| end > self.blocks.count()) {
            return Err(DiskError::OutOfRange);
        }
        self.blocks.read(block, buf).map_err(|_| {
            self.failed = true;
            DiskError::Device
        })
    }

    fn device_write(&mut self, block: u64, data: &[u8]) -> Result<(), DiskError> {
        let count = (data.len() / BLOCK) as u64;
        if block.checked_add(count).map_or(true, |end| end > self.blocks.count()) {
            return Err(DiskError::OutOfRange);
        }
        self.blocks.write(block, data).map_err(|_| {
            self.failed = true;
            DiskError::Device
        })
    }

    /// The slot holding `block`, read from the device if it is not cached.
    fn slot(&mut self, block: u64) -> Result<usize, DiskError> {
        if let Some(&at) = self.index.get(&block) {
            self.slots[at].used = true;
            return Ok(at);
        }
        let mut data = [0u8; BLOCK];
        self.device_read(block, &mut data)?;
        let at = if self.slots.len() < CACHE_BLOCKS {
            self.slots.push(Slot { block, data, dirty: false, used: true });
            self.slots.len() - 1
        } else {
            let victim = match self.victim() {
                Some(victim) => victim,
                None => {
                    // Every cached block is written and not yet on the
                    // device, so put them there and take any of them.
                    self.flush()?;
                    self.victim().ok_or(DiskError::Device)?
                }
            };
            let old = self.slots[victim].block;
            self.index.remove(&old);
            self.slots[victim] = Slot { block, data, dirty: false, used: true };
            victim
        };
        self.index.insert(block, at);
        Ok(at)
    }

    /// A clean slot not used since the hand last passed it, clearing the used
    /// marks on the way. Two passes find one if any slot is clean.
    fn victim(&mut self) -> Option<usize> {
        let len = self.slots.len();
        for _ in 0..2 * len {
            let at = self.hand;
            self.hand = (self.hand + 1) % len;
            let slot = &mut self.slots[at];
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

    /// Read `buf.len()` bytes at `offset`, which the caller has checked lie
    /// inside the stream.
    pub fn read_at(&mut self, mut offset: u64, buf: &mut [u8]) -> Result<(), DiskError> {
        let mut done = 0;
        while done < buf.len() {
            let block = offset / BLOCK_U64;
            let within = (offset % BLOCK_U64) as usize;
            let left = buf.len() - done;
            if within == 0 && left >= BLOCK {
                let count = (left / BLOCK).min(DIRECT_MAX);
                let bytes = count * BLOCK;
                self.device_read(block, &mut buf[done..done + bytes])?;
                // A cached block written in memory is newer than the device.
                for (&cached, &at) in self.index.range(block..block + count as u64) {
                    if self.slots[at].dirty {
                        let start = done + ((cached - block) as usize) * BLOCK;
                        buf[start..start + BLOCK].copy_from_slice(&self.slots[at].data);
                    }
                }
                done += bytes;
                offset += bytes as u64;
            } else {
                let at = self.slot(block)?;
                let n = (BLOCK - within).min(left);
                buf[done..done + n].copy_from_slice(&self.slots[at].data[within..within + n]);
                done += n;
                offset += n as u64;
            }
        }
        Ok(())
    }

    /// Write `data` at `offset`, which the caller has checked lie inside the
    /// stream.
    pub fn write_at(&mut self, mut offset: u64, data: &[u8]) -> Result<(), DiskError> {
        if !self.writable {
            return Err(DiskError::ReadOnly);
        }
        let mut done = 0;
        while done < data.len() {
            let block = offset / BLOCK_U64;
            let within = (offset % BLOCK_U64) as usize;
            let left = data.len() - done;
            if within == 0 && left >= BLOCK {
                let count = (left / BLOCK).min(DIRECT_MAX);
                let bytes = count * BLOCK;
                self.device_write(block, &data[done..done + bytes])?;
                // What the device now holds replaces any cached copy, written
                // in memory or not.
                let cached: Vec<(u64, usize)> =
                    self.index.range(block..block + count as u64).map(|(&b, &at)| (b, at)).collect();
                for (cached_block, at) in cached {
                    let start = done + ((cached_block - block) as usize) * BLOCK;
                    let slot = &mut self.slots[at];
                    slot.data.copy_from_slice(&data[start..start + BLOCK]);
                    slot.dirty = false;
                }
                done += bytes;
                offset += bytes as u64;
            } else {
                let at = self.slot(block)?;
                let n = (BLOCK - within).min(left);
                let slot = &mut self.slots[at];
                slot.data[within..within + n].copy_from_slice(&data[done..done + n]);
                slot.dirty = true;
                done += n;
                offset += n as u64;
            }
        }
        Ok(())
    }

    /// Put every block written in memory on the device, in block order, as
    /// few commands as the runs of adjacent blocks allow.
    pub fn flush(&mut self) -> Result<(), DiskError> {
        let dirty: Vec<(u64, usize)> =
            self.index.iter().filter(|(_, &at)| self.slots[at].dirty).map(|(&b, &at)| (b, at)).collect();
        let mut run: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < dirty.len() {
            let first = dirty[i].0;
            let mut j = i;
            run.clear();
            while j < dirty.len() && dirty[j].0 == first + (j - i) as u64 && j - i < DIRECT_MAX {
                run.extend_from_slice(&self.slots[dirty[j].1].data);
                j += 1;
            }
            self.device_write(first, &run)?;
            for &(_, at) in &dirty[i..j] {
                self.slots[at].dirty = false;
            }
            i = j;
        }
        Ok(())
    }
}

/// The stream one mounted filesystem reads and writes: a handle on the shared
/// state and a position in it.
pub struct Disk<B: Blocks> {
    state: Shared<B>,
    pos: u64,
}

impl<B: Blocks> Disk<B> {
    pub fn new(state: Shared<B>) -> Disk<B> {
        Disk { state, pos: 0 }
    }
}

impl<B: Blocks> fatfs::IoBase for Disk<B> {
    type Error = DiskError;
}

impl<B: Blocks> fatfs::Read for Disk<B> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, DiskError> {
        let mut state = self.state.borrow_mut();
        state.charge()?;
        let size = state.bytes();
        if self.pos >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(size - self.pos) as usize;
        state.read_at(self.pos, &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<B: Blocks> fatfs::Write for Disk<B> {
    fn write(&mut self, data: &[u8]) -> Result<usize, DiskError> {
        let mut state = self.state.borrow_mut();
        state.charge()?;
        let size = state.bytes();
        if data.is_empty() {
            return Ok(0);
        }
        if self.pos >= size || data.len() as u64 > size - self.pos {
            return Err(DiskError::OutOfRange);
        }
        state.write_at(self.pos, data)?;
        self.pos += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), DiskError> {
        let mut state = self.state.borrow_mut();
        state.charge()?;
        state.flush()
    }
}

impl<B: Blocks> fatfs::Seek for Disk<B> {
    fn seek(&mut self, pos: fatfs::SeekFrom) -> Result<u64, DiskError> {
        let mut state = self.state.borrow_mut();
        state.charge()?;
        let size = state.bytes();
        let target = match pos {
            fatfs::SeekFrom::Start(at) => Some(at),
            fatfs::SeekFrom::Current(delta) => offset_by(self.pos, delta),
            fatfs::SeekFrom::End(delta) => offset_by(size, delta),
        };
        match target {
            Some(at) if at <= size => {
                self.pos = at;
                Ok(at)
            }
            _ => Err(DiskError::OutOfRange),
        }
    }
}

fn offset_by(base: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
}
