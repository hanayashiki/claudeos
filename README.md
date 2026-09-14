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
make cloudflared             # fetch Cloudflare's own cloudflared to run
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
ARCH=aarch64 ./scripts/test.sh           # every section but the telnet console
```

Both third-party images are fetched for the machine `ARCH` names, so the same
nine sections run on either one. x86-64 runs a tenth, the telnet console, which
needs the network card that QEMU's `raspi4b` does not emulate. busybox.net has no aarch64 build among its
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

`reboot` at the shell restarts the machine, and `poweroff` and `halt` stop it.
On the board a restart goes back through the firmware, so a board that boots
over the network fetches its kernel again. The board's watchdog is started at
boot when the device tree describes it, and the timer interrupt feeds it, so a
kernel stuck with interrupts masked is reset after 15 seconds. A kernel panic
restarts the machine 10 seconds after its message. Two words on the kernel
command line change that:

```
panic=N          restart N seconds after a panic; 0 stays stopped, below 0 restarts at once
watchdog=off     leave the watchdog stopped, for a debugger holding the processor still
```

## The telnet console

The console the serial cable carries can also be reached over the network, on
TCP port 23 of the board's address. It is the same terminal, not a second one.
What is typed over the connection goes through the line discipline the serial
port's input goes through, so echo, backspace and `Ctrl-C` behave as they do on
the cable, and everything the console prints, kernel messages included, goes
to the serial port and to the connection both. It is not a login service, and
there are no pseudo-terminals.

**It is an unauthenticated root shell for the first client on the network to
connect.** Anyone who can reach port 23 on the board gets a root shell without
a password. That was accepted for a home network. It is not safe on a network
shared with anyone you do not trust. `telnet=off` on the kernel command line
turns it off; without that word it is on.

macOS ships no telnet client, so `scripts/console.py` is one. It needs only
Python's standard library.

```sh
scripts/console.py                        # find the board, then an interactive terminal
scripts/console.py 192.168.86.57          # the same, at the address given
scripts/console.py 192.168.86.57 --send 'uname -a' --until '^Linux'
```

Without an address it looks in the Mac's ARP table for a Raspberry Pi hardware
address, which begins `dc:a6:32`. The table holds the board once the Mac has
exchanged packets with it, which netbooting it from the Mac does. When the
board is not there the script says so and asks for the address, which the
board prints on the serial console at boot:

```
dhcp: 192.168.86.57/24 gateway 192.168.86.1 dns 192.168.86.1, lease 86400 s from 192.168.86.1
telnet: the console is on 192.168.86.57 port 23
```

With no steps it is interactive: the Mac's terminal is put in raw mode, every
key goes to the board, `Ctrl-C` included, and `Ctrl-]` quits. With steps --
`--send`, `--type`, `--until`, `--wait`, `--stall` -- it runs them in order and
prints what the board prints meanwhile, so a script can run a command on the
board and read the result. `--timestamps` puts the seconds since connecting in
front of each line, and `--help` lists the rest.

A connection is sent the kernel log first, which is what `dmesg` reads, so a
client that connects long after boot still sees how the boot went. Then the
kernel prints `telnet: console attached from` and the client's address, on the
serial port and the connection both, and live output follows. No prompt is
printed for the new connection; Enter gets one. Pushing a newline into the line
discipline would have produced a prompt, but that newline is input like any
other: at the shell it would run whatever half-typed line was on the serial
side, and in any other program it would answer a read nobody typed.

One connection at a time. Another that arrives while one is attached is sent
`telnet console busy: in use from` and the attached client's address, and is
closed. The attached connection is sent a telnet no-op at the same moment. If
its client went away without closing, as a laptop that sleeps does, TCP gives
up on it once the no-op's retransmissions run out, which on a local network
is about thirty seconds, and the next attempt gets in.

Nothing the network does can hold the console up. Output is copied to the
connection where it is written to the serial port, with interrupts masked, so
the copy only appends to a queue of 64 KiB, and the network task moves the
queue into the TCP connection. A client that stops reading lets that queue and
TCP's own 64 KiB fill. The connection is then reset rather than waited for, and
the kernel logs one line saying so. A reset, rather than output quietly
dropped, is what tells that client its output is incomplete.

It listens once the network has a configuration, from DHCP or `ip=`, and only
on a machine with a network card, so under QEMU's `raspi4b` it never starts.
Under QEMU on x86-64 a port on the Mac can be forwarded to it:

```sh
./scripts/run.sh --timeout 3600 --hostfwd tcp:127.0.0.1:2323-:23 --initrd build/initramfs.cpio
scripts/console.py 127.0.0.1 --port 2323
```

A kernel panic's message reaches the serial port and not the connection: the
machine stops before the network task can send it.

The protocol is RFC 854, with the echo option of RFC 857 and the suppressed
go-ahead of RFC 858. The kernel offers both when a client connects, which puts
the client in character mode with echo left to the board, and it parses and
drops whatever the client sends about options, subnegotiation included. CR NUL
and CR LF from the client reach the line discipline as the single CR that
Enter is on the cable. In the output, a 0xFF byte is doubled and a CR on its
own is sent as CR NUL.

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

The tables themselves are given back the same way. A last-level table covers
two megabytes, and the one an unmap leaves with nothing in it is freed, and the
ones above it while they keep emptying, so a program that maps and unmaps its
way across a wide range costs nothing that the width of the range decides.
Taking a table away throws every translation on the machine away with it: the
address translation hardware caches walks it has not finished as well as ones
it has, and the entry that named the table is in them, so invalidating the one
address the unmap removed would leave the other five hundred and eleven able to
reach a frame that has gone back to the allocator. Naming those addresses one
at a time instead was measured: a loop that empties a table on every unmap ran
five times slower on x86-64 and eighteen times slower on the emulated Pi, which
has no invalidation by range.

**Processes.** Each task owns a kernel stack, a file descriptor table, and a
page table hierarchy whose upper half is shared with the kernel. `fork` is
copy-on-write: the two sides share every writable page read-only until one of
them writes, and the fault handler hands out the private copy. `clone` with
`CLONE_VM` shares the address space outright, which is what threads use.
Scheduling is round-robin, preemptive, driven by the 100 Hz timer tick.
Anonymous memory is demand-paged: `mmap` and `brk` record a region and the
page fault handler supplies pages on first touch.

**Time.** `CLOCK_MONOTONIC` is the CPU's free-running cycle counter, so it has
real resolution rather than the 10 ms of the tick. What makes that counter a
clock is the rate it runs at, and the machine is asked for that rather than the
tick: aarch64 states it in `cntfrq_el0`, and x86-64, which states it nowhere, is
measured against a channel of the interval timer counting down. Neither answer
comes through the tick, so the tick's own length can be checked against it. The
tick still drives scheduling and timeouts.

**Random numbers.** `/dev/random`, `/dev/urandom`, `getrandom`, the `AT_RANDOM`
bytes a program's libc is handed and TCP's initial sequence numbers all come
from one generator: ChaCha20 run over a counter, keyed from an entropy pool.
A request runs the cipher from block zero, keeps the first half of the block as
the next key and hands the second half out, so the key that answered a request
is gone before the answer is. Reading the output says nothing about the state:
getting either key back from thirty-two bytes of keystream is the problem the
cipher is built to be hard at. What was there before was a xorshift, whose
state *is* its last output word, so anyone who read eight bytes of
`/dev/urandom` could compute every value it had produced and every value it
would produce, the sequence number secret among them.

Seeding is the weaker half. Where the processor has a generator of its own it
is asked -- RDSEED or RDRAND on x86-64, FEAT_RNG on aarch64 -- and its words
are mixed into the pool rather than taken as the key, so a machine with one is
no worse off than a machine without if it should turn out to be worth nothing.
The Pi 4's Cortex-A72 has no such instruction. The board does have a hardware
generator in its peripheral window, and it is not driven: QEMU's `raspi4b`
emulates nothing at that address and a read there takes an external abort, so
there is no way to try a driver for it before it meets a board.

What the rest of the seed is worth was measured rather than assumed, over 300
boots of each emulated machine, back to back:

```
                                            x86-64    Raspberry Pi 4
