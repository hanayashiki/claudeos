//! Linux userspace ABI: error codes, flags, and the structs that cross the
//! boundary. The system call numbers differ from one architecture to the next
//! and live in `arch::nr`.
//!
//! Most of what is here is the same whatever the machine is. The handful of
//! layouts and flag values Linux lets the architecture choose come from
//! `layout`, at the bottom of this file.

#![allow(non_camel_case_types)]

pub use layout::*;

/// Linux error numbers, returned to userspace as `-errno`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Errno {
    EPERM = 1,
    ENOENT = 2,
    ESRCH = 3,
    EINTR = 4,
    EIO = 5,
    ENXIO = 6,
    E2BIG = 7,
    ENOEXEC = 8,
    EBADF = 9,
    ECHILD = 10,
    EAGAIN = 11,
    ENOMEM = 12,
    EACCES = 13,
    EFAULT = 14,
    EBUSY = 16,
    EEXIST = 17,
    EXDEV = 18,
    ENODEV = 19,
    ENOTDIR = 20,
    EISDIR = 21,
    EINVAL = 22,
    ENFILE = 23,
    EMFILE = 24,
    ENOTTY = 25,
    ETXTBSY = 26,
    EFBIG = 27,
    ENOSPC = 28,
    ESPIPE = 29,
    EROFS = 30,
    EMLINK = 31,
    EPIPE = 32,
    EDOM = 33,
    ERANGE = 34,
    ENAMETOOLONG = 36,
    ENOSYS = 38,
    ENOTEMPTY = 39,
    ELOOP = 40,
    ENOTSOCK = 88,
    EDESTADDRREQ = 89,
    EMSGSIZE = 90,
    EPROTOTYPE = 91,
    ENOPROTOOPT = 92,
    EPROTONOSUPPORT = 93,
    EOPNOTSUPP = 95,
    EAFNOSUPPORT = 97,
    EADDRINUSE = 98,
    EADDRNOTAVAIL = 99,
    ENETDOWN = 100,
    ENETUNREACH = 101,
    ECONNABORTED = 103,
    ECONNRESET = 104,
    ENOBUFS = 105,
    EISCONN = 106,
    ENOTCONN = 107,
    ESHUTDOWN = 108,
    ETIMEDOUT = 110,
    ECONNREFUSED = 111,
    EHOSTUNREACH = 113,
    EALREADY = 114,
    EINPROGRESS = 115,
}

pub type SysResult = Result<u64, Errno>;

impl Errno {
    pub fn as_ret(self) -> u64 {
        (-(self as i64)) as u64
    }
}

// open(2) flags
pub const O_RDONLY: u32 = 0o0;
pub const O_WRONLY: u32 = 0o1;
pub const O_RDWR: u32 = 0o2;
pub const O_ACCMODE: u32 = 0o3;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_NOCTTY: u32 = 0o400;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
// O_DIRECTORY, O_NOFOLLOW, O_DIRECT and O_LARGEFILE are the four open flags
// whose values the architecture picks; they come from `layout`.
pub const O_CLOEXEC: u32 = 0o2000000;
pub const O_PATH: u32 = 0o10000000;

pub const AT_FDCWD: i64 = -100;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_REMOVEDIR: u32 = 0x200;
pub const AT_EMPTY_PATH: u32 = 0x1000;

// mmap(2)
pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_GROWSDOWN: u64 = 0x0100;
pub const MAP_DENYWRITE: u64 = 0x0800;
pub const MAP_NORESERVE: u64 = 0x4000;
pub const MAP_STACK: u64 = 0x20000;
pub const MAP_FAILED: u64 = u64::MAX;

// File type bits in st_mode.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;
/// `linkat`: resolve the last component of the source if it is a symlink.
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;

// getdents64 d_type values.
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

// lseek whence
pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;

// clone flags
pub const CLONE_VM: u64 = 0x00000100;
pub const CLONE_FS: u64 = 0x00000200;
pub const CLONE_FILES: u64 = 0x00000400;
pub const CLONE_SIGHAND: u64 = 0x00000800;
pub const CLONE_VFORK: u64 = 0x00004000;
pub const CLONE_PARENT: u64 = 0x00008000;
pub const CLONE_THREAD: u64 = 0x00010000;
pub const CLONE_SETTLS: u64 = 0x00080000;
pub const CLONE_PARENT_SETTID: u64 = 0x00100000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x00200000;
pub const CLONE_CHILD_SETTID: u64 = 0x01000000;

