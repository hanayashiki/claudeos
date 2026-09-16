//! /data: the FAT32 volume on the Raspberry Pi 4's microSD card.
//!
//! The layers, from the bottom:
//!
//! - `emmc2`: the card slot's host controller and its supplies, and how it
//!   differs from the plain SDHCI in `crate::mmc::sdhci`.
//! - `card`: the SD card's initialisation and its block commands.
//! - `card::partition`: which volume on the card is /data, and the `Partition`
//!   that is the only way to its blocks.
//! - `fat`: the kernel's own FAT32: the block cache, the boot sector, the FAT,
//!   names, directories, and the operations with the order their writes reach
//!   the card in. It uses nothing from the kernel, and `tools/fatdisk` compiles
//!   the same files on the Mac.
//! - `vfs`: the nodes /data is made of in the kernel's tree.
//!
//! **Keeping /data away from the system.** The kernel, init and the shell, and
//! what they need to reach the network and the console, come from the
//! initramfs, which the firmware has loaded before this kernel starts, so
//! nothing on the data volume is needed to boot or to reach the machine. What
//! is on the card is what a person changes by hand, and four rules follow:
//!
//! 1. Nothing about the card stops boot or a restart. Bring-up runs in a kernel
//!    task, init waits for it for at most `WAIT_SECONDS`, every wait inside it
//!    is bounded, and each way it can fail ends in one
//!    `data: /data is not mounted: ...` line. Unmounting before a restart, halt
//!    or power-off is waited for at most `UNMOUNT_SECONDS`, and the machine
//!    stops when they run out whatever the card is doing.
//! 2. Nothing about the card panics the kernel. The FAT code checks every
//!    number it reads off the card before using it, bounds every chain walk by
//!    the volume's cluster count, and returns an error instead: EIO when the
//!    card fails a command, EUCLEAN when the volume contradicts itself (see
//!    `fat/mod.rs`). `tools/fatdisk` runs the same files against damaged
//!    images, and the harness boots damaged cards.
//! 3. /data cannot take the system's memory: 4 MiB of cached blocks, 32 open
//!    files, 32 remembered directories, a listing only for a directory a
//!    descriptor is reading, and a node only for what something holds.
//! 4. A write is on the card when the system call returns, and a failed one is
//!    an error to the program. `sync` writes FSInfo and marks the volume
//!    dismounted cleanly.
//!
//! **Which volume.** The first FAT32 volume whose label is the one `data=`
//! gives, `CLAUDEDATA` when it gives none, and never one whose root holds
//! `start4.elf` or `kernel8.img`: that is a boot partition, and a `data=` that
//! names one by mistake must not make it writable. `data=off` leaves the card
//! alone.
//!
//! **/usr and /root.** Programs that are not needed to boot live on the card
//! in /data/usr, and the home directory in /data/root. Once the volume is
//! mounted, and before `data:` is printed, bring-up makes either directory
//! on the card if it is missing and replaces the empty in-memory /usr and
//! /root with symbolic links to them. It is done here rather than by init
//! because this is the one place that knows whether and when the volume was
//! mounted, including a mount that ends after init has been started, and so
//! every boot gets the same layout whatever program is init. Links rather
//! than a second mount of a subdirectory, because the tree already resolves
//! links, the program loader included, so a PT_INTERP of
//! /lib/ld-musl-aarch64.so.1 that links to /usr/lib reaches the card with no
//! code of its own, and `rmdir` or `mv` of /data/usr needs no refusal: the link
//! is left dangling, as on Linux. A directory that cannot be made, or a file
//! in its place, leaves that one in memory, and the `data:` line says so.

mod card;
mod emmc2;
mod fat;
pub mod vfs;

use crate::arch;
use crate::arch::paging::AddressSpace;
use crate::mmc::delay::{now_us, sleep_ms, Deadline};
use crate::sched;
use crate::sched::WaitQueue;
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// The label looked for when the command line names none.
pub const DEFAULT_LABEL: &str = "CLAUDEDATA";

/// How long init is held back for the volume. A card that answers is mounted
/// in about a second; this leaves room for a slow card, and for a damaged root
/// directory that is scanned to its end before it is refused.
const WAIT_SECONDS: u64 = 20;

/// How long a restart, halt or power-off waits for /data to be unmounted. The
/// writes are FSInfo, FAT[1] in each copy and one byte of the boot sector,
/// tens of milliseconds on a card that answers, and counting the free clusters
/// first when no operation has counted them, which on a 32 GiB card of 32 KiB
/// clusters is a read of about 8 MiB of FAT.
const UNMOUNT_SECONDS: u64 = 5;

/// The label, handed from boot to the task.
static LABEL: Spinlock<Option<String>> = Spinlock::new(None);
/// Set when bring-up has finished, whichever way it ended.
static SETTLED: AtomicBool = AtomicBool::new(false);