the processor's own generator              present            absent
the cycle counter at a fixed point        11.1 bits         8.5 bits
a 63-sample jitter loop, taken whole       9.7 bits        14.5 bits
one gap between two timer ticks            6.1 bits         5.5 bits
eight consecutive gaps                   >15.5 bits       >15.5 bits
memory the firmware left behind              0 bits           0 bits
the machine's real-time clock              8.8 bits    0 bits, has none
```

Those are collision entropy, so the minimum entropy is at least half of each;
`>15.5` means no two of the 300 boots agreed, which is all 300 samples can
show. Memory the firmware left behind returned one value across all 300 boots
on both machines, so it is not used, and a board that comes up the same way
every time will have the same nothing to offer. The tick gaps do add up: two
consecutive gaps measured twice what one did, and eight went past what 300
samples can measure. That is what makes the reseeding below worth doing.

An emulated machine is not the board. Under QEMU the cycle counter is derived
from the host's clock, so what those timing figures measure is the host's
scheduling noise; a Pi 4 reads its own counter and starts up far more
repeatably. Take the timing lines as an upper bound for the real board rather
than a measurement of it.

So the generator reseeds. Every interrupt handler folds the cycle counter into
four words with plain arithmetic -- no lock, a few nanoseconds -- and once a
second, if at least sixteen interrupts have arrived since, those four words go
into the pool along with the clock, another word from the processor's generator
where there is one, and the key the generator is using now, and the generator
takes a new key from the pool. Mixing the old key back in first is what makes a
reseed unable to make things worse. A machine that has been up for a minute has
folded in six thousand tick arrivals; one that booted a second ago has not.

On a machine with no generator of its own, the seed at the moment the first
process starts is boot timing, and is worth tens of bits rather than hundreds;
someone who knows roughly when such a machine was started can search that. It
is enough for what it is used for here: sequence numbers an off-machine
attacker cannot predict, `AT_RANDOM` bytes, a program that wants something to
vary. It is not enough to generate a long-term key with on a Pi 4 in the first
seconds of its uptime. Which of the two a machine is, it says at boot:

```
random: chacha20 seeded from boot timing and 8 words from the cpu's own generator, boot id ...
random: chacha20 seeded from boot timing alone -- this cpu has no generator, boot id ...
```

The boot id is a value derived from the pool that gives nothing about it away.
It is printed so that two boots producing one stream is something the suites
can see rather than something nobody would notice. Three hundred back-to-back
boots of the emulated Pi 4, the machine with the least to go on, produced three
hundred different ids.

The cost, measured on the emulated machines: a ChaCha block is 0.48 microseconds
on x86-64 and 0.58 on aarch64, which is what one outgoing connection's sequence
number costs and what an eight-byte `getrandom` costs. A 64 KiB read of
`/dev/urandom` costs 0.39 ms and 0.27 ms against 0.16 ms and 0.24 ms for the
xorshift, because the xorshift was being asked for eight bytes at a time and
ChaCha produces sixty-four. Seeding at boot costs 142 microseconds on both, most
of it the jitter loop. A reseed costs 8.5 and 5.3 microseconds and happens at
most once a second.

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
return address at the address the handler returns through, and `rt_sigreturn`
puts the interrupted state back. On x86-64 that address is the restorer the
program registered and there is nothing else it could be, so a disposition
that names none is refused where the signal would be delivered, which is what
Linux does there too. On aarch64 Linux does not read that field at all: it
maps a page of its own holding `mov x8, #139; svc #0` into every program and
sends a handler back through that, so a program built for that machine has no
reason to fill the field in. This kernel maps the same page, at the last page
of the half a program owns, and uses it when the field is empty. musl fills
the field in on both machines and is still sent back through what it
registered; Go fills it in on neither, and on aarch64 that page is the whole
reason a Go program survives its first signal. The page goes in read-only and
executable, and the instruction cache is told about the bytes the kernel wrote
before any program can fetch them -- the two caches are not coherent on that
machine, and leaving that out is a program executing whatever the cache was
holding, which emulation never shows.

