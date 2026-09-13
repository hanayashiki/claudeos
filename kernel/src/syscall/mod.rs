//! Linux system call dispatch.
//!
//! The numbers are per-architecture and come from `arch::nr`; so does the way
//! a call arrives and the way its result goes back.

pub mod file;
pub mod mem;
pub mod net;
pub mod proc;

use crate::abi::*;
use crate::arch::{self, nr, TrapFrame};
use crate::sched;

pub fn init() {
    arch::init_syscall_entry();
}

/// `nanos` as a whole number of timer ticks, rounding up.
///
/// How long to wait reaches the kernel in a register, and turning it into
/// ticks multiplies it out: milliseconds by a million, seconds by a thousand
/// million. A length with no tick count wraps to a short one, and the wait
/// ends at once instead of not at all, so anything past what the conversion
/// holds is capped here at about a hundred and forty years.
fn wait_ticks(nanos: u64) -> u64 {
    const LONGEST_NS: u64 = 1 << 62;
    crate::time::ns_to_ticks(nanos.min(LONGEST_NS))
}

/// The tick count a wait of `nanos` nanoseconds ends at.
pub fn deadline_in(nanos: u64) -> u64 {
    crate::trap::ticks() + wait_ticks(nanos)
}

/// The same for a wait given in milliseconds, where a negative count is the
/// wait with no end.
pub fn deadline_in_ms(timeout_ms: i64) -> u64 {
    if timeout_ms < 0 {
        return u64::MAX;
    }
    deadline_in((timeout_ms as u64).saturating_mul(1_000_000))
}

/// Which system calls to log: -1 for none, -2 for all, otherwise the number
/// of the one to follow. A signed sentinel keeps 0 (read) traceable.
pub static mut TRACE: i64 = -1;
pub const TRACE_OFF: i64 = -1;
pub const TRACE_ALL: i64 = -2;

#[no_mangle]
pub extern "C" fn syscall_dispatch(frame: &mut TrapFrame) {
    let number = arch::syscall_number(frame);
    let args = arch::syscall_args(frame);

    let trace = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TRACE)) };
    let traced = trace == TRACE_ALL || (trace >= 0 && trace as u64 == number);

    let result = handle(number, &args, frame);
    arch::set_syscall_result(
        frame,
        match result {
            Ok(value) => value,
            Err(err) => err.as_ret(),
        },
    );

    if traced {
        crate::println!(
            "[syscall] pid={} {}({:#x}, {:#x}, {:#x}, {:#x}) = {}",
            sched::current().pid,
            name_of(number),
            args[0],
            args[1],
            args[2],
            args[3],
            match result {
                Ok(value) => alloc::format!("{}", value as i64),
                Err(err) => alloc::format!("-{:?}", err),
            }
        );
    }

    sched::check_signals();

    // Here the kernel holds nothing and interrupts are on, which is what the
    // heap needs and cannot ask for: an allocation that runs the free list out
    // maps its own pages, and it does that inside whatever critical section
    // the caller was in. Mapping ahead from here keeps the ordinary allocation
    // out of that.
    crate::mm::heap::top_up();
}

