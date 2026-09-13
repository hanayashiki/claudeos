//! Exceptions and interrupt requests: what the vector table saves, how a
//! syndrome register is turned into the vector numbering the portable half
//! uses, the interrupt controller, and the periodic timer.
//!
//! The numbering is this architecture's own. Exceptions are numbered by their
//! exception class, of which there are sixty-four, and device interrupts
//! follow on above them. The four classes that mean "a translation did not
//! work" are all reported as one vector, because the kernel repairs them the
//! same way whichever of the four arrived.

use super::gic;
use crate::abi::{SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// Whether the message about an interrupt this interface may not claim has
/// been printed already.
static SAID_NOT_OURS: AtomicBool = AtomicBool::new(false);

/// Whether the message about a line past the end of the handler table has been
/// printed already.
static SAID_NO_HANDLER: AtomicBool = AtomicBool::new(false);

/// What an exception entry saves, in the order `vectors.s` writes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    /// x0 through x30.
    pub x: [u64; 31],
    /// The stack pointer of the level below, untouched when the exception came
    /// from this level.
    pub sp: u64,
    /// Where to carry on.
    pub elr: u64,
    /// The processor state to carry on with.
    pub spsr: u64,
    /// What the exception was.
    pub esr: u64,
    /// The address it was about, for the ones that are about an address.
    pub far: u64,
    /// The number the portable half knows this trap by, filled in on the way
    /// into the dispatcher.
    pub vector: u64,
    /// Which of the sixteen vector slots was taken.
    pub slot: u64,
}

/// What `vectors.s` is given for this structure. Every offset the entry and
/// exit paths name comes from here rather than being written out there, so a
/// field that moves takes the instruction that saves it with it. What is left
/// for the assertions below is what a constant cannot carry: that the fields a
/// single `stp` writes as a pair lie next to each other, in that order, and
/// that the frame keeps the stack sixteen-byte aligned.
pub(super) const FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();
pub(super) const OFF_X: usize = core::mem::offset_of!(TrapFrame, x);
pub(super) const OFF_X30: usize = OFF_X + 30 * 8;
pub(super) const OFF_ELR: usize = core::mem::offset_of!(TrapFrame, elr);
pub(super) const OFF_ESR: usize = core::mem::offset_of!(TrapFrame, esr);
pub(super) const OFF_VECTOR: usize = core::mem::offset_of!(TrapFrame, vector);
pub(super) const OFF_SLOT: usize = core::mem::offset_of!(TrapFrame, slot);

const _: () = {
    // The general registers are addressed as fifteen consecutive pairs from
    // the base of the frame.
    assert!(OFF_X == 0);
    assert!(core::mem::size_of::<[u64; 31]>() == 31 * 8);
    // `stp x30, x21` writes x[30] and then the stack pointer of the level
    // below; `stp x22, x23` writes ELR and then SPSR, and then ESR and FAR.
    assert!(core::mem::offset_of!(TrapFrame, sp) == OFF_X30 + 8);
    assert!(core::mem::offset_of!(TrapFrame, spsr) == OFF_ELR + 8);
    assert!(core::mem::offset_of!(TrapFrame, far) == OFF_ESR + 8);
    // Nothing else may sit between the frame's end and the last field, because
    // an exception from EL1 records the stack pointer as the frame's end.
    assert!(FRAME_SIZE == OFF_SLOT + 8);
    assert!(FRAME_SIZE % 16 == 0);
};

impl TrapFrame {
    /// True when the exception came from a program rather than the kernel.
    /// Mode zero in the saved processor state is EL0.
    pub fn from_user(&self) -> bool {
        self.spsr & 0xF == 0
    }

    /// True when interrupts were enabled in the code this exception
    /// interrupted. The bit is a mask, so it is set when they were off.
    fn interrupts_were_enabled(&self) -> bool {
        self.spsr & SPSR_I == 0
    }
}

/// The interrupt mask in a saved processor state.
const SPSR_I: u64 = 1 << 7;

/// Vectors 0..EXCEPTION_COUNT are the exception classes the syndrome register
/// reports, of which there are as many as six bits can hold.
pub const EXCEPTION_COUNT: u8 = 64;
/// The one exception the kernel repairs rather than reports: a data abort from
/// the level below. The three other classes that mean the same thing are
/// reported through this one.
pub const PAGE_FAULT_VECTOR: u8 = 0x24;

