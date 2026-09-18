//! What an AArch64 translation table descriptor is made of, and how a cached
//! translation is thrown away. 4 KiB granule, 48-bit addresses, four levels.
//!
//! The walk itself, and every change to a table, is in `mm::walk`, written
//! once for both machines. This file is what that module asks each machine
//! for.
//!
//! The descriptor bits are not the ones x86 uses, and two of them do not exist
//! at all. Write permission is expressed the other way round here — bit 7 set
//! means read-only — and whether a page may be executed is two bits, one per
//! privilege level. So the flags the portable half passes are kept in the
//! descriptor as software bits, in the positions the architecture leaves for
//! an operating system, and the hardware bits are derived from them on every
//! write. Reading them back therefore gives the same word that was asked for.

use crate::mm::HHDM_BASE;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The descriptor is valid. At the last level a page descriptor also needs
/// bit 1, which `leaf` adds.
pub const PRESENT: u64 = 1 << 0;
/// Software bit: the page may be written. Bits 55..58 are reserved for the
/// operating system's own use.
pub const WRITABLE: u64 = 1 << 55;
/// AP[1]: the level below may reach this page.
pub const USER: u64 = 1 << 6;
/// The low bit of the attribute index, which selects slot one of MAIR_EL1:
/// device memory, which is not cached, reordered or merged.
pub const NO_CACHE: u64 = 1 << 2;
/// UXN. `leaf` sets the matching privileged bit alongside it.
pub const NO_EXECUTE: u64 = 1 << 54;
/// Software bit: the page is shared with another address space and must be
/// copied before it is written to.
pub const COW: u64 = 1 << 56;

/// The output address of a descriptor.
pub const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Bit 1 of a last-level descriptor, which says page rather than block. A
/// descriptor above the last level has it set when it names a table, so a
/// present descriptor without it there is a block.
const PAGE_DESCRIPTOR: u64 = 1 << 1;
/// The access flag. Hardware faults on any access to a page that has it clear,
/// which is how a system tracks what has been touched. Nothing here does, so
/// every mapping sets it.
const ACCESSED: u64 = 1 << 10;
/// Inner shareable, which is what memory wants on a machine whose cores must
/// see each other's writes.
const SHARED: u64 = 3 << 8;
/// AP[2]: the page may be read but not written.
const READ_ONLY: u64 = 1 << 7;
/// Privileged execute never.
const NO_EXECUTE_EL1: u64 = 1 << 53;
/// The bits of a descriptor `flags` hands back and `leaf` accepts: everything
/// except the output address.
const FLAG_MASK: u64 = !ADDR_MASK;

/// Turn the flags the portable half speaks into a last-level descriptor for
/// `phys`. The software bits are kept as they were asked for; the hardware
/// permission bits are derived from them.
#[inline]
pub fn leaf(phys: u64, flags: u64) -> u64 {
    let mut bits = (phys & ADDR_MASK) | (flags & FLAG_MASK) | PAGE_DESCRIPTOR | ACCESSED | SHARED;
    if flags & WRITABLE == 0 || flags & COW != 0 {
        bits |= READ_ONLY;
    } else {
        bits &= !READ_ONLY;
    }
    // Nothing the kernel maps for a program is ever executed at EL1, and
    // anything the portable half calls non-executable is non-executable at
    // both levels.
    if flags & (NO_EXECUTE | USER) != 0 {
        bits |= NO_EXECUTE_EL1;
    }
    bits
}

/// A descriptor naming the table at `phys`. Permission is decided at the last
/// level here, so nothing of the leaf's flags belongs in it: a table
/// descriptor can forbid what the pages under it allow, and using that would
/// mean revisiting every ancestor whenever one page's permissions changed.
#[inline]
pub fn table(phys: u64, _under: u64) -> u64 {
    (phys & ADDR_MASK) | PRESENT | PAGE_DESCRIPTOR
}

#[inline]
pub fn present(bits: u64) -> bool {
    bits & PRESENT != 0
}

/// A present descriptor above the last level that does not name a table is a
/// block, and the walk stops there.
#[inline]
pub fn ends_walk(bits: u64, level: u32) -> bool {
    level > 0 && bits & PAGE_DESCRIPTOR == 0
}

#[inline]
pub fn addr(bits: u64) -> u64 {
    bits & ADDR_MASK
}

#[inline]
pub fn flags(bits: u64) -> u64 {
    bits & FLAG_MASK
}

/// Write permission is a software bit here and `leaf` derives read-only from
/// it and from the mark together, so a shared page keeps the permission it was
/// asked for and the mark is what makes the hardware refuse the store.
#[inline]
pub fn shared(flags: u64) -> Option<u64> {
    if flags & WRITABLE != 0 {
        Some(flags | COW)
    } else {
        None
    }
}

/// Nothing: the table levels say nothing about permission here, so a leaf is
/// reachable whatever is above it.
#[inline]
pub fn widen(_table_bits: u64, _leaf_flags: u64) -> Option<u64> {
    None
}

