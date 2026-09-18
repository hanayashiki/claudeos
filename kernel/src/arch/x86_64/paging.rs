//! What an x86-64 page table entry is made of, and how a cached translation is
//! thrown away. Four levels, 4 KiB pages.
//!
//! The walk itself, and every change to a table, is in `mm::walk`, written
//! once for both machines. This file is what that module asks each machine
//! for.

use crate::mm::HHDM_BASE;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;
pub const ACCESSED: u64 = 1 << 5;
pub const DIRTY: u64 = 1 << 6;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;
/// Software bit: the page is shared with another address space and must be
/// copied before it is written to. Bits 9..11 are free for the kernel's use.
pub const COW: u64 = 1 << 9;

/// Bits software may use freely in a non-present entry.
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// A last-level entry naming `phys` with `flags`. The bits go in as they were
/// asked for; this machine spells permission the way the portable half does.
#[inline]
pub fn leaf(phys: u64, flags: u64) -> u64 {
    (phys & ADDR_MASK) | (flags & !ADDR_MASK) | PRESENT
}

/// An entry naming the table at `phys`. This machine forbids from above what
/// the level below allows, so a table on the way to a user leaf has to be
/// reachable from user mode itself; `widen` opens one that already exists.
#[inline]
pub fn table(phys: u64, under: u64) -> u64 {
    (phys & ADDR_MASK) | PRESENT | WRITABLE | (under & USER)
}

#[inline]
pub fn present(bits: u64) -> bool {
    bits & PRESENT != 0
}

/// A large page ends the walk above the last level.
#[inline]
pub fn ends_walk(bits: u64, level: u32) -> bool {
    level > 0 && bits & HUGE != 0
}

#[inline]
pub fn addr(bits: u64) -> u64 {
    bits & ADDR_MASK
}

#[inline]
pub fn flags(bits: u64) -> u64 {
    bits & !ADDR_MASK
}

/// A user leaf is unreachable if any table above it lacks the user bit, so the
/// way down is opened as the walk descends.
#[inline]
pub fn widen(table_bits: u64, leaf_flags: u64) -> Option<u64> {
    if leaf_flags & USER != 0 && table_bits & USER == 0 {
        Some(table_bits | USER)
    } else {
        None
    }
}

/// Nothing to emit: this processor orders a walker's reads against its own
/// stores. It is spelled at all because the other machine needs a barrier and
/// the two halves keep one shape; writing the word directly is how that
/// barrier gets left out on the machine that needs it.
#[inline]
pub fn store_barrier() {}

#[inline]
pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading a control register.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value & ADDR_MASK
}

/// # Safety
///
/// `phys` must be a top table that maps the kernel half, and it must stay
/// alive for as long as the processor is on it.
#[inline]
pub unsafe fn write_cr3(phys: u64) {
    asm!("mov cr3, {}", in(reg) phys, options(nostack, preserves_flags));
}

#[inline]
pub fn flush_tlb(virt: u64) {
    // SAFETY: an invalidation touches no memory of its own.
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

/// The whole space: a reload of the table base register. Since no entry in
/// this kernel is marked global it discards the unfinished walks with
/// everything else.
#[inline]
pub fn flush_tlb_all() {
    // SAFETY: the register is written back with what it already held.
    unsafe { write_cr3(read_cr3()) };
}

/// The PML4 the kernel was running on when it took over the machine.
///
/// Every address space copies its kernel half from it, and the processor is
/// put back on it when it is left on no address space of a program's. It is
/// in the kernel image rather than taken from the frame allocator, and nothing
/// frees it.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Record the tables the processor is on as the kernel's own. Once, at boot,
/// before anything is mapped into the kernel half and before any address space
/// exists.
pub fn adopt_boot_tables() {
    KERNEL_PML4.store(read_cr3(), Ordering::Release);
}

/// The kernel's own top table, which `adopt_boot_tables` recorded.
pub fn kernel_root() -> u64 {
    let pml4 = KERNEL_PML4.load(Ordering::Acquire);
    assert!(pml4 != 0, "the kernel's tables are used before they are recorded");
    pml4
}

/// The PML4 the processor is walking right now.
pub fn live_root() -> u64 {
    read_cr3()
}

/// How an entry's permissions are named in a fault report.
pub fn describe(bits: u64, level: u32) -> alloc::string::String {
    use alloc::format;
    format!(
        "present{}{}{}{}",
        if bits & WRITABLE != 0 { " writable" } else { " read-only" },
        if bits & USER != 0 { " user" } else { " supervisor" },
        if bits & HUGE != 0 { " huge" } else { "" },
        if level == 0 && bits & COW != 0 { " copy-on-write" } else { "" },
    )
}

/// Drop the boot-time identity map of the low 4 GiB now that the kernel runs
/// entirely out of the higher half. This frees PML4[0] for user programs.
///
/// # Safety
///
/// Nothing may still be running out of a low address.
pub unsafe fn drop_identity_map() {
    core::ptr::write(crate::mm::phys_to_virt(read_cr3()) as *mut u64, 0);
    flush_tlb_all();
}

/// Sign-extend a 48-bit virtual address into canonical form.
#[inline]
pub fn sign_extend(virt: u64) -> u64 {
    ((virt << 16) as i64 >> 16) as u64
}

/// True when `virt` is a canonical user-space address.
#[inline]
pub fn is_user_addr(virt: u64) -> bool {
    virt < 0x0000_8000_0000_0000
}

#[inline]
pub fn is_hhdm_addr(virt: u64) -> bool {
    virt >= HHDM_BASE && virt < HHDM_BASE + crate::mm::HHDM_LIMIT
}
