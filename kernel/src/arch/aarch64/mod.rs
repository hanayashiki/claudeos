//! aarch64, on a Raspberry Pi 4.
//!
//! The submodules below are private: everything the portable half of the
//! kernel is allowed to reach is re-exported from here, so this file answers
//! `arch/x86_64/mod.rs` name for name. The exceptions are `paging` and `nr`,
//! which are namespaces rather than single names, and `fdt` and `mailbox`,
//! which are namespaces too and which only a driver that exists on this
//! machine alone reaches.

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
pub mod mailbox;
pub mod nr;
pub mod paging;

global_asm!(include_str!("boot.s"), FIRST_DEVICE_BLOCK = const FIRST_DEVICE_BLOCK);
// The entry and exit paths address the trap frame by the offsets the structure
// in trap.rs actually has, rather than by numbers written out beside it.
global_asm!(
    include_str!("vectors.s"),
    FRAME_SIZE = const trap::FRAME_SIZE,
    OFF_X = const trap::OFF_X,
    OFF_X30 = const trap::OFF_X30,
    OFF_ELR = const trap::OFF_ELR,
    OFF_ESR = const trap::OFF_ESR,
    OFF_VECTOR = const trap::OFF_VECTOR,
    OFF_SLOT = const trap::OFF_SLOT,
);
global_asm!(include_str!("switch.s"), FRAME_SIZE = const task::SWITCH_FRAME_SIZE);

// Some of these name a part of the interface without being called from the
// portable half today; they are listed here because this file is the contract.
#[allow(unused_imports)]
pub use clock::{counter_frequency, cycle_counter, read_wall_clock, WallClock};
pub use signal_frame::{
    enter_signal_handler, leave_signal_handler, map_signal_trampoline, MIN_ALT_STACK,
};
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
    dump_registers, end_of_interrupt, exception_name, exception_fault, fault_probe_registers,
    init_interrupt_controller, init_timer, instruction_pointer, irq_vector, mask_irq, page_fault,
    register_irq_handler, register_trap_handler, stack_pointer, trap_error_code, trap_vector,
    unmask_irq, vector_irq, Handler, PageFault, TrapFrame, EXCEPTION_COUNT, IRQ_COUNT,
    KEYBOARD_IRQ, PAGE_FAULT_VECTOR, SERIAL_IRQ, SERIAL_IRQ_ALT, TICK_HZ, TIMER_IRQ,
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
/// board reports memory running all the way up to this address. It is also
/// the point at which the 2 MiB blocks `boot.s` builds change attribute, and
/// that code is given the number below rather than carrying one of its own.
pub const DEVICE_PHYS_BASE: u64 = 0xFC00_0000;

/// Which of the fourth gigabyte's 2 MiB blocks is the first `boot.s` gives
/// device attributes to. It is handed to that code as a constant, so the two
/// cannot name different addresses.
const FIRST_DEVICE_BLOCK: u64 = (DEVICE_PHYS_BASE - 3 * 1024 * 1024 * 1024) / (2 * 1024 * 1024);