// wait4 options
pub const WNOHANG: u64 = 1;
pub const WUNTRACED: u64 = 2;
pub const WCONTINUED: u64 = 8;

// clock ids
pub const CLOCK_REALTIME: u64 = 0;
pub const CLOCK_MONOTONIC: u64 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: u64 = 3;

/// `clock_nanosleep` flag: the time given is a reading of the clock to wake at,
/// not a length of time to sleep for.
pub const TIMER_ABSTIME: u32 = 1;

// futex ops
pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CLOCK_REALTIME: u32 = 256;
pub const FUTEX_CMD_MASK: u32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

// fcntl commands
pub const F_DUPFD: u32 = 0;
pub const F_GETFD: u32 = 1;
pub const F_SETFD: u32 = 2;
pub const F_GETFL: u32 = 3;
pub const F_SETFL: u32 = 4;
pub const F_DUPFD_CLOEXEC: u32 = 1030;
pub const FD_CLOEXEC: u32 = 1;

// Generic descriptor ioctls, used by runtimes to toggle non-blocking mode.
pub const FIONREAD: u64 = 0x541B;
pub const FIONBIO: u64 = 0x5421;
pub const FIOCLEX: u64 = 0x5451;
pub const FIONCLEX: u64 = 0x5450;

// ioctl requests used by terminal setup.
pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
pub const TIOCGPGRP: u64 = 0x540F;
pub const TIOCSPGRP: u64 = 0x5410;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;

// auxiliary vector keys
pub const AT_NULL: u64 = 0;
pub const AT_IGNORE: u64 = 1;
pub const AT_EXECFD: u64 = 2;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_NOTELF: u64 = 10;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_PLATFORM: u64 = 15;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_EXECFN: u64 = 31;
pub const AT_SYSINFO_EHDR: u64 = 33;
pub const AT_MINSIGSTKSZ: u64 = 51;

// Signals. The numbers are the interface's, but nothing here passes one
// around as a number: what carries a signal is the type next door, whose
// values these are.
pub use crate::signal::{
    SIGABRT, SIGALRM, SIGCHLD, SIGCONT, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGKILL, SIGPIPE,
    SIGQUIT, SIGSEGV, SIGSTOP, SIGTERM, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
};

/// What the kernel knows about a file. This is not the structure user code
/// reads: Linux orders and sizes those fields differently on each machine, so
/// `stat` and `fstat` convert this into `StatAbi` on the way out.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    pub base: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RLimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

pub const RLIM_INFINITY: u64 = u64::MAX;

/// `stack_t`: the alternate stack a handler runs on when its disposition
/// asked for one. Twenty-four bytes on both machines, the flags word padded
/// out to the alignment the two pointers need.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigAltStack {
    pub ss_sp: u64,
    pub ss_flags: i32,
    /// What the compiler would leave between the flags and the size anyway.
    /// It is named because the structure is written to user memory as a value
    /// rather than as bytes, and a hole nothing assigns to is whatever the
    /// kernel stack held there.
    pub _pad: u32,
    pub ss_size: u64,
}

impl SigAltStack {
    /// Whether a stack has been installed at all. Taking it away is `ss_size`
    /// going to zero, which is what `SS_DISABLE` asks for.
    pub fn installed(&self) -> bool {
        self.ss_size != 0
    }

    /// Whether `sp` is inside it.
    pub fn contains(&self, sp: u64) -> bool {
        self.installed() && sp >= self.ss_sp && sp < self.ss_sp + self.ss_size
    }

    /// What `sigaltstack` reports for the stack in place, given where the
    /// program's stack pointer is now.
    pub fn flags_at(&self, sp: u64) -> i32 {
        if !self.installed() {
            SS_DISABLE
        } else if self.contains(sp) {
            SS_ONSTACK
        } else {
            0
        }
    }
}

pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// `struct termios`, in the shape the kernel interface gives it: nineteen
/// control characters and nothing after them, thirty-six bytes in all. x86-64
/// and aarch64 both take the asm-generic definition, so it is the same on
/// either.
///
/// This is the size the call writes, and it is not the size of the structure a
/// C library declares. musl's has thirty-two control characters and two speeds
/// after them, glibc's the same, and Go's is thirty-six bytes plus two speeds:
/// each is at least as large, and Linux fills the first thirty-six bytes of
/// whichever it is handed and leaves the rest alone. Writing the larger shape
/// instead puts the two speeds past the end of a caller whose structure ends at
/// thirty-six, over whatever is next -- a return address, where the structure
/// is a local.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

