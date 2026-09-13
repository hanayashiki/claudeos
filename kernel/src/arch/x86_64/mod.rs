//! x86-64.
//!
//! The submodules below are private: everything the portable half of the
//! kernel is allowed to reach is re-exported from here, so this file is the
//! whole of the interface an implementation of another architecture has to
//! provide. The two exceptions are `paging` and `nr`, which are namespaces
//! rather than single names.

use core::arch::asm;
use core::arch::global_asm;

mod clock;
mod cpu;
mod io;
mod keyboard;
mod multiboot;
mod signal_frame;
mod syscall;
mod task;
mod trap;
mod uart;

pub mod nr;
pub mod paging;

global_asm!(include_str!("boot.s"), options(att_syntax));
global_asm!(include_str!("cpu/interrupts.s"), options(att_syntax));
global_asm!(include_str!("switch.s"), options(att_syntax));
global_asm!(include_str!("syscall_entry.s"), options(att_syntax));

// Some of these name a part of the interface without being called from the
// portable half today; they are listed here because this file is the contract.
#[allow(unused_imports)]
pub use clock::{cycle_counter, read_wall_clock, WallClock};
pub use signal_frame::{enter_signal_handler, leave_signal_handler};
pub use syscall::{
    arch_prctl, clone_args, fork_child_frame, init_syscall_entry, set_syscall_result, syscall_args,
    syscall_number, syscall_result,
};
pub use task::{
    prepare_kernel_entry, return_to_user, set_current_task, set_kernel_entry_stack, start_user_at,
    switch_context, trap_frame_at, TaskContext,
};
#[allow(unused_imports)]
pub use trap::{
    dump_registers, end_of_interrupt, exception_name, exception_signal, fault_probe_registers,
    init_interrupt_controller, init_timer, instruction_pointer, irq_vector, mask_irq, page_fault,
    register_irq_handler, register_trap_handler, trap_error_code, trap_vector, unmask_irq,
    vector_irq, Handler, PageFault, TrapFrame, EXCEPTION_COUNT, IRQ_COUNT, KEYBOARD_IRQ,
    PAGE_FAULT_VECTOR, SERIAL_IRQ, SERIAL_IRQ_ALT, TICK_HZ, TIMER_IRQ,
};

/// What this machine calls itself: `uname`'s `machine` field, and the string a
/// program reads out of the auxiliary vector as AT_PLATFORM.
pub const MACHINE: &str = "x86_64";

/// The `e_machine` an ELF file must carry to run here (EM_X86_64).
pub const ELF_MACHINE: u16 = 0x3E;

extern "C" {
    static kernel_stack_top: u8;
}

// ---------------------------------------------------------------------------
// Where things sit in the address space
// ---------------------------------------------------------------------------

/// Direct map of physical memory, installed by the boot trampoline.
pub const HHDM_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Size of the region the boot trampoline direct-maps (low 4 GiB).
pub const HHDM_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
/// Where the direct map stops covering memory, which here is nowhere: the
/// trampoline maps all four gigabytes write-back, and what is registers
/// rather than memory is what the loader's map calls unusable. The frame
/// allocator asks every machine this, so the answer has to be a number.
pub const DEVICE_PHYS_BASE: u64 = HHDM_LIMIT;

/// Virtual base the kernel image is linked at.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;
/// Physical address the kernel image is loaded at (see linker.ld: `. = 1M`).
pub const KERNEL_PHYS_START: u64 = 0x10_0000;

pub const KERNEL_HEAP_BASE: u64 = 0xFFFF_C000_0000_0000;
/// Ceiling on heap growth. It stays inside the single PDPT the heap's PML4
/// entry points at, so growing never has to touch a PML4 shared with an
/// address space that already exists.
pub const KERNEL_HEAP_MAX: usize = 512 * 1024 * 1024;

/// Physical memory this machine claims for itself whatever the loader says
/// about it: the real-mode interrupt table, the BIOS data area, the extended
/// BIOS data area, video memory and the option ROMs all live under 1 MiB.
pub const RESERVED_PHYS: &[(u64, u64)] = &[(0, 0x10_0000)];

// ---------------------------------------------------------------------------
// Bringing the processor up
// ---------------------------------------------------------------------------

/// Where `boot.s` lands once it has reached the higher half. The loader hands
/// over a pointer to its info blob and a magic number saying what the blob is;
/// both are decoded here, before low memory stops being reachable.
#[no_mangle]
pub extern "C" fn kmain(mb_info_phys: u64, magic: u64) -> ! {
    crate::serial::init();
    if magic as u32 != multiboot::MULTIBOOT_BOOTLOADER_MAGIC {
        panic!("bad multiboot magic {:#x}", magic);
    }
    let boot = unsafe { multiboot::parse(mb_info_phys) };
    crate::start(&boot)
}

/// Install the tables the CPU consults on a trap, and name the stack it
/// switches to when one arrives from user mode. Nothing may fault before this
/// has run.
pub fn init_traps() {
    cpu::gdt::init();
    cpu::gdt::set_kernel_stack(core::ptr::addr_of!(kernel_stack_top) as u64);
    cpu::idt::init();
}

