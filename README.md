# claudeos

An x86-64 operating system kernel written from scratch in Rust that implements
enough of the Linux system call interface to run unmodified static Linux
binaries.

It boots an unmodified Alpine Linux root filesystem. It also runs a stock
`rustc --target x86_64-unknown-linux-musl` executable, a C program linked
against musl, and an upstream busybox binary downloaded from busybox.net. The
userland shipped here is built the same way: an ordinary Linux program, not
something written against a private kernel interface.

```
claudeos: starting /bin/sh as pid 1
/ # cat /etc/alpine-release
3.19.1
/ # busybox | head -n 1
BusyBox v1.36.1 (2023-11-07 18:53:09 UTC) multi-call binary.
/ # echo hello | gzip | gunzip
hello
```

```
claudeos:/root# uname -a
Linux claudeos 6.1.0-claudeos #1 SMP x86_64 claudeos
claudeos:/root# ps
  PID  PPID  PGID STAT  COMMAND
    0     0     1 R     idle
    1     0     1 S     init
    2     1     2 S     sh
    7     2     2 R     ps
claudeos:/root# seq 1 20 | grep 1 | wc -l
12
```

## Building and running

Requirements: a Rust toolchain, QEMU, Python 3, and optionally clang for the C
demo. No network access is needed at build time; the musl C library comes from
the Rust toolchain's own `self-contained` directory.

```sh
rustup target add x86_64-unknown-none x86_64-unknown-linux-musl
brew install qemu            # or your platform's package manager

make                         # build the kernel and the userland
make run                     # boot into an interactive shell
make test                    # run every self-test suite
make demo                    # run the scripted tour
make busybox                 # fetch an upstream busybox to test against
make alpine                  # fetch an Alpine root filesystem to boot
```

After `make alpine`, boot into Alpine itself:

```sh
./scripts/run.sh --initrd build/alpine.cpio --append 'init=/bin/sh'
```

`make run` gives a shell on the serial console. `exit` powers the machine off.

## What the kernel does

**Boot.** A multiboot1 header and a 32-bit trampoline build the initial page
tables, switch the CPU into long mode, and jump to Rust code linked at
`0xFFFFFFFF80000000`. QEMU loads the image directly with `-kernel`, so there is
no bootloader to install. The build converts the linked ELF64 to ELF32 with
`llvm-objcopy` because QEMU's multiboot loader only accepts 32-bit ELF headers;
the physical load addresses are what it actually uses.

**Memory.** A bitmap frame allocator covers all usable physical memory reported
by the boot loader's E820 map. Physical memory is also mapped in one piece at
`0xFFFF800000000000`, so page tables and frame contents are reachable without
temporary mappings. On top of that sit a 4-level page table implementation and
a coalescing kernel heap.

**Processes.** Each task owns a kernel stack, a file descriptor table, and a
page table hierarchy whose upper half is shared with the kernel. `fork` is
copy-on-write: the two sides share every writable page read-only until one of
them writes, and the fault handler hands out the private copy. `clone` with
`CLONE_VM` shares the address space outright, which is what threads use.
Scheduling is round-robin, preemptive, driven by the 100 Hz timer tick.
Anonymous memory is demand-paged: `mmap` and `brk` record a region and the
page fault handler supplies pages on first touch.

**Blocking.** A task waiting for the terminal or a pipe sleeps on a wait queue
rather than spinning, so the scheduler reaches the idle task and the CPU halts
until an interrupt arrives. Sitting at the shell prompt costs about 1% of a
core, which is the timer tick.

**System calls.** Entry is through `syscall`/`MSR_LSTAR`. The frame the entry
stub builds has the same layout as the one an interrupt builds, so one code
path serves both and both return through `iretq`. Interrupts are re-enabled
once the kernel stack is in place, so the kernel is preemptible and a blocking
read does not shut out the device it is waiting for. Around 130 Linux system
call numbers are implemented, including the file, memory, process, thread,
signal, and time groups that a libc start-up sequence and a threaded program
actually exercise.

**Programs.** The ELF loader takes static executables, static-PIE, and
dynamically linked ones. For the last, it loads the program interpreter the
binary names, reports the interpreter's load address in `AT_BASE` and the
program's own entry in `AT_ENTRY`, and starts execution in the interpreter,
which then relocates and runs the program. That is what lets Alpine's musl
loader bring up Alpine's userland.