A disposition that asked for `SA_ONSTACK` is entered on the stack
`sigaltstack` named rather than on the interrupted one, and `sigaltstack` is
answered rather than accepted and forgotten: a program that asks where its
handler would run is told, and one that has named no stack is told that. A
runtime that reads the answer back before deciding what to do needs both, and
Go is one -- its scheduler interrupts a running thread with a signal, and the
first thing its handler does is work out which stack it is standing on.

Terminal signals are raised from the interrupt that receives the
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
open, data with acknowledgements, and an orderly close, and it is written for
a link that loses packets rather than one that does not.

A segment that arrives before the bytes in front of it is held until they come
rather than thrown away, so one packet taking a different route costs nothing.
What is held is bounded by the window every held byte lies inside, by a count
of separate runs, and by a count every connection shares, so nobody can make
the kernel hold memory without end by opening connections and leaving a gap in
each; a gap that never fills is given up on and what was behind it released.
The retransmission timeout is RFC 6298's estimator over round trips that were
measured, with Karn's rule deciding which of them may be: a segment sent twice
tells you nothing, because its acknowledgement does not say which copy it
answers. A segment lost out of the middle of a stream is sent again on the
third acknowledgement that repeats rather than when the clock runs out, with
RFC 5681's congestion response beside it and RFC 6582's handling of a second
loss inside the same window. Eight megabytes each way, over a link losing one
frame in twenty in each direction, arrive byte for byte in a few seconds.

