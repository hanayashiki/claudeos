//! Linux x86_64 system call dispatch.

pub mod file;
pub mod mem;
pub mod proc;

use crate::abi::*;
use crate::cpu::idt::TrapFrame;
use crate::cpu::msr;
use crate::sched;

extern "C" {
    fn syscall_entry();
}

pub fn init() {
    use crate::cpu::gdt::{STAR_KERNEL_BASE, STAR_USER_BASE};
    msr::write(msr::IA32_STAR, (STAR_USER_BASE << 48) | (STAR_KERNEL_BASE << 32));
    msr::write(msr::IA32_LSTAR, syscall_entry as usize as u64);
    // Clear IF, TF, DF, NT, AC and IOPL on entry so the kernel starts in a
    // known state with interrupts off.
    msr::write(msr::IA32_FMASK, 0x47700);
    let efer = msr::read(msr::IA32_EFER);
    msr::write(msr::IA32_EFER, efer | msr::EFER_SCE);
}

/// Set to a syscall number to trace it, or `u64::MAX` to trace everything.
pub static mut TRACE: u64 = 0;

#[no_mangle]
pub extern "C" fn syscall_dispatch(frame: &mut TrapFrame) {
    let number = frame.rax;
    let args = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];

    let trace = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TRACE)) };
    if trace == u64::MAX || (trace != 0 && trace == number) {
        crate::println!(
            "[syscall] pid={} {} ({}) args={:#x} {:#x} {:#x} {:#x}",
            sched::current().pid,
            number,
            name_of(number),
            args[0],
            args[1],
            args[2],
            args[3]
        );
    }

    let result = handle(number, &args, frame);
    frame.rax = match result {
        Ok(value) => value,
        Err(err) => err.as_ret(),
    };

    sched::check_signals();
}

