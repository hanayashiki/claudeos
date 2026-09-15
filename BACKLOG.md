# Backlog

Known problems and open decisions that are not being worked on. Each entry says
what is wrong, where, and what a fix involves. Remove an entry when it is fixed
or decided.

## Paths, arguments and environment are text, where Linux has bytes

Linux hands a program's paths, `argv` and `envp` to the kernel as
NUL-terminated byte strings: any byte except NUL is allowed, and `/` separates
path components. The kernel never asks whether they are UTF-8. claudeos does,
in two layers.

**Kernel.**
- `uaccess::read_cstr` (kernel/src/uaccess.rs) ends with
  `String::from_utf8(out).map_err(|_| Errno::EINVAL)`. Every path system call
  in kernel/src/syscall/file.rs reads its path through it (open, stat, mkdir,
  symlink targets and the rest), so a path that is not valid UTF-8 fails with
  EINVAL where Linux accepts it.
- `execve` reads `argv` and `envp` through `task::read_string_array` into
  `Vec<String>`, with the same result.
- The in-memory filesystem names entries with `String`
  (`children: BTreeMap<String, NodeRef>` and `DirEntry::name` in
  kernel/src/fs/mod.rs), and its lookups take `&str`.

**cbox.**
- user/cbox/src/main.rs converts its arguments with `to_string_lossy`, so a
  byte that is not UTF-8 becomes U+FFFD before the kernel sees it.
- user/cbox/src/shell.rs reads scripts and non-terminal input with
  `String::from_utf8_lossy`, and word expansion, variables and command
  substitution all work on `String`.
- The system call wrappers in user/cbox/src/sys.rs take `&str` and `String`
  (`open`, `chdir`, `execve`, `statfs`).
- The data tools (cat, grep, tr, sed and the line tools) already move bytes
  without deciding they are text.
- The line editor (user/cbox/src/edit.rs) inserted each typed byte as its own
  `char`, which double-encoded non-ASCII input. It is being changed to hold
  bytes, but still hands the shell a `String` through `from_utf8_lossy` until
  the shell takes bytes.

**Effect.** Names that are not UTF-8 (tar and zip archives with Shift-JIS or
Latin-1 names, test suites that use arbitrary bytes on purpose, binary values
in the environment) cannot be created, opened, listed or passed as arguments.
Everything run so far uses UTF-8, which is why it has not shown up.

**Fix.**
1. Kernel first, since it defines the contract. `read_cstr` returns bytes, a
   byte-path type replaces `&str` in the path functions, and filesystem entry
   names, `argv` and `envp` become byte strings. Text remains only where text
   is required:
   - the kernel log, which prints bytes lossily without changing them;
   - FAT long names on /data, which are UTF-16 on the card, so a name that is
     not UTF-8 gets EINVAL there, as Linux's vfat does with `utf8`.
2. Then cbox: `OsString`/`OsStr` and `Vec<u8>` for arguments, environment,
   shell words, variables and paths, and system call wrappers that take
   `&[u8]`.

Tests should create, list, execute and pass as arguments names that hold
bytes such as 0xff, on both machines.

## WiFi robustness (kernel/src/net/wifi/mod.rs)

- **No recovery when the chip stops answering.** Polling failures are printed
  five times and then ignored; there is no chip reset or firmware reload. The
  watchdog does not catch it, because the kernel keeps taking the tick.
- **Firmware roaming is unhandled.** Roaming is neither disabled nor followed.
  On a mesh with several access points of one name, a roam would need a new
  handshake. While the link is up the driver does not scan, so it stays with
  the access point it joined until that one drops the link.
- **Group key rekey unobserved.** It has only been exercised by the self test,
  never seen on the board: the router did not rekey in about two hours.

## Networking

- **WiFi needs `net=wifi`.** Without it the wired port always takes DHCP and
  the default route, even with no cable, and WiFi never starts, because the
  stack holds one interface. `scripts/mkcard.sh` writes the word when the
  board image holds `/etc/wifi.conf`, so only a hand-written command line
  can miss it.
- **README says QUIC does not work.** README.md says QUIC fails because
  `sendmsg` drops the destination. `sendmsg` and `recvmmsg` have since been
  fixed, and cloudflared's own pre-check reported a successful QUIC connection
  on the board. Check with `--protocol quic` and update the README.

## Kernel

- **x86-64 page faults run with interrupts masked** for the whole handler,
  because every IDT entry is an interrupt gate and nothing re-enables them.
  Linux reads CR2 first, then re-enables interrupts if the faulting code had
  them on. aarch64 already does the equivalent, and the copy-on-write repair
  masks explicitly either way.
- **`fork` does not copy the parent's signal mask**; Linux does.
- **Carried over from the 2026-09-14 notes, not re-checked since:**
  - `openat(AT_FDCWD, "")` returns the current directory, where Linux returns
    ENOENT;
  - `mmap` ignores a hint below `USER_MMAP_BASE`;
  - `nanosleep` rounds up to the 100 Hz tick;
  - `CLONE_SIGHAND` copies the signal disposition table instead of sharing it;
  - one kernel fault under heavy forking, never reproduced.

## Tests

- **"a stopped job shows T" failed once** (tests/suite.sh, x86-64, main
  cd228b8, 2026-09-16), while another agent's QEMU runs shared the Mac. The
  rerun passed. The check runs `kill -STOP` on a sleeping job and then `ps` at
  once. The stop takes effect when the stopped task next runs, as it does on
  Linux (`do_signal_stop` runs in the task being stopped), so `ps` can read the
  state before that. The check should wait for the state, for a bounded time,
  rather than read it once.

