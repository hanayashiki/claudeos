//! Linux system call numbers for aarch64.
//!
//! The numbering is per-architecture: the same name carries a different number
//! on every Linux port, so this table belongs on this side of the boundary.
//! aarch64 was added late enough to take the generic numbering unchanged,
//! which means it has no number at all for the calls that had already been
//! replaced by an `at`-suffixed form by then: no `open`, only `openat`; no
//! `stat`, only `fstatat`. Those names still have to exist, because the
//! dispatcher is written once for every architecture, so they are given
//! numbers above everything Linux numbers here.

/// First of the numbers that mean "this call does not exist here". A program
/// can name one -- the number the dispatcher sees came out of a register, and
/// a register holds any number -- so the dispatcher turns everything from here
/// up away before it matches anything against the table.
pub const ABSENT: u64 = 0x1_0000;

pub const READ: u64 = 63;
pub const WRITE: u64 = 64;
pub const OPEN: u64 = ABSENT;
pub const CLOSE: u64 = 57;
pub const STAT: u64 = ABSENT + 1;
pub const FSTAT: u64 = 80;
pub const LSTAT: u64 = ABSENT + 2;
pub const POLL: u64 = ABSENT + 3;
pub const LSEEK: u64 = 62;
pub const MMAP: u64 = 222;
pub const MPROTECT: u64 = 226;
pub const MUNMAP: u64 = 215;
pub const BRK: u64 = 214;
pub const RT_SIGACTION: u64 = 134;
pub const RT_SIGPROCMASK: u64 = 135;
pub const RT_SIGRETURN: u64 = 139;
pub const IOCTL: u64 = 29;
pub const PREAD64: u64 = 67;
pub const PWRITE64: u64 = 68;
pub const READV: u64 = 65;
pub const WRITEV: u64 = 66;
pub const ACCESS: u64 = ABSENT + 4;
pub const PIPE: u64 = ABSENT + 5;
pub const SELECT: u64 = ABSENT + 6;
pub const SCHED_YIELD: u64 = 124;
pub const MREMAP: u64 = 216;
pub const MSYNC: u64 = 227;
pub const MADVISE: u64 = 233;
pub const DUP: u64 = 23;
pub const DUP2: u64 = ABSENT + 7;
pub const PAUSE: u64 = ABSENT + 8;
pub const NANOSLEEP: u64 = 101;
pub const GETPID: u64 = 172;
pub const SENDFILE: u64 = 71;
pub const SOCKET: u64 = 198;
pub const CONNECT: u64 = 203;
pub const ACCEPT: u64 = 202;
pub const BIND: u64 = 200;
pub const LISTEN: u64 = 201;
pub const CLONE: u64 = 220;
pub const FORK: u64 = ABSENT + 9;
pub const VFORK: u64 = ABSENT + 10;
pub const EXECVE: u64 = 221;
pub const EXIT: u64 = 93;
pub const WAIT4: u64 = 260;
pub const KILL: u64 = 129;
pub const UNAME: u64 = 160;
pub const FCNTL: u64 = 25;
pub const FSYNC: u64 = 82;
pub const TRUNCATE: u64 = 45;
pub const FTRUNCATE: u64 = 46;
pub const GETDENTS: u64 = ABSENT + 11;
pub const GETCWD: u64 = 17;
pub const CHDIR: u64 = 49;
pub const FCHDIR: u64 = 50;
pub const RENAME: u64 = ABSENT + 12;
pub const MKDIR: u64 = ABSENT + 13;
pub const RMDIR: u64 = ABSENT + 14;
pub const CREAT: u64 = ABSENT + 15;
pub const LINK: u64 = ABSENT + 16;
pub const UNLINK: u64 = ABSENT + 17;
pub const SYMLINK: u64 = ABSENT + 18;
pub const READLINK: u64 = ABSENT + 19;
pub const CHMOD: u64 = ABSENT + 20;
pub const FCHMOD: u64 = 52;
pub const CHOWN: u64 = ABSENT + 21;
pub const FCHOWN: u64 = 55;
pub const LCHOWN: u64 = ABSENT + 22;
pub const FCHOWNAT: u64 = 54;
pub const UMASK: u64 = 166;
pub const GETTIMEOFDAY: u64 = 169;
pub const GETRLIMIT: u64 = 163;
pub const GETRUSAGE: u64 = 165;
pub const SYSINFO: u64 = 179;
pub const TIMES: u64 = 153;
pub const GETUID: u64 = 174;
pub const GETGID: u64 = 176;
pub const SETUID: u64 = 146;
pub const SETGID: u64 = 144;
pub const GETEUID: u64 = 175;
pub const GETEGID: u64 = 177;
pub const SETPGID: u64 = 154;
pub const GETPPID: u64 = 173;
pub const GETPGRP: u64 = ABSENT + 23;
pub const SETSID: u64 = 157;
pub const GETGROUPS: u64 = 158;
pub const SETGROUPS: u64 = 159;
pub const GETPGID: u64 = 155;
pub const GETSID: u64 = 156;
pub const RT_SIGSUSPEND: u64 = 133;
pub const SIGALTSTACK: u64 = 132;
pub const STATFS: u64 = 43;
pub const FSTATFS: u64 = 44;
pub const SCHED_GETPARAM: u64 = 121;
pub const SCHED_GETSCHEDULER: u64 = 120;
pub const SCHED_GET_PRIORITY_MAX: u64 = 125;
pub const SCHED_GET_PRIORITY_MIN: u64 = 126;
pub const PRCTL: u64 = 167;
/// There is no such call here: the thread pointer is a register a program
/// writes itself.
pub const ARCH_PRCTL: u64 = ABSENT + 24;
pub const SETRLIMIT: u64 = 164;
pub const SYNC: u64 = 81;
pub const REBOOT: u64 = 142;
pub const GETTID: u64 = 178;
pub const TIME: u64 = ABSENT + 25;
pub const FUTEX: u64 = 98;
pub const SCHED_GETAFFINITY: u64 = 123;
pub const GETDENTS64: u64 = 61;
pub const SET_TID_ADDRESS: u64 = 96;
pub const CLOCK_GETTIME: u64 = 113;
pub const CLOCK_GETRES: u64 = 114;
pub const CLOCK_NANOSLEEP: u64 = 115;
pub const EXIT_GROUP: u64 = 94;
pub const EPOLL_CTL: u64 = 21;
pub const TKILL: u64 = 130;
pub const TGKILL: u64 = 131;
pub const OPENAT: u64 = 56;
pub const MKDIRAT: u64 = 34;
pub const FSTATAT: u64 = 79;
pub const UNLINKAT: u64 = 35;
pub const RENAMEAT: u64 = 38;
pub const LINKAT: u64 = 37;
pub const SYMLINKAT: u64 = 36;
pub const MKNOD: u64 = ABSENT + 26;
pub const MKNODAT: u64 = 33;
pub const FLOCK: u64 = 32;
pub const GETPRIORITY: u64 = 141;
pub const SETPRIORITY: u64 = 140;
pub const IOPRIO_SET: u64 = 30;
pub const IOPRIO_GET: u64 = 31;
pub const SYSLOG: u64 = 116;
pub const READLINKAT: u64 = 78;
pub const FCHMODAT: u64 = 53;
pub const FACCESSAT: u64 = 48;
pub const PSELECT6: u64 = 72;
pub const PPOLL: u64 = 73;
pub const SET_ROBUST_LIST: u64 = 99;
pub const GET_ROBUST_LIST: u64 = 100;
pub const UTIMENSAT: u64 = 88;
pub const EPOLL_PWAIT: u64 = 22;
pub const EVENTFD: u64 = ABSENT + 27;
pub const ACCEPT4: u64 = 242;
pub const EPOLL_CREATE1: u64 = 20;
pub const EPOLL_CREATE: u64 = ABSENT + 28;
pub const EPOLL_WAIT: u64 = ABSENT + 29;
pub const EVENTFD2: u64 = 19;
pub const SOCKETPAIR: u64 = 199;
pub const SENDTO: u64 = 206;
pub const RECVFROM: u64 = 207;
pub const SENDMSG: u64 = 211;
pub const RECVMSG: u64 = 212;
pub const SHUTDOWN: u64 = 210;
pub const SETSOCKOPT: u64 = 208;
pub const GETSOCKOPT: u64 = 209;
pub const GETSOCKNAME: u64 = 204;
pub const GETPEERNAME: u64 = 205;
pub const DUP3: u64 = 24;
pub const PIPE2: u64 = 59;
pub const PRLIMIT64: u64 = 261;
pub const GETRANDOM: u64 = 278;
pub const MEMFD_CREATE: u64 = 279;
pub const EXECVEAT: u64 = 281;
pub const STATX: u64 = 291;
pub const RSEQ: u64 = 293;
pub const CLONE3: u64 = 435;
pub const CLOSE_RANGE: u64 = 436;
pub const FACCESSAT2: u64 = 439;