fn handle(number: u64, args: &[u64; 6], frame: &mut TrapFrame) -> SysResult {
    match number {
        // ---- file I/O -------------------------------------------------
        nr::READ => file::read(args[0] as i32, args[1], args[2]),
        nr::WRITE => file::write(args[0] as i32, args[1], args[2]),
        nr::OPEN => file::openat(AT_FDCWD, args[0], args[1] as u32, args[2] as u32),
        nr::OPENAT => file::openat(args[0] as i64, args[1], args[2] as u32, args[3] as u32),
        nr::CLOSE => file::close(args[0] as i32),
        nr::LSEEK => file::lseek(args[0] as i32, args[1] as i64, args[2] as u32),
        nr::READV => file::readv(args[0] as i32, args[1], args[2] as usize),
        nr::WRITEV => file::writev(args[0] as i32, args[1], args[2] as usize),
        nr::PREAD64 => file::pread(args[0] as i32, args[1], args[2], args[3]),
        nr::PWRITE64 => file::pwrite(args[0] as i32, args[1], args[2], args[3]),
        nr::STAT => file::stat_path(AT_FDCWD, args[0], args[1], 0),
        nr::LSTAT => file::stat_path(AT_FDCWD, args[0], args[1], AT_SYMLINK_NOFOLLOW),
        nr::FSTAT => file::fstat(args[0] as i32, args[1]),
        nr::FSTATAT => file::stat_path(args[0] as i64, args[1], args[2], args[3] as u32),
        nr::STATX => file::statx(args[0] as i64, args[1], args[2] as u32, args[3] as u32, args[4]),
        nr::ACCESS => file::access(AT_FDCWD, args[0]),
        nr::FACCESSAT | nr::FACCESSAT2 => file::access(args[0] as i64, args[1]),
        nr::GETDENTS64 | nr::GETDENTS => {
            file::getdents64(args[0] as i32, args[1], args[2] as usize)
        }
        nr::GETCWD => file::getcwd(args[0], args[1] as usize),
        nr::CHDIR => file::chdir(args[0]),
        nr::FCHDIR => file::fchdir(args[0] as i32),
        nr::MKDIR => file::mkdirat(AT_FDCWD, args[0], args[1] as u32),
        nr::MKDIRAT => file::mkdirat(args[0] as i64, args[1], args[2] as u32),
        nr::RMDIR => file::unlinkat(AT_FDCWD, args[0], AT_REMOVEDIR),
        nr::UNLINK => file::unlinkat(AT_FDCWD, args[0], 0),
        nr::UNLINKAT => file::unlinkat(args[0] as i64, args[1], args[2] as u32),
        nr::RENAME => file::rename(AT_FDCWD, args[0], AT_FDCWD, args[1]),
        nr::RENAMEAT => file::rename(args[0] as i64, args[1], args[2] as i64, args[3]),
        nr::SYMLINK => file::symlinkat(args[0], AT_FDCWD, args[1]),
        nr::SYMLINKAT => file::symlinkat(args[0], args[1] as i64, args[2]),
        nr::READLINK => file::readlinkat(AT_FDCWD, args[0], args[1], args[2] as usize),
        nr::READLINKAT => file::readlinkat(args[0] as i64, args[1], args[2], args[3] as usize),
        nr::CREAT => file::openat(AT_FDCWD, args[0], O_CREAT | O_WRONLY | O_TRUNC, args[1] as u32),
        nr::TRUNCATE => file::truncate(args[0], args[1]),
        nr::FTRUNCATE => file::ftruncate(args[0] as i32, args[1]),
        nr::DUP => file::dup(args[0] as i32),
        nr::DUP2 => file::dup2(args[0] as i32, args[1] as i32, 0),
        nr::DUP3 => file::dup2(args[0] as i32, args[1] as i32, args[2] as u32),
        nr::PIPE => file::pipe2(args[0], 0),
        nr::PIPE2 => file::pipe2(args[0], args[1] as u32),
        nr::FCNTL => file::fcntl(args[0] as i32, args[1] as u32, args[2]),
        nr::IOCTL => file::ioctl(args[0] as i32, args[1], args[2]),
        nr::POLL | nr::PPOLL => file::poll(args[0], args[1] as usize, args[2] as i64),
        nr::SELECT | nr::PSELECT6 => file::select(args[0] as i32, args[1], args[2], args[3]),
        nr::CHMOD | nr::FCHMOD | nr::FCHMODAT => Ok(0),
        // Everything runs as root on a single-user system.
        nr::CHOWN | nr::FCHOWN | nr::LCHOWN | nr::FCHOWNAT => Ok(0),
        nr::FSYNC | nr::SYNC | nr::MSYNC => Ok(0),
        nr::UMASK => {
            let task = sched::current();
            let old = task.umask;
            task.umask = args[0] as u32 & 0o777;
            Ok(old as u64)
        }
        nr::STATFS => file::statfs(args[0], args[1]),
        nr::FSTATFS => file::fstatfs(args[0] as i32, args[1]),
        nr::UTIMENSAT => Ok(0),
        nr::MEMFD_CREATE => file::memfd_create(args[0], args[1] as u32),
        nr::SENDFILE => file::sendfile(args[0] as i32, args[1] as i32, args[2], args[3] as usize),

        // ---- memory ---------------------------------------------------
        nr::BRK => mem::brk(args[0]),
        nr::MMAP => mem::mmap(args[0], args[1], args[2], args[3], args[4] as i64, args[5]),
        nr::MUNMAP => mem::munmap(args[0], args[1]),
        nr::MPROTECT => mem::mprotect(args[0], args[1], args[2]),
        nr::MREMAP => mem::mremap(args[0], args[1], args[2], args[3]),
        nr::MADVISE => Ok(0),

        // ---- process --------------------------------------------------
        nr::FORK => proc::fork(frame, 0, 0, 0, 0, 0),
        nr::VFORK => proc::fork(frame, 0, 0, 0, 0, 0),
        nr::CLONE => proc::fork(frame, args[0], args[1], args[2], args[3], args[4]),
        nr::EXECVE => proc::execve(args[0], args[1], args[2], frame),
        nr::EXIT => sched::exit_current((args[0] as i32 & 0xFF) << 8),
        nr::EXIT_GROUP => sched::exit_group((args[0] as i32 & 0xFF) << 8),
        nr::WAIT4 => proc::wait4(args[0] as i64, args[1], args[2] as u64),
        nr::KILL => proc::kill(args[0] as i64, args[1] as i32),
        nr::TKILL => proc::kill(args[0] as i64, args[1] as i32),
        nr::TGKILL => proc::kill(args[1] as i64, args[2] as i32),
        nr::GETPID => Ok(sched::current().tgid as u64),
        nr::GETTID => Ok(sched::current().pid as u64),
        nr::GETPPID => Ok(sched::current().ppid as u64),
        nr::GETPGRP | nr::GETPGID => Ok(sched::current().pgid as u64),
        nr::SETPGID => proc::setpgid(args[0] as u32, args[1] as u32),
        nr::GETSID | nr::SETSID => Ok(sched::current().pgid as u64),
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(0),
        nr::SETUID | nr::SETGID => Ok(0),
        nr::GETGROUPS => Ok(0),
        nr::SETGROUPS => Ok(0),
        nr::SCHED_YIELD => {
            sched::yield_now();
            Ok(0)
        }
        nr::ARCH_PRCTL => proc::arch_prctl(args[0], args[1]),
        nr::SET_TID_ADDRESS => {
            sched::current().clear_child_tid = args[0];
            Ok(sched::current().pid as u64)
        }
        nr::SET_ROBUST_LIST => {
            sched::current().robust_list = args[0];
            Ok(0)
        }
        nr::GET_ROBUST_LIST => Ok(0),
        nr::RSEQ => Err(Errno::ENOSYS),
        nr::PRCTL => Ok(0),
        nr::UNAME => proc::uname(args[0]),
        nr::SYSINFO => proc::sysinfo(args[0]),
        nr::GETRLIMIT => proc::getrlimit(args[0], args[1]),
        nr::SETRLIMIT => Ok(0),
        nr::PRLIMIT64 => proc::prlimit64(args[1], args[2], args[3]),
        nr::GETRUSAGE => proc::getrusage(args[1]),
        nr::TIMES => Ok(crate::trap::ticks()),
        nr::SCHED_GETAFFINITY => proc::sched_getaffinity(args[2], args[1] as usize),
        nr::SCHED_GETPARAM | nr::SCHED_GETSCHEDULER => Ok(0),
        nr::SCHED_GET_PRIORITY_MAX => Ok(0),
        nr::SCHED_GET_PRIORITY_MIN => Ok(0),

        // ---- time -----------------------------------------------------
        nr::CLOCK_GETTIME => proc::clock_gettime(args[0], args[1]),
        nr::CLOCK_GETRES => proc::clock_getres(args[0], args[1]),
        nr::GETTIMEOFDAY => proc::gettimeofday(args[0], args[1]),
        nr::TIME => {
            let now = crate::time::unix_time();
            if args[0] != 0 {
                crate::uaccess::write_struct(args[0], &now)?;
            }
            Ok(now as u64)
        }
        nr::NANOSLEEP => proc::nanosleep(args[0], args[1]),
        nr::CLOCK_NANOSLEEP => proc::nanosleep(args[2], args[3]),
        nr::PAUSE => {
            sched::sleep_ticks(u64::MAX / 2);
            Err(Errno::EINTR)
        }

        // ---- signals (default actions only) ---------------------------
        nr::RT_SIGACTION => proc::rt_sigaction(args[0] as usize, args[1], args[2]),
        nr::RT_SIGPROCMASK => proc::rt_sigprocmask(args[0] as u32, args[1], args[2]),
        nr::RT_SIGSUSPEND => Err(Errno::EINTR),
        nr::SIGALTSTACK => Ok(0),
        nr::RT_SIGRETURN => proc::rt_sigreturn(frame),

        // ---- misc -----------------------------------------------------
        nr::GETRANDOM => proc::getrandom(args[0], args[1] as usize),
        nr::FUTEX => proc::futex(args[0], args[1] as u32, args[2] as u32, args[3]),
        nr::EPOLL_CREATE1 | nr::EPOLL_CTL | nr::EPOLL_PWAIT => Err(Errno::ENOSYS),
        nr::SOCKET => Err(Errno::EAFNOSUPPORT),
        nr::CLOSE_RANGE => file::close_range(args[0] as u32, args[1] as u32),

        _ => {
            crate::println!(
                "[syscall] pid={} unimplemented {} ({}) rip={:#x}",
                sched::current().pid,
                number,
                name_of(number),
                frame.rip
            );
            Err(Errno::ENOSYS)
        }
    }
}

