//! Memory management: physical frames, page tables, kernel heap.

pub mod frame;
pub mod heap;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SIZE_U64: u64 = 4096;

/// Direct map of physical memory, installed by the boot trampoline.
pub const HHDM_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Size of the region the boot trampoline direct-maps (low 4 GiB).
pub const HHDM_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// Virtual base the kernel image is linked at.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;
/// Physical address the kernel image is loaded at (see linker.ld: `. = 1M`).
pub const KERNEL_PHYS_START: u64 = 0x10_0000;

pub const KERNEL_HEAP_BASE: u64 = 0xFFFF_C000_0000_0000;
/// Mapped at boot; the heap grows from here on demand.
pub const KERNEL_HEAP_SIZE: usize = 16 * 1024 * 1024;
/// Ceiling on heap growth. It stays inside the single PDPT the heap's PML4
/// entry points at, so growing never has to touch a PML4 shared with an
/// address space that already exists.
pub const KERNEL_HEAP_MAX: usize = 512 * 1024 * 1024;
/// Physical memory kept back from the heap for user pages.
pub const FRAME_RESERVE: usize = 8 * 1024 * 1024;

/// Where user mmap allocations start growing up from.
pub const USER_MMAP_BASE: u64 = 0x0000_7F00_0000_0000;
/// Top of the initial user stack (grows down from here).
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const USER_STACK_SIZE: u64 = 1024 * 1024;

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
