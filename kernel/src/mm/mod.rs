//! Memory management: physical frames, page tables, kernel heap.

pub mod active;
pub mod frame;
pub mod heap;
pub mod selftest;
pub mod space;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SIZE_U64: u64 = 4096;

/// Where the kernel image, the direct map of physical memory and the heap sit
/// is a property of the machine, so the numbers come from `arch`. They are
/// named again here because this is where the rest of the kernel looks for
/// them.
pub use crate::arch::{
    HHDM_BASE, HHDM_LIMIT, KERNEL_HEAP_BASE, KERNEL_HEAP_MAX, KERNEL_PHYS_START, KERNEL_VMA,
};

/// Mapped at boot; the heap grows from here on demand.
pub const KERNEL_HEAP_SIZE: usize = 16 * 1024 * 1024;
/// Physical memory kept back from the heap for user pages.
pub const FRAME_RESERVE: usize = 8 * 1024 * 1024;
/// Mapped heap the free list is kept holding over what it has handed out.
///
/// An allocation that finds no hole is the one that maps the pages the heap
/// grows by, and it does that inside whatever critical section its caller is
/// in. This is how much room is kept ahead of it so that the ordinary
/// allocation is not that one.
///
/// It is counted in free bytes rather than in one run of them, so what it
/// answers for is the small allocation -- a node, a string, a formatted line,
/// which is every allocation this kernel makes under another lock -- and not a
/// request larger than whatever hole the free list happens to have.
pub const HEAP_MARGIN: usize = 4 * 1024 * 1024;

/// Where user mmap allocations start growing up from.
pub const USER_MMAP_BASE: u64 = 0x0000_7F00_0000_0000;
/// Top of the initial user stack (grows down from here).
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const USER_STACK_SIZE: u64 = 1024 * 1024;
/// Where a machine that supplies its own code to a program puts it: the page
/// a signal handler returns through on aarch64.
///
/// It is the last page of the half a program owns, and every other thing
/// placed in that half is below it. An image and the break are refused above
/// `USER_MMAP_BASE`. An mmap with no address of its own is placed from
/// `USER_MMAP_BASE` up, past the regions already recorded, of which this is
/// one. The stack occupies `USER_STACK_TOP - STACK_RESERVE` to
/// `USER_STACK_TOP` and grows downwards, so this page starts where the stack
/// ends and nothing about it moves.
pub const USER_TRAMPOLINE: u64 = USER_STACK_TOP;

const _: () = {
    // The page has to fit in the half a program owns, whose top is where the
    // kernel's half begins.
    assert!(USER_TRAMPOLINE % PAGE_SIZE_U64 == 0);
    assert!(USER_TRAMPOLINE + PAGE_SIZE_U64 <= 0x0000_8000_0000_0000);
    assert!(USER_TRAMPOLINE >= USER_STACK_TOP);
};

extern "C" {
    static __kernel_end_virt: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
}

/// Physical memory not spoken for, minus the reserve kept for user pages.
///
/// This deliberately ignores the heap's own free list, because the heap calls
/// it while holding its lock.
pub fn available_bytes() -> usize {
    let (used, total) = frame::stats();
    let free = (total - used) * PAGE_SIZE;
    free.saturating_sub(FRAME_RESERVE)
}

/// What a file may still grow into: unclaimed physical memory plus the space
/// the heap has already mapped and is not using. Without the second term a
/// machine that once held a large file could never hold another one, because
/// the heap does not hand pages back.
pub fn file_available_bytes() -> usize {
    let (heap_used, heap_total) = heap::stats();
    available_bytes() + (heap_total - heap_used)
}

#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    HHDM_BASE + phys
}

#[inline]
pub unsafe fn phys_ptr<T>(phys: u64) -> *mut T {
    phys_to_virt(phys) as *mut T
}

/// Physical address just past the end of the loaded kernel image.
pub fn kernel_phys_end() -> u64 {
    let virt = core::ptr::addr_of!(__kernel_end_virt) as u64;
    virt - KERNEL_VMA
}

pub fn kernel_text_range() -> (u64, u64) {
    (
        core::ptr::addr_of!(__text_start) as u64,
        core::ptr::addr_of!(__text_end) as u64,
    )
}

pub fn kernel_rodata_range() -> (u64, u64) {
    (
        core::ptr::addr_of!(__rodata_start) as u64,
        core::ptr::addr_of!(__rodata_end) as u64,
    )
}

#[inline]
pub const fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

#[inline]
pub const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

#[inline]
pub const fn page_align_down(value: u64) -> u64 {
    align_down(value, PAGE_SIZE_U64)
}

#[inline]
pub const fn page_align_up(value: u64) -> u64 {
    align_up(value, PAGE_SIZE_U64)
}
