//! Raw Linux system calls.
//!
//! The shell needs process-group and descriptor control that `std` does not
//! expose, so those paths go straight to the kernel.

use std::arch::asm;
use std::ffi::CString;

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
#[allow(dead_code)]
pub const STDERR: i32 = 2;

pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
#[allow(dead_code)]
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0o100;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;

// The call numbers and the instruction that makes the call are the machine's
// own. x86-64 keeps its historical table; aarch64 uses the asm-generic one,
// which dropped every call that a later "at" form replaced, so `open`,
// `dup2`, `fork`, `mknod` and `epoll_wait` have no number there and the shims
// further down make the equivalent call instead.
#[cfg(target_arch = "x86_64")]
mod numbers {
    pub const SYS_READ: u64 = 0;
    pub const SYS_WRITE: u64 = 1;
    pub const SYS_OPEN: u64 = 2;
    pub const SYS_CLOSE: u64 = 3;
    pub const SYS_PREAD64: u64 = 17;
    pub const SYS_PWRITE64: u64 = 18;
    pub const SYS_PIPE2: u64 = 293;
    pub const SYS_DUP2: u64 = 33;
    pub const SYS_FORK: u64 = 57;
    pub const SYS_EXECVE: u64 = 59;
    pub const SYS_EXIT_GROUP: u64 = 231;
    pub const SYS_WAIT4: u64 = 61;
    pub const SYS_SETPGID: u64 = 109;
    pub const SYS_GETPID: u64 = 39;
    #[allow(dead_code)]
    pub const SYS_GETPPID: u64 = 110;
    pub const SYS_CHDIR: u64 = 80;
    pub const SYS_KILL: u64 = 62;
    pub const SYS_RT_SIGACTION: u64 = 13;
    pub const SYS_IOCTL: u64 = 16;
    pub const SYS_SYNC: u64 = 162;
    pub const SYS_FSYNC: u64 = 74;
    pub const SYS_MKNOD: u64 = 133;
    pub const SYS_SYSLOG: u64 = 103;
    pub const SYS_EVENTFD2: u64 = 290;
    pub const SYS_EPOLL_CREATE1: u64 = 291;
    pub const SYS_EPOLL_CTL: u64 = 233;
    pub const SYS_EPOLL_WAIT: u64 = 232;
    pub const SYS_STATFS: u64 = 137;
    pub const SYS_TIMES: u64 = 100;
    pub const SYS_CLOCK_GETRES: u64 = 229;
    pub const SYS_REBOOT: u64 = 169;

    /// `struct epoll_event` is declared packed on x86-64, so the 8-byte data
    /// word follows the 4-byte mask with no gap.
    pub const EPOLL_EVENT_SIZE: usize = 12;
}

#[cfg(target_arch = "aarch64")]
mod numbers {
    pub const SYS_READ: u64 = 63;
    pub const SYS_WRITE: u64 = 64;
    pub const SYS_OPENAT: u64 = 56;
    pub const SYS_CLOSE: u64 = 57;
    pub const SYS_PREAD64: u64 = 67;
    pub const SYS_PWRITE64: u64 = 68;
    pub const SYS_PIPE2: u64 = 59;
    pub const SYS_DUP3: u64 = 24;
    pub const SYS_CLONE: u64 = 220;
    pub const SYS_EXECVE: u64 = 221;
    pub const SYS_EXIT_GROUP: u64 = 94;
    pub const SYS_WAIT4: u64 = 260;
    pub const SYS_SETPGID: u64 = 154;
    pub const SYS_GETPID: u64 = 172;
    #[allow(dead_code)]
    pub const SYS_GETPPID: u64 = 173;
    pub const SYS_CHDIR: u64 = 49;
    pub const SYS_KILL: u64 = 129;
    pub const SYS_RT_SIGACTION: u64 = 134;
    pub const SYS_IOCTL: u64 = 29;
    pub const SYS_SYNC: u64 = 81;
    pub const SYS_FSYNC: u64 = 82;
    pub const SYS_MKNODAT: u64 = 33;
    pub const SYS_SYSLOG: u64 = 116;
    pub const SYS_EVENTFD2: u64 = 19;
    pub const SYS_EPOLL_CREATE1: u64 = 20;
    pub const SYS_EPOLL_CTL: u64 = 21;
    pub const SYS_EPOLL_PWAIT: u64 = 22;
    pub const SYS_STATFS: u64 = 43;
    pub const SYS_TIMES: u64 = 153;
    pub const SYS_CLOCK_GETRES: u64 = 114;
    pub const SYS_REBOOT: u64 = 142;

