//! The machine half of a task: the register state a context switch carries,
//! the layout of a kernel stack, and the two ways into user mode.

use super::trap::TrapFrame;
use core::arch::asm;

extern "C" {
    #[link_name = "switch_context"]
    fn switch_context_asm(save_sp: *mut u64, new_sp: u64);
    fn enter_user_mode(frame: *const TrapFrame) -> !;
}

/// Bytes of kernel stack the user register frame occupies.
pub const TRAP_FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();

/// The vector recorded in a frame built by a system call rather than by a
/// fault or an interrupt.
pub const VECTOR_SYSCALL: u64 = 0x100;

/// How many registers `switch.s` puts on the stack: x19 through x28, the frame
/// pointer, and the address to carry on at.
const SWITCH_FRAME_WORDS: u64 = 12;

/// The thirty-two vector registers and the two words that control them.
///
/// The kernel is built without access to these, so everything in them between
/// a trap and the next context switch still belongs to the task that was
/// interrupted. Nothing else preserves them, and compiled user code reaches
/// for them constantly: the string and memory routines in a C library are
/// written in terms of them. A task preempted in the middle of one would come
/// back holding whatever the task that ran in between had left there, and
/// store that to memory. It shows up as a program quietly producing the wrong
/// bytes rather than as a crash.
///
/// The instructions that reach these registers cannot be written in Rust here,
/// only in assembly, and the assembler is told about them one block at a time.
/// Nothing is declared clobbered because the compiler never puts anything of
/// its own in them.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpuState {
    registers: [u128; 32],
    control: u64,
    status: u64,
}

impl FpuState {
    const fn zeroed() -> FpuState {
        FpuState { registers: [0; 32], control: 0, status: 0 }
    }

    #[inline]
    fn save(&mut self) {
        let at = self.registers.as_mut_ptr();
        unsafe {
            asm!(
                ".arch_extension fp",
                "stp q0,  q1,  [{at}, #16 * 0]",
                "stp q2,  q3,  [{at}, #16 * 2]",
                "stp q4,  q5,  [{at}, #16 * 4]",
                "stp q6,  q7,  [{at}, #16 * 6]",
                "stp q8,  q9,  [{at}, #16 * 8]",
                "stp q10, q11, [{at}, #16 * 10]",
                "stp q12, q13, [{at}, #16 * 12]",
                "stp q14, q15, [{at}, #16 * 14]",
                "stp q16, q17, [{at}, #16 * 16]",
                "stp q18, q19, [{at}, #16 * 18]",
                "stp q20, q21, [{at}, #16 * 20]",
                "stp q22, q23, [{at}, #16 * 22]",
                "stp q24, q25, [{at}, #16 * 24]",
                "stp q26, q27, [{at}, #16 * 26]",
                "stp q28, q29, [{at}, #16 * 28]",
                "stp q30, q31, [{at}, #16 * 30]",
                "mrs {control}, fpcr",
                "mrs {status}, fpsr",
                at = in(reg) at,
                control = out(reg) self.control,
                status = out(reg) self.status,
                options(nostack, preserves_flags),
            );
        }
    }

    #[inline]
    fn restore(&self) {
        let at = self.registers.as_ptr();
        unsafe {
            asm!(
                ".arch_extension fp",
                "msr fpcr, {control}",
                "msr fpsr, {status}",
                "ldp q0,  q1,  [{at}, #16 * 0]",
                "ldp q2,  q3,  [{at}, #16 * 2]",
                "ldp q4,  q5,  [{at}, #16 * 4]",
                "ldp q6,  q7,  [{at}, #16 * 6]",
                "ldp q8,  q9,  [{at}, #16 * 8]",
                "ldp q10, q11, [{at}, #16 * 10]",
                "ldp q12, q13, [{at}, #16 * 12]",
                "ldp q14, q15, [{at}, #16 * 14]",
                "ldp q16, q17, [{at}, #16 * 16]",
                "ldp q18, q19, [{at}, #16 * 18]",
                "ldp q20, q21, [{at}, #16 * 20]",
                "ldp q22, q23, [{at}, #16 * 22]",
                "ldp q24, q25, [{at}, #16 * 24]",
                "ldp q26, q27, [{at}, #16 * 26]",
                "ldp q28, q29, [{at}, #16 * 28]",
                "ldp q30, q31, [{at}, #16 * 30]",
                at = in(reg) at,
                control = in(reg) self.control,
                status = in(reg) self.status,
                options(nostack, readonly, preserves_flags),
            );
        }
    }
}

