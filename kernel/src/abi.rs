//! Linux x86_64 userspace ABI: syscall numbers, error codes, and the structs
//! that cross the boundary.

#![allow(non_camel_case_types)]

pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    pub const SELECT: u64 = 23;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MSYNC: u64 = 26;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const PAUSE: u64 = 34;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const SENDFILE: u64 = 40;
    pub const SOCKET: u64 = 41;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const TRUNCATE: u64 = 76;
    pub const FTRUNCATE: u64 = 77;
    pub const GETDENTS: u64 = 78;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const FCHDIR: u64 = 81;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const CREAT: u64 = 85;
    pub const LINK: u64 = 86;
    pub const UNLINK: u64 = 87;
    pub const SYMLINK: u64 = 88;
    pub const READLINK: u64 = 89;
    pub const CHMOD: u64 = 90;
    pub const FCHMOD: u64 = 91;
    pub const CHOWN: u64 = 92;
    pub const FCHOWN: u64 = 93;
    pub const LCHOWN: u64 = 94;
    pub const FCHOWNAT: u64 = 260;
    pub const UMASK: u64 = 95;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const GETRLIMIT: u64 = 97;
    pub const GETRUSAGE: u64 = 98;
    pub const SYSINFO: u64 = 99;
    pub const TIMES: u64 = 100;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const SETUID: u64 = 105;
    pub const SETGID: u64 = 106;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const SETPGID: u64 = 109;
    pub const GETPPID: u64 = 110;
    pub const GETPGRP: u64 = 111;
    pub const SETSID: u64 = 112;
    pub const GETGROUPS: u64 = 115;
    pub const SETGROUPS: u64 = 116;
    pub const GETPGID: u64 = 121;
    pub const GETSID: u64 = 124;
    pub const RT_SIGSUSPEND: u64 = 130;
    pub const SIGALTSTACK: u64 = 131;
    pub const STATFS: u64 = 137;
    pub const FSTATFS: u64 = 138;
    pub const SCHED_GETPARAM: u64 = 143;
    pub const SCHED_GETSCHEDULER: u64 = 145;
    pub const SCHED_GET_PRIORITY_MAX: u64 = 146;
    pub const SCHED_GET_PRIORITY_MIN: u64 = 147;
    pub const PRCTL: u64 = 157;
    pub const ARCH_PRCTL: u64 = 158;
    pub const SETRLIMIT: u64 = 160;
    pub const SYNC: u64 = 162;
    pub const GETTID: u64 = 186;
    pub const TIME: u64 = 201;
    pub const FUTEX: u64 = 202;
    pub const SCHED_GETAFFINITY: u64 = 204;
    pub const GETDENTS64: u64 = 217;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const EPOLL_CTL: u64 = 233;
    pub const TKILL: u64 = 200;
    pub const TGKILL: u64 = 234;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const FSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const RENAMEAT: u64 = 264;
    pub const LINKAT: u64 = 265;
    pub const SYMLINKAT: u64 = 266;
    pub const MKNOD: u64 = 133;
    pub const MKNODAT: u64 = 259;
    pub const FLOCK: u64 = 73;
    pub const GETPRIORITY: u64 = 140;
    pub const SETPRIORITY: u64 = 141;
    pub const IOPRIO_SET: u64 = 251;
    pub const IOPRIO_GET: u64 = 252;
    pub const SYSLOG: u64 = 103;
    pub const READLINKAT: u64 = 267;
    pub const FCHMODAT: u64 = 268;
    pub const FACCESSAT: u64 = 269;
    pub const PSELECT6: u64 = 270;
    pub const PPOLL: u64 = 271;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const GET_ROBUST_LIST: u64 = 274;
    pub const UTIMENSAT: u64 = 280;
    pub const EPOLL_PWAIT: u64 = 281;
    pub const EVENTFD: u64 = 284;
    pub const ACCEPT4: u64 = 288;
    pub const EPOLL_CREATE1: u64 = 291;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const PRLIMIT64: u64 = 302;
    pub const GETRANDOM: u64 = 318;
    pub const MEMFD_CREATE: u64 = 319;
    pub const EXECVEAT: u64 = 322;
    pub const STATX: u64 = 332;
    pub const RSEQ: u64 = 334;
    pub const CLONE3: u64 = 435;
    pub const CLOSE_RANGE: u64 = 436;
    pub const FACCESSAT2: u64 = 439;
}

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
    EOPNOTSUPP = 95,
    EAFNOSUPPORT = 97,
    ECONNRESET = 104,
    ETIMEDOUT = 110,
    ECONNREFUSED = 111,
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
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
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

// arch_prctl codes
pub const ARCH_SET_GS: u64 = 0x1001;
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;
pub const ARCH_GET_GS: u64 = 0x1004;

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

// Signals
pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGSEGV: i32 = 11;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;

/// Signals whose default action is to stop the task.
pub fn is_stop_signal(signal: i32) -> bool {
    matches!(signal, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

/// `struct stat` as x86_64 Linux defines it (144 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
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

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// `struct termios` with the x86_64 field layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
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

impl Default for Termios {
    fn default() -> Self {
        let mut c_cc = [0u8; 32];
        c_cc[0] = 3; // VINTR
        c_cc[1] = 28; // VQUIT
        c_cc[2] = 127; // VERASE
        c_cc[3] = 21; // VKILL
        c_cc[4] = 4; // VEOF
        c_cc[6] = 1; // VMIN
        c_cc[5] = 0; // VTIME
        Termios {
            c_iflag: ICRNL | IXON,
            c_oflag: OPOST | ONLCR,
            c_cflag: 0o2277, // B38400 | CS8 | CREAD
            c_lflag: ISIG | ICANON | ECHO | ECHOE,
            c_line: 0,
            c_cc,
            c_ispeed: 38400,
            c_ospeed: 38400,
        }
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
