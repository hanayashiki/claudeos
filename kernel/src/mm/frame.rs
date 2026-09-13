//! Bitmap physical frame allocator.
//!
//! One bit per 4 KiB frame over the whole usable physical range; a set bit
//! means the frame is in use. The bitmap itself lives in the first usable
//! hole large enough to hold it, past everything the boot loader placed.

use super::{page_align_up, phys_to_virt, HHDM_LIMIT, KERNEL_PHYS_START, PAGE_SIZE_U64};
use crate::arch::{DEVICE_PHYS_BASE, RESERVED_PHYS};
use crate::boot::BootInfo;
use crate::sync::Spinlock;

pub struct BitmapAllocator {
    bitmap: *mut u64,
    words: usize,
    /// One reference count per frame, so a frame shared by copy-on-write is
    /// only released once its last user lets go.
    refcounts: *mut u16,
    /// Frames the bitmap covers: every one under the top of usable memory.
    frames: usize,
    /// Of those, the ones that are memory. A hole between the banks the
    /// memory map reports is covered by the bitmap and is not memory, so it
    /// is held for ever and is not counted in what the machine reports it
    /// has.
    managed: usize,
    /// Bits set, holes included.
    used: usize,
    /// Where the next linear search starts, to avoid rescanning from zero.
    hint: usize,
    bitmap_phys: u64,
    bitmap_bytes: usize,
}

unsafe impl Send for BitmapAllocator {}

impl BitmapAllocator {
    #[inline]
    fn test(&self, frame: usize) -> bool {
        unsafe { (*self.bitmap.add(frame / 64) >> (frame % 64)) & 1 != 0 }
    }

    #[inline]
    fn set(&mut self, frame: usize) {
        unsafe {
            let word = self.bitmap.add(frame / 64);
            *word |= 1u64 << (frame % 64);
        }
    }

    #[inline]
    fn clear(&mut self, frame: usize) {
        unsafe {
            let word = self.bitmap.add(frame / 64);
            *word &= !(1u64 << (frame % 64));
        }
    }

    #[inline]
    fn set_count(&mut self, frame: usize, value: u16) {
        unsafe { *self.refcounts.add(frame) = value };
    }

    #[inline]
    fn count(&self, frame: usize) -> u16 {
        unsafe { *self.refcounts.add(frame) }
    }

    fn mark_range_used(&mut self, start: u64, end: u64) {
        let first = (start / PAGE_SIZE_U64) as usize;
        let last = (page_align_up(end) / PAGE_SIZE_U64) as usize;
        for f in first..last.min(self.frames) {
            if !self.test(f) {
                self.used += 1;
            }
            self.set(f);
            // Never hand these back; one permanent reference keeps them held.
            self.set_count(f, 1);
        }
    }

    fn mark_range_free(&mut self, start: u64, end: u64) {
        let first = (page_align_up(start) / PAGE_SIZE_U64) as usize;
        let last = (end / PAGE_SIZE_U64) as usize;
        for f in first..last.min(self.frames) {
            if self.test(f) {
                self.used -= 1;
            }
            self.clear(f);
        }
    }

    /// Allocate one 4 KiB frame, returning its physical address.
    pub fn alloc(&mut self) -> Option<u64> {
        if self.used >= self.frames {
            return None;
        }
        for pass in 0..2 {
            let start = if pass == 0 { self.hint } else { 0 };
            let end = if pass == 0 { self.frames } else { self.hint };
            let mut w = start / 64;
            let wend = (end + 63) / 64;
            while w < wend {
                let word = unsafe { *self.bitmap.add(w) };
                if word != u64::MAX {
                    let bit = (!word).trailing_zeros() as usize;
                    let frame = w * 64 + bit;
                    if frame < self.frames && !self.test(frame) {
                        self.set(frame);
                        self.set_count(frame, 1);
                        self.used += 1;
                        self.hint = frame + 1;
                        return Some(frame as u64 * PAGE_SIZE_U64);
                    }
                }
                w += 1;
            }
        }
        None
    }