    /// `struct epoll_event` is not packed here, so the data word is aligned to
    /// 8 and the structure is 16 bytes.
    pub const EPOLL_EVENT_SIZE: usize = 16;
}

pub use numbers::*;

/// `openat`, `mknodat` and the rest take this where a path is relative.
#[cfg(target_arch = "aarch64")]
const AT_FDCWD: u64 = -100i64 as u64;

// The system call instruction: number in rax and arguments in rdi, rsi, rdx,
// r10 on x86-64; number in x8 and arguments in x0 upwards on aarch64, which
// returns in x0 and clobbers nothing else.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall0(n: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall1(n: u64, a: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall2(n: u64, a: u64, b: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b, in("rdx") c,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b, in("rdx") c,
         in("r10") d, lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall0(n: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, lateout("x0") ret, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall1(n: u64, a: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall2(n: u64, a: u64, b: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, in("x1") b, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, in("x1") b, in("x2") c,
         options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, in("x1") b, in("x2") c, in("x3") d,
         options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall6(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let ret: i64;
    asm!("svc #0", in("x8") n, inlateout("x0") a => ret, in("x1") b, in("x2") c, in("x3") d,
         in("x4") e, in("x5") f, options(nostack));
    ret
}

/// Make the call `number` with no arguments, whatever number that is. This is
/// for asking whether a call exists: a probe that walks the numbers is an
/// ordinary thing for a program to do, and the answer for a number the kernel
/// has no call at is ENOSYS.
pub fn probe(number: u64) -> i64 {
    unsafe { syscall3(number, 0, 0, 0) }
}

/// The `alarm` system call itself. Only x86-64 has one, and a C library makes
/// its `alarm` out of `setitimer`, so this is the only way to reach it.
#[cfg(target_arch = "x86_64")]
pub fn alarm_call(seconds: u32) -> i64 {
    const SYS_ALARM: u64 = 37;
    unsafe { syscall1(SYS_ALARM, seconds as u64) }
}

#[cfg(target_arch = "x86_64")]
pub fn fork() -> i64 {
    unsafe { syscall0(SYS_FORK) }
}

/// aarch64 has no `fork`; a `clone` whose only flag is the signal to raise on
/// exit is the same thing.
#[cfg(target_arch = "aarch64")]
pub fn fork() -> i64 {
    const SIGCHLD: u64 = 17;
    unsafe { syscall4(SYS_CLONE, SIGCHLD, 0, 0, 0) }
}

pub fn getpid() -> i64 {
    unsafe { syscall0(SYS_GETPID) }
}

#[allow(dead_code)]
pub fn getppid() -> i64 {
    unsafe { syscall0(SYS_GETPPID) }
}

pub fn setpgid(pid: i32, pgid: i32) -> i64 {
    unsafe { syscall2(SYS_SETPGID, pid as u64, pgid as u64) }
}

#[cfg(target_arch = "x86_64")]
pub fn dup2(old: i32, new: i32) -> i64 {
    unsafe { syscall2(SYS_DUP2, old as u64, new as u64) }
}

/// `dup3` with no flags is `dup2`, except that it refuses to duplicate a
/// descriptor onto itself. The shell never asks for that.
#[cfg(target_arch = "aarch64")]
pub fn dup2(old: i32, new: i32) -> i64 {
    if old == new {
        return new as i64;
    }
    unsafe { syscall3(SYS_DUP3, old as u64, new as u64, 0) }
}

pub fn close(fd: i32) -> i64 {
    unsafe { syscall1(SYS_CLOSE, fd as u64) }
}

pub fn pipe() -> Result<(i32, i32), i64> {
    let mut fds = [0i32; 2];
    let rc = unsafe { syscall2(SYS_PIPE2, fds.as_mut_ptr() as u64, 0) };
    if rc < 0 {
        Err(rc)
    } else {
        Ok((fds[0], fds[1]))
    }
}

pub fn open(path: &str, flags: u64, mode: u64) -> i64 {
    let c = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return -22,
    };
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe { syscall3(SYS_OPEN, c.as_ptr() as u64, flags, mode) };
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe { syscall4(SYS_OPENAT, AT_FDCWD, c.as_ptr() as u64, flags, mode) };
    ret
}

#[allow(dead_code)]
pub fn write(fd: i32, data: &[u8]) -> i64 {
    unsafe { syscall3(SYS_WRITE, fd as u64, data.as_ptr() as u64, data.len() as u64) }
}

pub fn read(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// Read and write at a position the caller names, leaving the descriptor's own
/// where it was. The position goes in as the raw 64-bit number the register
/// carries, so a caller can hand the kernel one no file has a byte at.
pub fn pread(fd: i32, buf: &mut [u8], offset: u64) -> i64 {
    unsafe { syscall4(SYS_PREAD64, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, offset) }
}

pub fn pwrite(fd: i32, data: &[u8], offset: u64) -> i64 {
    unsafe { syscall4(SYS_PWRITE64, fd as u64, data.as_ptr() as u64, data.len() as u64, offset) }
}

pub fn kill(pid: i32, signal: i32) -> i64 {
    unsafe { syscall2(SYS_KILL, pid as u64, signal as u64) }
}

/// Install a handler with the restorer field left empty.
///
/// `struct sigaction` is handler, flags, restorer and mask, and libc fills the
/// restorer in on both machines, so nothing that goes through `signal` or
/// `sigaction` ever asks for a disposition without one. A program built for
/// Linux on aarch64 does: the kernel there maps the return sequence itself and
/// never reads the field. This is the only way to ask from here.
pub fn set_handler_without_restorer(signum: i32, handler: usize) -> i64 {
    let action = [handler as u64, 0, 0, 0];
    unsafe { syscall4(SYS_RT_SIGACTION, signum as u64, action.as_ptr() as u64, 0, 8) }
}

pub fn chdir(path: &str) -> i64 {
    let c = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return -22,
    };
    unsafe { syscall1(SYS_CHDIR, c.as_ptr() as u64) }
}

/// Create a node the open/create path cannot: a named pipe.
pub fn mknod(path: &CString, mode: u32) -> i64 {
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe { syscall3(SYS_MKNOD, path.as_ptr() as u64, mode as u64, 0) };
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe { syscall4(SYS_MKNODAT, AT_FDCWD, path.as_ptr() as u64, mode as u64, 0) };
    ret
}

/// Read back what the kernel has printed.
pub fn klog(buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_SYSLOG, 3, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub const EPOLLIN: u32 = 0x001;

pub fn eventfd(initial: u32, flags: u32) -> i64 {
    unsafe { syscall2(SYS_EVENTFD2, initial as u64, flags as u64) }
}

pub fn epoll_create() -> i64 {
    unsafe { syscall1(SYS_EPOLL_CREATE1, 0) }
}

/// A `struct epoll_event`: a 4-byte mask, then 8 bytes of caller data at
/// whatever offset this machine's packing puts them.
pub fn epoll_add(epfd: i32, fd: i32, events: u32, data: u64) -> i64 {
    let mut event = [0u8; EPOLL_EVENT_SIZE];
    event[..4].copy_from_slice(&events.to_le_bytes());
    event[EPOLL_EVENT_SIZE - 8..].copy_from_slice(&data.to_le_bytes());
    unsafe {
        syscall4(SYS_EPOLL_CTL, epfd as u64, 1, fd as u64, event.as_ptr() as u64)
    }
}

pub fn epoll_wait(epfd: i32, events: &mut [u8], timeout_ms: i64) -> i64 {
    let max = events.len() / EPOLL_EVENT_SIZE;
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe {
        syscall4(
            SYS_EPOLL_WAIT,
            epfd as u64,
            events.as_mut_ptr() as u64,
            max as u64,
            timeout_ms as u64,
        )
    };
    // aarch64 has only the form that also takes a mask to wait under.
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe {
        syscall6(
            SYS_EPOLL_PWAIT,
            epfd as u64,
            events.as_mut_ptr() as u64,
            max as u64,
            timeout_ms as u64,
            0,
            0,
        )
    };
    ret
}

pub fn sync() {
    unsafe { syscall0(SYS_SYNC) };
}

/// Zero, or a negative errno.
pub fn fsync(fd: i32) -> i64 {
    unsafe { syscall1(SYS_FSYNC, fd as u64) }
}

/// The first magic number `reboot` has to be given, and the first of the four
/// second ones it accepts.
pub const REBOOT_MAGIC1: u32 = 0xfee1dead;
pub const REBOOT_MAGIC2: u32 = 672274793;
pub const REBOOT_CMD_RESTART: u32 = 0x01234567;
pub const REBOOT_CMD_HALT: u32 = 0xCDEF0123;
pub const REBOOT_CMD_POWER_OFF: u32 = 0x4321FEDC;

/// `reboot`, with the magic numbers as the caller gives them, so that a caller
/// can hand the kernel ones it should refuse. Returns only when refused.
pub fn reboot(magic1: u32, magic2: u32, command: u32) -> i64 {
    unsafe { syscall3(SYS_REBOOT, magic1 as u64, magic2 as u64, command as u64) }
}

/// Wait for a child. Returns (pid, wait status).
pub fn wait4(pid: i32, options: u64) -> (i64, i32) {
    let mut status: i32 = 0;
    let rc = unsafe {
        syscall4(SYS_WAIT4, pid as i64 as u64, &mut status as *mut i32 as u64, options, 0)
    };
    (rc, status)
}

pub fn exit_group(code: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT_GROUP, code as u64);
    }
    unreachable!()
}

/// Replace this process with `path`. Only returns on failure.
pub fn execve(path: &str, argv: &[String], envp: &[String]) -> i64 {
    let path_c = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return -22,
    };
    let argv_c: Vec<CString> = argv.iter().filter_map(|a| CString::new(a.as_str()).ok()).collect();
    let envp_c: Vec<CString> = envp.iter().filter_map(|e| CString::new(e.as_str()).ok()).collect();

    let mut argv_ptrs: Vec<*const u8> = argv_c.iter().map(|c| c.as_ptr() as *const u8).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const u8> = envp_c.iter().map(|c| c.as_ptr() as *const u8).collect();
    envp_ptrs.push(std::ptr::null());

    unsafe {
        syscall3(
            SYS_EXECVE,
            path_c.as_ptr() as u64,
            argv_ptrs.as_ptr() as u64,
            envp_ptrs.as_ptr() as u64,
        )
    }
}