**Signals.** A handler installed with `rt_sigaction` is really entered: the
kernel writes the same `rt_sigframe` Linux does onto the user stack, points the
return address at the libc restorer, and `rt_sigreturn` puts the interrupted
state back. Terminal signals are raised from the interrupt that receives the
character, so `Ctrl-C` reaches a running job while the shell is blocked waiting
for it. The shell puts each job in its own process group and hands it the
terminal, so interrupting a job leaves the shell running.

**Job control.** `Ctrl-Z` stops the foreground job rather than killing it: a
stopped task leaves the run queue until something sends it `SIGCONT`, and the
stop and the later continue are reported to whoever is in `wait4` with
`WUNTRACED` or `WCONTINUED`. A background job that reads the terminal is
stopped with `SIGTTIN` instead of taking the input, and picks the read up where
it left off once it is continued in the foreground. The shell has `jobs`, `fg`
and `bg`, `kill %1` takes a job number, and `ps` shows a stopped task as `T`.

**Filesystem.** An in-memory tree is populated at boot from a cpio archive
passed as a multiboot module. Character devices (`/dev/null`, `/dev/zero`,
`/dev/random`, `/dev/console`) and a generated `/proc` (`meminfo`, `uptime`,
`cpuinfo`, `tasks`, and a directory per process) are mounted into the same
tree. Pipes, symbolic links, and `getdents64` all work. A file can have more
than one name: nodes are reference counted and a directory entry is the
reference, so `link` is a second entry for the same node and `st_nlink` counts
them. `mkfifo` makes a named pipe whose two ends meet at one buffer, with the
open of each side waiting for the other. `/proc/<pid>/fd` is rebuilt whenever
something looks inside it, so it lists the descriptors the process has open
right now.

**Kernel log.** Everything the kernel prints is also kept in a 16 KiB ring, and
`syslog` hands it back, so `dmesg` shows the boot messages.

**Waiting on several things.** `eventfd` is a counter two tasks can wait on.
`socketpair` gives two connected `AF_UNIX` endpoints, each reading what the
other writes, and `sendto`, `recvfrom`, `sendmsg`, `recvmsg` and `shutdown`
work on them. `epoll_create1`, `epoll_ctl` and `epoll_wait` watch any mix of
pipes, sockets, counters and the terminal. Readiness is worked out when the set
is waited on rather than pushed in from each descriptor, so epoll here is
level-triggered. A task in `poll`, `select` or `epoll_wait` sleeps on one queue
that every readiness change wakes, with the timeout as the sleep's deadline, so
it neither spins nor waits out a tick to notice a byte that has arrived. The
condition is re-tested from inside the sleep, on descriptors held across it, so
a change that lands between the check and the sleep cannot be missed.

**Console.** A 16550 UART and a PS/2 keyboard feed one input ring. A line
discipline implements canonical mode with echo, backspace, `Ctrl-C`, `Ctrl-D`
and `Ctrl-U`, and honours the `termios` settings a program sets through
`ioctl`, so raw-mode programs work too.

## What the userland is

`cbox` is a single static binary built for `x86_64-unknown-linux-musl` that
provides about fifty applets chosen by `argv[0]`, the way busybox does. The
initramfs contains the binary once and a symbolic link per applet.

The shell supports pipelines, redirection (`>` `>>` `<` `2>`), here-documents
(`<<` and `<<-`, with a quoted delimiter suppressing expansion), `&&` `||` `;`
`&`, `!` to invert a status, `( ... )` subshells, globbing, single and double
quoting, `$VAR` and `${VAR}` expansion, command substitution with `$(...)` and
backticks, arithmetic with `$((...))`, `if`/`elif`/`else`, `while`, `until`,
`for`, `case` with alternation patterns, functions with positional parameters,
`jobs`, `fg`, `bg`,
and the usual builtins. Word expansion is a single pass, so text a command
substitution produced is not rescanned. A reserved word is reserved only where
a command can start, so `echo done` prints "done".

A syntax error names the script, the line and the text that was found, and the
commands before it still run, the way a shell that reads command by command
behaves.

At the prompt it puts the terminal in raw mode and edits the line itself:
arrow-key history, left/right cursor movement, Home/End/Delete,
Ctrl-A/E/B/F/K/U/W/L, a `history` builtin, and tab completion of command names
from PATH and of file paths elsewhere. Cooked mode comes back before a command
runs, so the job owns the terminal.

