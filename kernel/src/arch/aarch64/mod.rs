//! aarch64, on a Raspberry Pi 4.
//!
//! The submodules below are private: everything the portable half of the
//! kernel is allowed to reach is re-exported from here, so this file answers
//! `arch/x86_64/mod.rs` name for name. The exceptions are `paging` and `nr`,
//! which are namespaces rather than single names, and `fdt`, which is one too
//! and which only a driver that exists on this machine alone reaches.

use core::arch::asm;
use core::arch::global_asm;

mod atags;
mod clock;
mod gic;
mod signal_frame;
mod syscall;
mod task;
mod trap;
mod uart;

pub mod fdt;
pub mod nr;
pub mod paging;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));
global_asm!(include_str!("switch.s"));

// Some of these name a part of the interface without being called from the
// portable half today; they are listed here because this file is the contract.
#[allow(unused_imports)]
pub use clock::{counter_frequency, cycle_counter, read_wall_clock, WallClock};
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
pub const MACHINE: &str = "aarch64";

/// The `e_machine` an ELF file must carry to run here (EM_AARCH64).
pub const ELF_MACHINE: u16 = 0xB7;

/// Where this board's registers live. Earlier Pis used a different base, which
/// is the first thing to change when pointing this at another one.
const PERIPHERAL_BASE: u64 = 0xFE00_0000;

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
/// Size of the region the boot code direct-maps: the low four gigabytes,
/// which on this board is memory and then the peripherals.
pub const HHDM_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
/// Where the direct map stops covering memory and starts covering registers.
/// Everything from here to `HHDM_LIMIT` carries device attributes, so a
/// device found there is already reachable uncached through `phys_to_virt`
/// and needs no mapping of its own.
///
/// It is also the ceiling on what the frame allocator may hand out: a frame
/// above it has no cacheable alias, and the firmware on a 4 GiB or 8 GiB
/// board reports memory running all the way up to this address. `boot.s`
/// carries the same number, as the point its 2 MiB blocks change attribute.
pub const DEVICE_PHYS_BASE: u64 = 0xFC00_0000;

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
/// higher half.
///
/// `handoff` is whatever the firmware left in x0. A board following the Linux
/// AArch64 boot protocol puts the physical address of a device tree there; a
/// board with no device tree to give falls back to the older tag list, which
/// is what QEMU's emulated Pi 4 does. Which one arrived is decided by looking
/// at what is actually in memory, because nothing else says.
#[no_mangle]
pub extern "C" fn kmain(handoff: u64) -> ! {
    crate::serial::init();

    let mut boot = crate::boot::BootInfo::new();
    let described = if fdt::parse(handoff, &mut boot) {
        "device tree"
    } else if atags::parse(handoff, &mut boot) {
        "tag list"
    } else {
        // Neither handoff is there, so fall back to what is known about the
        // board: a gigabyte of memory from zero, which is the least a Pi 4
        // has, and no ram disk.
        boot.add_region(0, 1024 * 1024 * 1024, true);
        "nothing"
    };
    crate::println!("handoff: {} at {:#x}", described, handoff);
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

/// Make bytes the kernel has just written fetchable as instructions.
///
/// The two caches are not coherent with each other here. Bytes the kernel
/// stores go into the data cache; the instruction side fetches past it, and
/// may already hold whatever was at those addresses before. So the data cache
/// has to be cleaned down to the level both sides share, and the instruction
/// cache told to forget what it has, before anything jumps there.
///
/// Leaving this out fails in a way emulation never shows: a program executes
/// whatever the instruction cache happened to be holding, which depends on
/// what ran before it and on where the page boundaries fall.
pub fn sync_instruction_cache(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let ctr: u64;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    // Both fields hold the log2 of the line length in words.
    let data_line = 4u64 << ((ctr >> 16) & 0xF);
    let instruction_line = 4u64 << (ctr & 0xF);
    let end = start + len as u64;

    let mut at = start & !(data_line - 1);
    while at < end {
        unsafe { asm!("dc cvau, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += data_line;
    }
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };

    let mut at = start & !(instruction_line - 1);
    while at < end {
        unsafe { asm!("ic ivau, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += instruction_line;
    }
    unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
}

// ---------------------------------------------------------------------------
// Handing memory to a device that reads and writes it itself
// ---------------------------------------------------------------------------
//
// A device fetching a packet out of memory is not a processor and does not
// look in the processor's caches. Nothing on this board promises otherwise:
// the device tree says which blocks are coherent with the caches, and the
// Ethernet controller on a Pi 4 is not one of them. So a buffer the kernel has
// written has to be pushed out of the cache before the device is told to read
// it, and a buffer the device has written has to be dropped from the cache
// before the kernel reads it, or the read is answered from a line fetched
// before the transfer.
//
// "Point of coherency" is the level at which the processor and everything else
// that reaches memory agree on what is there, which is what `dc cvac`,
// `dc ivac` and `dc civac` operate to. The barrier at the end of each is
// `dsb sy`, not the `dsb ish` the instruction-side one uses, because the
// observer being waited for is outside the inner shareable domain.
//
// Cache maintenance works on whole lines, so a range that does not start and
// end on a line boundary shares its first and last line with whatever is
// next to it in memory. Plain invalidation of those two would throw away a
// neighbour's unwritten data, so they are cleaned as well as invalidated and
// the neighbour's bytes go to memory instead of being lost. This is what
// Linux's `__pi_dcache_inval_poc` does, and the boot-time check exercises it.

/// Line length of the data cache, from CTR_EL0, whose field holds the log2 of
/// the length in words.
#[inline]
fn data_cache_line() -> u64 {
    let ctr: u64;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    4u64 << ((ctr >> 16) & 0xF)
}

/// Push the kernel's writes over `start..start+len` out to where a device
/// reading memory will see them. Call before handing the range to a device.
pub fn clean_data_cache(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let line = data_cache_line();
    let end = start + len as u64;
    let mut at = start & !(line - 1);
    while at < end {
        unsafe { asm!("dc cvac, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Throw away anything cached over `start..start+len`, so a read afterwards
/// fetches what a device wrote there. Call after the device has finished.
///
/// A partial line at either end is cleaned as well as invalidated, so that a
/// neighbour sharing that line keeps its data.
pub fn invalidate_data_cache(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let line = data_cache_line();
    let end = start + len as u64;
    let first = start & !(line - 1);
    // One past the last line the range touches.
    let last = (end + line - 1) & !(line - 1);

    let mut at = first;
    while at < last {
        let partial = (at == first && start != first) || (at + line == last && end != last);
        unsafe {
            if partial {
                asm!("dc civac, {}", in(reg) at, options(nostack, preserves_flags));
            } else {
                asm!("dc ivac, {}", in(reg) at, options(nostack, preserves_flags));
            }
        }
        at += line;
    }
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Both at once: the kernel's writes go out, and nothing stale is left behind.
/// This is what a buffer wants when it has just been filled in and is about to
/// be lent to a device that will overwrite it.
pub fn flush_data_cache(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let line = data_cache_line();
    let end = start + len as u64;
    let mut at = start & !(line - 1);
    while at < end {
        unsafe { asm!("dc civac, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
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

// Deliberately not `nomem`. These two instructions touch no memory themselves,
// but every critical section in the kernel is built on them, and the point of
// such a section is that a store lands before interrupts come back on.
// Asserting `nomem` tells the compiler the asm reads and writes nothing, which
// lets it move loads and stores across it. Leaving it off makes the asm a
// barrier the compiler will not reorder memory operations past.
#[inline]
pub fn disable_interrupts() {
    unsafe { asm!("msr daifset, #2", options(nostack, preserves_flags)) };
}

#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("msr daifclr, #2", options(nostack, preserves_flags)) };
}

// ---------------------------------------------------------------------------
// The debug console
// ---------------------------------------------------------------------------

/// Where the console's registers are. Named out here so that a check can hold
/// the tree's answer up against the kernel's own.
pub const CONSOLE_PHYS: u64 = uart::UART0;

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

/// The power management block, and the word that has to be in the top of every
/// write to it for the write to count.
const POWER_MANAGEMENT: u64 = PERIPHERAL_BASE + 0x10_0000;
const PM_PASSWORD: u32 = 0x5A00_0000;
/// Which partition to come back up in; zero is the ordinary one.
const PM_RSTS: u64 = 0x20;
/// The watchdog's countdown, in ticks of a 65 kHz clock.
const PM_WDOG: u64 = 0x24;
/// Reset control. Asking for a full reset here is how anything on this board
/// restarts it; there is no way to cut the power from software.
const PM_RSTC: u64 = 0x1C;
const RSTC_FULL_RESET: u32 = 0x20;
const RSTC_CONFIG_MASK: u32 = 0xFFFF_FFCF;
const RSTS_PARTITION_MASK: u32 = 0xFFFF_FAAA;

/// Stop the machine, by asking the watchdog to restart it and then not being
/// there when it does. A board told not to reboot stops instead, which is what
/// a finished test wants.
pub fn power_off() -> ! {
    disable_interrupts();
    unsafe {
        let at = |offset: u64| crate::mm::phys_to_virt(POWER_MANAGEMENT + offset) as *mut u32;
        let partition = core::ptr::read_volatile(at(PM_RSTS)) & RSTS_PARTITION_MASK;
        core::ptr::write_volatile(at(PM_RSTS), PM_PASSWORD | partition);
        core::ptr::write_volatile(at(PM_WDOG), PM_PASSWORD | 10);
        let control = core::ptr::read_volatile(at(PM_RSTC)) & RSTC_CONFIG_MASK;
        core::ptr::write_volatile(at(PM_RSTC), PM_PASSWORD | control | RSTC_FULL_RESET);
    }
    loop {
        halt();
    }
}
