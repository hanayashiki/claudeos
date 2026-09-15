//! macOS's own FAT tools, the reference the kernel's FAT code is checked
//! against: newfs_msdos makes volumes, fsck_msdos checks them, and hdiutil
//! attaches and mounts image files. None of them needs root.
//!
//! newfs_msdos refuses a plain file ("Cannot get partition offset"), so an
//! image is attached as a disk first. Every disk this file formats is checked
//! to be a disk image before newfs_msdos runs on it.

use crate::image::{write_at, MIB, SECTOR};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|e| format!("{:?}: {}", command, e))?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!("{:?} failed: {}", command, text.trim()))
    }
}

fn detach(disk: &str) {
    for attempt in 0..10 {
        let mut command = Command::new("hdiutil");
        command.arg("detach").arg(disk);
        if attempt >= 3 {
            command.arg("-force");
        }
        if command.output().is_ok_and(|output| output.status.success()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// An image file attached as a disk, detached when dropped.
pub struct Attached {
    pub disk: String,
}

impl Attached {
    /// Attach `image` without mounting anything on it, and check that the
    /// disk it became says it is a disk image.
    pub fn new(image: &Path) -> Result<Attached, String> {
        let out = run(Command::new("hdiutil").args(["attach", "-nomount", "-imagekey", "diskimage-class=CRawDiskImage"]).arg(image))?;
        let disk = out.lines().next().and_then(|line| line.split_whitespace().next()).unwrap_or("").to_string();
        if !disk.starts_with("/dev/disk") {
            return Err(format!("hdiutil did not attach {}: {}", image.display(), out));
        }
        let attached = Attached { disk };
        let info = run(Command::new("diskutil").args(["info", &attached.disk]))?;
        if !info.lines().any(|line| line.contains("Protocol:") && line.contains("Disk Image")) {
            return Err(format!("{} does not say it is a disk image; nothing was written", attached.disk));
        }
        Ok(attached)
    }

    /// The raw device of the whole disk, or of MBR slot `slot` counted from 1.
    pub fn raw(&self, slot: Option<usize>) -> String {
        let name = self.disk.trim_start_matches("/dev/");
        match slot {
            Some(slot) => format!("/dev/r{}s{}", name, slot),
            None => format!("/dev/r{}", name),
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        detach(&self.disk);
    }
}

/// A sparse image file of `mib` MiB.
pub fn create_image(image: &Path, mib: u64) -> Result<(), String> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(image).map_err(|e| format!("{}: {}", image.display(), e))?;
    file.set_len(mib * MIB).map_err(|e| format!("{}: {}", image.display(), e))
}

/// The newfs_msdos arguments for a FAT32 volume of `sectors` sectors: one
/// sector per cluster when the default would give fewer than the 65525
/// clusters FAT32 needs, which is below about 260 MiB.
fn newfs_args(label: &str, sectors: u64, cluster_sectors: Option<u32>) -> Vec<String> {
    let mut args = vec![String::from("-F"), String::from("32"), String::from("-v"), label.to_string()];
    let cluster = cluster_sectors.or(if sectors * SECTOR < 260 * MIB { Some(1) } else { None });
    if let Some(cluster) = cluster {
        args.push(String::from("-c"));
        args.push(cluster.to_string());
    }
    args
}

/// A new image of `mib` MiB holding one FAT32 volume across it, made by
/// newfs_msdos.
pub fn newfs_whole(image: &Path, mib: u64, label: &str, cluster_sectors: Option<u32>) -> Result<(), String> {
    create_image(image, mib)?;
    let attached = Attached::new(image)?;
    run(Command::new("newfs_msdos").args(newfs_args(label, mib * MIB / SECTOR, cluster_sectors)).arg(attached.raw(None)))?;
    drop(attached);
    Ok(())
}

/// A new image of `mib` MiB with an MBR and one FAT32 partition per entry of
/// `parts`, a label and a size in MiB, `None` for the rest of the image. The
/// first starts at 1 MiB, as partitioning tools put it. Returns each
/// partition's first sector and length.
pub fn newfs_mbr(image: &Path, mib: u64, parts: &[(String, Option<u64>)]) -> Result<Vec<(u64, u64)>, String> {
    if parts.len() > 4 {
        return Err(String::from("an MBR has four entries"));
    }
    create_image(image, mib)?;
    let total = mib * MIB / SECTOR;
    let mut mbr = [0u8; 512];
    let mut next = 2048u64;
    let mut spans = Vec::new();
    for (slot, (_, size)) in parts.iter().enumerate() {
        let length = match size {
            Some(size) => size * MIB / SECTOR,
            None => total.saturating_sub(next),
        };
        if length == 0 || next + length > total {
            return Err(String::from("the partitions do not fit"));
        }
        let entry = &mut mbr[446 + slot * 16..462 + slot * 16];
        entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        entry[4] = 0x0C;
        entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        entry[8..12].copy_from_slice(&(next as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(length as u32).to_le_bytes());
        spans.push((next, length));
        next = (next + length).div_ceil(2048) * 2048;
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let mut file = OpenOptions::new().write(true).open(image).map_err(|e| e.to_string())?;
    write_at(&mut file, 0, &mbr).map_err(|e| e.to_string())?;
    drop(file);
    let attached = Attached::new(image)?;
    for (slot, ((label, _), (_, length))) in parts.iter().zip(spans.iter()).enumerate() {
        let device = attached.raw(Some(slot + 1));
        // The slice device appears a moment after the disk.
        for _ in 0..50 {
            if Path::new(&device).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        run(Command::new("newfs_msdos").args(newfs_args(label, *length, None)).arg(&device))?;
    }
    drop(attached);
    Ok(spans)
}

/// What fsck_msdos -n said about the volume in `volume_file`, a file holding
/// only that volume: its exit status and its output.
pub fn fsck(volume_file: &Path) -> (i32, String) {
    match Command::new("fsck_msdos").arg("-n").arg(volume_file).output() {
        Ok(output) => {
            let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            (output.status.code().unwrap_or(-1), text)
        }
        Err(e) => (-1, format!("fsck_msdos: {}", e)),
    }
}

/// The lines of fsck_msdos output that report something: everything but the
/// file name, the phase headings and the closing count of files and free
/// space.
pub fn findings(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("** "))
        .filter(|line| !(line.starts_with("Warning: ") && line.contains(" files, ") && line.contains(" free (")))
        .map(String::from)
        .collect()
}

/// An image mounted by macOS, detached when dropped.
pub struct Mounted {
    pub point: PathBuf,
    disk: String,
}

/// Mount the FAT volume across `image` at `point`, read-only or not, without
/// it appearing in the Finder.
pub fn mount(image: &Path, point: &Path, readonly: bool) -> Result<Mounted, String> {
    std::fs::create_dir_all(point).map_err(|e| e.to_string())?;
    let mut command = Command::new("hdiutil");
    command.args(["attach", "-nobrowse", "-imagekey", "diskimage-class=CRawDiskImage", "-mountpoint"]).arg(point);
    if readonly {
        command.arg("-readonly");
    }
    let out = run(command.arg(image))?;
    let disk = out.lines().next().and_then(|line| line.split_whitespace().next()).unwrap_or("").to_string();
    if !disk.starts_with("/dev/disk") {
        return Err(format!("hdiutil did not mount {}: {}", image.display(), out));
    }
    Ok(Mounted { point: point.to_path_buf(), disk })
}

impl Drop for Mounted {
    fn drop(&mut self) {
        detach(&self.disk);
    }
}