/// Decode a wait status into an exit code the shell can report.
pub fn exit_code_of(status: i32) -> i32 {
    if status & 0x7F == 0 {
        (status >> 8) & 0xFF
    } else {
        128 + (status & 0x7F)
    }
}

pub fn signal_of(status: i32) -> Option<i32> {
    let signal = status & 0x7F;
    if signal == 0 || status & 0xFF == 0x7F {
        None
    } else {
        Some(signal)
    }
}

/// The signal that stopped the child, when the status reports a stop.
pub fn stop_signal_of(status: i32) -> Option<i32> {
    if status & 0xFF == 0x7F {
        Some((status >> 8) & 0xFF)
    } else {
        None
    }
}

pub fn is_continued(status: i32) -> bool {
    status == 0xFFFF
}

pub const WNOHANG: u64 = 1;
pub const WUNTRACED: u64 = 2;
pub const WCONTINUED: u64 = 8;

pub const TIOCSPGRP: u64 = 0x5410;

pub fn set_foreground_group(pgid: i32) {
    let value = pgid;
    unsafe {
        syscall3(SYS_IOCTL, STDIN as u64, TIOCSPGRP, &value as *const i32 as u64);
    }
}

// musl provides these; the shell uses them for job control.
extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn tcsetpgrp(fd: i32, pgid: i32) -> i32;
    fn getpgrp() -> i32;
}

pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGPIPE: i32 = 13;
#[allow(dead_code)]
pub const SIGTERM: i32 = 15;
pub const SIGCONT: i32 = 18;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;

pub fn set_signal(signum: i32, handler: usize) {
    unsafe {
        signal(signum, handler);
    }
}

pub fn own_process_group() -> i32 {
    unsafe { getpgrp() }
}

/// Hand the terminal to `pgid` so it receives keyboard-generated signals.
pub fn give_terminal_to(pgid: i32) {
    unsafe {
        tcsetpgrp(STDIN, pgid);
    }
}

/// `struct termios`. musl lays this out the same way on both machines: the
/// kernel's own structure stops after `c_cc`, and the two speeds are musl's.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 32],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;

// c_lflag bits
pub const ISIG: u32 = 0o000001;
pub const ICANON: u32 = 0o000002;
pub const ECHO: u32 = 0o000010;

pub fn tcgets(fd: i32) -> Option<Termios> {
    let mut termios = Termios::default();
    let rc = unsafe {
        syscall3(SYS_IOCTL, fd as u64, TCGETS, &mut termios as *mut Termios as u64)
    };
    if rc < 0 {
        None
    } else {
        Some(termios)
    }
}

pub fn tcsets(fd: i32, termios: &Termios) -> bool {
    let rc = unsafe {
        syscall3(SYS_IOCTL, fd as u64, TCSETS, termios as *const Termios as u64)
    };
    rc >= 0
}

