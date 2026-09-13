//! The machine half of a task: the register state a context switch carries,
//! the layout of a kernel stack, and the two ways into user mode.

use super::cpu::idt::TrapFrame;
use super::cpu::{gdt, msr, per_cpu};

extern "C" {
    #[link_name = "switch_context"]
    fn switch_context_asm(save_sp: *mut u64, new_sp: u64);
    fn enter_user_mode(frame: *const TrapFrame) -> !;
}

/// Bytes of kernel stack the user register frame occupies.
pub const TRAP_FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();

/// The vector recorded in a frame that did not come from an interrupt. The
/// `syscall` entry stub is given this to write.
pub const VECTOR_SYSCALL: u64 = 0x100;

/// How many words `switch.s` leaves on the stack of a task it switched away
/// from: the seven it pushes, and the address it will return to. `switch.s`
/// builds that frame with pushes rather than offsets, so this is the only
/// number the two halves share, and it is checked there.
pub(super) const SWITCH_FRAME_WORDS: usize = 8;

/// The x87, MMX and SSE register file, as `fxsave` lays it out.
///
/// The kernel is built without SSE and never touches these registers, so
/// everything in them between a trap and the next context switch still belongs
/// to the task that was interrupted. Nothing else preserves them, so a task
/// preempted in the middle of an SSE memcpy would come back holding whatever
/// the task that ran in between had left in `xmm0`, and store that to memory.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpuState([u8; 512]);

/// Size of the register image a signal frame carries.
pub const FPU_STATE_SIZE: usize = 512;

impl FpuState {
    /// The state a program starts with: x87 precision control set the way the
    /// ABI asks for, and every SSE exception masked.
    pub fn initial() -> FpuState {
        let mut state = FpuState([0; 512]);
        state.0[0..2].copy_from_slice(&0x037Fu16.to_le_bytes()); // FCW
        state.0[24..28].copy_from_slice(&0x0000_1F80u32.to_le_bytes()); // MXCSR
        state.0[28..32].copy_from_slice(&0x0000_FFFFu32.to_le_bytes()); // MXCSR_MASK
        state
    }

    /// Take the registers as they stand into this buffer.
    #[inline]
    pub fn save(&mut self) {
        unsafe {
            core::arch::asm!(
                "fxsave64 [{}]",
                in(reg) self.0.as_mut_ptr(),
                options(nostack, preserves_flags),
            );
        }
    }

    pub(super) fn bytes(&self) -> &[u8; 512] {
        &self.0
    }

    /// Take a saved image back, rejecting the control-word bits the hardware
    /// would fault on: the image came off the user stack.
    pub(super) fn from_bytes(&mut self, src: &[u8]) -> bool {
        if src.len() < 512 {
            return false;
        }
        self.0.copy_from_slice(&src[..512]);
        let mut mxcsr = [0u8; 4];
        mxcsr.copy_from_slice(&self.0[24..28]);
        let value = u32::from_le_bytes(mxcsr) & 0x0000_FFBF;
        self.0[24..28].copy_from_slice(&value.to_le_bytes());
        true
    }

    /// Put this buffer back into the registers.
    #[inline]
    pub fn restore(&self) {
        unsafe {
            core::arch::asm!(
                "fxrstor64 [{}]",
                in(reg) self.0.as_ptr(),
                options(nostack, readonly, preserves_flags),
            );
        }
    }
}

/// Register state a task owns that the trap frame on its kernel stack does not
/// hold: the thread pointer, the second user segment base, and the floating
/// point and vector registers. Carried from task to task by hand at every
/// context switch, because nothing in the hardware does it.
#[derive(Clone, Copy)]
pub struct TaskContext {
    /// FS base, which is where user code keeps its thread pointer.
    pub(super) fs_base: u64,
    /// The user's GS base. It sits in KERNEL_GS_BASE while the kernel runs;
    /// `swapgs` puts it back on the way out.
    pub(super) gs_base: u64,
    pub(super) fpu: FpuState,
}

impl TaskContext {
    /// What a task that has never run holds.
    pub fn new() -> TaskContext {
        TaskContext { fs_base: 0, gs_base: 0, fpu: FpuState::initial() }
    }

    /// Copy the state out of the CPU into this record. The caller must be the
    /// task the CPU is running, or about to become it: a fork takes the
    /// parent's state this way to give to the child.
    pub fn save(&mut self) {
        self.fs_base = msr::read(msr::IA32_FS_BASE);
        self.gs_base = msr::read(msr::IA32_KERNEL_GS_BASE);
        self.fpu.save();
    }

    /// Install this record on the CPU.
    pub fn restore(&self) {
        msr::write(msr::IA32_FS_BASE, self.fs_base);
        msr::write(msr::IA32_KERNEL_GS_BASE, self.gs_base);
        self.fpu.restore();
    }

    /// Point the saved thread pointer at `value` without touching the CPU.
    /// `clone` with CLONE_SETTLS gives the new thread its pointer this way.
    pub fn set_thread_pointer(&mut self, value: u64) {
        self.fs_base = value;
    }

    /// Put the state back to what a freshly loaded program expects and install
    /// it, which is what exec owes the new image: a clean x87 and SSE state
    /// and no thread pointer.
    pub fn reset_for_exec(&mut self) {
        self.fpu = FpuState::initial();
        self.fpu.restore();
        self.fs_base = 0;
        msr::write(msr::IA32_FS_BASE, 0);
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
    let frame = (base - (SWITCH_FRAME_WORDS * 8) as u64) as *mut u64;
    unsafe {
        // Mirrors what switch_context pops: rflags, r15, r14, r13, r12,
        // rbx, rbp, then the address it returns to.
        *frame.add(0) = 0x0000_0002; // rflags with interrupts off
        *frame.add(1) = 0; // r15
        *frame.add(2) = 0; // r14
        *frame.add(3) = 0; // r13
        *frame.add(4) = 0; // r12
        *frame.add(5) = 0; // rbx
        *frame.add(6) = 0; // rbp
        *frame.add(SWITCH_FRAME_WORDS - 1) = entry;
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
    frame.rip = entry;
    frame.cs = gdt::USER_CODE as u64;
    frame.rflags = 0x202; // interrupts enabled
    frame.rsp = stack;
    frame.ss = gdt::USER_DATA as u64;
    frame.vector = VECTOR_SYSCALL;
}

/// Leave the kernel for user mode with the registers in `frame`. Never
/// returns. The caller must already have told the CPU which kernel stack to
/// come back on, and must not be holding a lock.
pub unsafe fn return_to_user(frame: *const TrapFrame) -> ! {
    enter_user_mode(frame)
}

/// Name the stack the CPU switches to on the next entry from user mode.
/// Called on every context switch, with interrupts off.
pub fn set_kernel_entry_stack(kstack_top: u64) {
    gdt::set_kernel_stack(kstack_top);
    per_cpu().kernel_rsp = kstack_top;
}

/// Record which task is on the CPU, for the entry stubs and for debugging.
pub fn set_current_task(task: u64) {
    per_cpu().current = task;
}

/// Put the outgoing task's callee-saved registers on its own kernel stack,
/// record where they ended up in `*save_sp`, and resume the incoming task from
/// the mirror-image frame at `new_sp`. Must be called with interrupts off.
pub unsafe fn switch_context(save_sp: *mut u64, new_sp: u64) {
    switch_context_asm(save_sp, new_sp);
}