fn handle(number: u64, args: &[u64; 6], frame: &mut TrapFrame) -> SysResult {
    // A call the architecture's numbering has no number for is still named
    // here, because the dispatcher is written once for every architecture, and
    // it is given a placeholder above everything Linux numbers. The number
    // matched below came out of a register, so a program can name a
    // placeholder: matching one would run the call it stands for. Nothing is
    // reported, because a number that names no call is not a call that is
    // missing.
    if number >= nr::ABSENT {
        return Err(Errno::ENOSYS);
    }
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
        nr::LINK => file::linkat(AT_FDCWD, args[0], AT_FDCWD, args[1], 0),
        nr::LINKAT => {
            file::linkat(args[0] as i64, args[1], args[2] as i64, args[3], args[4] as u32)
        }
        nr::MKNOD => file::mknodat(AT_FDCWD, args[0], args[1] as u32, args[2]),
        nr::MKNODAT => file::mknodat(args[0] as i64, args[1], args[2] as u32, args[3]),
        // One task at a time and no shared storage, so an advisory lock has
        // nothing to arbitrate between; taking it always succeeds.
        nr::FLOCK => Ok(0),
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
        nr::POLL => file::poll(args[0], args[1] as usize, args[2] as i64),
        nr::PPOLL => file::ppoll(args[0], args[1] as usize, args[2]),
        nr::SELECT => {
            file::select(args[0] as i32, args[1], args[2], args[3], args[4], 1_000)
        }
        nr::PSELECT6 => file::select(args[0] as i32, args[1], args[2], args[3], args[4], 1),
        nr::CHMOD => file::chmod(AT_FDCWD, args[0], args[1] as u32),
        nr::FCHMOD => file::fchmod(args[0] as i32, args[1] as u32),
        nr::FCHMODAT => file::chmod(args[0] as i64, args[1], args[2] as u32),
        // Everything runs as root on a single-user system.
        nr::CHOWN | nr::FCHOWN | nr::LCHOWN | nr::FCHOWNAT => Ok(0),
        nr::FSYNC | nr::SYNC | nr::MSYNC => Ok(0),
        nr::UMASK => {
            let task = sched::current();
            let old = task.umask.get();
            task.umask.set(args[0] as u32 & 0o777);
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
        nr::VFORK => proc::fork(frame, (CLONE_VM | CLONE_VFORK) as u64, 0, 0, 0, 0),
        nr::CLONE => {
            let (flags, stack, parent_tid, child_tid, tls) = arch::clone_args(args);
            proc::fork(frame, flags, stack, parent_tid, child_tid, tls)
        }
        nr::EXECVE => proc::execve(args[0], args[1], args[2], frame),
        nr::EXIT => sched::exit_current((args[0] as i32 & 0xFF) << 8),
        nr::EXIT_GROUP => sched::exit_group((args[0] as i32 & 0xFF) << 8),
        nr::WAIT4 => proc::wait4(args[0] as i64, args[1], args[2] as u64),
        nr::KILL => proc::kill(args[0] as i64, args[1] as i32),
        nr::TKILL => proc::kill(args[0] as i64, args[1] as i32),
        nr::TGKILL => proc::kill(args[1] as i64, args[2] as i32),
        nr::GETPID => Ok(sched::current().tgid as u64),
        nr::GETTID => Ok(sched::current().pid as u64),
        nr::GETPPID => Ok(sched::current().ppid.get() as u64),
        nr::GETPGRP | nr::GETPGID => Ok(sched::current().pgid.get() as u64),
        nr::SETPGID => proc::setpgid(args[0] as u32, args[1] as u32),
        nr::GETSID | nr::SETSID => Ok(sched::current().pgid.get() as u64),
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(0),
        nr::SETUID | nr::SETGID => Ok(0),
        nr::GETGROUPS => Ok(0),
        nr::SETGROUPS => Ok(0),
        nr::SCHED_YIELD => {
            sched::yield_now();
            Ok(0)
        }
        nr::ARCH_PRCTL => arch::arch_prctl(args[0], args[1]),
        nr::SET_TID_ADDRESS => {
            sched::current().clear_child_tid.set(args[0]);
            Ok(sched::current().pid as u64)
        }
        nr::SET_ROBUST_LIST => {
            sched::current().robust_list.set(args[0]);
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
        nr::SETPRIORITY => proc::setpriority(args[0], args[1], args[2] as i64),
        nr::GETPRIORITY => proc::getpriority(args[0], args[1]),
        // No I/O scheduler to ask, so the class is whatever was set.
        nr::IOPRIO_SET => Ok(0),
        nr::IOPRIO_GET => Ok(0),
        nr::SYSLOG => proc::syslog(args[0], args[1], args[2] as i64),
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
        nr::RT_SIGACTION => proc::rt_sigaction(args[0] as i32, args[1], args[2]),
        nr::RT_SIGPROCMASK => proc::rt_sigprocmask(args[0] as u32, args[1], args[2]),
        nr::RT_SIGSUSPEND => Err(Errno::EINTR),
        nr::SIGALTSTACK => Ok(0),
        nr::RT_SIGRETURN => proc::rt_sigreturn(frame),

        // ---- misc -----------------------------------------------------
        nr::GETRANDOM => proc::getrandom(args[0], args[1] as usize),
        nr::FUTEX => proc::futex(args[0], args[1] as u32, args[2] as u32, args[3]),
        nr::EPOLL_CREATE1 => file::epoll_create(args[0] as u32),
        nr::EPOLL_CREATE => file::epoll_create(0),
        nr::EPOLL_CTL => file::epoll_ctl(args[0] as i32, args[1] as u32, args[2] as i32, args[3]),
        nr::EPOLL_WAIT | nr::EPOLL_PWAIT => {
            file::epoll_wait(args[0] as i32, args[1], args[2] as i32, args[3] as i64)
        }
        nr::EVENTFD => file::eventfd(args[0] as u32, 0),
        nr::EVENTFD2 => file::eventfd(args[0] as u32, args[1] as u32),
        nr::SOCKETPAIR => {
            file::socketpair(args[0] as u32, args[1] as u32, args[2] as u32, args[3])
        }
        nr::SENDTO => net::sendto(
            args[0] as i32,
            args[1],
            args[2] as usize,
            args[3] as u32,
            args[4],
            args[5],
        ),
        nr::RECVFROM => net::recvfrom(
            args[0] as i32,
            args[1],
            args[2] as usize,
            args[3] as u32,
            args[4],
            args[5],
        ),
        nr::SENDMSG => file::sendmsg(args[0] as i32, args[1]),
        nr::RECVMSG => file::recvmsg(args[0] as i32, args[1]),
        nr::SHUTDOWN => net::shutdown(args[0] as i32, args[1] as u32),
        nr::SETSOCKOPT => net::setsockopt(
            args[0] as i32,
            args[1] as u32,
            args[2] as u32,
            args[3],
            args[4],
        ),
        nr::GETSOCKOPT => net::getsockopt(
            args[0] as i32,
            args[1] as u32,
            args[2] as u32,
            args[3],
            args[4],
        ),
        nr::GETSOCKNAME => net::getsockname(args[0] as i32, args[1], args[2]),
        nr::GETPEERNAME => net::getpeername(args[0] as i32, args[1], args[2]),
        nr::SOCKET => net::socket(args[0] as u32, args[1] as u32, args[2] as u32),
        nr::BIND => net::bind(args[0] as i32, args[1], args[2]),
        nr::LISTEN => net::listen(args[0] as i32, args[1] as i32),
        nr::ACCEPT => net::accept4(args[0] as i32, args[1], args[2], 0),
        nr::ACCEPT4 => net::accept4(args[0] as i32, args[1], args[2], args[3] as u32),
        nr::CONNECT => net::connect(args[0] as i32, args[1], args[2]),
        nr::CLOSE_RANGE => file::close_range(args[0] as u32, args[1] as u32),

        _ => {
            crate::println!(
                "[syscall] pid={} unimplemented {} ({}) rip={:#x}",
                sched::current().pid,
                number,
                name_of(number),
                arch::instruction_pointer(frame)
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
        nr::SOCKET => "socket",
        nr::BIND => "bind",
        nr::LISTEN => "listen",
        nr::ACCEPT => "accept",
        nr::ACCEPT4 => "accept4",
        nr::CONNECT => "connect",
        nr::SENDTO => "sendto",
        nr::RECVFROM => "recvfrom",
        nr::SHUTDOWN => "shutdown",
        nr::SETSOCKOPT => "setsockopt",
        nr::GETSOCKOPT => "getsockopt",
        nr::GETSOCKNAME => "getsockname",
        nr::GETPEERNAME => "getpeername",
        _ => "?",
    }
}