    /// Allocate `count` physically contiguous frames.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<u64> {
        if count == 0 {
            return None;
        }
        if count == 1 {
            return self.alloc();
        }
        let mut run = 0usize;
        for f in 0..self.frames {
            if self.test(f) {
                run = 0;
                continue;
            }
            run += 1;
            if run == count {
                let first = f + 1 - count;
                for x in first..=f {
                    self.set(x);
                    self.set_count(x, 1);
                }
                self.used += count;
                return Some(first as u64 * PAGE_SIZE_U64);
            }
        }
        None
    }

    /// Drop one reference. The frame is only released when the last goes.
    pub fn free(&mut self, phys: u64) {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame >= self.frames || !self.test(frame) {
            return;
        }
        let remaining = self.count(frame).saturating_sub(1);
        self.set_count(frame, remaining);
        if remaining > 0 {
            return;
        }
        self.used -= 1;
        self.clear(frame);
        if frame < self.hint {
            self.hint = frame;
        }
    }

    pub fn share(&mut self, phys: u64) {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame < self.frames && self.test(frame) {
            let count = self.count(frame);
            self.set_count(frame, count.saturating_add(1));
        }
    }

    pub fn references(&self, phys: u64) -> u16 {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame < self.frames {
            self.count(frame)
        } else {
            0
        }
    }

    /// How much memory this machine has, in frames. The holes the bitmap
    /// also covers are not memory and are left out, so nothing reported from
    /// here claims memory the kernel cannot touch.
    pub fn total_frames(&self) -> usize {
        self.managed
    }
    /// How much of that memory is held, in frames.
    pub fn used_frames(&self) -> usize {
        self.used - (self.frames - self.managed)
    }
    pub fn free_frames(&self) -> usize {
        self.frames - self.used
    }
    pub fn bitmap_region(&self) -> (u64, usize) {
        (self.bitmap_phys, self.bitmap_bytes)
    }
}

static ALLOCATOR: Spinlock<Option<BitmapAllocator>> = Spinlock::new(None);

/// Top of the memory an allocator built from this map will manage.
///
/// Two things bound it. The direct map covers the low four gigabytes and
/// nothing above them, so a frame past that has no kernel-side address at
/// all. Below that, the last part of the direct map is where the peripherals
/// are, and the kernel reaches them through it with device attributes: a
/// frame there has no cacheable alias, so copying it issues unaligned
/// accesses that fault, a page table in it is read by the walker through a
/// different attribute from the one it was written through, and a page of it
/// given to the heap or to a program is written twice over with two
/// attributes. The firmware on a 4 GiB board reports memory running up to the
/// peripherals, so this is what keeps that memory out of the allocator rather
/// than anything the memory map says.
pub fn usable_limit(boot: &BootInfo) -> u64 {
    let mut max_addr = 0u64;
    for r in boot.regions() {
        if r.usable {
            max_addr = max_addr.max(r.end());
        }
    }
    max_addr.min(DEVICE_PHYS_BASE).min(HHDM_LIMIT)
}

/// How much memory an allocator reaching up to `limit` needs for its bitmap
/// and its reference counts. The two are laid out one after the other, each
/// rounded up to whole pages.
pub fn metadata_bytes(limit: u64) -> usize {
    let frames = (limit / PAGE_SIZE_U64) as usize;
    page_align_up(((frames + 7) / 8) as u64) as usize + page_align_up((frames * 2) as u64) as usize
}

/// Build an allocator over the map `boot` describes, with its bitmap and
/// reference counts at `metadata_phys`, which must be `metadata_bytes(limit)`
/// long and clear of everything the map calls usable.
///
/// `init` takes that memory out of the map itself. A check that builds an
/// allocator over a map describing a machine this is not has to say where the
/// metadata may go, because nothing in such a map is memory here.
pub fn build(boot: &BootInfo, limit: u64, metadata_phys: u64) -> BitmapAllocator {
    let frames = (limit / PAGE_SIZE_U64) as usize;
    let bitmap_bytes = page_align_up(((frames + 7) / 8) as u64) as usize;
    let refcount_bytes = page_align_up((frames * 2) as u64) as usize;
    let metadata = bitmap_bytes + refcount_bytes;

    let bitmap = phys_to_virt(metadata_phys) as *mut u64;
    let refcounts = phys_to_virt(metadata_phys + bitmap_bytes as u64) as *mut u16;
    let words = bitmap_bytes / 8;
    unsafe {
        // Byte counts, so the fills stop at the end of each array rather
        // than running on for the width of its element type.
        core::ptr::write_bytes(bitmap as *mut u8, 0xFF, bitmap_bytes);
        core::ptr::write_bytes(refcounts as *mut u8, 0, refcount_bytes);
    }

    let mut alloc = BitmapAllocator {
        bitmap,
        words,
        refcounts,
        frames,
        managed: 0,
        used: frames,
        hint: 0,
        bitmap_phys: metadata_phys,
        bitmap_bytes: metadata,
    };

    // Release usable RAM, then take back everything that is already spoken for.
    for r in boot.regions() {
        if r.usable {
            alloc.mark_range_free(r.addr, r.end().min(limit));
        }
    }
    // What has just been released is all the memory there is. Whatever the
    // bitmap still covers is a hole between the banks the map reports, or the
    // part of the map that was clamped away above; neither is memory, so
    // neither is counted in what the machine reports it has.
    alloc.managed = alloc.frames - alloc.used;

    for &(start, end) in RESERVED_PHYS {
        alloc.mark_range_used(start, end);
    }
    alloc.mark_range_used(KERNEL_PHYS_START, super::kernel_phys_end());
    alloc.mark_range_used(metadata_phys, metadata_phys + metadata as u64);
    for r in boot.reserved() {
        alloc.mark_range_used(r.start, r.end);
    }
    for m in boot.modules() {
        alloc.mark_range_used(m.start, m.end);
    }
    alloc
}

