//! Raw Linux system calls.
//!
//! The shell needs process-group and descriptor control that `std` does not
//! expose, so those paths go straight to the kernel.

use std::arch::asm;
use std::ffi::CString;

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
pub const STDERR: i32 = 2;

pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
#[allow(dead_code)]
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0o100;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
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
pub const SYS_IOCTL: u64 = 16;
pub const SYS_SYNC: u64 = 162;

#[inline(always)]
unsafe fn syscall0(n: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall1(n: u64, a: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: u64, a: u64, b: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b, in("rdx") c,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let ret: i64;
    asm!("syscall", inlateout("rax") n as i64 => ret, in("rdi") a, in("rsi") b, in("rdx") c,
         in("r10") d, lateout("rcx") _, lateout("r11") _, options(nostack));
    ret
}

pub fn fork() -> i64 {
    unsafe { syscall0(SYS_FORK) }
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

pub fn dup2(old: i32, new: i32) -> i64 {
    unsafe { syscall2(SYS_DUP2, old as u64, new as u64) }
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
    unsafe { syscall3(SYS_OPEN, c.as_ptr() as u64, flags, mode) }
}

#[allow(dead_code)]
pub fn write(fd: i32, data: &[u8]) -> i64 {
    unsafe { syscall3(SYS_WRITE, fd as u64, data.as_ptr() as u64, data.len() as u64) }
}

pub fn read(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub fn kill(pid: i32, signal: i32) -> i64 {
    unsafe { syscall2(SYS_KILL, pid as u64, signal as u64) }
}

pub fn chdir(path: &str) -> i64 {
    let c = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return -22,
    };
    unsafe { syscall1(SYS_CHDIR, c.as_ptr() as u64) }
}

pub fn sync() {
    unsafe { syscall0(SYS_SYNC) };
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
    if signal == 0 {
        None
    } else {
        Some(signal)
    }
}

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
#[allow(dead_code)]
pub const SIGTERM: i32 = 15;
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

/// `struct termios` with the x86_64 Linux layout.
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
