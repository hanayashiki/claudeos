# claudeos

An operating system kernel written from scratch in Rust that implements enough
of the Linux system call interface to run unmodified static Linux binaries. It
runs on x86-64 and on aarch64, where the machine it targets is a Raspberry Pi 4.

It boots an unmodified Alpine Linux root filesystem. It also runs a stock
`rustc --target x86_64-unknown-linux-musl` executable, a C program linked
against musl, and an upstream busybox binary downloaded from busybox.net, or
from Alpine's package repository on aarch64, which busybox.net has no build
for. The userland shipped here is built the same way: an ordinary Linux
program, not something written against a private kernel interface.

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
QEMU's monitor is multiplexed onto that same console, which is what leaves
`Ctrl-C` to the guest: it interrupts the job running there rather than killing
the emulator. `Ctrl-A` then `X` is the way out of the emulator.

### The other machine

The kernel also builds for aarch64 and boots on a Raspberry Pi 4, emulated or
real. `ARCH` picks which one; everything defaults to x86-64.

```sh
rustup target add aarch64-unknown-none-softfloat aarch64-unknown-linux-musl

ARCH=aarch64 ./scripts/build.sh          # build/kernel8.img
./scripts/build-user-aarch64.sh          # build/initramfs-aarch64.cpio
ARCH=aarch64 ./scripts/run.sh --initrd build/initramfs-aarch64.cpio \
    --append 'init=/bin/init'
ARCH=aarch64 make busybox                # an aarch64 busybox to test against
ARCH=aarch64 make alpine                 # an aarch64 Alpine root filesystem
ARCH=aarch64 ./scripts/test.sh           # all eight suites
```

Both third-party images are fetched for the machine `ARCH` names, so the same
eight suites run on either one. busybox.net has no aarch64 build among its
prebuilt binaries, so that one comes from Alpine's `busybox-static` package
instead; `scripts/fetch-busybox.sh` refuses anything that is not a static ELF
for the machine asked for, since a dynamically linked one has no interpreter to
load it here.

## Putting it on a Raspberry Pi 4

A Pi 4 boots from one FAT32 partition on an SD card. Its bootloader is in an
EEPROM on the board and can read nothing else -- not ext4, not GPT -- so
everything it needs is a plain file on that partition, found by name. There is
no boot sector to install and nothing to mark bootable.

```sh
ARCH=aarch64 ./scripts/build.sh
./scripts/build-user-aarch64.sh
./scripts/mkcard.sh                      # assemble build/boot and stop
./scripts/mkcard.sh /dev/disk4           # and write it to that card
```

Run it once with no argument first and look at what it assembled. With a device
named it erases that card, so it refuses anything that is not a removable whole
disk and then asks you to type the path again before it writes. Find the path
with `diskutil list` on macOS or `lsblk` on Linux, and check the size against
the card in your hand: naming the wrong one destroys whatever was on it.

What ends up on the card:

```
start4.elf                 the firmware the EEPROM bootloader loads
fixup4.dat                 how it splits memory with the video core
bcm2711-rpi-4-b.dtb        the device tree describing this board
overlays/disable-bt.dtbo   the change to it described below
config.txt                 what the firmware reads before anything else
cmdline.txt                the kernel's command line
kernel8.img                this kernel, as a flat image
initramfs-aarch64.cpio     the userland
```

The first four are Raspberry Pi firmware. They are not in this repository; the
script fetches them from the Raspberry Pi firmware repository and caches them
under `build/`.

The lines in `config.txt` that matter:

```
arm_64bit=1
enable_uart=1
dtoverlay=disable-bt
initramfs initramfs-aarch64.cpio followkernel
```

`arm_64bit` starts the processor in 64-bit mode and makes the firmware look for
`kernel8.img`. `enable_uart` turns the serial console on. `initramfs` loads the
ram disk and tells the kernel where it put it, through the device tree; the
word takes a space rather than an equals sign, which is a quirk of the file.

`dtoverlay=disable-bt` is the one that is not obvious. This board has two
serial ports, and the good one is wired to the Bluetooth radio: the pins a
cable clips onto carry the cut-down one instead. `enable_uart=1` alone does not
move it. Without the overlay the kernel writes correctly formed bytes into a
port whose pins go to the radio and the cable shows nothing. The kernel also
sets those pins itself at start-up, so it works either way, but the overlay is
what a Linux system would do and leaving it in means the two agree.

To watch it: the two lines cross over, because one end's transmit is the
other's receive. The header is labelled by position rather than by these names,
and the three pins you need sit next to each other on the outer row:

```
pin 6    ground          -> adapter ground
pin 8    GPIO 14, TXD0   -> adapter receive
pin 10   GPIO 15, RXD0   -> adapter transmit
```

The Pi transmits on GPIO 14, so the adapter's receive line goes there, and it
listens on GPIO 15, so the adapter's transmit line goes there. Leave the
adapter's power line unconnected: the Pi has its own supply, and joining the
two can push current back through the board. Read it at 115200 baud, 8 bits, no parity, one stop bit --
`screen /dev/tty.usbserial-* 115200` on macOS, `screen /dev/ttyUSB0 115200`
on Linux.

