//! Exceptions and interrupt requests: the vector numbering, the interrupt
//! controller, and what a trap frame says about the fault that built it.

use super::cpu::idt::{self, EXCEPTION_NAMES};
use super::cpu::{pic, pit, read_cr2};

pub use super::cpu::idt::{Handler, TrapFrame};

/// Vectors 0..EXCEPTION_COUNT are faults raised by the CPU itself.
pub const EXCEPTION_COUNT: u8 = 32;
/// The one exception the kernel repairs rather than reports.
pub const PAGE_FAULT_VECTOR: u8 = 14;

/// How many device interrupt lines the machine has.
pub const IRQ_COUNT: u8 = 16;
/// The line the periodic timer arrives on.
pub const TIMER_IRQ: u8 = 0;
/// The line the machine's own keyboard arrives on.
pub const KEYBOARD_IRQ: u8 = 1;
/// The line the console UART arrives on.
pub const SERIAL_IRQ: u8 = 4;
/// The second UART's line. PCI interrupts are shared with it on this machine,
/// so console input can be announced there too.
pub const SERIAL_IRQ_ALT: u8 = 3;

/// Timer interrupts per second. Every timeout in the kernel is a whole number
/// of these.
pub const TICK_HZ: u32 = pit::TICK_HZ;

/// The vector an interrupt on `irq` is delivered through.
#[inline]
pub fn irq_vector(irq: u8) -> u8 {
    pic::PIC1_OFFSET + irq
}

/// The line a vector belongs to, or nothing when it is not a device interrupt.
#[inline]
pub fn vector_irq(vector: u64) -> Option<u8> {
    let base = pic::PIC1_OFFSET as u64;
    if vector >= base && vector < base + IRQ_COUNT as u64 {
        Some((vector - base) as u8)
    } else {
        None
    }
}

/// Call `handler` on every trap through `vector`. Written during single
/// threaded start-up only.
pub fn register_trap_handler(vector: u8, handler: Handler) {
    idt::register(vector, handler);
}

/// Call `handler` on every interrupt from `irq`.
pub fn register_irq_handler(irq: u8, handler: Handler) {
    idt::register(irq_vector(irq), handler);
}

/// Bring the interrupt controller up with every line masked.
pub fn init_interrupt_controller() {
    pic::init();
}

/// Start the periodic timer at `hz` interrupts a second.
pub fn init_timer(hz: u32) {
    pit::init(hz);
}

/// Let interrupts from `irq` through.
pub fn unmask_irq(irq: u8) {
    pic::unmask(irq);
}

/// Stop interrupts from `irq`.
pub fn mask_irq(irq: u8) {
    pic::mask(irq);
}

/// Tell the controller the handler for `irq` is finished. Until this is done
/// no further interrupt on that line is delivered.
pub fn end_of_interrupt(irq: u8) {
    pic::end_of_interrupt(irq);
}

/// The name of a CPU exception, for a message.
pub fn exception_name(vector: u64) -> &'static str {
    EXCEPTION_NAMES.get(vector as usize).copied().unwrap_or("unknown")
}

/// The signal a user-mode program is sent when it takes the exception `frame`
/// describes, with the code and address a handler is told, as Linux's
/// arch/x86/kernel/traps.c raises them: a divide error is SIGFPE at the
/// instruction, an invalid opcode SIGILL at the instruction, a general
/// protection fault SIGSEGV from the kernel with no address, and a missing or
/// bad stack segment or an alignment check SIGBUS.
pub fn exception_fault(frame: &TrapFrame) -> crate::signal::Fault {
    use crate::signal::*;
    let at = frame.rip;
    let (signal, code, address) = match frame.vector {
        0 => (SIGFPE, FPE_INTDIV, at),
        1 | 3 => (SIGTRAP, TRAP_BRKPT, at),
        6 => (SIGILL, ILL_ILLOPN, at),
        16 | 19 => (SIGFPE, FPE_FLTUNK, at),
        11 | 12 => (SIGBUS, SI_KERNEL, 0),
        17 => (SIGBUS, BUS_ADRALN, at),
        _ => (SIGSEGV, SI_KERNEL, 0),
    };
    Fault { signal, code, address }
}

/// What the CPU says about a page fault.
pub struct PageFault {
    /// The address the access was to. Virtual, in the faulting task's own
    /// address space.
    pub address: u64,
    /// The machine's own encoding of the cause, for a message.
    pub raw: u64,
    /// A page was there and the access was refused, rather than nothing being
    /// mapped at all.
    pub present: bool,
    /// The access was a write.
    pub write: bool,
    /// The access was made by user-mode code.
    pub user: bool,
    /// The access was an instruction fetch.
    pub instruction_fetch: bool,
}

/// Read the fault out of the frame the CPU built. Only meaningful inside the
/// page-fault handler, and only before interrupts are re-enabled: the faulting
/// address is held in a register the next fault would overwrite.
pub fn page_fault(frame: &TrapFrame) -> PageFault {
    let code = frame.error_code;
    PageFault {
        address: read_cr2(),
        raw: code,
        present: code & 1 != 0,
        write: code & 2 != 0,
        user: code & 4 != 0,
        instruction_fetch: code & 16 != 0,
    }
}

/// Where the interrupted code was executing.
#[inline]
pub fn instruction_pointer(frame: &TrapFrame) -> u64 {
    frame.rip
}

/// Where the interrupted code's own stack pointer was.
#[inline]
pub fn stack_pointer(frame: &TrapFrame) -> u64 {
    frame.rsp
}

/// Which exception or interrupt built this frame.
#[inline]
pub fn trap_vector(frame: &TrapFrame) -> u64 {
    frame.vector
}

/// The machine's own extra word describing the trap, for a message. Zero for
/// traps that carry none.
#[inline]
pub fn trap_error_code(frame: &TrapFrame) -> u64 {
    frame.error_code
}

/// Registers whose contents are worth reading memory at after a fault: they
/// name the structure the program was walking when it went wrong.
pub fn fault_probe_registers(frame: &TrapFrame) -> [(&'static str, u64); 3] {
    [("r15", frame.r15), ("rbx", frame.rbx), ("rsp", frame.rsp)]
}

/// Print every register in the frame.
pub fn dump_registers(frame: &TrapFrame) {
    crate::println!(
        "  rip {:#018x}  cs  {:#06x}  rflags {:#018x}",
        frame.rip,
        frame.cs,
        frame.rflags
    );
    crate::println!("  rsp {:#018x}  ss  {:#06x}", frame.rsp, frame.ss);
    crate::println!(
        "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}",
        frame.rax,
        frame.rbx,
        frame.rcx
    );
    crate::println!(
        "  rdx {:#018x}  rsi {:#018x}  rdi {:#018x}",
        frame.rdx,
        frame.rsi,
        frame.rdi
    );
    crate::println!(
        "  rbp {:#018x}  r8  {:#018x}  r9  {:#018x}",
        frame.rbp,
        frame.r8,
        frame.r9
    );
    crate::println!(
        "  r10 {:#018x}  r11 {:#018x}  r12 {:#018x}",
        frame.r10,
        frame.r11,
        frame.r12
    );
    crate::println!(
        "  r13 {:#018x}  r14 {:#018x}  r15 {:#018x}",
        frame.r13,
        frame.r14,
        frame.r15
    );
}