/// Put what was written into a table or a page in memory before the descriptor
/// naming it is there. Without it the walker can reach the descriptor and read
/// what was at that address before. QEMU does not model this; a board does.
#[inline]
pub fn store_barrier() {
    // SAFETY: a barrier touches no memory of its own.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
}

#[inline]
pub fn read_ttbr0() -> u64 {
    let value: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, ttbr0_el1", out(reg) value, options(nomem, nostack)) };
    value & ADDR_MASK
}

/// # Safety
///
/// `phys` must be a top table that maps the kernel half, and it must stay
/// alive for as long as the processor is on it.
#[inline]
pub unsafe fn write_ttbr(phys: u64) {
    // One table serves both bases: the walk takes the same nine bits for the
    // top level whichever register it came from, and the kernel's addresses
    // all land in the upper half of it.
    //
    // The leading barrier is what makes whatever was written into this table
    // visible to the walkers before they are pointed at it; without it they
    // are entitled to read what was there before. Nothing is left of the
    // previous space afterwards, because no address space identifiers are in
    // use and every entry still cached belongs to it.
    asm!(
        "dsb ishst",
        "msr ttbr0_el1, {table}",
        "msr ttbr1_el1, {table}",
        "isb",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
        table = in(reg) phys,
    );
}

/// The operand `tlbi` by address takes: bits 55 to 12 of the address in the
/// low forty-four bits, and a hint in bits 47 to 44 saying which level of the
/// walk the entry came from. The mask is what keeps the address out of the
/// hint. An address in the kernel's half is sign-extended, so shifting alone
/// would leave its bits 59 to 56 sitting in that field, all ones, claiming a
/// level that does not match this granule. The Pi 4's core does not implement
/// the field and ignores it; on a part that does, the invalidation is then
/// permitted not to happen.
#[inline]
pub const fn invalidation_operand(virt: u64) -> u64 {
    (virt >> 12) & 0xFFF_FFFF_FFFF
}

#[inline]
pub fn flush_tlb(virt: u64) {
    // SAFETY: an invalidation touches no memory of its own.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {page}",
            "dsb ish",
            "isb",
            page = in(reg) invalidation_operand(virt),
        );
    }
}

/// This core has no invalidation by range, which leaves five hundred and
/// twelve operations at the last level and a quarter of a million above it
/// against one that takes the whole space, so the whole space goes. The
/// trailing barrier is what a caller waits on before releasing a frame.
#[inline]
pub fn flush_tlb_all() {
    // SAFETY: as `flush_tlb`.
    unsafe {
        asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb");
    }
}

/// The top table the kernel was running on when it took over the machine.
///
/// Every address space copies its kernel half from it, and the processor is
/// put back on it when it is left on no address space of a program's. It is
/// in the kernel image rather than taken from the frame allocator, and nothing
/// frees it.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// Record the tables the processor is on as the kernel's own. Once, at boot,
/// before anything is mapped into the kernel half and before any address space
/// exists.
pub fn adopt_boot_tables() {
    KERNEL_ROOT.store(read_ttbr0(), Ordering::Release);
}

/// The kernel's own top table, which `adopt_boot_tables` recorded.
pub fn kernel_root() -> u64 {
    let root = KERNEL_ROOT.load(Ordering::Acquire);
    assert!(root != 0, "the kernel's tables are used before they are recorded");
    root
}

/// The top table the processor is walking right now.
pub fn live_root() -> u64 {
    read_ttbr0()
}

/// How a descriptor's permissions are named in a fault report. Only the ones
/// that end the walk get one: the bits that say read-only and reachable from
/// the level below mean nothing in a descriptor that points at another table,
/// so printing them there would be reporting a permission the walk does not
/// have.
pub fn describe(bits: u64, level: u32) -> alloc::string::String {
    use alloc::format;
    if level > 0 && !ends_walk(bits, level) {
        return format!("present, a table");
    }
    format!(
        "present{}{}{}{}",
        if bits & READ_ONLY != 0 { " read-only" } else { " writable" },
        if bits & USER != 0 { " user" } else { " supervisor" },
        if bits & COW != 0 { " copy-on-write" } else { "" },
        if ends_walk(bits, level) { ", a block" } else { "" },
    )
}

/// Drop the boot-time identity map of the low 4 GiB now that the kernel runs
/// entirely out of the higher half. This frees the low half for programs.
///
/// # Safety
///
/// Nothing may still be running out of a low address.
pub unsafe fn drop_identity_map() {
    store_barrier();
    core::ptr::write(crate::mm::phys_to_virt(read_ttbr0()) as *mut u64, 0);
    flush_tlb_all();
}

/// Sign-extend a 48-bit virtual address into canonical form.
#[inline]
pub fn sign_extend(virt: u64) -> u64 {
    ((virt << 16) as i64 >> 16) as u64
}

/// True when `virt` is an address the lower half translates, which is the half
/// a program owns.
#[inline]
pub fn is_user_addr(virt: u64) -> bool {
    virt < 0x0000_8000_0000_0000
}

#[inline]
pub fn is_hhdm_addr(virt: u64) -> bool {
    virt >= HHDM_BASE && virt < HHDM_BASE + crate::mm::HHDM_LIMIT
}