/// Set up the per-CPU block the entry stubs reach through, and enable the
/// floating point and vector unit compiled user code expects to find working.
pub fn init_cpu() {
    cpu::init_per_cpu();
    cpu::init_sse();
}

/// Stop the CPU until the next interrupt.
#[inline]
pub fn halt() {
    cpu::halt();
}

/// Make bytes the kernel has just written fetchable as instructions.
///
/// Nothing to do here: this processor keeps its instruction cache coherent
/// with stores, so writing a page and then jumping into it works without
/// being told. The call is in the interface because it is not free everywhere.
#[inline]
pub fn sync_instruction_cache(_start: u64, _len: usize) {}

// ---------------------------------------------------------------------------
// Handing memory to a device that reads and writes it itself
// ---------------------------------------------------------------------------
//
// All three are nothing here. A PCI device's transfers are coherent with the
// caches on this machine: the chipset snoops them, so a device reading memory
// sees a line the CPU has only written into its cache, and a line the device
// overwrites is dropped from the CPU's cache rather than left stale. The calls
// are in the interface because none of that is promised on the other machine.

/// Push the kernel's writes over `start..start+len` out to where a device
/// reading memory will see them. Call before handing the range to a device.
#[inline]
pub fn clean_data_cache(_start: u64, _len: usize) {}

/// Throw away anything cached over `start..start+len`, so a read afterwards
/// fetches what a device wrote there. Call after the device has finished.
#[inline]
pub fn invalidate_data_cache(_start: u64, _len: usize) {}

/// Both at once: the kernel's writes go out, and nothing stale is left behind.
#[inline]
pub fn flush_data_cache(_start: u64, _len: usize) {}

// ---------------------------------------------------------------------------
// Interrupt enable state
// ---------------------------------------------------------------------------

#[inline]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    flags & (1 << 9) != 0
}

#[inline]
pub fn disable_interrupts() {
    unsafe { asm!("cli", options(nomem, nostack)) };
}

#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

// ---------------------------------------------------------------------------
// The debug console
// ---------------------------------------------------------------------------

pub fn console_init() {
    uart::init();
}

pub fn console_write_byte(byte: u8) {
    uart::write_byte(byte);
}

pub fn console_read_byte() -> Option<u8> {
    uart::read_byte()
}

pub fn console_enable_rx_interrupt() {
    uart::enable_rx_interrupt();
}

/// One character from a keyboard attached to the machine itself, if it has
/// one and a key produced a character.
pub fn keyboard_byte() -> Option<u8> {
    keyboard::read_byte()
}

// ---------------------------------------------------------------------------
// PCI configuration space
// ---------------------------------------------------------------------------

const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// The word written to 0xCF8 to select one 32-bit word of one function's
/// configuration space.
#[inline]
fn pci_config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    1 << 31
        | (bus as u32) << 16
        | (device as u32 & 0x1F) << 11
        | (function as u32 & 0x07) << 8
        | (offset as u32 & 0xFC)
}

/// Read the aligned 32-bit word of PCI configuration space containing
/// `offset`. Reading a function that is not there gives all ones back.
pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        io::outl(PCI_CONFIG_ADDRESS, pci_config_address(bus, device, function, offset));
        io::inl(PCI_CONFIG_DATA)
    }
}

/// Write the aligned 32-bit word of PCI configuration space containing
/// `offset`.
pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        io::outl(PCI_CONFIG_ADDRESS, pci_config_address(bus, device, function, offset));
        io::outl(PCI_CONFIG_DATA, value);
    }
}

// ---------------------------------------------------------------------------
// Reporting and shutdown
// ---------------------------------------------------------------------------

/// The body of `/proc/cpuinfo`, which says what this processor is.
pub fn cpu_info_text() -> alloc::string::String {
    use alloc::string::String;
    let mut out = String::from("processor\t: 0\nvendor_id\t: ");
    let leaf = cpu::cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf.ecx.to_le_bytes());
    out.push_str(core::str::from_utf8(&vendor).unwrap_or("unknown"));
    out.push_str("\ncpu family\t: 6\nmodel name\t: claudeos virtual CPU\n");
    out.push_str("flags\t\t: fpu tsc msr pae cx8 apic sse sse2 syscall nx lm\n\n");
    out
}

/// Ask QEMU's isa-debug-exit device to terminate with `code`.
pub fn qemu_exit(code: u32) -> ! {
    unsafe { io::outl(0xf4, code) };
    loop {
        halt();
    }
}

/// Shut the machine down. Tries the ACPI sleep register QEMU exposes, then the
/// debug-exit device, then simply stops.
pub fn power_off() -> ! {
    unsafe {
        io::outw(0x604, 0x2000); // QEMU / modern ACPI
        io::outw(0xB004, 0x2000); // older QEMU
        io::outw(0x4004, 0x3400); // virt machines
        io::outl(0xf4, 0);
    }
    disable_interrupts();
    loop {
        halt();
    }
}