Nothing is written to the card at run time, so the machine comes up the same
way every time and a bad experiment costs a rebuild rather than a reflash.

## What the kernel does

**Boot.** A multiboot1 header and a 32-bit trampoline build the initial page
tables, switch the CPU into long mode, and jump to Rust code linked at
`0xFFFFFFFF80000000`. QEMU loads the image directly with `-kernel`, so there is
no bootloader to install. The build converts the linked ELF64 to ELF32 with
`llvm-objcopy` because QEMU's multiboot loader only accepts 32-bit ELF headers;
the physical load addresses are what it actually uses.

**Memory.** A bitmap frame allocator covers all usable physical memory reported
by the boot loader's E820 map. A frame is handed out as an owned value whose
`Drop` releases it, and a page table entry is what holds that value, so the
reference a mapping owns is given back by taking the mapping away rather than
by remembering to call free. Physical memory is also mapped in one piece at
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

**Time.** The timestamp counter is calibrated against the timer tick at boot,
so `CLOCK_MONOTONIC` has real resolution rather than the 10 ms of the tick. The
tick still drives scheduling and timeouts.

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
dynamically linked ones. A program is not copied into memory at `exec`: its
pages are read from the file as it reaches them, and only the pages an image
cannot leave to a fault, a partial head or tail and anything past the file's
contents, are assembled up front. For the last, it loads the program interpreter the
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
open of each side waiting for the other, or holding both ends at once when it
is opened read-write. `/proc/<pid>/fd` is rebuilt whenever
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

**Networking.** A PCI scan finds an emulated Intel gigabit card and brings it
up: two descriptor rings, buffers the card reaches by physical address, and
registers mapped uncached into the kernel half. Its interrupt handler only
moves frames off the ring and wakes a kernel task, because protocol work with
interrupts masked would hold off the timer for as long as it ran. Above that
sit Ethernet, address resolution with a cache, IPv4, ICMP, UDP and TCP, and the
`AF_INET` socket calls, which report readiness through the same machinery
`poll`, `select` and `epoll` already used. TCP does a passive and an active
open, in-order data with acknowledgements, retransmission on a timer, and an
orderly close. It is correct on a quiet link. There is no reassembly queue, no
fast retransmit and no round-trip estimator, so it is not correct on a lossy
one.

The Pi's wired port is the other driver under the same seam. Its controller is
not on a bus that can be enumerated, so the device tree is what says where it
is, which line it raises, and what hardware address the board was built with;
its descriptors live inside the device rather than in memory; its packet
buffers are cleaned out of the data cache before it reads one and invalidated
after it writes one, because nothing on that chip snoops; and its link is a
separate chip on a management bus, which has to negotiate before anything can
be sent and has to be asked what it settled on.

```
$ ./scripts/run.sh --hostfwd tcp::8080-:8080 --initrd build/initramfs.cpio \
      --append 'init=/bin/inet serve 8080'
inet: listening on 0.0.0.0:8080
inet: connection from 10.0.2.2:55451

$ curl -i http://127.0.0.1:8080/
HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: 20

hello from claudeos
```

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

`read` splits a line into as many fields as it is given names for, honouring
IFS, and a reserved word is reserved only where a command can start, so `echo
done` prints "done".

The interrupt key abandons the whole command, not just the process that was
running when it was pressed: a `while` or `for` loop, an enclosing list, a
function body and a command substitution all give up with it and the prompt
comes back with a status of 130. An interactive shell ignores the key itself,
so what it has to go on is the wait status of the job it handed the terminal
to. A script's shell is in the same process group as its children, so the key
kills it outright, and a child that dies of `SIGINT` for its own reasons there
does not end the script -- which is what bash and dash do.

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
`?`. `sed` takes line, `$`, regex and range addresses, `!`, and the `s`, `y`, `p`,
`d`, `q` and `=` commands, with `-n`, `-e`, `-E` and `-i`. A group's text can be
put back with `\1`, and `grep -o` prints what matched rather than the line.
`grep -r` with no path searches the working directory, since a recursive
search of standard input is not something that can be asked for.

## Tests

`make test` boots the OS once per suite and requires each to report zero
failures.

- `tests/suite.sh` runs **251 checks** inside the OS, driving the shell through
  pipelines, redirection, here-documents, globbing, control flow, `case`,
  subshells, functions, file and script execution, `chmod`, devices,
  subprocesses and `/proc`.
- The `rtest` applet runs **46 checks** against the Rust standard library:
  multi-megabyte allocations, sorting two million elements, eight threads
  incrementing an atomic, a mutex shared across threads, an `mpsc` channel,
  thread sleep against the monotonic clock, file read/write/seek/append,
  directory iteration, `std::process::Command` capturing a child's output
  through pipes, signal handlers running and returning, a `UnixStream` pair
  carrying bytes both ways, and an epoll set woken by a counter and a socket,
  timing out when it should and waking promptly when a write arrives. The last
  two run a thread alongside a sibling failing an exec over and over, which is
  a smoke test for a race rather than proof of its absence.