/// Register state a task owns that the trap frame on its kernel stack does not
/// hold: the thread pointer, which a program writes for itself and the kernel
/// only has to carry from one task to the next, and the vector registers.
/// Carried from task to task by hand at every context switch, because nothing
/// in the hardware does it.
#[derive(Clone, Copy)]
pub struct TaskContext {
    thread_pointer: u64,
    fpu: FpuState,
}

impl TaskContext {
    /// What a task that has never run holds.
    pub fn new() -> TaskContext {
        TaskContext { thread_pointer: 0, fpu: FpuState::zeroed() }
    }

    /// Copy the state out of the CPU into this record. The caller must be the
    /// task the CPU is running, or about to become it: a fork takes the
    /// parent's state this way to give to the child.
    pub fn save(&mut self) {
        unsafe { asm!("mrs {}, tpidr_el0", out(reg) self.thread_pointer) };
        self.fpu.save();
    }

    /// Install this record on the CPU.
    pub fn restore(&self) {
        unsafe { asm!("msr tpidr_el0, {}", in(reg) self.thread_pointer) };
        self.fpu.restore();
    }

    /// Point the saved thread pointer at `value` without touching the CPU.
    /// `clone` with CLONE_SETTLS gives the new thread its pointer this way.
    pub fn set_thread_pointer(&mut self, value: u64) {
        self.thread_pointer = value;
    }

    /// Put the state back to what a freshly loaded program expects and install
    /// it, which is what exec owes the new image: clean vector registers and
    /// no thread pointer.
    pub fn reset_for_exec(&mut self) {
        self.thread_pointer = 0;
        self.fpu = FpuState::zeroed();
        self.restore();
    }
}

/// Where the user register frame sits on a kernel stack. Both entry paths
/// start with the stack pointer at the top, so the frame is always the
/// topmost thing on it.
pub fn trap_frame_at(kstack_top: u64) -> *mut TrapFrame {
    (kstack_top - TRAP_FRAME_SIZE as u64) as *mut TrapFrame
}

/// Lay out a kernel stack so that the first context switch into it returns
/// into `entry`. Gives back the stack pointer to record for that switch.
pub fn prepare_kernel_entry(kstack_top: u64, entry: u64) -> u64 {
    // Leave the trap frame area untouched and build the switch frame below it.
    let base = kstack_top - TRAP_FRAME_SIZE as u64;
    let frame = (base - SWITCH_FRAME_WORDS * 8) as *mut u64;
    unsafe {
        for slot in 0..SWITCH_FRAME_WORDS {
            *frame.add(slot as usize) = 0;
        }
        // Mirrors what switch_context pops: x19..x28, then the frame pointer
        // and the address it returns to.
        *frame.add(SWITCH_FRAME_WORDS as usize - 1) = entry;
    }
    frame as u64
}

/// Fill a trap frame so that returning through it starts user code at `entry`
/// with stack pointer `stack` and interrupts on. Everything else is cleared,
/// so nothing of the previous image leaks into the new one.
pub fn start_user_at(frame: &mut TrapFrame, entry: u64, stack: u64) {
    unsafe {
        core::ptr::write_bytes(frame as *mut TrapFrame as *mut u8, 0, TRAP_FRAME_SIZE);
    }
    frame.elr = entry;
    frame.sp = stack;
    // EL0 with its own stack pointer and none of the four masks set, so the
    // program runs with interrupts enabled.
    frame.spsr = 0;
    frame.vector = VECTOR_SYSCALL;
}

/// Leave the kernel for user mode with the registers in `frame`. Never
/// returns. The caller must already have told the CPU which kernel stack to
/// come back on, and must not be holding a lock.
pub unsafe fn return_to_user(frame: *const TrapFrame) -> ! {
    enter_user_mode(frame)
}

/// The stack the next entry from user mode will arrive on.
static mut KERNEL_ENTRY_STACK: u64 = 0;

/// Name the stack the CPU switches to on the next entry from user mode.
///
/// The hardware needs no telling: an exception from EL0 lands on whatever the
/// stack pointer of this level was left at, and `enter_user_mode` leaves it at
/// the top of the frame it unwound. So this records the value and nothing
/// else.
pub fn set_kernel_entry_stack(kstack_top: u64) {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(KERNEL_ENTRY_STACK), kstack_top)
    };
}

/// Record which task is on the CPU, for the entry stubs and for debugging.
pub fn set_current_task(task: u64) {
    unsafe { asm!("msr tpidr_el1, {}", in(reg) task) };
}

/// Put the outgoing task's callee-saved registers on its own kernel stack,
/// record where they ended up in `*save_sp`, and resume the incoming task from
/// the mirror-image frame at `new_sp`. Must be called with interrupts off.
pub unsafe fn switch_context(save_sp: *mut u64, new_sp: u64) {
    switch_context_asm(save_sp, new_sp);
}