// termios c_lflag bits
pub const ISIG: u32 = 0o000001;
pub const ICANON: u32 = 0o000002;
pub const ECHO: u32 = 0o000010;
pub const ECHOE: u32 = 0o000020;
pub const ECHONL: u32 = 0o000100;
// termios c_iflag bits
pub const ICRNL: u32 = 0o000400;
pub const IXON: u32 = 0o002000;
// termios c_oflag bits
pub const OPOST: u32 = 0o000001;
pub const ONLCR: u32 = 0o000004;

impl Termios {
    /// What the terminal starts out set to.
    pub const CONSOLE: Termios = Termios {
        c_iflag: ICRNL | IXON,
        c_oflag: OPOST | ONLCR,
        c_cflag: 0o2277, // B38400 | CS8 | CREAD
        c_lflag: ISIG | ICANON | ECHO | ECHOE,
        c_line: 0,
        // VINTR, VQUIT, VERASE, VKILL, VEOF, VTIME, VMIN, VSWTC, VSTART,
        // VSTOP, VSUSP, then the rest unset.
        c_cc: [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 0, 0, 0, 0, 0, 0, 0],
    };
}

impl Default for Termios {
    fn default() -> Self {
        Termios::CONSOLE
    }
}

/// `struct sysinfo`
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SysInfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub pad: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
    pub padding: [u8; 4],
}

// reboot(2): the two magic numbers every call has to carry, any one of the
// four second ones, and the commands. From include/uapi/linux/reboot.h.
pub const LINUX_REBOOT_MAGIC1: u32 = 0xfee1dead;
pub const LINUX_REBOOT_MAGIC2: u32 = 672274793;
pub const LINUX_REBOOT_MAGIC2A: u32 = 85072278;
pub const LINUX_REBOOT_MAGIC2B: u32 = 369367448;
pub const LINUX_REBOOT_MAGIC2C: u32 = 537993216;
pub const LINUX_REBOOT_CMD_RESTART: u32 = 0x01234567;
pub const LINUX_REBOOT_CMD_HALT: u32 = 0xCDEF0123;
pub const LINUX_REBOOT_CMD_CAD_ON: u32 = 0x89ABCDEF;
pub const LINUX_REBOOT_CMD_CAD_OFF: u32 = 0x00000000;
pub const LINUX_REBOOT_CMD_POWER_OFF: u32 = 0x4321FEDC;