- `tests/busybox.sh` runs **39 checks** against an upstream busybox binary that
  this project did not build: `awk`, `sed`, `tar` create and extract, `find`,
  `md5sum` and `sha256sum` (whose digests are compared against the ones the
  host computes for the same input), `ps`, `df`, `xargs`, `timeout`, and
  busybox's own `ash` shell running loops, pipelines and arithmetic. Run
  `make busybox` first to fetch it; the suite is skipped when it is absent.
  The x86-64 binary is busybox.net's own 1.35.0 build against musl; the aarch64
  one is Alpine's `busybox-static` 1.36.1, because busybox.net publishes no
  aarch64 build.
- `tests/alpine.sh` runs **34 checks** inside an unmodified Alpine Linux root
  filesystem, where every program is dynamically linked and loaded by Alpine's
  own musl loader: `awk`, `sed`, `tar` with gzip, `md5sum` and `sha256sum`
  against digests the host computes, `find`, `stat`, `ps`, and ash running
  loops, `case` and here-documents. Run `make alpine` first; the suite is
  skipped when it is absent. Alpine publishes the same minimal root filesystem
  for both machines, so the same 34 checks run on either.
- An **interactive session** is driven over the serial console: typing after
  boot, backspace and Ctrl-U line editing, `Ctrl-C` on a running job, `Ctrl-Z`
  followed by `jobs`, `bg` and `kill %1`, a background `cat` stopped for
  reading the terminal and resumed with `fg`, and the clock advancing while the
  shell is blocked in a read.
- The **interrupt key at a terminal** is a second session, driven through
  `scripts/run.sh` on a pseudo-terminal rather than over a socket. A socket
  hands the guest whatever byte is written to it, so it cannot say whether the
  terminal in front of QEMU would have kept `Ctrl-C` for the host; this one
  interrupts a foreground `cat` and then a `while true` loop, and requires the
  prompt back and the next command run each time.

Every suite is an ordinary Linux program. Nothing in them is aware that they
are not running on Linux.

## Layout

```
kernel/src
  arch/               everything that only makes sense on one instruction set
  arch/mod.rs         picks the target and re-exports it as `arch`
  arch/x86_64/        boot trampoline, descriptor tables, PIC and PIT, port
                      I/O, page tables, UART and keyboard, timestamp counter
                      and CMOS clock, trap and system call entry, trap frame,
                      signal frame, system call numbers, multiboot decoder
  arch/aarch64/       entry from firmware down to EL1, translation tables,
                      GIC and architected timer, PL011 and its pins, exception
                      vectors and trap frame, signal frame, system call
                      numbers, device tree and tag list decoders
  boot.rs             what a machine looks like, whatever told the kernel
  main.rs             start-up sequence and kernel command line
  mm/                 frame allocator and kernel heap
  syscall/            the Linux system call implementations
  fs/                 in-memory filesystem, devices, pipes, cpio, /proc
  elf.rs              ELF64 loader for static and static-PIE executables
  task.rs             task control block, user stack and auxiliary vector
  sched.rs            round-robin scheduler, exit and reaping
  uaccess.rs          validated copying between kernel and user memory
  console.rs          input ring and terminal line discipline
  signal.rs           signal dispositions and default actions
  trap.rs             exception and interrupt handling

user/cbox             the multicall userland binary (shell, init, coreutils)
user/c/hello.c        a C program linked against musl
tools/mkcpio.py       initramfs builder
tools/drive.py        drives the console over a socket, rendering as a terminal
scripts/reap-stale.sh clears QEMU instances an earlier run left behind
scripts/mkcard.sh     assembles the boot partition for a Pi, and writes a card
tests/suite.sh        in-OS shell and userland test suite
tests/busybox.sh      in-OS suite driving an upstream busybox
tests/alpine.sh       in-OS suite run inside an Alpine root filesystem
```

## Limitations

Single CPU; no SMP. There is no block device driver and no on-disk filesystem:
the root filesystem lives in RAM and changes do not survive a reboot.

The Pi's Ethernet driver has never run. QEMU's `raspi4b` machine emulates no
network device at all, so nothing about it can be tried before it meets a
board: what can be checked without one is checked at boot, and the rest is
written against Linux's driver and u-boot's and has to be taken on that. The
PCIe root complex on the same board is still not driven, so anything on it --
which is where the USB controller is -- is out of reach. There is nothing on
the board that remembers the time across a power cycle, so the clock starts
from the newest date on the ram disk rather than from the real one.

The TCP is correct on a quiet link and not on a lossy one: no reassembly queue,
no fast retransmit, no round-trip estimator. There is no DHCP and no resolver,
so addresses come from the kernel command line. IPv4 only, and fragments are
dropped rather than reassembled.
