//! Raw Linux system calls.
//!
//! The checks here need calls the standard library does not expose at all --
//! `brk`, `mmap` with an address of its choosing, `execve` -- and they need
//! `fork` and `wait4` rather than the process plumbing `std::process` wraps
//! around them, because what is being watched is which task reaps which.

use std::arch::asm;

pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;

// The call numbers and the instruction that makes the call are the machine's
// own. x86-64 keeps its historical table; aarch64 uses the asm-generic one,
// which has no `fork`, so a `clone` whose only flag is the signal to raise on
// exit stands in for it.
#[cfg(target_arch = "x86_64")]
mod numbers {
    pub const SYS_READ: u64 = 0;
    pub const SYS_WRITE: u64 = 1;
    pub const SYS_CLOSE: u64 = 3;
    pub const SYS_OPENAT: u64 = 257;
    pub const SYS_MMAP: u64 = 9;
    pub const SYS_MUNMAP: u64 = 11;
    pub const SYS_BRK: u64 = 12;
    pub const SYS_FORK: u64 = 57;
    pub const SYS_EXECVE: u64 = 59;
    pub const SYS_WAIT4: u64 = 61;
    pub const SYS_RENAMEAT: u64 = 264;
    pub const SYS_EXIT_GROUP: u64 = 231;
    pub const SYS_KILL: u64 = 62;
    pub const SYS_NANOSLEEP: u64 = 35;
}

#[cfg(target_arch = "aarch64")]
mod numbers {
    pub const SYS_READ: u64 = 63;
    pub const SYS_WRITE: u64 = 64;
    pub const SYS_CLOSE: u64 = 57;
    pub const SYS_OPENAT: u64 = 56;
    pub const SYS_MMAP: u64 = 222;
    pub const SYS_MUNMAP: u64 = 215;
    pub const SYS_BRK: u64 = 214;
    pub const SYS_CLONE: u64 = 220;
    pub const SYS_EXECVE: u64 = 221;
    pub const SYS_WAIT4: u64 = 260;
    pub const SYS_RENAMEAT: u64 = 38;
    pub const SYS_EXIT_GROUP: u64 = 94;
    pub const SYS_KILL: u64 = 129;
    pub const SYS_NANOSLEEP: u64 = 101;
}

use numbers::*;

// Number in rax and arguments in rdi, rsi, rdx, r10, r8, r9 on x86-64; number
// in x8 and arguments in x0 upwards on aarch64, which returns in x0 and
// clobbers nothing else.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b,
         in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, in("x1") b, in("x2") c,
         in("x3") d, in("x4") e, in("x5") f, options(nostack));
    ret
}

/// The current program break, or the one asked for. Zero asks without moving
/// it.
pub fn brk(request: u64) -> u64 {
    unsafe { syscall(SYS_BRK, request, 0, 0, 0, 0, 0) as u64 }
}

/// An anonymous private mapping. `addr` is a hint, not a demand: the kernel is
/// free to place it elsewhere, and where it places it is the point here.
pub fn mmap_anon(addr: u64, len: u64) -> i64 {
    unsafe {
        syscall(
            SYS_MMAP,
            addr,
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1i64 as u64,
            0,
        )
    }
}

/// The same, but demanding that address rather than suggesting it.
pub fn mmap_fixed(addr: u64, len: u64) -> i64 {
    const MAP_FIXED: u64 = 0x10;
    unsafe {
        syscall(
            SYS_MMAP,
            addr,
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
            -1i64 as u64,
            0,
        )
    }
}

/// A private mapping of a file, at the address named rather than suggested, so
/// a sibling thread knows where it will be before it is there.
pub fn mmap_file_fixed(addr: u64, len: u64, prot: u64, fd: i32, offset: u64) -> i64 {
    const MAP_FIXED: u64 = 0x10;
    unsafe {
        syscall(
            SYS_MMAP,
            addr,
            len,
            prot,
            MAP_PRIVATE | MAP_FIXED,
            fd as i64 as u64,
            offset,
        )
    }
}

pub fn munmap(addr: u64, len: u64) -> i64 {
    unsafe { syscall(SYS_MUNMAP, addr, len, 0, 0, 0, 0) }
}

