//! aarch64, on a Raspberry Pi 4.
//!
//! The submodules below are private: everything the portable half of the
//! kernel is allowed to reach is re-exported from here, so this file answers
//! `arch/x86_64/mod.rs` name for name. The two exceptions are `paging` and
//! `nr`, which are namespaces rather than single names.

use core::arch::asm;
use core::arch::global_asm;

mod clock;
mod fdt;
mod gic;
mod signal_frame;
mod syscall;
mod task;
mod trap;
mod uart;

pub mod nr;
pub mod paging;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));
global_asm!(include_str!("switch.s"));

// Some of these name a part of the interface without being called from the
// portable half today; they are listed here because this file is the contract.
#[allow(unused_imports)]
pub use clock::{cycle_counter, read_wall_clock, WallClock};
pub use signal_frame::{enter_signal_handler, leave_signal_handler};
pub use syscall::{
    arch_prctl, fork_child_frame, init_syscall_entry, set_syscall_result, syscall_args,
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
pub const MACHINE: &str = "aarch64";

/// The `e_machine` an ELF file must carry to run here (EM_AARCH64).
pub const ELF_MACHINE: u16 = 0xB7;

/// Where this board's registers live. Earlier Pis used a different base, which
/// is the first thing to change when pointing this at another one.
pub const PERIPHERAL_BASE: u64 = 0xFE00_0000;

extern "C" {
    static kernel_stack_top: u8;
}

// ---------------------------------------------------------------------------
// Where things sit in the address space
// ---------------------------------------------------------------------------

/// Direct map of physical memory, installed by the boot code. The two halves
/// of the address space are translated through separate base registers here,
/// and everything from this address up goes through the second one.
pub const HHDM_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Size of the region the boot code direct-maps: four 1 GiB blocks, which on
/// this board is memory and then the peripherals.
pub const HHDM_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// Virtual base the kernel image is linked at.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;
/// Physical address the firmware loads an AArch64 kernel at.
pub const KERNEL_PHYS_START: u64 = 0x8_0000;

pub const KERNEL_HEAP_BASE: u64 = 0xFFFF_C000_0000_0000;
/// Ceiling on heap growth. It stays inside the single second-level table the
/// heap's top-level entry points at, so growing never has to touch a table
/// shared with an address space that already exists.
pub const KERNEL_HEAP_MAX: usize = 512 * 1024 * 1024;

/// Physical memory this board claims for itself whatever the firmware says
/// about it: everything under the address a kernel is loaded at holds the
/// firmware's own stub and the tables it left behind.
pub const RESERVED_PHYS: &[(u64, u64)] = &[(0, KERNEL_PHYS_START)];

// ---------------------------------------------------------------------------
// Bringing the processor up
// ---------------------------------------------------------------------------

/// Where `boot.s` lands once translation is on and it is running out of the
/// higher half. `handoff` is whatever the firmware left in x0, which on a
/// board following the Linux AArch64 boot protocol is the physical address of
/// a device tree describing the machine.
#[no_mangle]
pub extern "C" fn kmain(handoff: u64) -> ! {
    crate::serial::init();

    let mut boot = crate::boot::BootInfo::new();
    if !fdt::parse(handoff, &mut boot) {
        // No device tree, so fall back to what is known about the board: a
        // gigabyte of memory from zero, which is the least any Pi 4 has.
        boot.add_region(0, 1024 * 1024 * 1024, true);
    }
    crate::start(&boot)
}

/// Point the processor at the exception vectors. Nothing may fault before this
/// has run.
pub fn init_traps() {
    trap::init_vectors();
}

/// Nothing more to set up on the processor itself: the boot code already gave
/// this level and the one below access to the floating point and vector unit,
/// and there is no per-CPU block for the entry stubs to reach through, because
/// the entry stubs reach the frame through the stack pointer.
pub fn init_cpu() {}

/// Stop the CPU until the next interrupt.
#[inline]
pub fn halt() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

// ---------------------------------------------------------------------------
// Interrupt enable state
// ---------------------------------------------------------------------------

#[inline]
pub fn interrupts_enabled() -> bool {
    let state: u64;
    unsafe { asm!("mrs {}, daif", out(reg) state, options(nomem, nostack)) };
    // The bit is a mask, so it is set when interrupts are off.
    state & (1 << 7) == 0
}

#[inline]
pub fn disable_interrupts() {
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
}

#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
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

/// This board has no keyboard of its own; everything typed at it arrives over
/// the serial port.
pub fn keyboard_byte() -> Option<u8> {
    None
}

// ---------------------------------------------------------------------------
// PCI configuration space
// ---------------------------------------------------------------------------

/// There is a PCIe root complex on this board, but it is reached through
/// memory-mapped registers that have to be found in the device tree first, not
/// through a pair of ports. Until that is written, a scan finds nothing, which
/// is what all-ones means to the caller.
pub fn pci_config_read32(_bus: u8, _device: u8, _function: u8, _offset: u8) -> u32 {
    0xFFFF_FFFF
}

pub fn pci_config_write32(_bus: u8, _device: u8, _function: u8, _offset: u8, _value: u32) {}

// ---------------------------------------------------------------------------
// Reporting and shutdown
// ---------------------------------------------------------------------------

/// The body of `/proc/cpuinfo`, which says what this processor is.
pub fn cpu_info_text() -> alloc::string::String {
    use alloc::string::String;
    let midr: u64;
    unsafe { asm!("mrs {}, midr_el1", out(reg) midr, options(nomem, nostack)) };
    let mut out = String::from("processor\t: 0\n");
    out.push_str("BogoMIPS\t: 108.00\n");
    out.push_str("Features\t: fp asimd\n");
    out.push_str("CPU implementer\t: 0x41\n");
    out.push_str("CPU architecture: 8\n");
    let _ = midr;
    out.push_str("CPU variant\t: 0x0\nCPU part\t: 0xd08\nCPU revision\t: 3\n\n");
    out
}

/// There is no debug-exit device on this board, so a test that wants the
/// machine to stop gets it stopped and nothing more.
pub fn qemu_exit(_code: u32) -> ! {
    power_off()
}

/// Stop the machine. Nothing here can cut the power, so this masks everything
/// and parks the core.
pub fn power_off() -> ! {
    disable_interrupts();
    loop {
        halt();
    }
}
