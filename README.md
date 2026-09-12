# claudeos

An x86-64 operating system kernel written from scratch in Rust that implements
enough of the Linux system call interface to run unmodified static Linux
binaries.

A stock `rustc --target x86_64-unknown-linux-musl` executable, or a C program
linked against musl, runs on it without changes. The userland shipped here is
built that way: it is an ordinary Linux program, not something written against
a private kernel interface.

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
make test                    # run both self-test suites
make demo                    # run the scripted tour
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
page table hierarchy whose upper half is shared with the kernel. `fork` copies
the user half frame by frame; `clone` with `CLONE_VM` shares it, which is what
threads use. Scheduling is round-robin, preemptive, driven by the 100 Hz timer
tick. Anonymous memory is demand-paged: `mmap` and `brk` record a region and
the page fault handler supplies pages on first touch.

**System calls.** Entry is through `syscall`/`MSR_LSTAR`. The frame the entry
stub builds has the same layout as the one an interrupt builds, so one code
path serves both and both return through `iretq`. Interrupts are re-enabled
once the kernel stack is in place, so the kernel is preemptible and a blocking
read does not shut out the device it is waiting for. Around 130 Linux system
call numbers are implemented, including the file, memory, process, thread,
signal, and time groups that a libc start-up sequence and a threaded program
actually exercise.

**Signals.** A handler installed with `rt_sigaction` is really entered: the
kernel writes the same `rt_sigframe` Linux does onto the user stack, points the
return address at the libc restorer, and `rt_sigreturn` puts the interrupted
state back. Terminal signals are raised from the interrupt that receives the
character, so `Ctrl-C` reaches a running job while the shell is blocked waiting
for it. The shell puts each job in its own process group and hands it the
terminal, so interrupting a job leaves the shell running.

**Filesystem.** An in-memory tree is populated at boot from a cpio archive
passed as a multiboot module. Character devices (`/dev/null`, `/dev/zero`,
`/dev/random`, `/dev/console`) and a generated `/proc` (`meminfo`, `uptime`,
`cpuinfo`, `tasks`, and a directory per process) are mounted into the same
tree. Pipes, symbolic links, and `getdents64` all work.

**Console.** A 16550 UART and a PS/2 keyboard feed one input ring. A line
discipline implements canonical mode with echo, backspace, `Ctrl-C`, `Ctrl-D`
and `Ctrl-U`, and honours the `termios` settings a program sets through
`ioctl`, so raw-mode programs work too.

## What the userland is

`cbox` is a single static binary built for `x86_64-unknown-linux-musl` that
provides about fifty applets chosen by `argv[0]`, the way busybox does. The
initramfs contains the binary once and a symbolic link per applet.

The shell supports pipelines, redirection (`>` `>>` `<` `2>`), `&&` `||` `;`
`&`, globbing, single and double quoting, `$VAR` and `${VAR}` expansion,
command substitution with `$(...)` and backticks, arithmetic with `$((...))`,
`if`/`elif`/`else`, `while`, `until`, `for`, functions with positional
parameters, and the usual builtins.

The coreutils cover the common set: `ls cat cp mv rm mkdir rmdir touch ln stat
find du df echo wc head tail grep sort uniq cut tr tee seq rev printf expr test
ps free uptime date env id uname hostname mount kill sleep clear hexdump
basename dirname yes true false`.

## Tests

`make test` boots the OS twice and requires both suites to report zero
failures.

- `tests/suite.sh` runs **68 checks** inside the OS, driving the shell through
  pipelines, redirection, globbing, control flow, functions, file operations,
  devices, subprocesses and `/proc`.
- The `rtest` applet runs **25 checks** against the Rust standard library:
  multi-megabyte allocations, sorting two million elements, eight threads
  incrementing an atomic, a mutex shared across threads, an `mpsc` channel,
  thread sleep against the monotonic clock, file read/write/seek/append,
  directory iteration, `std::process::Command` capturing a child's output
  through pipes, and signal handlers running and returning.
- An **interactive session** is driven over the serial console: typing after
  boot, `Ctrl-C` on a running job, a background job, and the clock advancing
  while the shell is blocked in a read.

Both suites are ordinary Linux programs. Nothing in them is aware that they are
not running on Linux.

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

user/cbox             the multicall userland binary
user/c/hello.c        a C program linked against musl
tools/mkcpio.py       initramfs builder
tools/drive.py        drives the console over a socket for interactive tests
tests/suite.sh        in-OS shell and userland test suite
```

## Limitations

Single CPU; no SMP. There is no block device driver or on-disk filesystem: the
root filesystem lives in RAM and changes do not survive a reboot. There is no
networking, so the socket calls return `EAFNOSUPPORT`. Dynamically linked
executables are rejected; only static and static-PIE ELF binaries load. Job
control stops at process groups and the foreground terminal: `SIGTSTP` and `fg`
are not implemented. `futex` waits by polling rather than by queueing waiters.