pub const TIOCGWINSZ: u64 = 0x5413;

/// Raw ioctl with a pointer argument.
pub fn ioctl_ptr(fd: i32, request: u64, argument: u64) -> i64 {
    unsafe { syscall3(SYS_IOCTL, fd as u64, request, argument) }
}

/// The fields of `struct statfs` this system needs.
pub struct StatFs {
    pub block_size: u64,
    pub blocks: u64,
    pub free: u64,
}

pub fn statfs(path: &str) -> Option<StatFs> {
    let c = CString::new(path).ok()?;
    let mut buf = [0u8; 120];
    let rc = unsafe { syscall2(SYS_STATFS, c.as_ptr() as u64, buf.as_mut_ptr() as u64) };
    if rc < 0 {
        return None;
    }
    let field = |offset: usize| -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[offset..offset + 8]);
        u64::from_le_bytes(bytes)
    };
    Some(StatFs { block_size: field(8), blocks: field(16), free: field(24) })
}

/// The timer tick count, which is what `times` returns as its value. The
/// buffer of process times it can also fill is not wanted here, and a null
/// pointer asks for none of it.
pub fn tick_count() -> u64 {
    unsafe { syscall1(SYS_TIMES, 0) as u64 }
}

/// How long one timer tick is, in nanoseconds, or zero if the kernel will not
/// say: the resolution of CLOCK_MONOTONIC is the tick a deadline is counted
/// in.
pub fn tick_nanoseconds() -> u64 {
    const CLOCK_MONOTONIC: u64 = 1;
    let mut spec = [0i64; 2];
    if unsafe { syscall2(SYS_CLOCK_GETRES, CLOCK_MONOTONIC, spec.as_mut_ptr() as u64) } < 0 {
        return 0;
    }
    spec[0] as u64 * 1_000_000_000 + spec[1] as u64
}