The coreutils cover the common set: `ls cat cp mv rm mkdir rmdir touch ln mkfifo
chmod stat find du df echo wc head tail grep sed xargs sort uniq cut tr tee
seq rev printf expr test ps free uptime dmesg date env id uname hostname mount
kill sleep clear hexdump basename dirname yes true false`.

`grep` and `sed` share a backtracking regex engine written for them: anchors,
`.`, character classes and `*`, with `-E` adding alternation, groups, `+` and
`?`. `sed` takes line, `$`, regex and range addresses, `!`, and the `s`, `y`,
`p`, `d`, `q` and `=` commands, with `-n`, `-e` and `-i`.

## Tests

`make test` boots the OS once per suite and requires each to report zero
failures.

- `tests/suite.sh` runs **219 checks** inside the OS, driving the shell through
  pipelines, redirection, here-documents, globbing, control flow, `case`,
  subshells, functions, file and script execution, `chmod`, devices,
  subprocesses and `/proc`.
- The `rtest` applet runs **37 checks** against the Rust standard library:
  multi-megabyte allocations, sorting two million elements, eight threads
  incrementing an atomic, a mutex shared across threads, an `mpsc` channel,
  thread sleep against the monotonic clock, file read/write/seek/append,
  directory iteration, `std::process::Command` capturing a child's output
  through pipes, signal handlers running and returning, a `UnixStream` pair
  carrying bytes both ways, and an epoll set woken by a counter and a socket,
  timing out when it should and waking promptly when a write arrives.
- `tests/busybox.sh` runs **36 checks** against an upstream busybox binary that
  this project did not build: `awk`, `sed`, `tar` create and extract, `find`,
  `md5sum` and `sha256sum` (whose digests are compared against the ones the
  host computes for the same input), `ps`, `df`, `xargs`, `timeout`, and
  busybox's own `ash` shell running loops, pipelines and arithmetic. Run
  `make busybox` first to fetch it; the suite is skipped when it is absent.
- `tests/alpine.sh` runs **34 checks** inside an unmodified Alpine Linux root
  filesystem, where every program is dynamically linked and loaded by Alpine's
  own musl loader: `awk`, `sed`, `tar` with gzip, `md5sum` and `sha256sum`
  against digests the host computes, `find`, `stat`, `ps`, and ash running
  loops, `case` and here-documents. Run `make alpine` first; the suite is
  skipped when it is absent.
- An **interactive session** is driven over the serial console: typing after
  boot, backspace and Ctrl-U line editing, `Ctrl-C` on a running job, `Ctrl-Z`
  followed by `jobs`, `bg` and `kill %1`, a background `cat` stopped for
  reading the terminal and resumed with `fg`, and the clock advancing while the
  shell is blocked in a read.

Every suite is an ordinary Linux program. Nothing in them is aware that they
are not running on Linux.

## Layout

```
kernel/src
  boot.s              multiboot header, 32-bit trampoline into long mode
  main.rs             start-up sequence and kernel command line
  mm/                 frame allocator, page tables, kernel heap
  cpu/                GDT and TSS, IDT and stubs, PIC, PIT, MSRs
  syscall/            entry stub and the Linux system call implementations
  fs/                 in-memory filesystem, devices, pipes, cpio, /proc
  elf.rs              ELF64 loader for static and static-PIE executables
  task.rs             task control block, user stack and auxiliary vector
  sched.rs            round-robin scheduler, exit and reaping
  uaccess.rs          validated copying between kernel and user memory
  console.rs          input ring and terminal line discipline
  signal.rs           signal frames, delivery and rt_sigreturn

user/cbox             the multicall userland binary (shell, init, coreutils)
user/c/hello.c        a C program linked against musl
tools/mkcpio.py       initramfs builder
tools/drive.py        drives the console over a socket, rendering as a terminal
scripts/reap-stale.sh clears QEMU instances an earlier run left behind
tests/suite.sh        in-OS shell and userland test suite
tests/busybox.sh      in-OS suite driving an upstream busybox
tests/alpine.sh       in-OS suite run inside an Alpine root filesystem
```

## Limitations

Single CPU; no SMP. There is no block device driver or on-disk filesystem: the
root filesystem lives in RAM and changes do not survive a reboot. There is no
networking, so the socket calls return `EAFNOSUPPORT`. `futex` still waits by re-checking rather than by queueing,
though it yields or sleeps rather than spins. Sockets are `socketpair` only:
there is no `bind` or `connect`, so nothing can be reached by name or over a
network.
