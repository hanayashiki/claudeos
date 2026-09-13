//! CPU setup: descriptor tables, interrupt controllers, per-CPU state.

pub mod gdt;
pub mod idt;
pub mod msr;
pub mod pic;
pub mod pit;

use core::arch::asm;

/// Per-CPU scratch area reached through GS. The field offsets are relied on by
/// the syscall entry stub in syscall.s.
#[repr(C)]
pub struct PerCpu {
    /// +0: kernel stack top to install on entry from user mode.
    pub kernel_rsp: u64,
    /// +8: scratch slot holding the user's RSP across the entry sequence.
    pub user_rsp: u64,
    /// +16: currently running task.
    pub current: u64,
    /// +24: nesting depth of kernel entries, for debugging.
    pub depth: u64,
}

static mut PER_CPU: PerCpu = PerCpu { kernel_rsp: 0, user_rsp: 0, current: 0, depth: 0 };

pub fn per_cpu() -> &'static mut PerCpu {
    unsafe { &mut *core::ptr::addr_of_mut!(PER_CPU) }
}

/// Point KERNEL_GS_BASE at the per-CPU block. User mode runs with GS_BASE
/// holding whatever the program set, and `swapgs` on entry brings ours back.
pub fn init_per_cpu() {
    let addr = core::ptr::addr_of!(PER_CPU) as u64;
    msr::write(msr::IA32_KERNEL_GS_BASE, addr);
    msr::write(msr::IA32_GS_BASE, addr);
}

// Deliberately not `nomem`, for the same reason `cli` and `sti` are not. The
// instruction touches no memory itself, and what it is for is to wait until an
// interrupt handler has changed some: the idle loop halts and then asks the
// scheduler what to run, and the answer is what the handler wrote. `nomem`
// lets the compiler carry a value across the wait in a register.
#[inline]
pub fn halt() {
    unsafe { asm!("hlt", options(nostack)) };
}

#[inline]
pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

#[inline]
pub fn read_cr0() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

#[inline]
pub unsafe fn write_cr0(value: u64) {
    asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
}

#[inline]
pub fn read_cr4() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

#[inline]
pub unsafe fn write_cr4(value: u64) {
    asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
}

pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    let (eax, ebx, ecx, edx);
    unsafe {
        asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "xchg {tmp:r}, rbx",
            tmp = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    CpuidResult { eax, ebx, ecx, edx }
}

/// Turn on SSE so the FPU state used by compiled user code is usable.
pub fn init_sse() {
    unsafe {
        let mut cr0 = read_cr0();
        cr0 &= !(1 << 2); // clear EM: no x87 emulation
        cr0 |= 1 << 1; // set MP
        write_cr0(cr0);

        let mut cr4 = read_cr4();
        cr4 |= 1 << 9; // OSFXSR
        cr4 |= 1 << 10; // OSXMMEXCPT
        write_cr4(cr4);

        asm!("fninit", options(nostack));
    }
}