/// A system call, which is an exception class of its own.
const EC_SVC: u64 = 0x15;
const EC_INSTRUCTION_ABORT_LOWER: u64 = 0x20;
const EC_INSTRUCTION_ABORT: u64 = 0x21;
const EC_DATA_ABORT_LOWER: u64 = 0x24;
const EC_DATA_ABORT: u64 = 0x25;

/// How many device interrupt lines to make room for. The controller reports
/// its own count at start-up; this is the ceiling, chosen so that the last
/// interrupt's vector still fits in a byte.
pub const IRQ_COUNT: u8 = 192;
/// The line the periodic timer arrives on: the virtual timer's private
/// interrupt, which is readable from this level without asking the one above.
pub const TIMER_IRQ: u8 = 27;
/// The line the console UART arrives on.
pub const SERIAL_IRQ: u8 = 153;
/// The second UART's line, which is where a board wired to the cut-down serial
/// port announces input.
pub const SERIAL_IRQ_ALT: u8 = 125;
/// The number the portable console knows a keyboard's line by. There is no
/// keyboard on this board and no number free to stand in for one: every number
/// this controller implements is a line the chip can raise, and this one is a
/// shared peripheral interrupt two above the Ethernet controller's. So the
/// line is left masked -- `unmask_irq` declines it -- because a handler that
/// ends an interrupt it never cleared leaves a level-triggered line asserting
/// again as soon as it returns.
pub const KEYBOARD_IRQ: u8 = 191;

/// Timer interrupts per second. Every timeout in the kernel is a whole number
/// of these.
pub const TICK_HZ: u32 = 100;

/// The vector an interrupt on `irq` is delivered through.
#[inline]
pub fn irq_vector(irq: u8) -> u8 {
    EXCEPTION_COUNT + irq
}

/// The line a vector belongs to, or nothing when it is not a device interrupt.
#[inline]
pub fn vector_irq(vector: u64) -> Option<u8> {
    let base = EXCEPTION_COUNT as u64;
    if vector >= base && vector < base + IRQ_COUNT as u64 {
        Some((vector - base) as u8)
    } else {
        None
    }
}

pub type Handler = fn(&mut TrapFrame);

// Written only during single-threaded start-up, read on every trap. A lock
// here would be taken with interrupts already disabled on every trap, and a
// fault taken inside it would deadlock.
static mut HANDLERS: [Option<Handler>; 256] = [None; 256];

/// Call `handler` on every trap through `vector`.
pub fn register_trap_handler(vector: u8, handler: Handler) {
    unsafe {
        let handlers = &mut *core::ptr::addr_of_mut!(HANDLERS);
        handlers[vector as usize] = Some(handler);
    }
}

/// Call `handler` on every interrupt from `irq`.
pub fn register_irq_handler(irq: u8, handler: Handler) {
    register_trap_handler(irq_vector(irq), handler);
}

/// Point the processor at the vector table. Nothing may fault before this has
/// run.
pub fn init_vectors() {
    unsafe {
        extern "C" {
            static exception_vectors: u8;
        }
        asm!(
            "msr vbar_el1, {table}",
            "isb",
            table = in(reg) core::ptr::addr_of!(exception_vectors) as u64,
        );
    }
}

/// Bring the interrupt controller up with every line masked.
pub fn init_interrupt_controller() {
    gic::init();
}

/// Counter ticks between timer interrupts, worked out once at start-up.
static mut TIMER_INTERVAL: u64 = 0;
/// The counter reading the last tick was due at. The next one is due an
/// interval after it, not an interval after the handler got around to looking.
static mut TIMER_DEADLINE: u64 = 0;

/// Start the periodic timer at `hz` interrupts a second.
pub fn init_timer(hz: u32) {
    // Zero would divide the next deadline by nothing. Firmware that did not
    // set the counter's rate leaves a timer that never fires, which is a worse
    // machine but a running one.
    let interval = (super::clock::counter_frequency() / hz as u64).max(1);
    let deadline = super::clock::cycle_counter() + interval;
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(TIMER_INTERVAL), interval);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(TIMER_DEADLINE), deadline);
        asm!("msr cntv_cval_el0, {}", in(reg) deadline);
        asm!("msr cntv_ctl_el0, {}", in(reg) 1u64); // enabled, not masked
    }
}