The Pi's wired port is the other driver under the same seam. Its controller is
not on a bus that can be enumerated, so the device tree is what says where it
is, which line it raises, and what hardware address the board was built with;
its descriptors live inside the device rather than in memory; its packet
buffers are cleaned out of the data cache before it reads one and invalidated
after it writes one, because nothing on that chip snoops; and its link is a
separate chip on a management bus, which has to negotiate before anything can
be sent and has to be asked what it settled on.

**Addresses.** With no `ip=` on the command line the kernel asks the network.
An RFC 2131 DHCP client in the network task sends DISCOVER and REQUEST, takes
the ACK, renews with the server that granted the lease at T1 and with any
server at T2, and gives the address up when the lease runs out. It talks
through an ordinary UDP socket on port 68; what lets that work before there is
an address is that a machine with none may send a broadcast from 0.0.0.0, and
takes nothing off the card but broadcasts. Init is held back until the first
lease or for fifteen seconds, whichever comes first, so what it starts finds
the network configured and a machine with no cable or no server still boots.
The configuration -- address, netmask, gateway, name servers -- is replaced
whole, and a machine without one holds none rather than an address of 0.0.0.0:
a socket that needs an address before then gets ENETUNREACH. The name servers
are written to `/etc/resolv.conf`.

```
dhcp: waiting up to 15 s for an address before starting /bin/init
dhcp: 10.0.2.15/24 gateway 10.0.2.2 dns 10.0.2.3, lease 86400 s from 10.0.2.2
```

`ip=192.168.86.57` sets the address instead, and DHCP does not run. `netmask=`,
`gateway=` and `nameserver=` go with it; a netmask left out is the one the
address's class implies, as on Linux, and a gateway or name server left out is
none. `nameserver=` without `ip=` replaces the name servers a lease names.
`ip=off` leaves the machine with no address and nothing asking for one.

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

**Something large that is not ours.** `make cloudflared` fetches Cloudflare's
own `cloudflared`, a forty-megabyte static Go program, and puts it in the
image. It is the largest thing here that this project did not build, and it
asks for a different half of the interface from everything else: Go brings its
own threads, its own scheduler, its own resolver and its own TLS rather than
calling a libc for any of them.

```
/ # cloudflared tunnel --protocol http2 --url http://localhost:8080
INF Requesting new quick Tunnel on trycloudflare.com...
INF |  Your quick Tunnel has been created! Visit it at:                 |
INF |  https://success-intermediate-debian-generating.trycloudflare.com |
INF Registered tunnel connection connIndex=0 location=nrt07 protocol=http2
inet: connection from 127.0.0.1:49247
```

```
$ curl -i https://success-intermediate-debian-generating.trycloudflare.com/
HTTP/2 200
server: cloudflare

hello from claudeos
```

It resolves `api.cloudflare.com` with its own resolver over this stack's UDP,
verifies the certificate against the bundle in `/etc/ssl/certs`, opens an
HTTP/2 tunnel to the edge, and proxies what arrives into the server above.

