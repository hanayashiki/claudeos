//! Bitmap physical frame allocator.
//!
//! One bit per 4 KiB frame over the whole usable physical range; a set bit
//! means the frame is in use. The bitmap itself lives in the first usable
//! hole large enough to hold it, past everything the boot loader placed.

use super::{page_align_up, phys_to_virt, HHDM_LIMIT, KERNEL_PHYS_START, PAGE_SIZE_U64};
use crate::multiboot::BootInfo;
use crate::sync::Spinlock;

pub struct BitmapAllocator {
    bitmap: *mut u64,
    words: usize,
    /// One reference count per frame, so a frame shared by copy-on-write is
    /// only released once its last user lets go.
    refcounts: *mut u16,
    total_frames: usize,
    used_frames: usize,
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
        for f in first..last.min(self.total_frames) {
            if !self.test(f) {
                self.used_frames += 1;
            }
            self.set(f);
            // Never hand these back; one permanent reference keeps them held.
            self.set_count(f, 1);
        }
    }

    fn mark_range_free(&mut self, start: u64, end: u64) {
        let first = (page_align_up(start) / PAGE_SIZE_U64) as usize;
        let last = (end / PAGE_SIZE_U64) as usize;
        for f in first..last.min(self.total_frames) {
            if self.test(f) {
                self.used_frames -= 1;
            }
            self.clear(f);
        }
    }

    /// Allocate one 4 KiB frame, returning its physical address.
    pub fn alloc(&mut self) -> Option<u64> {
        if self.used_frames >= self.total_frames {
            return None;
        }
        for pass in 0..2 {
            let start = if pass == 0 { self.hint } else { 0 };
            let end = if pass == 0 { self.total_frames } else { self.hint };
            let mut w = start / 64;
            let wend = (end + 63) / 64;
            while w < wend {
                let word = unsafe { *self.bitmap.add(w) };
                if word != u64::MAX {
                    let bit = (!word).trailing_zeros() as usize;
                    let frame = w * 64 + bit;
                    if frame < self.total_frames && !self.test(frame) {
                        self.set(frame);
                        self.set_count(frame, 1);
                        self.used_frames += 1;
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
        for f in 0..self.total_frames {
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
                self.used_frames += count;
                return Some(first as u64 * PAGE_SIZE_U64);
            }
        }
        None
    }

    /// Drop one reference. The frame is only released when the last goes.
    pub fn free(&mut self, phys: u64) {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame >= self.total_frames || !self.test(frame) {
            return;
        }
        let remaining = self.count(frame).saturating_sub(1);
        self.set_count(frame, remaining);
        if remaining > 0 {
            return;
        }
        self.used_frames -= 1;
        self.clear(frame);
        if frame < self.hint {
            self.hint = frame;
        }
    }

    pub fn share(&mut self, phys: u64) {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame < self.total_frames && self.test(frame) {
            let count = self.count(frame);
            self.set_count(frame, count.saturating_add(1));
        }
    }

    pub fn references(&self, phys: u64) -> u16 {
        let frame = (phys / PAGE_SIZE_U64) as usize;
        if frame < self.total_frames {
            self.count(frame)
        } else {
            0
        }
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }
    pub fn used_frames(&self) -> usize {
        self.used_frames
    }
    pub fn free_frames(&self) -> usize {
        self.total_frames - self.used_frames
    }
    pub fn bitmap_region(&self) -> (u64, usize) {
        (self.bitmap_phys, self.bitmap_bytes)
    }
}

static ALLOCATOR: Spinlock<Option<BitmapAllocator>> = Spinlock::new(None);

/// Build the frame allocator from the boot loader's memory map.
pub fn init(boot: &BootInfo) {
    // Highest usable physical address, clamped to what the boot trampoline
    // direct-maps; frames above that are unreachable through the HHDM.
    let mut max_addr = 0u64;
    for r in &boot.regions[..boot.region_count] {
        if r.is_usable() {
            max_addr = max_addr.max(r.end());
        }
    }
    if max_addr > HHDM_LIMIT {
        max_addr = HHDM_LIMIT;
    }

    let total_frames = (max_addr / PAGE_SIZE_U64) as usize;
    let bitmap_bytes = page_align_up(((total_frames + 7) / 8) as u64) as usize;
    let refcount_bytes = page_align_up((total_frames * 2) as u64) as usize;
    let metadata_bytes = bitmap_bytes + refcount_bytes;

    // The bitmap must not land on the kernel image, the modules, or the
    // multiboot blob, so start looking past all of them.
    let barrier = super::kernel_phys_end().max(boot.reserved_end).max(0x10_0000);
    let mut bitmap_phys = 0u64;
    for r in &boot.regions[..boot.region_count] {
        if !r.is_usable() {
            continue;
        }
        let start = page_align_up(r.addr.max(barrier));
        if start + metadata_bytes as u64 <= r.end() {
            bitmap_phys = start;
            break;
        }
    }
    assert!(bitmap_phys != 0, "no room for the frame bitmap");

    let bitmap = phys_to_virt(bitmap_phys) as *mut u64;
    let refcounts = phys_to_virt(bitmap_phys + bitmap_bytes as u64) as *mut u16;
    let words = bitmap_bytes / 8;
    unsafe {
        core::ptr::write_bytes(bitmap, 0xFF, bitmap_bytes);
        core::ptr::write_bytes(refcounts, 0, refcount_bytes);
    }

    let mut alloc = BitmapAllocator {
        bitmap,
        words,
        refcounts,
        total_frames,
        used_frames: total_frames,
        hint: 0,
        bitmap_phys,
        bitmap_bytes: metadata_bytes,
    };

    // Release usable RAM, then take back everything that is already spoken for.
    for r in &boot.regions[..boot.region_count] {
        if r.is_usable() {
            alloc.mark_range_free(r.addr, r.end().min(max_addr));
        }
    }
    alloc.mark_range_used(0, 0x10_0000);
    alloc.mark_range_used(KERNEL_PHYS_START, super::kernel_phys_end());
    alloc.mark_range_used(bitmap_phys, bitmap_phys + metadata_bytes as u64);
    alloc.mark_range_used(boot.info_phys, boot.reserved_end);
    for m in &boot.modules[..boot.module_count] {
        alloc.mark_range_used(m.start, m.end);
    }

    *ALLOCATOR.lock() = Some(alloc);
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