/// Set the timer going again for the next tick.
///
/// The comparator moves on from the deadline that just passed rather than
/// being reloaded with an interval from here, because the time between the
/// deadline and this handler running is delivery latency: counted from here it
/// is added to the period, and since it is paid on every tick it accumulates.
/// On the emulated board that is 4 ms a tick, which makes the 10 ms tick 14 ms
/// and everything counted in ticks half as long again in real time.
///
/// A machine held off for longer than a whole interval leaves the next
/// deadline already behind: the tick that was due then cannot be delivered on
/// time whatever is done, so the deadline moves forward by whole intervals to
/// the first one still ahead and those ticks are not counted. The count
/// therefore runs short over a stall, and a sleep spanning one comes back late
/// by what was skipped. Delivering the missed ticks back to back instead keeps
/// the count true, but on a machine already slower than real time the backlog
/// is worked through more slowly than it grows and the kernel does nothing
/// else until it is; and the other machine's timer, which free-runs and whose
/// interrupts coalesce while masked, loses them in exactly this way already.
#[inline]
fn rearm_timer() {
    unsafe {
        let interval = core::ptr::read_volatile(core::ptr::addr_of!(TIMER_INTERVAL));
        let mut deadline =
            core::ptr::read_volatile(core::ptr::addr_of!(TIMER_DEADLINE)) + interval;
        let now = super::clock::cycle_counter();
        if deadline <= now {
            deadline += ((now - deadline) / interval + 1) * interval;
        }
        core::ptr::write_volatile(core::ptr::addr_of_mut!(TIMER_DEADLINE), deadline);
        asm!("msr cntv_cval_el0, {}", in(reg) deadline);
    }
}

/// Let interrupts from `irq` through.
pub fn unmask_irq(irq: u8) {
    // Except the keyboard's, which names a device this board does not have on
    // a line that belongs to something else: see KEYBOARD_IRQ.
    if irq == KEYBOARD_IRQ {
        return;
    }
    gic::unmask(irq);
}

/// Stop interrupts from `irq`.
pub fn mask_irq(irq: u8) {
    gic::mask(irq);
}

/// The claim the interrupt now being handled holds, until whoever ends it
/// takes it from here.
///
/// The end-of-interrupt belongs in the handler the line dispatches to. It has
/// to come after the device is cleared, or a level-triggered line asserts
/// again as soon as the handler returns; and it has to come before the handler
/// reschedules, because the running priority stays raised across a context
/// switch and nothing of that priority or lower -- the timer among them --
/// would be delivered for as long as the other task ran. A handler is the
/// portable half's and takes a trap frame and nothing else, so the claim is
/// left here for `end_of_interrupt` to take.
///
/// Read and written with interrupts masked. A handler that reschedules leaves
/// this entry suspended while another one runs on its own kernel stack, which
/// is why `interrupt` below puts back what it found rather than clearing the
/// slot.
static mut OUTSTANDING: Option<gic::Claimed> = None;

/// Put `claim` in the slot and give back what was there.
fn swap_outstanding(claim: Option<gic::Claimed>) -> Option<gic::Claimed> {
    unsafe { core::mem::replace(&mut *core::ptr::addr_of_mut!(OUTSTANDING), claim) }
}

/// Tell the controller the handler for the interrupt being serviced is
/// finished.
///
/// The line ended is the one the acknowledge handed out rather than the one
/// named here; they are the same number for every line that has a handler, and
/// the controller accepts no other. A call with nothing outstanding does
/// nothing, so a handler called any other way than from an interrupt cannot
/// drop the running priority of one that is still being serviced.
pub fn end_of_interrupt(_irq: u8) {
    if let Some(claim) = swap_outstanding(None) {
        claim.end();
    }
}

/// Where every vector slot arrives.
#[no_mangle]
pub extern "C" fn exception_entry(frame: &mut TrapFrame) {
    // The sixteen slots are four kinds of exception for each of four origins,
    // so the kind is the slot within its group of four.
    match frame.slot & 3 {
        0 => synchronous(frame),
        1 => {
            interrupt(frame);
        }
        2 => fast_interrupt(frame),
        _ => {
            crate::println!("[trap] system error at {:#x} esr {:#x}", frame.elr, frame.esr);
        }
    }
}

