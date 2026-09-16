//! x86-64.
//!
//! The submodules below are private: everything the portable half of the
//! kernel is allowed to reach is re-exported from here, so this file is the
//! whole of the interface an implementation of another architecture has to
//! provide. The two exceptions are `paging` and `nr`, which are namespaces
//! rather than single names.

use core::arch::asm;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU8, Ordering};

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
// The three paths in and out of a task address the trap frame, the per-CPU
// block and the user selectors by the numbers the Rust that defines them
// actually has, rather than by numbers written out beside them.
global_asm!(
    include_str!("cpu/interrupts.s"),
    OFF_CS = const cpu::idt::OFF_CS,
    OFF_VECTOR = const cpu::idt::OFF_VECTOR,
    OFF_RIP = const cpu::idt::OFF_RIP,
    options(att_syntax),
);
global_asm!(
    include_str!("switch.s"),
    FRAME_WORDS = const task::SWITCH_FRAME_WORDS,
    options(att_syntax),
);
global_asm!(
    include_str!("syscall_entry.s"),
    FRAME_SIZE = const cpu::idt::FRAME_SIZE,
    OFF_RAX = const cpu::idt::OFF_RAX,
    OFF_RBX = const cpu::idt::OFF_RBX,
    OFF_RCX = const cpu::idt::OFF_RCX,
    OFF_RDX = const cpu::idt::OFF_RDX,
    OFF_RSI = const cpu::idt::OFF_RSI,
    OFF_RDI = const cpu::idt::OFF_RDI,
    OFF_RBP = const cpu::idt::OFF_RBP,
    OFF_R8 = const cpu::idt::OFF_R8,
    OFF_R9 = const cpu::idt::OFF_R9,
    OFF_R10 = const cpu::idt::OFF_R10,
    OFF_R11 = const cpu::idt::OFF_R11,
    OFF_R12 = const cpu::idt::OFF_R12,
    OFF_R13 = const cpu::idt::OFF_R13,
    OFF_R14 = const cpu::idt::OFF_R14,
    OFF_R15 = const cpu::idt::OFF_R15,
    OFF_VECTOR = const cpu::idt::OFF_VECTOR,
    OFF_ERROR_CODE = const cpu::idt::OFF_ERROR_CODE,
    OFF_RIP = const cpu::idt::OFF_RIP,
    OFF_CS = const cpu::idt::OFF_CS,
    OFF_RFLAGS = const cpu::idt::OFF_RFLAGS,
    OFF_RSP = const cpu::idt::OFF_RSP,
    OFF_SS = const cpu::idt::OFF_SS,
    PER_CPU_KERNEL_RSP = const cpu::OFF_KERNEL_RSP,
    PER_CPU_USER_RSP = const cpu::OFF_USER_RSP,
    USER_CS = const cpu::gdt::USER_CODE,
    USER_SS = const cpu::gdt::USER_DATA,
    VECTOR_SYSCALL = const task::VECTOR_SYSCALL,
    options(att_syntax),
);

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

// Deliberately not `nomem`. These two instructions touch no memory themselves,
// but every critical section in the kernel is built on them, and the point of
// such a section is that a store lands before interrupts come back on.
// Asserting `nomem` tells the compiler the asm reads and writes nothing, which
// lets it move loads and stores across it. Leaving it off makes the asm a
// barrier the compiler will not reorder memory operations past.
#[inline]
pub fn disable_interrupts() {
    unsafe { asm!("cli", options(nostack, preserves_flags)) };
}

#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("sti", options(nostack, preserves_flags)) };
}

// ---------------------------------------------------------------------------
// The debug console
// ---------------------------------------------------------------------------

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
// The processor's own random numbers
// ---------------------------------------------------------------------------

/// Which instruction this processor answers with, worked out once.
///
/// RDSEED is the entropy source itself and RDRAND is a generator reseeded from
/// it, so RDSEED is preferred where both are there; a processor with neither
/// leaves `hardware_random` with nothing to offer.
static RANDOM_SOURCE: AtomicU8 = AtomicU8::new(SOURCE_UNKNOWN);
const SOURCE_UNKNOWN: u8 = 0;
const SOURCE_NONE: u8 = 1;
const SOURCE_RDRAND: u8 = 2;
const SOURCE_RDSEED: u8 = 3;

fn random_source() -> u8 {
    let known = RANDOM_SOURCE.load(Ordering::Relaxed);
    if known != SOURCE_UNKNOWN {
        return known;
    }
    // CPUID leaf 7 does not exist on a processor whose leaf 0 does not reach
    // it, and asking for a leaf that is not there answers with some other
    // leaf's contents rather than with zeroes.
    let highest = cpu::cpuid(0, 0).eax;
    let found = if highest >= 7 && cpu::cpuid(7, 0).ebx & (1 << 18) != 0 {
        SOURCE_RDSEED
    } else if cpu::cpuid(1, 0).ecx & (1 << 30) != 0 {
        SOURCE_RDRAND
    } else {
        SOURCE_NONE
    };
    RANDOM_SOURCE.store(found, Ordering::Relaxed);
    found
}