// ---------------------------------------------------------------------------
// The layouts and flag values the architecture chooses
//
// This belongs under `arch/<target>/`, beside the system call numbers, and
// should move there once the aarch64 branch and this one meet; `arch/` has a
// single architecture in it while the port is in progress, so the two
// alternatives sit here behind a `cfg` instead.
//
// Everything left in the portable half above was checked against the aarch64
// definitions and is the same on both machines: timespec, timeval, iovec,
// rlimit, utsname, winsize, sysinfo, termios, statfs (120 bytes, asm-generic
// on both), msghdr (iov at 16, iovlen at 24, controllen at 40, flags at 48),
// linux_dirent64, sockaddr_in, the errno numbers, the signal numbers and the
// 8-byte signal set, and the PROT_, MAP_, CLONE_, AT_, F_, FUTEX_, CLOCK_ and
// ioctl constants. `struct sigaction` is the same 32-byte handler/flags/
// restorer/mask as well: arm64's uapi header defines SA_RESTORER for the sake
// of AArch32 binaries, which makes asm-generic give the native structure an
// sa_restorer field too.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod layout {
    use super::Stat;

    // asm-generic/fcntl.h, which x86-64 takes unchanged.
    pub const O_DIRECT: u32 = 0o40000;
    pub const O_LARGEFILE: u32 = 0o100000;
    pub const O_DIRECTORY: u32 = 0o200000;
    pub const O_NOFOLLOW: u32 = 0o400000;

    // `arch_prctl` codes. The call exists on x86-64 alone: it is how user code
    // sets the base register its thread-local storage hangs off, and aarch64
    // writes that register itself.
    pub const ARCH_SET_GS: u64 = 0x1001;
    pub const ARCH_SET_FS: u64 = 0x1002;
    pub const ARCH_GET_FS: u64 = 0x1003;
    pub const ARCH_GET_GS: u64 = 0x1004;

    /// `struct epoll_event` is declared packed on x86-64 and nowhere else, so
    /// the 8-byte data word follows the 4-byte mask with no gap.
    pub const EPOLL_EVENT_SIZE: u64 = 12;
    pub const EPOLL_EVENT_DATA: u64 = 4;

    /// `struct stat` as x86-64 Linux lays it out.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct StatAbi {
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_nlink: u64,
        pub st_mode: u32,
        pub st_uid: u32,
        pub st_gid: u32,
        pub __pad0: u32,
        pub st_rdev: u64,
        pub st_size: i64,
        pub st_blksize: i64,
        pub st_blocks: i64,
        pub st_atime: i64,
        pub st_atime_nsec: i64,
        pub st_mtime: i64,
        pub st_mtime_nsec: i64,
        pub st_ctime: i64,
        pub st_ctime_nsec: i64,
        pub __unused: [i64; 3],
    }

    const _: () = assert!(core::mem::size_of::<StatAbi>() == 144);

    impl From<&Stat> for StatAbi {
        fn from(stat: &Stat) -> Self {
            StatAbi {
                st_dev: stat.st_dev,
                st_ino: stat.st_ino,
                st_nlink: stat.st_nlink,
                st_mode: stat.st_mode,
                st_uid: stat.st_uid,
                st_gid: stat.st_gid,
                __pad0: 0,
                st_rdev: stat.st_rdev,
                st_size: stat.st_size,
                st_blksize: stat.st_blksize,
                st_blocks: stat.st_blocks,
                st_atime: stat.st_atime,
                st_atime_nsec: stat.st_atime_nsec,
                st_mtime: stat.st_mtime,
                st_mtime_nsec: stat.st_mtime_nsec,
                st_ctime: stat.st_ctime,
                st_ctime_nsec: stat.st_ctime_nsec,
                __unused: [0; 3],
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod layout {
    use super::Stat;

    // arch/arm64/include/uapi/asm/fcntl.h overrides four of the asm-generic
    // values, keeping the ones 32-bit ARM uses so an AArch32 binary running in
    // compatibility mode sees the numbers it was built with. Reusing the
    // x86-64 values here would make every musl `open` — which always adds
    // O_LARGEFILE — look like it had asked for O_NOFOLLOW.
    pub const O_DIRECTORY: u32 = 0o40000;
    pub const O_NOFOLLOW: u32 = 0o100000;
    pub const O_DIRECT: u32 = 0o200000;
    pub const O_LARGEFILE: u32 = 0o400000;

    /// `struct epoll_event` is not packed here, so the data word is aligned to
    /// 8 and the structure is 16 bytes rather than 12.
    pub const EPOLL_EVENT_SIZE: u64 = 16;
    pub const EPOLL_EVENT_DATA: u64 = 8;

    /// `struct stat` as aarch64 Linux lays it out: 128 bytes, from
    /// asm-generic/stat.h. Beyond the size, three things differ from x86-64's
    /// version — mode comes before nlink, the padding sits after rdev rather
    /// than after gid, and nlink and blksize are 32 bits wide.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct StatAbi {
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_mode: u32,
        pub st_nlink: u32,
        pub st_uid: u32,
        pub st_gid: u32,
        pub st_rdev: u64,
        pub __pad1: u64,
        pub st_size: i64,
        pub st_blksize: i32,
        pub __pad2: i32,
        pub st_blocks: i64,
        pub st_atime: i64,
        pub st_atime_nsec: i64,
        pub st_mtime: i64,
        pub st_mtime_nsec: i64,
        pub st_ctime: i64,
        pub st_ctime_nsec: i64,
        pub __unused: [u32; 2],
    }

    const _: () = assert!(core::mem::size_of::<StatAbi>() == 128);

    impl From<&Stat> for StatAbi {
        fn from(stat: &Stat) -> Self {
            StatAbi {
                st_dev: stat.st_dev,
                st_ino: stat.st_ino,
                st_mode: stat.st_mode,
                st_nlink: stat.st_nlink as u32,
                st_uid: stat.st_uid,
                st_gid: stat.st_gid,
                st_rdev: stat.st_rdev,
                __pad1: 0,
                st_size: stat.st_size,
                st_blksize: stat.st_blksize as i32,
                __pad2: 0,
                st_blocks: stat.st_blocks,
                st_atime: stat.st_atime,
                st_atime_nsec: stat.st_atime_nsec,
                st_mtime: stat.st_mtime,
                st_mtime_nsec: stat.st_mtime_nsec,
                st_ctime: stat.st_ctime,
                st_ctime_nsec: stat.st_ctime_nsec,
                __unused: [0; 2],
            }
        }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("this target has no ABI layouts in kernel/src/abi.rs");