/// A request to the storage task to unmount, and how it went.
static UNMOUNT_ASKED: AtomicBool = AtomicBool::new(false);
static UNMOUNT_QUEUE: WaitQueue = WaitQueue::new();
static UNMOUNT_OUTCOME: AtomicU8 = AtomicU8::new(PENDING);
const PENDING: u8 = 0;
const UNMOUNTED: u8 = 1;
const UNMOUNT_FAILED: u8 = 2;

/// What `data=` asks for: a label, or nothing for `data=off`. The last such
/// word before `--` counts, as for every other word the kernel reads.
fn requested(cmdline: &str) -> Result<Option<String>, String> {
    let mut value = None;
    for word in cmdline.split_whitespace() {
        if word == "--" {
            break;
        }
        if let Some(given) = word.strip_prefix("data=") {
            value = Some(given);
        }
    }
    match value {
        None => Ok(Some(String::from(DEFAULT_LABEL))),
        Some("off") => Ok(None),
        Some(label) if (1..=11).contains(&label.len()) && label.bytes().all(|b| b.is_ascii_graphic()) => {
            Ok(Some(String::from(label)))
        }
        Some(label) => Err(format!("data={} is not a FAT volume label, which is 1 to 11 printable characters", label)),
    }
}

/// Start looking for /data and wait, for a while, for it to be found. Called
/// from the boot context once init has its pid.
pub fn start(cmdline: &str, init: &str) {
    let label = match requested(cmdline) {
        Ok(Some(label)) => label,
        Ok(None) => {
            crate::println!("data: off, from the command line; the card is left alone");
            return;
        }
        Err(why) => {
            crate::println!("data: /data is not mounted: {}", why);
            return;
        }
    };
    *LABEL.lock() = Some(label);
    let Some(mut task) = Task::new("sdcard", AddressSpace::current()) else {
        crate::println!("data: /data is not mounted: no memory for the storage task");
        return;
    };
    task.prepare_kernel_frame(task_main as extern "C" fn() -> ! as usize as u64);
    sched::register(task);

    let deadline = crate::trap::ticks() + WAIT_SECONDS * arch::TICK_HZ as u64;
    if !sched::idle_until(deadline, || SETTLED.load(Ordering::Relaxed)) {
        crate::println!(
            "data: the card is still being read after {} s; starting {} now, and /data appears if it is mounted",
            WAIT_SECONDS,
            init
        );
    }
}

extern "C" fn task_main() -> ! {
    let label = LABEL.lock().take().unwrap_or_else(|| String::from(DEFAULT_LABEL));
    let started = now_us();
    // What bring-up learns about the controller and the card goes on the end
    // of the one line either outcome prints, so that a card that fails still
    // says what it is.
    let mut found = Vec::new();
    let line = match bring_up(&label, &mut found) {
        Ok((what, layout)) => format!("mounted {} at /data, in {} ms; {}", what, (now_us() - started) / 1000, layout),
        Err(why) => format!("/data is not mounted: {}", why),
    };
    if found.is_empty() {
        crate::println!("data: {}", line);
    } else {
        crate::println!("data: {}; {}", line, found.join("; "));
    }
    SETTLED.store(true, Ordering::Relaxed);
    // What is left for this task is unmounting when the machine is about to
    // stop. It is done here rather than in the task that asks, so that the
    // asking task can stop waiting when the card does not answer.
    loop {
        UNMOUNT_QUEUE.wait_until(|| UNMOUNT_ASKED.load(Ordering::Relaxed));
        UNMOUNT_ASKED.store(false, Ordering::Relaxed);
        let outcome = if vfs::sync().is_ok() { UNMOUNTED } else { UNMOUNT_FAILED };
        UNMOUNT_OUTCOME.store(outcome, Ordering::Relaxed);
    }
}

/// Unmount /data before the machine stops: FSInfo written and the volume
/// marked dismounted cleanly, by the storage task, waited for at most
/// `UNMOUNT_SECONDS`. Called by `reboot` for restart, halt and power-off, and
/// never on the panic path. Returns why the volume is left marked in use.
pub fn unmount() -> Result<(), String> {
    if !vfs::mounted() {
        return Ok(());
    }
    UNMOUNT_OUTCOME.store(PENDING, Ordering::Relaxed);
    UNMOUNT_ASKED.store(true, Ordering::Relaxed);
    UNMOUNT_QUEUE.wake_all();
    let deadline = Deadline::after_ms(UNMOUNT_SECONDS * 1000);
    loop {
        match UNMOUNT_OUTCOME.load(Ordering::Relaxed) {
            UNMOUNTED => return Ok(()),
            UNMOUNT_FAILED => {
                return Err(String::from("unmounting /data failed, so the volume is left marked in use; stopping anyway"))
            }
            _ => {}
        }
        if deadline.expired() {
            return Err(format!("/data was not unmounted within {} s, so the volume is left marked in use; stopping anyway", UNMOUNT_SECONDS));
        }
        sleep_ms(10);
    }
}

