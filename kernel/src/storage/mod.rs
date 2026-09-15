//! /data: the FAT32 volume on the Raspberry Pi 4's microSD card.
//!
//! The layers, from the bottom:
//!
//! - `emmc2`: the card slot's host controller and its supplies, and how it
//!   differs from the plain SDHCI in `crate::mmc::sdhci`.
//! - `card`: the SD card's initialisation and its block commands.
//! - `card::partition`: which volume on the card is /data, and the `Partition`
//!   that is the only way to its blocks.
//! - `disk`: the cache, the byte stream rust-fatfs reads, and each operation's
//!   budget.
//! - `volume`: rust-fatfs, the checks made before it is trusted, and what it
//!   lacks.
//! - `vfs`: the nodes /data is made of in the kernel's tree.
//!
//! **Keeping /data away from the system.** The kernel and everything it runs
//! come from the initramfs, which the firmware has loaded before this kernel
//! starts, so nothing on the data volume is needed to boot or to run. What is
//! on the card is what a person changes by hand, and four rules follow:
//!
//! 1. Nothing about the card stops boot. Bring-up runs in a kernel task, init
//!    waits for it for at most `WAIT_SECONDS`, every wait inside it is bounded,
//!    and each way it can fail ends in one `data: /data is not mounted: ...`
//!    line.
//! 2. Nothing about the card panics the kernel. The boot sector is checked
//!    before rust-fatfs sees it; rust-fatfs is built without overflow checks,
//!    so a damaged number wraps rather than panicking (see kernel/Cargo.toml);
//!    and every read and write rust-fatfs asks for is checked against the
//!    volume's length and charged to a budget, so a looping cluster chain
//!    ends. `tools/fatdisk` runs the same code against damaged images, and the
//!    harness boots damaged cards.
//! 3. /data cannot take the system's memory: 4 MiB of cached blocks, 32 open
//!    files, 32 remembered directories, a listing only for a directory a
//!    descriptor is reading, and a node only for what something holds.
//! 4. A write is on the card when the system call returns, and a failed one is
//!    an error to the program. `fsync` puts the directory entry there too, and
//!    `sync` leaves the volume marked clean.
//!
//! **Which volume.** The first FAT32 volume whose label is the one `data=`
//! gives, `CLAUDEDATA` when it gives none, and never one whose root holds
//! `start4.elf` or `kernel8.img`: that is a boot partition, and a `data=` that
//! names one by mistake must not make it writable. `data=off` leaves the card
//! alone.

mod card;
pub mod disk;
mod emmc2;
pub mod vfs;
pub mod volume;

use crate::arch;
use crate::arch::paging::AddressSpace;
use crate::mmc::delay::{now_us, sleep_ms};
use crate::sched;
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// The label looked for when the command line names none.
pub const DEFAULT_LABEL: &str = "CLAUDEDATA";

/// How long init is held back for the volume. A card that answers is mounted
/// in about a second; this leaves room for a slow card, and for the budget a
/// damaged root directory can use up while it is checked.
const WAIT_SECONDS: u64 = 20;

/// The label, handed from boot to the task.
static LABEL: Spinlock<Option<String>> = Spinlock::new(None);
/// Set when bring-up has finished, whichever way it ended.
static SETTLED: AtomicBool = AtomicBool::new(false);

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
        Ok(what) => format!("mounted {} at /data, in {} ms", what, (now_us() - started) / 1000),
        Err(why) => format!("/data is not mounted: {}", why),
    };
    if found.is_empty() {
        crate::println!("data: {}", line);
    } else {
        crate::println!("data: {}; {}", line, found.join("; "));
    }
    SETTLED.store(true, Ordering::Relaxed);
    loop {
        sleep_ms(60_000);
    }
}

fn bring_up(label: &str, found: &mut Vec<String>) -> Result<String, String> {
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
    let geometry = chosen.probe.geometry;
    let found_label = chosen.probe.label();
    let what = chosen.what.clone();
    let device = match chosen.slot {
        Some(slot) => format!("/dev/mmcblk0p{}", slot),
        None => String::from("/dev/mmcblk0"),
    };
    let state = disk::share(chosen.partition);
    let clock = volume::Clock { now: crate::time::unix_time };
    let mut volume = volume::Volume::mount(state, geometry, clock)
        .map_err(|e| format!("{} is labelled {} but rust-fatfs could not mount it: {:?}", what, found_label, e))?;

    // Looked at before anything may be written: the volume is read-only until
    // `allow_writes`, and dropping it on the way out writes nothing.
    match volume.boot_file() {
        Ok(Some(name)) => {
            return Err(format!(
                "{} is labelled {} but its root holds {}, so it is a boot partition, and it is left alone",
                what, found_label, name
            ))
        }
        Ok(None) => {}
        Err(e) => return Err(format!("the root directory of {} could not be listed: {:?}", what, e)),
    }
    volume.allow_writes();
    vfs::publish(volume, device);
    Ok(format!(
        "{} of the card, labelled {}, {} MiB in {} clusters of {} bytes",
        what,
        found_label,
        geometry.sectors / 2048,
        geometry.clusters,
        geometry.cluster_blocks as u64 * 512
    ))
}