`--protocol http2` is on that command line because the default transport is
QUIC and QUIC does not work here. A QUIC sender holds one unconnected socket
and names a destination on each datagram; `sendmsg` drops that name and sends
on the descriptor, which an unconnected socket refuses, and the batching read
it pairs with, `recvmmsg`, is not implemented. Everything else cloudflared asks
for it gets.

It runs on aarch64 too. Go registers its handlers there with no restorer,
which is right for that machine, and the page described under **Signals** is
what they return through: `cloudflared --version` on the emulated Pi comes
back out of a dozen or more handlers before it prints its line. That machine
has no network device under emulation, so a tunnel cannot be established
there; what the page buys is that the program lives past its first signal.

**Console.** A 16550 UART and a PS/2 keyboard feed one input ring. A line
discipline implements canonical mode with echo, backspace, `Ctrl-C`, `Ctrl-D`
and `Ctrl-U`, and honours the `termios` settings a program sets through
`ioctl`, so raw-mode programs work too. The telnet console, described above,
is one more input to that line discipline and one more copy of its output.

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
kill sleep clear hexdump basename dirname yes true false reboot halt poweroff`.

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

- `tests/suite.sh` runs **261 checks** inside the OS, driving the shell through
  pipelines, redirection, here-documents, globbing, control flow, `case`,
  subshells, functions, file and script execution, `chmod`, devices,
  subprocesses and `/proc`. The device checks include `/dev/urandom`: that two
  reads differ and that 4 KiB of it holds nearly all 256 byte values, which is
  a check that the generator is running and not a check that it is any good.
- The `rtest` applet runs **66 checks** against the Rust standard library:
  multi-megabyte allocations, sorting two million elements, eight threads
  incrementing an atomic, a mutex shared across threads, an `mpsc` channel,
  thread sleep against the monotonic clock, the tick measured against that same
  clock to see that it lasts the hundredth of a second the kernel says it does,
  a sleep that still wakes on time while another thread sits inside a 32 MiB
  write, file read/write/seek/append, directory iteration,
  `std::process::Command` capturing a child's output through pipes, signal
  handlers running and returning, a `UnixStream` pair carrying bytes both ways,
  an epoll set woken by a counter and a socket, timing out when it should and
  waking promptly when a write arrives, and a walk of the system call numbers
  past the end of the table, every one of which has to answer ENOSYS, and
  `reboot` given magic numbers or a command it does not know, which has to
  answer EINVAL. Two of
  them run a thread alongside a sibling failing an exec over and over, which is
  a smoke test for a race rather than proof of its absence.
- The **network protocols** run against a card that only records what it is
  asked to send: **216 checks** with frames handed in by hand and frames out
  compared byte for byte. Above that sits a peer with a link in each direction
  that is told before the run what to do with each segment -- lose this one,
  hold that one back behind the next, deliver the one after twice, damage the
  one after that -- so the behaviour that only shows when packets go missing is
  checked against the same packet going missing every time. What it says: a
  reordered segment costs nothing and the sender is never asked to send it
  again; a duplicate is taken once; a damaged segment is neither taken nor
  answered; a gap that never fills is given up on and its memory given back;
  128 connections each holding a segment past a gap hold no more between them
  than one does at full stretch; the timeout comes down from its opening guess
  once a round trip has been measured and stays doubled after one that was not;
  and a segment lost out of the middle of a 32 KiB stream is recovered by the
  third acknowledgement that repeats, with the clock never waited out. The DHCP
  client runs against the same card with its clock driven by hand: a lease
  taken, renewed with its server at T1, rebound with everyone at T2 and given
  up when it runs out, each message compared byte for byte; a NAK to a request
  and to a renewal; offers whose option lengths are wrong in eight different
  ways, none of them taken; and a parser fed every truncation of a good message
  and every value of every length byte in it. On aarch64 the same run adds the
  Pi's own Ethernet driver, for 287.
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
- The **kernel's own checks** run in a boot of their own and are counted into
  one summary with the memory and protocol checks: 15 of them are the random
  number generator, of which the three that matter compare the ChaCha20 block
  function against the test vectors published with RFC 8439. A generator can
  pass every statistical test there is while being a permutation anyone can
  invert, so what is worth asserting is that this is the cipher it claims to
  be. The rest check that a request leaves behind a key that is not the bytes
  it handed out.
- **Two boots, two streams** compares the boot id from every boot above. Two
  the same would mean the seed did not vary, and every byte the generator
  handed out would be the same in both. It costs no boot of its own.
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
- The **telnet console** boots x86-64 with a port on the host forwarded to the
  guest's port 23 and drives it with `scripts/console.py`: the kernel log on a
  connection made after boot, a command, backspace editing, `Ctrl-C` ending a
  `sleep 100`, a second connection turned away as busy while the first carries
  on, a reconnection, and a client that stops reading during `seq 1 200000`,
  which has to be dropped while the output carries on to the serial port and
  the next connection works. A second boot with `telnet=off` has to bring the
  network up with nothing answering on port 23. It does not run on aarch64,
  where QEMU emulates no network card.

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
  rng.rs              ChaCha20, the entropy pool, and what seeds it
  task.rs             task control block, user stack and auxiliary vector
  sched.rs            round-robin scheduler, exit and reaping
  uaccess.rs          validated copying between kernel and user memory
  console/mod.rs      input ring and terminal line discipline
  console/telnet.rs   the same terminal over TCP port 23
  signal.rs           signal dispositions and default actions
  trap.rs             exception and interrupt handling

user/cbox             the multicall userland binary (shell, init, coreutils)
user/c/hello.c        a C program linked against musl
tools/mkcpio.py       initramfs builder
tools/drive.py        drives the console over a socket, rendering as a terminal
scripts/reap-stale.sh clears QEMU instances an earlier run left behind
scripts/mkcard.sh     assembles the boot partition for a Pi, and writes a card
scripts/console.py    a telnet client for the telnet console, interactive or scripted
scripts/lossy-transfer.sh
                      megabytes each way through the card over a lossy link
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

The random number generator is ChaCha20 and its output does not give up its
state, but on a machine whose processor has no generator of its own its seed is
boot timing worth tens of bits until reseeding has had a few seconds to work.
That is not enough to generate a key with. The Pi 4's own hardware generator is
not driven.

The TCP has no selective acknowledgement. A lost segment is found by the
acknowledgements repeating, which finds one loss per round trip, rather than by
the other end naming what it holds; on a link that loses several segments out
of one window that is slower than a modern stack, and it is still correct. It
has no window scaling, so 64 KiB is the most it can offer and throughput is
bounded on any link whose delay and bandwidth multiply out past that, and no
timestamps, so nothing guards against a sequence number wrapping and only one
round trip at a time is being measured. There is no Nagle and no delayed
acknowledgement: every segment goes as soon as there is a window for it and is
answered as soon as it arrives. IPv4 only, and fragments are dropped rather
than reassembled.

Nothing in the kernel resolves names: the name servers a lease or `nameserver=`
names are written to `/etc/resolv.conf` for programs with a resolver of their
own. The DHCP client does not probe an offered address with ARP before taking
it, does not reuse a lease after a reboot, and does not release its lease when
the machine stops. A lease is kept while the link is down, so a cable moved to
another network keeps the old address until the lease comes up for renewal.

`sendmsg` drops the address a message names and sends on the descriptor
instead, so it reaches a connected socket and nothing else: a datagram sent
through it from an unconnected socket is refused. `recvmsg` says nothing about
where what it returned came from, and `recvmmsg` is not implemented at all.
Between them that is what a QUIC implementation uses -- one unconnected socket,
a destination per datagram, a batch per read -- so QUIC does not work here.

QEMU cannot be asked to lose a packet -- its netfilters delay, dump, mirror,
redirect and rewrite, and none of them drops one, nor is there a knob for it on
the user mode network -- so `netloss=N` on the kernel command line throws one
frame in N away in each direction, below the protocols and above the card.
`scripts/lossy-transfer.sh` is what the end-to-end figures come from. At one
frame in ten in each direction, 8 MiB out of the machine still arrives whole in
four seconds; the same amount into it does not finish, and what the captures
show stalling there is the sending end's own retransmission timer on the host
rather than anything this end sends.