/// A word from the processor's own generator, or nothing if it has none.
///
/// Both instructions report through the carry flag that they had nothing ready
/// rather than by blocking, and Intel's own guidance is to ask again a handful
/// of times before giving up. Giving up is not a failure here: the caller
/// treats this as one source among several.
pub fn hardware_random() -> Option<u64> {
    let source = random_source();
    if source == SOURCE_NONE {
        return None;
    }
    for _ in 0..16 {
        let value: u64;
        let ok: u8;
        unsafe {
            if source == SOURCE_RDSEED {
                asm!(
                    "rdseed {value}",
                    "setc {ok}",
                    value = out(reg) value,
                    ok = out(reg_byte) ok,
                    options(nomem, nostack),
                );
            } else {
                asm!(
                    "rdrand {value}",
                    "setc {ok}",
                    value = out(reg) value,
                    ok = out(reg_byte) ok,
                    options(nomem, nostack),
                );
            }
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// What a program is told about the processor
// ---------------------------------------------------------------------------

/// AT_HWCAP and AT_HWCAP2 for a program on this processor.
///
/// AT_HWCAP is the boot processor's CPUID leaf 1 EDX, which is what Linux's
/// `ELF_HWCAP` is: `boot_cpu_data.x86_capability[CPUID_1_EDX]`. A program can
/// run CPUID itself on this machine, so the word only repeats what it could
/// ask for.
///
/// AT_HWCAP2 is not a CPUID word. It carries the features Linux has turned on
/// for user mode, and x86's uapi/asm/hwcap2.h has two: HWCAP2_RING3MPL, which
/// Linux sets only after enabling ring-3 MWAIT through
/// MSR_MISC_FEATURES_ENABLES on a Xeon Phi, and HWCAP2_FSGSBASE, which it sets
/// only after setting CR4.FSGSBASE, with a context switch that saves the bases
/// a program writes with those instructions. This kernel does neither: it
/// never writes that MSR, and CR4 gets PAE and PGE from `boot.s` and OSFXSR
/// and OSXMMEXCPT from `init_sse` and nothing else. So the word is zero,
/// whatever the processor could do.
pub fn elf_hwcaps() -> (u64, u64) {
    (cpu::cpuid(1, 0).edx as u64, 0)
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
    crate::serial::drain();
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

/// The chipset's reset control register, on every Intel south bridge from the
/// PIIX3 in QEMU's `pc` machine onwards. Bit 1 asks for a hard reset rather
/// than a processor-only one; setting bit 2 is what starts it.
const RESET_CONTROL: u16 = 0xCF9;
/// The keyboard controller's command and status port. Status bit 1 is set
/// while its input buffer holds a byte it has not taken yet, and command 0xFE
/// pulses the line that resets the machine.
const KEYBOARD_CONTROLLER: u16 = 0x64;

/// Restart the machine.
///
/// Three ways, each reached only if the one before did not reset the machine.
/// The reset control register is first: it is the chipset's own reset and asks
/// nothing of any other device. The keyboard controller's reset line is next,
/// because it is older and nearly universal on a PC, but it is a command to a
/// controller that has to be idle to take it. A triple fault is last: a
/// processor that faults while it cannot deliver the fault stops, and the
/// board resets a stopped processor, so it needs nothing from the chipset, but
/// it is the processor's reset rather than a request to the board's.
///
/// QEMU's `pc` machine answers each of the three with the same system reset,
/// and `-no-reboot` turns that reset into QEMU exiting. The first is the one
/// that does it there: the PIIX3 it emulates acts on bit 2 of 0xCF9 at once.
pub fn restart() -> ! {
    crate::serial::drain();
    disable_interrupts();
    unsafe {
        io::outb(RESET_CONTROL, 0x02);
        settle();
        io::outb(RESET_CONTROL, 0x06);
        settle();

        for _ in 0..100_000 {
            if io::inb(KEYBOARD_CONTROLLER) & 0x02 == 0 {
                break;
            }
        }
        io::outb(KEYBOARD_CONTROLLER, 0xFE);
        settle();

        // An interrupt table with no room for any entry. The breakpoint cannot
        // be delivered through it, nor the general protection fault that says
        // so, nor the double fault after that, and the third is a shutdown.
        let empty = [0u8; 10];
        asm!("lidt [{}]", "int3", in(reg) empty.as_ptr());
    }
    loop {
        halt();
    }
}

/// About fifty milliseconds on a real machine, for a reset request to take
/// effect before the next one is tried. A write to port 0x80, the power-on
/// self-test port, costs about a microsecond on a real bus and nothing listens
/// to it.
unsafe fn settle() {
    for _ in 0..50_000 {
        io::io_wait();
    }
}

// ---------------------------------------------------------------------------
// The watchdog
// ---------------------------------------------------------------------------
//
// QEMU is not given a watchdog device for this machine, so there is nothing to
// start, feed or stop. These are here because the portable half calls them on
// every machine, and the board's are what they answer to.

/// Start the machine's watchdog. There is none, which is the `None`.
pub fn watchdog_start() -> Option<u32> {
    None
}

#[inline]
pub fn watchdog_feed() {}

pub fn watchdog_stop() {}
