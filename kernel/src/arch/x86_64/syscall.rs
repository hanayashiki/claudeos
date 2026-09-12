//! The system call entry path: how a program asks for a call, where the
//! number and the arguments are found, and where the result goes back.

use super::cpu::gdt::{STAR_KERNEL_BASE, STAR_USER_BASE};
use super::cpu::idt::TrapFrame;
use super::cpu::msr;
use crate::abi::{Errno, SysResult, ARCH_GET_FS, ARCH_GET_GS, ARCH_SET_FS, ARCH_SET_GS};
use crate::sched;
use crate::uaccess;

extern "C" {
    fn syscall_entry();
}

/// Arrange for the `syscall` instruction to land in the kernel.
pub fn init_syscall_entry() {
    msr::write(msr::IA32_STAR, (STAR_USER_BASE << 48) | (STAR_KERNEL_BASE << 32));
    msr::write(msr::IA32_LSTAR, syscall_entry as unsafe extern "C" fn() as usize as u64);
    // Clear IF, TF, DF, NT, AC and IOPL on entry so the kernel starts in a
    // known state with interrupts off.
    msr::write(msr::IA32_FMASK, 0x47700);
    let efer = msr::read(msr::IA32_EFER);
    msr::write(msr::IA32_EFER, efer | msr::EFER_SCE);
}

/// Which call the program asked for.
#[inline]
pub fn syscall_number(frame: &TrapFrame) -> u64 {
    frame.rax
}

/// The six argument registers, in the order the Linux calling convention
/// names them.
#[inline]
pub fn syscall_args(frame: &TrapFrame) -> [u64; 6] {
    [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9]
}

/// Where the result goes on the way back out.
#[inline]
pub fn set_syscall_result(frame: &mut TrapFrame, value: u64) {
    frame.rax = value;
}

/// What is currently in the result register.
#[inline]
pub fn syscall_result(frame: &TrapFrame) -> u64 {
    frame.rax
}

/// Make a child's frame resume where its parent's did, returning zero, and on
/// `stack` if one was named.
pub fn fork_child_frame(child: &mut TrapFrame, parent: &TrapFrame, stack: u64) {
    *child = *parent;
    child.rax = 0;
    if stack != 0 {
        child.rsp = stack;
    }
}

/// `arch_prctl`: read and write the bases user code addresses its thread-local
/// storage through. The whole call is architecture-specific, numbers and all.
pub fn arch_prctl(code: u64, addr: u64) -> SysResult {
    match code {
        ARCH_SET_FS => {
            msr::write(msr::IA32_FS_BASE, addr);
            sched::current().cpu.set_thread_pointer(addr);
            Ok(0)
        }
        ARCH_SET_GS => {
            // The user GS base lives in KERNEL_GS_BASE while we are in the
            // kernel; swapgs puts it back on the way out.
            msr::write(msr::IA32_KERNEL_GS_BASE, addr);
            sched::current().cpu.gs_base = addr;
            Ok(0)
        }
        ARCH_GET_FS => {
            uaccess::write_u64(addr, msr::read(msr::IA32_FS_BASE))?;
            Ok(0)
        }
        ARCH_GET_GS => {
            uaccess::write_u64(addr, msr::read(msr::IA32_KERNEL_GS_BASE))?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