/// A fast interrupt, which this kernel does not take.
///
/// Nothing signals a line as one. The controller is brought up with its
/// interface enabled for the group it acknowledges and the bit that would
/// route the other group to this vector left clear, and EL1 is entered with
/// all four masks set and nothing here clears this one: `disable_interrupts`
/// and `enable_interrupts` are `daifset` and `daifclr` of the ordinary kind
/// only. A program runs with every mask clear, so the level below is the one
/// place a fast interrupt could arrive at all, and only from a controller
/// programmed differently from this one.
///
/// So this is a refusal rather than a handler. Sending it down the ordinary
/// path would acknowledge on an interface that may not claim it and then call
/// whichever handler the line named, the scheduler among them, and because the
/// kernel masks the ordinary kind only, a fast interrupt that did arrive in
/// kernel code would arrive inside a critical section. A section re-entered at
/// random is indistinguishable from memory corruption and is found nowhere
/// near where it went wrong.
///
/// Masking both kinds wherever the kernel masks one is the other way to close
/// that, and it was not taken: unmasking would then have to clear both, and
/// clearing this mask is the one thing that would make a fast interrupt
/// deliverable in kernel mode, where today it cannot be. The hardening would
/// have opened the door it is meant to shut. A refusal rests on no mask at
/// all, and costs one arm of a match that is already being made.
fn fast_interrupt(frame: &mut TrapFrame) -> ! {
    crate::println!();
    crate::println!("FAST INTERRUPT from slot {} at {:#x}", frame.slot, frame.elr);
    crate::println!(
        "  the controller is routing a line this kernel does not take, and \
         this kernel masks the ordinary kind only, so one of these can land \
         inside a critical section"
    );
    dump_registers(frame);
    panic!("fast interrupt");
}

/// A synchronous exception, run with interrupts in the state the code that
/// took it was in.
///
/// The hardware sets all four masks on entry to EL1 and nothing below needs
/// them: the frame is complete before this is reached, and every lock the
/// handlers take masks for itself and puts back what it found. Left masked,
/// a system call that never blocks runs that way from entry to return, so the
/// timer does not tick for as long as the longest call takes: a 16 MiB copy
/// here, and on a board whose console is driven a character at a time, a
/// third of a second for a 4 KiB write. Sleeps and poll deadlines stretch by
/// that much, typed input is dropped, and a receive ring overruns.
///
/// The saved state decides rather than a blanket enable, so a fault taken
/// inside a kernel critical section is handled as masked as the code that
/// faulted was.
fn synchronous(frame: &mut TrapFrame) {
    let unmask = frame.interrupts_were_enabled();
    if unmask {
        super::enable_interrupts();
    }
    handle_synchronous(frame);
    // The return path masks everything again before it touches ELR and SPSR,
    // but this is not the only way out of here: a handler can switch away and
    // come back, and what it comes back to must be what it left.
    if unmask {
        super::disable_interrupts();
    }
}

fn handle_synchronous(frame: &mut TrapFrame) {
    let class = frame.esr >> 26;
    if class == EC_SVC {
        frame.vector = super::task::VECTOR_SYSCALL;
        crate::syscall::syscall_dispatch(frame);
        return;
    }
    // All four ways of saying "a translation did not work" are one vector.
    frame.vector = match class {
        EC_INSTRUCTION_ABORT_LOWER | EC_INSTRUCTION_ABORT | EC_DATA_ABORT_LOWER
        | EC_DATA_ABORT => {
            PAGE_FAULT_VECTOR as u64
        }
        other => other,
    };
    dispatch(frame);
}

fn interrupt(frame: &mut TrapFrame) -> gic::Ended {
    let claim = match gic::acknowledge() {
        gic::Acknowledged::Line(claim) => claim,
        // There was nothing pending by the time the interface was read.
        gic::Acknowledged::Spurious(ended) => return ended,
        gic::Acknowledged::NotOurs(ended) => {
            // Nothing cleared the device and nothing can end what was never
            // claimed, so a level-triggered line arrives again as soon as this
            // returns and the machine makes no further progress. Said once:
            // saying it every time is the same livelock with output.
            if !SAID_NOT_OURS.swap(true, Ordering::Relaxed) {
                crate::println!(
                    "[gic] an interrupt arrived that this interface may not claim; \
                     it cannot be ended and will arrive again"
                );
            }
            return ended;
        }
    };
    let line = claim.line();
    // A line the controller implements and the handler table has no room for.
    // This chip reports 256 lines and the table holds 192, so the controller
    // can hand out a number in between: the Ethernet controller's is 189, one
    // below the table's size. The claim is a real one, so it is ended here --
    // returning without ending it leaves the running priority at this line's
    // and no interrupt of that priority or lower, timer and console included,
    // is ever delivered again. Nothing will clear the device either, so the
    // line is stopped at the distributor as well, or it would simply be
    // delivered again.
    if line >= IRQ_COUNT as u32 {
        if !SAID_NO_HANDLER.swap(true, Ordering::Relaxed) {
            crate::println!("[gic] line {} is past the handler table; masking it", line);
        }
        return claim.mask_and_end();
    }
    if line == TIMER_IRQ as u32 {
        rearm_timer();
    }
    frame.vector = irq_vector(line as u8) as u64;
    // The handler takes the claim through `end_of_interrupt` and ends it
    // there. Whatever it leaves is ended here.
    let outer = swap_outstanding(Some(claim));
    dispatch(frame);
    gic::end_outstanding(swap_outstanding(outer))
}