pub fn name_of(number: u64) -> &'static str {
    match number {
        nr::READ => "read",
        nr::WRITE => "write",
        nr::OPEN => "open",
        nr::CLOSE => "close",
        nr::STAT => "stat",
        nr::FSTAT => "fstat",
        nr::LSTAT => "lstat",
        nr::POLL => "poll",
        nr::LSEEK => "lseek",
        nr::MMAP => "mmap",
        nr::MPROTECT => "mprotect",
        nr::MUNMAP => "munmap",
        nr::BRK => "brk",
        nr::RT_SIGACTION => "rt_sigaction",
        nr::RT_SIGPROCMASK => "rt_sigprocmask",
        nr::IOCTL => "ioctl",
        nr::READV => "readv",
        nr::WRITEV => "writev",
        nr::ACCESS => "access",
        nr::PIPE => "pipe",
        nr::SCHED_YIELD => "sched_yield",
        nr::MREMAP => "mremap",
        nr::MADVISE => "madvise",
        nr::DUP => "dup",
        nr::DUP2 => "dup2",
        nr::NANOSLEEP => "nanosleep",
        nr::GETPID => "getpid",
        nr::CLONE => "clone",
        nr::FORK => "fork",
        nr::EXECVE => "execve",
        nr::EXIT => "exit",
        nr::WAIT4 => "wait4",
        nr::KILL => "kill",
        nr::UNAME => "uname",
        nr::FCNTL => "fcntl",
        nr::GETDENTS64 => "getdents64",
        nr::GETCWD => "getcwd",
        nr::CHDIR => "chdir",
        nr::MKDIR => "mkdir",
        nr::UNLINK => "unlink",
        nr::READLINK => "readlink",
        nr::GETTIMEOFDAY => "gettimeofday",
        nr::GETRLIMIT => "getrlimit",
        nr::GETUID => "getuid",
        nr::GETGID => "getgid",
        nr::GETEUID => "geteuid",
        nr::GETEGID => "getegid",
        nr::GETPPID => "getppid",
        nr::ARCH_PRCTL => "arch_prctl",
        nr::GETTID => "gettid",
        nr::TIME => "time",
        nr::FUTEX => "futex",
        nr::SET_TID_ADDRESS => "set_tid_address",
        nr::CLOCK_GETTIME => "clock_gettime",
        nr::EXIT_GROUP => "exit_group",
        nr::OPENAT => "openat",
        nr::FSTATAT => "newfstatat",
        nr::UNLINKAT => "unlinkat",
        nr::READLINKAT => "readlinkat",
        nr::FACCESSAT => "faccessat",
        nr::SET_ROBUST_LIST => "set_robust_list",
        nr::PIPE2 => "pipe2",
        nr::PRLIMIT64 => "prlimit64",
        nr::GETRANDOM => "getrandom",
        nr::STATX => "statx",
        nr::RSEQ => "rseq",
        nr::CLONE3 => "clone3",
        _ => "?",
    }
}
