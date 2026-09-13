//! Kernel heap: an address-ordered free list with coalescing on free.

use crate::arch::paging::{AddressSpace, NO_EXECUTE, PRESENT, WRITABLE};
use super::{align_up, KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE, PAGE_SIZE_U64};
use crate::sync::Spinlock;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// Smallest block that can still hold a free-list node.
const MIN_BLOCK: usize = 16;

#[repr(C)]
struct Hole {
    size: usize,
    next: *mut Hole,
}

pub struct HoleList {
    head: *mut Hole,
    free_bytes: usize,
    total_bytes: usize,
    /// Bytes of heap address space mapped so far.
    mapped: usize,
}

unsafe impl Send for HoleList {}

/// Normalise a layout into the (size, align) pair the free list works in.
fn normalize(layout: Layout) -> (usize, usize) {
    let align = layout.align().max(MIN_BLOCK);
    let size = align_up(layout.size().max(MIN_BLOCK) as u64, MIN_BLOCK as u64) as usize;
    (size, align)
}

impl HoleList {
    pub const fn new() -> Self {
        HoleList { head: ptr::null_mut(), free_bytes: 0, total_bytes: 0, mapped: 0 }
    }

    /// Take a range of heap address space to map, or nothing when the heap has
    /// reached its ceiling or physical memory is too low to spare.
    ///
    /// `mapped` moves here rather than when the pages arrive, so the range
    /// belongs to this claim from now on and a second grower, running while
    /// this one has let the lock go, takes a different one.
    fn claim(&mut self, needed: usize) -> Option<Claim> {
        const CHUNK: usize = 8 * 1024 * 1024;
        let want = super::align_up(needed.max(CHUNK) as u64, PAGE_SIZE_U64) as usize;
        if self.mapped + want > super::KERNEL_HEAP_MAX {
            return None;
        }

        // Leave enough physical memory for the user pages the caller will
        // almost certainly need next.
        let (used, total) = super::frame::stats();
        let free_bytes = (total - used) * super::PAGE_SIZE;
        if free_bytes < want + super::FRAME_RESERVE {
            return None;
        }

        let start = super::KERNEL_HEAP_BASE + self.mapped as u64;
        self.mapped += want;
        Some(Claim { start, bytes: want })
    }

    /// Donate `[start, start + size)` to the heap.
    pub unsafe fn add_region(&mut self, start: u64, size: usize) {
        if size < MIN_BLOCK {
            return;
        }
        self.total_bytes += size;
        self.insert(start as *mut Hole, size);
    }

    /// Insert a block into the address-ordered list, merging with neighbours.
    unsafe fn insert(&mut self, block: *mut Hole, size: usize) {
        (*block).size = size;
        (*block).next = ptr::null_mut();
        self.free_bytes += size;

        let block_addr = block as usize;
        let mut prev: *mut Hole = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < block_addr {
            prev = cur;
            cur = (*cur).next;
        }

        (*block).next = cur;
        if prev.is_null() {
            self.head = block;
        } else {
            (*prev).next = block;
        }

        // Merge forward, then backward.
        if !cur.is_null() && block_addr + size == cur as usize {
            (*block).size += (*cur).size;
            (*block).next = (*cur).next;
        }
        if !prev.is_null() && prev as usize + (*prev).size == block_addr {
            (*prev).size += (*block).size;
            (*prev).next = (*block).next;
        }
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let (size, align) = normalize(layout);

        let mut prev: *mut Hole = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() {
            let hole_addr = cur as usize;
            let hole_size = (*cur).size;
            let alloc_start = align_up(hole_addr as u64, align as u64) as usize;
            let front = alloc_start - hole_addr;

            // Any leftover must itself be a usable block, or not exist.
            let fits = hole_size >= front + size
                && (front == 0 || front >= MIN_BLOCK)
                && {
                    let back = hole_size - front - size;
                    back == 0 || back >= MIN_BLOCK
                };

            if fits {
                let back = hole_size - front - size;
                let next = (*cur).next;

                // Unlink the hole, then re-insert whatever survives the split.
                if prev.is_null() {
                    self.head = next;
                } else {
                    (*prev).next = next;
                }
                self.free_bytes -= hole_size;

                if front > 0 {
                    self.insert(cur, front);
                }
                if back > 0 {
                    self.insert((alloc_start + size) as *mut Hole, back);
                }
                return alloc_start as *mut u8;
            }

            prev = cur;
            cur = (*cur).next;
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let (size, _) = normalize(layout);
        self.insert(ptr as *mut Hole, size);
    }

    pub fn free_bytes(&self) -> usize {
        self.free_bytes
    }
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Heap address space that belongs to one grower and has yet to be mapped.
struct Claim {
    start: u64,
    bytes: usize,
}

/// Put pages under a claim. Returns how many bytes got them, which is all of
/// them unless physical memory ran out partway.
///
/// Thousands of page table writes and an invalidation each, so this is what
/// the heap lock must not be held across.
unsafe fn map_claim(claim: &Claim) -> usize {
    let space = AddressSpace::current();
    let pages = claim.bytes as u64 / PAGE_SIZE_U64;
    for page in 0..pages {
        let virt = claim.start + page * PAGE_SIZE_U64;
        if space.map_new(virt, PRESENT | WRITABLE | NO_EXECUTE).is_err() {
            return (page * PAGE_SIZE_U64) as usize;
        }
    }
    claim.bytes
}

pub struct LockedHeap(Spinlock<HoleList>);

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        {
            let mut heap = self.0.lock();
            let ptr = heap.alloc(layout);
            if !ptr.is_null() {
                return ptr;
            }
        }

        // Out of room. The lock masks interrupts, and mapping a growth of the
        // heap is up to forty thousand page table writes with a translation
        // buffer invalidation each: done under the lock it held the timer off
        // for a third of a second on the Pi's instruction set. So the range is
        // claimed under the lock, mapped with it released, and handed to the
        // free list under it again.
        //
        // Another task allocating in that window and finding the heap full
        // claims a range of its own, which is a second growth rather than a
        // wrong one. A claim only partly mapped keeps the rest of its address
        // space, which costs nothing worth recovering: the only way there is
        // to be out of physical memory already.
        let claim = match self.0.lock().claim(layout.size()) {
            Some(claim) => claim,
            None => return ptr::null_mut(),
        };
        let mapped = map_claim(&claim);
        if mapped == 0 {
            return ptr::null_mut();
        }

        let mut heap = self.0.lock();
        heap.add_region(claim.start, mapped);
        heap.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock().dealloc(ptr, layout)
    }
}

#[global_allocator]
pub static HEAP: LockedHeap = LockedHeap(Spinlock::new(HoleList::new()));

/// Map the kernel heap and hand it to the allocator.
pub fn init() {
    let space = AddressSpace::current();
    let pages = KERNEL_HEAP_SIZE as u64 / PAGE_SIZE_U64;
    for i in 0..pages {
        let virt = KERNEL_HEAP_BASE + i * PAGE_SIZE_U64;
        space
            .map_new(virt, PRESENT | WRITABLE | NO_EXECUTE)
            .expect("failed to map kernel heap");
    }
    unsafe {
        let mut heap = HEAP.0.lock();
        heap.mapped = KERNEL_HEAP_SIZE;
        heap.add_region(KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE);
    }
}

pub fn stats() -> (usize, usize) {
    let heap = HEAP.0.lock();
    (heap.total_bytes() - heap.free_bytes(), heap.total_bytes())
}