fn dispatch(frame: &mut TrapFrame) {
    let handler = unsafe { (*core::ptr::addr_of!(HANDLERS))[frame.vector as usize & 0xFF] };
    match handler {
        Some(handler) => handler(frame),
        None => crate::trap::unhandled(frame),
    }
}

/// The name of an exception class, for a message.
pub fn exception_name(vector: u64) -> &'static str {
    match vector {
        0x00 => "unknown reason",
        0x01 => "trapped WFI or WFE",
        0x07 => "floating point or vector access trapped",
        0x0E => "illegal execution state",
        0x11 | 0x15 => "system call",
        0x18 => "trapped system register access",
        0x20 | 0x21 => "instruction abort",
        0x22 => "misaligned instruction fetch",
        0x24 | 0x25 => "data abort",
        0x26 => "stack pointer misaligned",
        0x28 | 0x2C => "floating point exception",
        0x2F => "system error",
        0x30 | 0x31 => "breakpoint",
        0x32 | 0x33 => "single step",
        0x34 | 0x35 => "watchpoint",
        0x3C => "breakpoint instruction",
        _ => "unknown",
    }
}

/// The signal a user-mode program is killed by when it takes exception
/// `vector`.
pub fn exception_signal(vector: u64) -> i32 {
    match vector {
        0x00 | 0x0E | 0x18 => SIGILL,
        0x22 | 0x26 => SIGILL,
        0x28 | 0x2C => SIGFPE,
        0x30..=0x35 | 0x3C => SIGTRAP,
        _ => SIGSEGV,
    }
}

/// What the processor says about a fault on a translation.
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

pub fn page_fault(frame: &TrapFrame) -> PageFault {
    let class = frame.esr >> 26;
    let instruction_fetch =
        class == EC_INSTRUCTION_ABORT_LOWER || class == EC_INSTRUCTION_ABORT;
    // The low six bits of the syndrome say which stage of the walk failed.
    // Codes 0b0011xx mean the permissions refused an access to something that
    // was mapped; everything below that means nothing was mapped at all.
    let status = frame.esr & 0x3F;
    PageFault {
        address: frame.far,
        raw: frame.esr,
        present: (0x0C..=0x0F).contains(&status),
        // Bit 6 says which way the access went, and only for data aborts.
        write: !instruction_fetch && frame.esr & (1 << 6) != 0,
        user: frame.from_user(),
        instruction_fetch,
    }
}

/// Where the interrupted code was executing.
#[inline]
pub fn instruction_pointer(frame: &TrapFrame) -> u64 {
    frame.elr
}

/// Which exception or interrupt built this frame.
#[inline]
pub fn trap_vector(frame: &TrapFrame) -> u64 {
    frame.vector
}

/// The machine's own extra word describing the trap, for a message.
#[inline]
pub fn trap_error_code(frame: &TrapFrame) -> u64 {
    frame.esr
}

/// Registers whose contents are worth reading memory at after a fault: they
/// name the structure the program was walking when it went wrong.
pub fn fault_probe_registers(frame: &TrapFrame) -> [(&'static str, u64); 3] {
    [("x0", frame.x[0]), ("x1", frame.x[1]), ("sp", frame.sp)]
}

/// Print every register in the frame.
pub fn dump_registers(frame: &TrapFrame) {
    crate::println!(
        "  pc  {:#018x}  sp  {:#018x}  pstate {:#018x}",
        frame.elr,
        frame.sp,
        frame.spsr
    );
    crate::println!("  esr {:#018x}  far {:#018x}", frame.esr, frame.far);
    for pair in 0..15 {
        crate::println!(
            "  x{:<2} {:#018x}  x{:<2} {:#018x}",
            pair * 2,
            frame.x[pair * 2],
            pair * 2 + 1,
            frame.x[pair * 2 + 1]
        );
    }
    crate::println!("  x30 {:#018x}", frame.x[30]);
}