#[cfg(target_arch = "x86_64")]
pub fn fork() -> i64 {
    unsafe { syscall(SYS_FORK, 0, 0, 0, 0, 0, 0) }
}

#[cfg(target_arch = "aarch64")]
pub fn fork() -> i64 {
    const SIGCHLD: u64 = 17;
    unsafe { syscall(SYS_CLONE, SIGCHLD, 0, 0, 0, 0, 0) }
}

fn cstr(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 1);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    out
}

/// Replace this program with the one at `path`, run with `argv` and an empty
/// environment. Only returns if it could not be done.
pub fn execve(path: &str, argv: &[&str]) -> i64 {
    let name = cstr(path);
    let args: Vec<Vec<u8>> = argv.iter().map(|a| cstr(a)).collect();
    let mut pointers: Vec<u64> = args.iter().map(|a| a.as_ptr() as u64).collect();
    pointers.push(0);
    let envp = [0u64];
    unsafe {
        syscall(
            SYS_EXECVE,
            name.as_ptr() as u64,
            pointers.as_ptr() as u64,
            envp.as_ptr() as u64,
            0,
            0,
            0,
        )
    }
}

/// Reap one child, giving back its pid and the exit code it carried. `pid` of
/// -1 takes whichever is ready, which is what a test that made more children
/// than it named needs.
pub fn wait4(pid: i32, options: u64) -> (i64, i32) {
    let mut status: i32 = 0;
    let rc = unsafe {
        syscall(
            SYS_WAIT4,
            pid as i64 as u64,
            &mut status as *mut i32 as u64,
            options,
            0,
            0,
            0,
        )
    };
    (rc, (status >> 8) & 0xFF)
}

/// Give `from` the name `to`, and give back the negated errno on failure
/// rather than the standard library's error type, since which errno it is is
/// what the checks are about. This is `renameat` because the aarch64 table has
/// no plain `rename`.
pub fn rename(from: &str, to: &str) -> i64 {
    const AT_FDCWD: i64 = -100;
    let from = cstr(from);
    let to = cstr(to);
    unsafe {
        syscall(
            SYS_RENAMEAT,
            AT_FDCWD as u64,
            from.as_ptr() as u64,
            AT_FDCWD as u64,
            to.as_ptr() as u64,
            0,
            0,
        )
    }
}

pub fn read(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe {
        syscall(
            SYS_READ,
            fd as i64 as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
            0,
        )
    }
}

/// Write from an address the caller names rather than from a slice, so the
/// buffer can be one a sibling thread is taking away underneath it.
pub fn write_raw(fd: i32, addr: u64, len: u64) -> i64 {
    unsafe { syscall(SYS_WRITE, fd as i64 as u64, addr, len, 0, 0, 0) }
}

/// Open the path at `addr`, for the same reason. `openat` on both machines:
/// the aarch64 table has no plain `open`.
pub fn open_raw(addr: u64, flags: u64) -> i64 {
    const AT_FDCWD: i64 = -100;
    unsafe { syscall(SYS_OPENAT, AT_FDCWD as u64, addr, flags, 0, 0, 0) }
}

pub fn open(path: &str, flags: u64) -> i64 {
    let path = cstr(path);
    open_raw(path.as_ptr() as u64, flags)
}

/// Send a signal to a process by pid.
pub fn kill(pid: i32, signal: i32) -> i64 {
    unsafe { syscall(SYS_KILL, pid as i64 as u64, signal as i64 as u64, 0, 0, 0, 0) }
}

/// Sleep for `millis`, or until a signal arrives. A child that is only there
/// to hold a share of its parent's pages waits here rather than spinning,
/// because the two are the only things on the machine and a spin would take
/// half of it.
pub fn sleep_ms(millis: u64) -> i64 {
    let spec = [millis / 1000, (millis % 1000) * 1_000_000];
    unsafe { syscall(SYS_NANOSLEEP, spec.as_ptr() as u64, 0, 0, 0, 0, 0) }
}

pub fn close(fd: i32) -> i64 {
    unsafe { syscall(SYS_CLOSE, fd as i64 as u64, 0, 0, 0, 0, 0) }
}

pub fn exit_group(code: i32) -> ! {
    unsafe {
        syscall(SYS_EXIT_GROUP, code as u64, 0, 0, 0, 0, 0);
    }
    unreachable!()
}