/// Build the frame allocator from the boot loader's memory map.
pub fn init(boot: &BootInfo) {
    let limit = usable_limit(boot);
    let metadata = metadata_bytes(limit);

    // The bitmap must not land on the kernel image, the modules, anything the
    // loader left behind, or memory the machine claims, so start looking past
    // all of them.
    let mut barrier = super::kernel_phys_end();
    for r in boot.reserved() {
        barrier = barrier.max(r.end);
    }
    for m in boot.modules() {
        barrier = barrier.max(m.end);
    }
    for &(_, end) in RESERVED_PHYS {
        barrier = barrier.max(end);
    }
    let mut bitmap_phys = 0u64;
    for r in boot.regions() {
        if !r.usable {
            continue;
        }
        let start = page_align_up(r.addr.max(barrier));
        // Clamped like the rest of the map: metadata reached through a device
        // alias would be read back with whatever attributes the walk of the
        // bitmap happened to use.
        if start + metadata as u64 <= r.end().min(limit) {
            bitmap_phys = start;
            break;
        }
    }
    assert!(bitmap_phys != 0, "no room for the frame bitmap");

    *ALLOCATOR.lock() = Some(build(boot, limit, bitmap_phys));
}

/// One reference to a physical frame.
///
/// A frame is released when its last reference goes, so everything that
/// records a frame holds one of these, and everything that stops recording it
/// drops one. Holding the reference in a value rather than counting by hand
/// is what makes a missed share or a second release a move error instead of a
/// page handed to two owners.
///
/// A page table entry is the one place a reference is recorded where the type
/// system cannot see it, so `into_recorded` and `from_recorded` mark the two
/// crossings.
pub struct Frame(u64);

impl Frame {
    pub fn addr(&self) -> u64 {
        self.0
    }

    /// A second reference to the same frame, for a mapping that shares it.
    pub fn share(&self) -> Frame {
        share_frame(self.0);
        Frame(self.0)
    }

    /// Give up the handle without releasing the frame: the reference is now
    /// recorded somewhere else, in practice a page table entry.
    pub fn into_recorded(self) -> u64 {
        let addr = self.0;
        core::mem::forget(self);
        addr
    }

    /// Take back a reference that was recorded elsewhere.
    ///
    /// # Safety
    ///
    /// The caller must be removing exactly one recorded reference, and must
    /// not use the recorded copy again.
    pub unsafe fn from_recorded(addr: u64) -> Frame {
        Frame(addr)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        free_frame(self.0);
    }
}

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Frame({:#x})", self.0)
    }
}

pub fn alloc() -> Option<Frame> {
    ALLOCATOR.lock().as_mut().and_then(|a| a.alloc()).map(Frame)
}

/// Allocate a frame and zero it through the direct map.
pub fn alloc_zeroed() -> Option<Frame> {
    let frame = alloc()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(frame.addr()) as *mut u8, 0, 4096) };
    Some(frame)
}

/// One more reference to a frame something else already holds, for a caller
/// that is about to record it.
///
/// # Safety
///
/// The frame must be one that is currently allocated, and the caller must
/// record the reference or drop it.
pub unsafe fn share_recorded(addr: u64) -> Frame {
    share_frame(addr);
    Frame(addr)
}

pub fn alloc_contiguous(count: usize) -> Option<u64> {
    ALLOCATOR.lock().as_mut().and_then(|a| a.alloc_contiguous(count))
}

fn free_frame(phys: u64) {
    if let Some(a) = ALLOCATOR.lock().as_mut() {
        a.free(phys);
    }
}

/// Take an extra reference to a frame that is about to be shared.
fn share_frame(phys: u64) {
    if let Some(a) = ALLOCATOR.lock().as_mut() {
        a.share(phys);
    }
}

pub fn frame_references(phys: u64) -> u16 {
    match ALLOCATOR.lock().as_ref() {
        Some(a) => a.references(phys),
        None => 0,
    }
}

pub fn stats() -> (usize, usize) {
    match ALLOCATOR.lock().as_ref() {
        Some(a) => (a.used_frames(), a.total_frames()),
        None => (0, 0),
    }
}
