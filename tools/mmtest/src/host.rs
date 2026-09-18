//! A machine for `walk` made of a `Vec`: physical memory is a buffer, frames
//! come from a bump allocator with reference counts beside it, and the
//! translation caches are a count of how many times they were told to throw
//! something away.
//!
//! The point of it is that the kernel's page-table code is then ordinary Rust
//! that `cargo test` runs and Miri checks. What Miri catches there is what no
//! boot can be made to show: a descriptor read out of a frame that was freed,
//! a pointer used past the allocation it came from, a `u64` read before
//! anything wrote it.
//!
//! What it deliberately does not model is the hardware walker, which reads
//! descriptors without any instruction naming them. Ordering against that
//! walker is what `store_barrier` is for, and nothing on the Mac can check it;
//! the counts below only say that it was called.

use crate::walk::{Machine, ENTRIES, PAGE_BYTES};
use std::cell::{Cell, RefCell};

/// Physical memory, the frames cut out of it, and what the machine was told
/// about its translation caches.
pub struct Host {
    /// Every frame's bytes, back to back. A frame's address is its offset.
    memory: RefCell<Vec<u64>>,
    /// One per frame: how many descriptors name it, or zero when it is free.
    /// `None` for a frame that has never been handed out.
    counts: RefCell<Vec<Option<u32>>>,
    /// The next frame to hand out, in frames.
    next: Cell<usize>,
    /// How many frames have been handed out and not given back.
    live: Cell<usize>,
    pub barriers: Cell<usize>,
    pub page_flushes: Cell<usize>,
    pub all_flushes: Cell<usize>,
    /// Frames freed while a descriptor still named them, which is the defect
    /// the whole of this exists to rule out. A test reads it as a count of
    /// mistakes rather than being told one by one.
    pub freed_twice: Cell<usize>,
}

/// Words in a frame.
const WORDS: usize = PAGE_BYTES / 8;

impl Host {
    /// A machine with `frames` frames of memory, none handed out.
    pub fn new(frames: usize) -> Host {
        Host {
            memory: RefCell::new(vec![0u64; frames * WORDS]),
            counts: RefCell::new(vec![None; frames]),
            next: Cell::new(1),
            live: Cell::new(0),
            barriers: Cell::new(0),
            page_flushes: Cell::new(0),
            all_flushes: Cell::new(0),
            freed_twice: Cell::new(0),
        }
    }

    /// Frames handed out and not given back. Every test ends by checking this
    /// is what it was before, which is what says a hierarchy released
    /// everything it held.
    pub fn live(&self) -> usize {
        self.live.get()
    }

    /// The top table of a fresh hierarchy: a zeroed frame whose one reference
    /// the caller holds, as `PageTables::new_user` does in the kernel.
    pub fn root(&self) -> u64 {
        self.alloc_zeroed().expect("a frame for a top table")
    }

    fn index(&self, phys: u64) -> usize {
        assert_eq!(phys as usize % PAGE_BYTES, 0, "a frame address that is not a frame");
        phys as usize / PAGE_BYTES
    }
}

// SAFETY: `table_ptr` names `ENTRIES` words inside the buffer, which is not
// reallocated for the life of a `Host`; `alloc_zeroed` hands out a frame that
// was never handed out before and zeroes it; `free` and `share` keep a count
// per frame; the barrier and the flushes only count.
unsafe impl Machine for Host {
    // The descriptor format is x86-64's, which is the one that spells write
    // permission the way the walk's callers do. Which format it is does not
    // matter to anything here; what matters is that the answers are
    // consistent, since the walk only ever compares its own output.
    fn leaf(&self, phys: u64, flags: u64) -> u64 {
        (phys & ADDR) | (flags & !ADDR) | PRESENT
    }

    fn table(&self, phys: u64, under: u64) -> u64 {
        (phys & ADDR) | PRESENT | WRITABLE | (under & USER)
    }

    fn present(&self, bits: u64) -> bool {
        bits & PRESENT != 0
    }

    fn ends_walk(&self, bits: u64, level: u32) -> bool {
        level > 0 && bits & HUGE != 0
    }

    fn addr(&self, bits: u64) -> u64 {
        bits & ADDR
    }

    fn flags(&self, bits: u64) -> u64 {
        bits & !ADDR
    }

    fn shared(&self, flags: u64) -> Option<u64> {
        if flags & WRITABLE != 0 {
            Some((flags & !WRITABLE) | COW)
        } else {
            None
        }
    }

    fn widen(&self, table_bits: u64, leaf_flags: u64) -> Option<u64> {
        if leaf_flags & USER != 0 && table_bits & USER == 0 {
            Some(table_bits | USER)
        } else {
            None
        }
    }

    fn table_ptr(&self, phys: u64) -> *mut u64 {
        let index = self.index(phys);
        assert!(
            self.counts.borrow()[index].unwrap_or(0) > 0,
            "a table read through a frame nothing holds: {:#x}",
            phys,
        );
        let mut memory = self.memory.borrow_mut();
        // The buffer is allocated once in `new` and never grows, so the
        // pointer stays good for the life of the machine; `ENTRIES` words from
        // here are inside it because a frame is that many words long.
        let base = memory.as_mut_ptr();
        assert!((index + 1) * WORDS <= memory.len(), "a frame outside memory");
        // SAFETY: `index * WORDS + ENTRIES` is within the buffer, checked
        // above, and the buffer is not reallocated.
        unsafe { base.add(index * WORDS) }
    }

    fn alloc_zeroed(&self) -> Option<u64> {
        let index = self.next.get();
        if (index + 1) * WORDS > self.memory.borrow().len() {
            return None;
        }
        self.next.set(index + 1);
        self.live.set(self.live.get() + 1);
        self.counts.borrow_mut()[index] = Some(1);
        self.memory.borrow_mut()[index * WORDS..(index + 1) * WORDS].fill(0);
        Some((index * PAGE_BYTES) as u64)
    }

    fn free(&self, phys: u64) {
        let index = self.index(phys);
        let mut counts = self.counts.borrow_mut();
        match counts[index] {
            Some(count) if count > 0 => {
                counts[index] = Some(count - 1);
                if count == 1 {
                    self.live.set(self.live.get() - 1);
                }
            }
            _ => self.freed_twice.set(self.freed_twice.get() + 1),
        }
    }

    fn share(&self, phys: u64) {
        let index = self.index(phys);
        let mut counts = self.counts.borrow_mut();
        let count = counts[index].expect("a share of a frame never handed out");
        counts[index] = Some(count + 1);
    }

    fn references(&self, phys: u64) -> u32 {
        self.counts.borrow()[self.index(phys)].unwrap_or(0)
    }

    fn store_barrier(&self) {
        self.barriers.set(self.barriers.get() + 1);
    }

    fn flush_page(&self, _virt: u64) {
        self.page_flushes.set(self.page_flushes.get() + 1);
    }

    fn flush_all(&self) {
        self.all_flushes.set(self.all_flushes.get() + 1);
    }
}

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const HUGE: u64 = 1 << 7;
pub const COW: u64 = 1 << 9;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;

/// What a page of a program is mapped with.
pub fn page_flags() -> u64 {
    PRESENT | WRITABLE | USER
}

const _: () = assert!(ENTRIES == WORDS);