/// What a failure of the FAT code says about the volume, for the bring-up line.
fn failure(error: fat::FsError) -> &'static str {
    match error {
        fat::FsError::Io => "the card failed a read",
        fat::FsError::Corrupt => "its structures are damaged",
        _ => "an operation on it failed",
    }
}

/// Mount the volume and put /usr and /root on it. Returns what was mounted, and
/// where /usr and /root are.
fn bring_up(label: &str, found: &mut Vec<String>) -> Result<(String, String), String> {
    let mut controller = emmc2::find()?;
    let (mut host, from_caps, from_firmware) = controller.host()?;
    found.push(format!(
        "EMMC2 at {:#x}, from {}, base clock {} Hz from the capabilities register and {} from the firmware, supplies: {}",
        controller.phys,
        controller.source,
        from_caps,
        match from_firmware {
            Some(rate) => format!("{} Hz", rate),
            None => String::from("nothing"),
        },
        controller.supplies()
    ));
    host.init().map_err(|e| format!("resetting the EMMC2 controller: {}", e))?;

    let card = card::attach(host, &mut |on| controller.supply(on))?;
    let id = card.identity;
    let product: String = id.product.iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
    found.push(format!(
        "card from manufacturer {:#04x}, product {}, {} MiB, {}, {} Hz{}, {} data lines",
        id.manufacturer,
        product,
        id.blocks / 2048,
        if id.block_addressed { "block addressed" } else { "byte addressed" },
        id.clock,
        if id.high_speed { " in high speed" } else { "" },
        if id.four_bit { 4 } else { 1 }
    ));
    if id.write_protected {
        return Err(String::from("the card says it is write-protected"));
    }

    let chosen = card::choose(card, label)?;
    let layout = chosen.probe.layout;
    let found_label = chosen.probe.label();
    let what = chosen.what.clone();
    let device = match chosen.slot {
        Some(slot) => format!("/dev/mmcblk0p{}", slot),
        None => String::from("/dev/mmcblk0"),
    };
    let mut volume = fat::Volume::mount(chosen.partition, layout, crate::time::unix_time)
        .map_err(|e| format!("{} is labelled {} but could not be mounted: {}", what, found_label, failure(e)))?;

    // Looked at before anything may be written: the volume refuses writes
    // until `allow_writes`.
    match volume.boot_file() {
        Ok(Some(name)) => {
            return Err(format!(
                "{} is labelled {} but its root holds {}, so it is a boot partition, and it is left alone",
                what, found_label, name
            ))
        }
        Ok(None) => {}
        Err(e) => return Err(format!("the root directory of {} could not be listed: {}", what, failure(e))),
    }
    let unclean = volume.dirty_at_mount();
    volume.allow_writes();
    let on_card: Vec<(&str, Result<(), &str>)> = ON_CARD.iter().map(|&name| (name, card_directory(&mut volume, name))).collect();
    vfs::publish(volume, device);
    let mut linked = Vec::new();
    let mut in_memory = Vec::new();
    for (name, outcome) in on_card {
        match outcome {
            Ok(()) => {
                vfs::link_to_card(name);
                linked.push(format!("/{} is a link to /data/{}", name, name));
            }
            Err(why) => in_memory.push(format!("/{} stays in memory: /data/{} {}", name, name, why)),
        }
    }
    linked.extend(in_memory);
    Ok((
        format!(
            "{} of the card, labelled {}, {} MiB in {} clusters of {} bytes{}",
            what,
            found_label,
            layout.sectors / 2048,
            layout.clusters,
            layout.cluster_bytes,
            if unclean {
                ", which FAT[1] says was not dismounted cleanly and stays marked so until fsck_msdos repairs it"
            } else {
                ""
            }
        ),
        linked.join("; "),
    ))
}

/// The directories of the root filesystem kept on the card, as their names
/// under / and at the root of the volume.
const ON_CARD: [&str; 2] = ["usr", "root"];

/// Make the directory `name` at the root of the volume unless it is there,
/// and say why it cannot be used when it is not a directory by the end.
fn card_directory(volume: &mut fat::Volume<card::Partition>, name: &str) -> Result<(), &'static str> {
    let made = match volume.lookup("", name) {
        Ok(entry) if entry.is_dir => return Ok(()),
        Ok(_) => return Err("is a file"),
        Err(fat::FsError::NotFound) => volume.mkdir("", name).map(|_| ()),
        Err(e) => Err(e),
    };
    made.map_err(|e| match e {
        fat::FsError::Io => "could not be made: the card failed a command",
        fat::FsError::Corrupt => "could not be made: the volume's structures are damaged",
        fat::FsError::NoSpace => "could not be made: the volume is full",
        _ => "could not be made",
    })
}