const _: () = {
    // `boot.s` covers the fourth gigabyte in 2 MiB blocks and changes
    // attribute at a block boundary inside it.
    assert!(DEVICE_PHYS_BASE >= 3 * 1024 * 1024 * 1024);
    assert!(DEVICE_PHYS_BASE < HHDM_LIMIT);
    assert!(DEVICE_PHYS_BASE % (2 * 1024 * 1024) == 0);
};

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
///
/// Deliberately not `nomem`, for the same reason `msr daifset` and `daifclr`
/// are not. The instruction touches no memory itself, and what it is for is to
/// wait until an interrupt handler has changed some: the idle loop halts and
/// then asks the scheduler what to run, and the answer is what the handler
/// wrote. `nomem` lets the compiler carry a value across the wait in a
/// register.
#[inline]
pub fn halt() {
    unsafe { asm!("wfi", options(nostack)) };
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

/// Take one byte if the transmitter has room for it, and say whether it did.
pub fn console_try_write_byte(byte: u8) -> bool {
    uart::try_write_byte(byte)
}

/// Whether every byte written has left the transmitter.
pub fn console_tx_idle() -> bool {
    uart::tx_idle()
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

/// A word from the processor's own generator, or nothing if it has none.
///
/// FEAT_RNG puts one behind the RNDR system register, and the top nibble of
/// `id_aa64isar0_el1` is what says whether it is there. Reading the register
/// on a processor that does not implement it is an undefined instruction, so
/// the check is what keeps the read from running at all rather than an
/// optimisation.
///
/// The Cortex-A72 in a Raspberry Pi 4 is an ARMv8.0 core and does not have it,
/// and neither does the one QEMU emulates, so nothing here has run this. The
/// board's own generator sits at a fixed address in the peripheral window
/// instead, and driving that would be a device driver rather than this.
///
/// The read reports through the condition flags: all clear for a word it
/// stands behind, Z set for one it could not produce, which the architecture
/// allows it for a while after a reset.
pub fn hardware_random() -> Option<u64> {
    let isar0: u64;
    unsafe { asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
    if isar0 >> 60 == 0 {
        return None;
    }
    for _ in 0..16 {
        let value: u64;
        let ok: u64;
        unsafe {
            // s3_3_c2_c4_0 is RNDR, spelled by its encoding because the
            // assembler only knows the name with the feature turned on, and
            // this kernel is built for a machine that does not have it.
            asm!(
                "mrs {value}, s3_3_c2_c4_0",
                "cset {ok}, ne",
                value = out(reg) value,
                ok = out(reg) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

/// There is no debug-exit device on this board, so a test that wants the
/// machine to stop gets it stopped and nothing more.
pub fn qemu_exit(_code: u32) -> ! {
    power_off()
}

/// The power management block, and the word that has to be in the top of every
/// write to it for the write to count.
///
/// Every register named below is the one Linux's `bcm2835_wdt.c` drives, and
/// the reset, the halt and the watchdog below follow what that driver does.
const POWER_MANAGEMENT: u64 = PERIPHERAL_BASE + 0x10_0000;
const PM_PASSWORD: u32 = 0x5A00_0000;
/// Which partition to come back up in, six bits spread over the even bits 0
/// to 10. Zero is the ordinary one and boots this kernel again. 63, all six
/// bits set, is the one the board's boot firmware takes as "stay halted".
const PM_RSTS: u64 = 0x20;
const RSTS_PARTITION_BOOT: u32 = 0;
const RSTS_PARTITION_HALT: u32 = 0x555;
/// The watchdog's countdown, in ticks of a clock running at 65536 Hz. The
/// field is the low twenty bits, so the longest countdown is just under
/// sixteen seconds.
const PM_WDOG: u64 = 0x24;
const WDOG_TIME_MASK: u32 = 0x000F_FFFF;
const WDOG_TICKS_PER_SECOND: u32 = 1 << 16;
/// Reset control. Asking for a full reset here is how anything on this board
/// restarts it; there is no way to cut the power from software. The same
/// setting is what makes the countdown above end in a reset, and writing
/// `RSTC_RESET` in its place is how Linux stops the watchdog.
const PM_RSTC: u64 = 0x1C;
const RSTC_FULL_RESET: u32 = 0x20;
const RSTC_RESET: u32 = 0x102;
const RSTC_CONFIG_MASK: u32 = 0xFFFF_FFCF;
const RSTS_PARTITION_MASK: u32 = 0xFFFF_FAAA;

/// How long the watchdog waits without a feed before it resets the board.
/// Fifteen seconds fits the twenty-bit countdown, and is long enough that no
/// section of this kernel that masks interrupts on purpose comes near it.
const WATCHDOG_SECONDS: u32 = 15;

const _: () = assert!(WATCHDOG_SECONDS * WDOG_TICKS_PER_SECOND <= WDOG_TIME_MASK);

/// Whether the watchdog was started, so that a feed never writes a countdown
/// into a watchdog that `watchdog=off` or a stopped panic left alone.
static WATCHDOG_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The address of one of the power management registers.
fn power_management(offset: u64) -> *mut u32 {
    crate::mm::phys_to_virt(POWER_MANAGEMENT + offset) as *mut u32
}

/// Set the partition the next reset of any kind comes back up in.
///
/// Read and written back rather than written outright, because the rest of
/// the register is status the firmware reads after the reset.
fn set_reset_partition(partition: u32) {
    unsafe {
        let status = core::ptr::read_volatile(power_management(PM_RSTS)) & RSTS_PARTITION_MASK;
        core::ptr::write_volatile(power_management(PM_RSTS), PM_PASSWORD | status | partition);
    }
}

/// Reset the board into `partition`, 150 microseconds from now.
///
/// The partition is written first. The watchdog may already be counting down
/// from its last feed when this is reached, and a countdown that ends between
/// two of these writes resets into whatever partition the register holds at
/// that moment; with the partition written first, that is the one asked for,
/// so a halt cannot come back up as a restart or a restart stay halted.
///
/// Nothing can feed the watchdog after the countdown below is set, which would
/// put the reset off by the length of a feed: interrupts are masked before the
/// first write and never unmasked again, and a feed only comes from the timer
/// interrupt or from the panic path that ends here.
fn reset_into(partition: u32) -> ! {
    // The reset follows the request by 150 microseconds, and the port may
    // still hold up to 2.8 ms of the last line printed.
    crate::serial::drain();
    disable_interrupts();
    set_reset_partition(partition);
    unsafe {
        core::ptr::write_volatile(power_management(PM_WDOG), PM_PASSWORD | 10);
        let control = core::ptr::read_volatile(power_management(PM_RSTC)) & RSTC_CONFIG_MASK;
        core::ptr::write_volatile(
            power_management(PM_RSTC),
            PM_PASSWORD | control | RSTC_FULL_RESET,
        );
    }
    loop {
        halt();
    }
}

/// Stop the machine, by asking the watchdog for a reset into the halt
/// partition, which the board's boot firmware takes as "stay stopped". There
/// is no way to cut the power from software. This is what Linux does on every
/// Raspberry Pi.
pub fn power_off() -> ! {
    reset_into(RSTS_PARTITION_HALT)
}

/// Restart the machine: the same reset as `power_off`, into the partition the
/// firmware boots normally, which on this board fetches and starts a kernel
/// again from wherever it came from.
pub fn restart() -> ! {
    reset_into(RSTS_PARTITION_BOOT)
}

/// Start the board's watchdog, and say how many seconds without a feed it
/// allows before it resets the board.
///
/// What it is fed from is the periodic timer interrupt, so what it catches is
/// the kernel no longer taking that interrupt: stuck with interrupts masked on
/// a lock nothing will release, looping inside the trap path, or stopped in a
/// panic that did not reach its own restart. It does not catch a kernel that
/// still takes the tick and gets nothing done, such as a scheduler that never
/// picks the task that would make progress, or a shell that is wedged; the
/// tick still arrives there, and the tick is all a feed is evidence of.
///
/// The expiry resets into the ordinary partition, so a board the watchdog
/// restarts boots again rather than staying halted. The partition is written
/// here because a halt leaves the halt partition in the register, and nothing
/// promises that what the firmware does next clears it.
///
/// Started only when the device tree describes it, and nothing when there is
/// no tree. The board's firmware always hands one over, and it has the node.
/// QEMU's emulated Pi 4 hands over a tag list instead, and QEMU 11.1.1 does
/// not count the watchdog down at all: it resets the machine on the write to
/// reset control that starts it, so starting it there would restart every boot
/// at this line. QEMU's development tree counts it down from commit
/// 21fcfb604608, which the 11.1.1 stable release does not contain.
pub fn watchdog_start() -> Option<u32> {
    let node = fdt::find_compatible(b"brcm,bcm2835-pm-wdt")?;
    if !node.enabled() {
        return None;
    }
    set_reset_partition(RSTS_PARTITION_BOOT);
    WATCHDOG_ARMED.store(true, core::sync::atomic::Ordering::Relaxed);
    watchdog_feed();
    unsafe {
        let control = core::ptr::read_volatile(power_management(PM_RSTC)) & RSTC_CONFIG_MASK;
        core::ptr::write_volatile(
            power_management(PM_RSTC),
            PM_PASSWORD | control | RSTC_FULL_RESET,
        );
    }
    Some(WATCHDOG_SECONDS)
}

/// Put the watchdog's countdown back to its full length. Does nothing if the
/// watchdog was never started or has been stopped.
///
/// One register write, so there is nothing for an interrupt to split.
#[inline]
pub fn watchdog_feed() {
    if !WATCHDOG_ARMED.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    unsafe {
        core::ptr::write_volatile(
            power_management(PM_WDOG),
            PM_PASSWORD | (WATCHDOG_SECONDS * WDOG_TICKS_PER_SECOND),
        );
    }
}

/// Stop the watchdog, whoever started it: this kernel, or firmware that left
/// it running. For a debugger that holds the processor still, and for a panic
/// that is meant to stay stopped.
pub fn watchdog_stop() {
    WATCHDOG_ARMED.store(false, core::sync::atomic::Ordering::Relaxed);
    unsafe {
        core::ptr::write_volatile(power_management(PM_RSTC), PM_PASSWORD | RSTC_RESET);
    }
}