## Telnet console

- **Freeing the slot is untested on the board.** A client that vanished
  without closing should give up the slot about 30 s after a second client is
  refused.
- **A long WiFi outage is untested.** An idle session stayed connected across
  a 6.9 s loss on the board (2026-09-16). An outage longer than TCP's roughly
  30 s of retransmissions would likely drop a session that sends during it;
  that has not been tried.

## Decided, not yet done

- **Boot services from two lists, both read once at boot (no start or stop
  while running).** Being built (2026-09-16).
  - **The system list** is in the image, covered by the boot checksums, and
    never edited on the board. It holds the time keeper, and cloudflared when
    the system runs one.
  - **The user list** is on /data and edited freely, from the Mac or the board.
  - Both have one line per service: a name, a restart policy (`always`,
    `once` or `off`), and a command.
  - Each service runs in its own process group, with a size-capped log in
    /var/log and a status file.
  - A missing, unmounted or malformed user list is logged and skipped, and
    never stops boot or the system services.
  - Open detail: whether programs on /data may be executed, since FAT has no
    execute bit and files there are reported as 0644.
  - The aarch64 busybox (Alpine's busybox-static) has no httpd; Alpine moved
    it to busybox-extras, which is linked against musl's loader. Both are
    being added to the aarch64 images, pinned and checksummed.

## Open decisions

- **When init dies.** Keep the kernel up instead of powering off.
- **A restart key.** A console key sequence that restarts the board with no
  working shell, like Linux's SysRq.
- **Starting cloudflared from init.** A quick tunnel's URL changes on every
  start; a fixed URL needs a named tunnel and a Cloudflare account.

## Over-the-air updates of the card, so no card reader is needed

**Goal.** Change the boot files on the card over the network, and prepare a
new card without a card reader.

**Why not overwrite the boot files.** A power cut or a bad build during an
update can leave the card unbootable. In production there is no Mac to fall
back to, so that means a card reader again.

**Mechanism.** Raspberry Pi's bootloader has A/B booting built in:
- `autoboot.txt` in the first FAT partition sets `boot_partition` under
  `[all]`, and another under `[tryboot]`.
- The `[tryboot]` choice applies only when the one-shot tryboot flag is set.
- Raspberry Pi's Linux sets that flag with firmware property tag
  `RPI_FIRMWARE_SET_REBOOT_FLAGS` (0x00038064), value 1, and then resets through
  the watchdog.
- The bootloader clears the flag on the next normal reboot.
- `bcm2835_wdt.c` shows the partition to boot being passed in PM_RSTS bits 0,
  2, 4, 6, 8 and 10.
- The bootloader log already reports `[sdcard] autoboot.txt not found`, so it
  looks for the file.

**Card layout,** four MBR primary partitions:
1. a few MiB of FAT, holding only `autoboot.txt`;
2. boot copy A: firmware, kernel8.img, the board image, cmdline.txt;
3. boot copy B;
4. /data, the rest of the card.

**Update flow.**
1. The Mac builds a complete boot-partition image with newfs_msdos, plus a
   checksum and a signature, so the Pi only copies bytes and never formats.
2. The Pi downloads the image and verifies it.
3. It writes the image to the copy not in use, then reads it back and verifies
   that.
4. It sets the tryboot flag and restarts.
5. If the new system comes up healthy (boot checksums ok, network up, services
   running), it rewrites `autoboot.txt` to make the new copy the default.
6. If the new system panics or hangs, the panic restart or the watchdog does a
   normal reset, which clears the flag, so the old copy boots.

**First card without a reader.** Boot the board over the network with the card
in it, and run a one-time `provision` command. It writes the partition table,
`autoboot.txt` and the first boot copy, and asks for typed confirmation, since
it erases the card.

**Pieces.**
- **Kernel:**
  - a path that writes whole boot partitions, kept separate from /data's
    filesystem. /data still cannot address the boot partitions, and the guard
    that refuses to mount a boot partition as /data stays.
  - the tryboot flag and partition selection in `arch::restart`.
- **Board:** an update program, which Go would give TLS and ed25519 for free.
- **Mac:** a script that builds and signs the partition image, replacing
  mkcard.sh's file copying for updates.
- **Tests:** in QEMU with a four-partition card image, then on the board.

**Still to check:**
- whether `autoboot.txt` must be in partition 1, or in any first FAT partition;
- the minimum bootloader version for `[tryboot]` in autoboot.txt;
- how a bare-metal kernel's firmware property call for the reboot flag
  interacts with the watchdog restart.

## Raspberry Pi firmware is fetched unpinned

scripts/mkcard.sh fetches `start4.elf`, `fixup4.dat`, `bcm2711-rpi-4-b.dtb` and
`overlays/disable-bt.dtbo` from `raw/master/boot` of
github.com/raspberrypi/firmware, with no commit and no checksum. It caches the
first download in build/thirdparty/firmware (the copy in use is dated
2026-09-13, firmware build a089929a of Sep 11 2026). So a fresh checkout gets
whatever master holds that day, and a changed or damaged download goes unnoticed.
scripts/mkeeprom.sh already pins a commit and checks a sha256 for each file;
mkcard.sh should do the same. The kernel's boot checksums cannot cover these
files: the GPU firmware loads them before the kernel runs.

## Boot time

- **About 2.5 minutes from reset to shell over WiFi TFTP,** most of it
  downloading the 40 MB image, which is mostly cloudflared. Options:
  `start4cd.elf` (about 0.8 MB instead of 2.3 MB), a smaller or compressed
  initramfs, or the Mac on Ethernet.
