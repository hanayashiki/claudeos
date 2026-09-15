//! fatdisk: SD card images for testing /data, made and read on the Mac without
//! root.
//!
//! ```text
//! fatdisk mbr IMAGE MIB PART...       an MBR card of MIB MiB with one FAT32
//!                                     partition per PART, LABEL:MIB or
//!                                     LABEL:rest, and +boot after either to
//!                                     put a boot partition's files in it
//! fatdisk whole IMAGE MIB LABEL[+boot] one FAT32 volume across the whole card
//! fatdisk fill IMAGE LABEL            the content the damage tests start from
//! fatdisk put IMAGE LABEL PATH TEXT   write TEXT to PATH in the volume
//! fatdisk cat IMAGE LABEL PATH        print a file
//! fatdisk ls IMAGE LABEL PATH         list a directory, directories with a /
//! fatdisk check IMAGE LABEL           read everything, and look for clusters
//!                                     two files share
//! fatdisk span IMAGE LABEL            the volume's first sector and length
//! fatdisk damage IMAGE LABEL MODE SEED [COUNT]
//!                                     boot: random bytes over the boot sector
//!                                     loops: a directory and a file whose
//!                                       cluster chains loop (after fill)
//!                                     cycle: www/css pointed at the root
//!                                       directory (after fill)
//!                                     meta: COUNT random bytes over the boot
//!                                       sector, the FATs and the root
//!                                     random: COUNT random bytes anywhere in
//!                                       the image
//! fatdisk fuzz FIRST LAST             the kernel's volume code against damaged
//!                                     images, one per seed, in this process
//! fatdisk walk IMAGE LABEL            walk the volume as `find` does, through
//!                                     the kernel's volume code, and time it
//! ```
//!
//! Every image is a sparse file the size given, so QEMU can take it as a card:
//! QEMU wants a card's size to be a power of two.

extern crate alloc;

// The kernel's volume code, compiled here as it is. `volume.rs` reaches
// `disk.rs` as `super::disk`, which at the root of this crate is this module.
#[allow(dead_code)]
#[path = "../../../kernel/src/storage/disk.rs"]
mod disk;
#[allow(dead_code)]
#[path = "../../../kernel/src/storage/volume.rs"]
mod volume;

use disk::{Blocks, DeviceError};
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::exit;
use std::time::Instant;
use volume::{Clock, Volume};

const SECTOR: u64 = 512;
const MIB: u64 = 1024 * 1024;
/// Where the first partition starts: 1 MiB, as partitioning tools put it.
const FIRST_START: u64 = 2048;

fn die(message: &str) -> ! {
    eprintln!("fatdisk: {}", message);
    exit(1)
}

// ---------------------------------------------------------------------------
// A region of an image
// ---------------------------------------------------------------------------

/// Part of an image file as `std::io`, for rust-fatfs's own std interface.
struct Slice {
    file: File,
    begin: u64,
    len: u64,
    pos: u64,
}

impl Read for Slice {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        self.file.seek(SeekFrom::Start(self.begin + self.pos))?;
        self.file.read_exact(&mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Slice {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        self.file.seek(SeekFrom::Start(self.begin + self.pos))?;
        self.file.write_all(&buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for Slice {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(at) => at as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => self.len as i64 + delta,
        };
        if target < 0 || target as u64 > self.len {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek outside the volume"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

/// Bytes in memory as the kernel's `Blocks`.
struct Memory {
    data: Vec<u8>,
    start: Instant,
}

impl Blocks for Memory {
    fn count(&self) -> u64 {
        self.data.len() as u64 / SECTOR
    }
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        let at = (block * SECTOR) as usize;
        buf.copy_from_slice(self.data.get(at..at + buf.len()).ok_or(DeviceError)?);
        Ok(())
    }
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError> {
        let at = (block * SECTOR) as usize;
        self.data.get_mut(at..at + data.len()).ok_or(DeviceError)?.copy_from_slice(data);
        Ok(())
    }
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

fn open(path: &str, write: bool) -> File {
    OpenOptions::new().read(true).write(write).open(path).unwrap_or_else(|e| die(&format!("{}: {}", path, e)))
}

fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) {
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.read_exact(buf)).unwrap_or_else(|e| die(&format!("reading: {}", e)));
}

fn write_at(file: &mut File, offset: u64, data: &[u8]) {
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.write_all(data)).unwrap_or_else(|e| die(&format!("writing: {}", e)));
}

// ---------------------------------------------------------------------------
// Finding a volume
// ---------------------------------------------------------------------------

/// The first sector and the length of every FAT32 volume on the image: the
/// whole image when block 0 is a boot sector, else the MBR's FAT32 entries.
fn volumes(file: &mut File) -> Vec<(u64, u64)> {
    let size = file.metadata().map(|m| m.len()).unwrap_or(0) / SECTOR;
    let mut sector = [0u8; 512];
    read_at(file, 0, &mut sector);
    if volume::looks_like_fat32(&sector) {
        return vec![(0, size)];
    }
    let mut out = Vec::new();
    for entry in sector[446..510].chunks_exact(16) {
        let start = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
        let length = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as u64;
        if (entry[4] == 0x0B || entry[4] == 0x0C) && start > 0 && length > 0 && start + length <= size {
            out.push((start, length));
        }
    }
    out
}

/// The volume labelled `label`, found the way the kernel finds it.
fn find(path: &str, label: &str, write: bool) -> (File, u64, u64) {
    let mut file = open(path, write);
    for (start, length) in volumes(&mut file) {
        let mut region = Region { file: file.try_clone().unwrap(), start, length };
        if let Ok(probe) = volume::probe(&mut region) {
            if probe.labelled(label) {
                return (file, start, length);
            }
        }
    }
    die(&format!("{}: no FAT32 volume labelled {}", path, label))
}

/// A region of an image file as the kernel's `Blocks`, for `probe`.
struct Region {
    file: File,
    start: u64,
    length: u64,
}

impl Blocks for Region {
    fn count(&self) -> u64 {
        self.length
    }
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.file.seek(SeekFrom::Start((self.start + block) * SECTOR)).and_then(|_| self.file.read_exact(buf)).map_err(|_| DeviceError)
    }
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError> {
        self.file.seek(SeekFrom::Start((self.start + block) * SECTOR)).and_then(|_| self.file.write_all(data)).map_err(|_| DeviceError)
    }
    fn now_ms(&self) -> u64 {
        0
    }
}

type HostFs = fatfs::FileSystem<fatfs::StdIoWrapper<Slice>>;

fn mount(path: &str, label: &str, write: bool) -> HostFs {
    let (file, start, length) = find(path, label, write);
    let slice = Slice { file, begin: start * SECTOR, len: length * SECTOR, pos: 0 };
    fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap_or_else(|e| die(&format!("mounting {}: {}", label, e)))
}

// ---------------------------------------------------------------------------
// Making images
// ---------------------------------------------------------------------------

fn label11(label: &str) -> [u8; 11] {
    if label.is_empty() || label.len() > 11 {
        die(&format!("{} is not a label of 1 to 11 characters", label));
    }
    let mut out = [b' '; 11];
    out[..label.len()].copy_from_slice(label.as_bytes());
    out
}

fn format(file: &File, start: u64, length: u64, label: &str, boot: bool) {
    let mut slice = fatfs::StdIoWrapper::new(Slice { file: file.try_clone().unwrap(), begin: start * SECTOR, len: length * SECTOR, pos: 0 });
    let mut options = fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(label11(label)).volume_id(0x5eed_da7a);
    // FAT32 needs 65525 clusters, which a small test volume only has with
    // 512-byte clusters: from about 34 MiB up.
    if length * SECTOR < 260 * MIB {
        options = options.bytes_per_cluster(512);
    }
    fatfs::format_volume(&mut slice, options).unwrap_or_else(|e| die(&format!("formatting {}: {}", label, e)));
    if boot {
        let fs = fatfs::FileSystem::new(slice.into_inner(), fatfs::FsOptions::new()).unwrap_or_else(|e| die(&format!("{}", e)));
        for (name, text) in [("start4.elf", "not firmware: a boot partition's file, for the test"), ("kernel8.img", "not a kernel"), ("config.txt", "arm_64bit=1\n")] {
            let mut f = fs.root_dir().create_file(name).unwrap();
            f.write_all(text.as_bytes()).unwrap();
        }
        fs.unmount().unwrap();
    }
}

fn create(path: &str, mib: u64) -> File {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap_or_else(|e| die(&format!("{}: {}", path, e)));
    file.set_len(mib * MIB).unwrap_or_else(|e| die(&format!("{}: {}", path, e)));
    file
}

fn split_part(spec: &str) -> (&str, bool) {
    match spec.strip_suffix("+boot") {
        Some(rest) => (rest, true),
        None => (spec, false),
    }
}

fn make_mbr(path: &str, mib: u64, parts: &[String]) {
    let mut file = create(path, mib);
    let total = mib * MIB / SECTOR;
    let mut mbr = [0u8; 512];
    let mut next = FIRST_START;
    let mut made = Vec::new();
    for (slot, spec) in parts.iter().enumerate() {
        if slot >= 4 {
            die("an MBR has four entries");
        }
        let (spec, boot) = split_part(spec);
        let (label, size) = spec.split_once(':').unwrap_or_else(|| die(&format!("{} is not LABEL:MIB", spec)));
        let length = if size == "rest" { total - next } else { size.parse::<u64>().unwrap_or_else(|_| die(size)) * MIB / SECTOR };
        if next + length > total {
            die("the partitions do not fit");
        }
        let entry = &mut mbr[446 + slot * 16..462 + slot * 16];
        entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        entry[4] = 0x0C;
        entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        entry[8..12].copy_from_slice(&(next as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&(length as u32).to_le_bytes());
        made.push((next, length, label.to_string(), boot));
        next = (next + length).div_ceil(2048) * 2048;
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    write_at(&mut file, 0, &mbr);
    for (start, length, label, boot) in made {
        format(&file, start, length, &label, boot);
    }
}

fn make_whole(path: &str, mib: u64, spec: &str) {
    let file = create(path, mib);
    let (label, boot) = split_part(spec);
    format(&file, 0, mib * MIB / SECTOR, label, boot);
}

/// What the damage tests start from: directories, long names, a file of
/// several clusters, a full directory for `loops`, and a file for it.
fn fill(path: &str, label: &str) {
    let fs = mount(path, label, true);
    let root = fs.root_dir();
    let www = root.create_dir("www").unwrap();
    let css = www.create_dir("css").unwrap();
    write_file(&www, "index.html", b"<h1>hello from the card</h1>\n");
    write_file(&www, "A page with a long name, spaces and (brackets).html", b"long\n");
    write_file(&css, "site.css", b"body { color: black }\n");
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    write_file(&root, "big.bin", &big);
    // A directory with exactly two full clusters of entries: `.` and `..`,
    // and names that take two entries each, a long name and a short one.
    let cluster = fs.stats().unwrap().cluster_size() as usize;
    let per_cluster = cluster / 32;
    let loopdir = root.create_dir("loopdir").unwrap();
    for i in 0..(2 * per_cluster - 2) / 2 {
        write_file(&loopdir, &format!("f{:04}.txt", i), b"x");
    }
    let chain: Vec<u8> = vec![7u8; cluster * 3];
    write_file(&root, "loop.bin", &chain);
    drop(www);
    drop(css);
    drop(loopdir);
    drop(root);
    fs.unmount().unwrap();
}

fn write_file(dir: &fatfs::Dir<'_, fatfs::StdIoWrapper<Slice>, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>, name: &str, data: &[u8]) {
    let mut file = dir.create_file(name).unwrap();
    file.truncate().unwrap();
    file.write_all(data).unwrap();
}

// ---------------------------------------------------------------------------
// Reading images
// ---------------------------------------------------------------------------

fn cat(path: &str, label: &str, file_path: &str) {
    let fs = mount(path, label, false);
    let mut file = fs.root_dir().open_file(file_path.trim_start_matches('/')).unwrap_or_else(|e| die(&format!("{}: {}", file_path, e)));
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap_or_else(|e| die(&format!("{}: {}", file_path, e)));
    std::io::stdout().write_all(&data).unwrap();
}

fn ls(path: &str, label: &str, dir_path: &str) {
    let fs = mount(path, label, false);
    let trimmed = dir_path.trim_matches('/');
    let dir = if trimmed.is_empty() { fs.root_dir() } else { fs.root_dir().open_dir(trimmed).unwrap_or_else(|e| die(&format!("{}: {}", dir_path, e))) };
    let mut names: Vec<String> = dir
        .iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name() != "." && e.file_name() != "..")
        .map(|e| if e.is_dir() { format!("{}/", e.file_name()) } else { e.file_name() })
        .collect();
    names.sort();
    for name in names {
        println!("{}", name);
    }
}

/// Read every file, and see whether any cluster belongs to two files.
fn check(path: &str, label: &str) {
    let fs = mount(path, label, false);
    let mut owners: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let (mut files, mut dirs, mut shared) = (0, 0, 0);
    let mut pending = vec![(String::new(), fs.root_dir())];
    while let Some((prefix, dir)) = pending.pop() {
        for entry in dir.iter() {
            let entry = entry.unwrap_or_else(|e| die(&format!("listing {}: {}", prefix, e)));
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let full = format!("{}/{}", prefix, name);
            if entry.is_dir() {
                dirs += 1;
                pending.push((full, entry.to_dir()));
                continue;
            }
            files += 1;
            let mut file = entry.to_file();
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap_or_else(|e| die(&format!("reading {}: {}", full, e)));
            if data.len() as u64 != entry.len() {
                die(&format!("{} is {} bytes long and {} could be read", full, entry.len(), data.len()));
            }
            for extent in file.extents() {
                let extent = extent.unwrap();
                if let Some(other) = owners.insert(extent.offset, full.clone()) {
                    eprintln!("fatdisk: {} and {} share the cluster at byte {}", other, full, extent.offset);
                    shared += 1;
                }
            }
        }
    }
    let stats = fs.stats().unwrap();
    println!("{} files and {} directories read whole; {} shared clusters; {} of {} clusters free", files, dirs, shared, stats.free_clusters(), stats.total_clusters());
    if shared > 0 {
        exit(1);
    }
}

// ---------------------------------------------------------------------------
// Damage
// ---------------------------------------------------------------------------

/// xorshift64*: the same bytes for the same seed on every run.
struct Random(Cell<u64>);

impl Random {
    fn new(seed: u64) -> Random {
        Random(Cell::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1))
    }
    fn next(&self) -> u64 {
        let mut x = self.0.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// The FAT32 layout a boot sector gives, read without checks: the first data
/// sector, sectors per cluster, the first FAT sector, sectors per FAT, FATs,
/// and the root's cluster.
fn layout(boot: &[u8]) -> (u64, u64, u64, u64, u64, u64) {
    let le16 = |at: usize| u16::from_le_bytes([boot[at], boot[at + 1]]) as u64;
    let le32 = |at: usize| u32::from_le_bytes(boot[at..at + 4].try_into().unwrap()) as u64;
    let (per_cluster, reserved, fats, fat_sectors, root) = (boot[13] as u64, le16(14), boot[16] as u64, le32(36), le32(44));
    (reserved + fats * fat_sectors, per_cluster.max(1), reserved, fat_sectors, fats, root)
}

/// Bytes of the volume at `start` in an image held in memory.
fn damage_bytes(image: &mut [u8], start: u64, length: u64, mode: &str, random: &Random, count: u64) {
    let base = (start * SECTOR) as usize;
    let boot: Vec<u8> = image[base..base + 512].to_vec();
    let (data_start, per_cluster, fat_start, fat_sectors, fats, _) = layout(&boot);
    match mode {
        "boot" => {
            for _ in 0..count.max(1) {
                let at = 3 + random.below(87) as usize;
                image[base + at] = random.next() as u8;
            }
        }
        "meta" => {
            let end = ((data_start + 16 * per_cluster) * SECTOR).min(length * SECTOR);
            for _ in 0..count {
                let at = base + random.below(end) as usize;
                image[at] = random.next() as u8;
            }
        }
        "random" => {
            for _ in 0..count {
                let at = random.below(image.len() as u64) as usize;
                image[at] = random.next() as u8;
            }
        }
        "fat" => {
            // Entries of clusters in use pointed at other clusters, which makes
            // loops, shared clusters and chains that end nowhere.
            let used = fat_sectors * SECTOR / 4;
            for _ in 0..count.max(1) {
                let cluster = 2 + random.below(512.min(used.saturating_sub(2)));
                let target = random.below(600) as u32;
                for copy in 0..fats.max(1) {
                    let at = base + ((fat_start + copy * fat_sectors) * SECTOR + cluster * 4) as usize;
                    if at + 4 <= image.len() {
                        image[at..at + 4].copy_from_slice(&target.to_le_bytes());
                    }
                }
            }
        }
        other => die(&format!("{} is not a damage mode", other)),
    }
}

/// The cluster chain of `loopdir` and `loop.bin` in the root made to loop.
fn damage_loops(file: &mut File, start: u64) {
    let mut boot = [0u8; 512];
    read_at(file, start * SECTOR, &mut boot);
    let (data_start, per_cluster, fat_start, fat_sectors, fats, root) = layout(&boot);
    let cluster_bytes = per_cluster * SECTOR;
    let offset = |cluster: u64| (start + data_start + (cluster - 2) * per_cluster) * SECTOR;
    let fat_get = |file: &mut File, cluster: u64| {
        let mut entry = [0u8; 4];
        read_at(file, (start + fat_start) * SECTOR + cluster * 4, &mut entry);
        u32::from_le_bytes(entry) as u64 & 0x0FFF_FFFF
    };
    let mut root_data = vec![0u8; cluster_bytes as usize];
    read_at(file, offset(root), &mut root_data);
    let mut done = 0;
    for raw in root_data.chunks_exact(32) {
        let name = &raw[..11];
        let first = (u16::from_le_bytes([raw[20], raw[21]]) as u64) << 16 | u16::from_le_bytes([raw[26], raw[27]]) as u64;
        if name == b"LOOPDIR    " || name == b"LOOP    BIN" {
            // Follow the chain to its last cluster and point that at the first.
            let mut last = first;
            for _ in 0..1_000_000 {
                let next = fat_get(file, last);
                if next >= 0x0FFF_FFF8 || next < 2 {
                    break;
                }
                last = next;
            }
            for copy in 0..fats {
                write_at(file, (start + fat_start + copy * fat_sectors) * SECTOR + last * 4, &(first as u32).to_le_bytes());
            }
            done += 1;
        }
    }
    if done != 2 {
        die("loopdir and loop.bin are not both in the root; run fill first");
    }
}

/// The entry for `www/css` pointed at the root directory's first cluster, so
/// that /www/css is the root again and the tree under it has no bottom.
fn damage_cycle(file: &mut File, start: u64) {
    let mut boot = [0u8; 512];
    read_at(file, start * SECTOR, &mut boot);
    let (data_start, per_cluster, _, _, _, root) = layout(&boot);
    let offset = |cluster: u64| (start + data_start + (cluster - 2) * per_cluster) * SECTOR;
    // The byte offset of the short entry called `name` in the first cluster
    // of the directory at `cluster`, and the first cluster it names.
    let entry_in = |file: &mut File, cluster: u64, name: &[u8]| -> Option<(u64, u64)> {
        let mut data = vec![0u8; (per_cluster * SECTOR) as usize];
        read_at(file, offset(cluster), &mut data);
        let at = data.chunks_exact(32).position(|raw| &raw[..11] == name)?;
        let raw = &data[at * 32..at * 32 + 32];
        let first = (u16::from_le_bytes([raw[20], raw[21]]) as u64) << 16 | u16::from_le_bytes([raw[26], raw[27]]) as u64;
        Some((offset(cluster) + at as u64 * 32, first))
    };
    let (_, www) = entry_in(file, root, b"WWW        ").unwrap_or_else(|| die("www is not in the root's first cluster; run fill first"));
    let (css, _) = entry_in(file, www, b"CSS        ").unwrap_or_else(|| die("css is not in the first cluster of www; run fill first"));
    let mut raw = [0u8; 32];
    read_at(file, css, &mut raw);
    raw[20..22].copy_from_slice(&((root >> 16) as u16).to_le_bytes());
    raw[26..28].copy_from_slice(&(root as u16).to_le_bytes());
    write_at(file, css, &raw);
}

fn damage(path: &str, label: &str, mode: &str, seed: u64, count: u64) {
    let (mut file, start, length) = find(path, label, true);
    if mode == "loops" {
        damage_loops(&mut file, start);
        return;
    }
    if mode == "cycle" {
        damage_cycle(&mut file, start);
        return;
    }
    // Only the region that can change is read and written back.
    let (from, to) = if mode == "random" { (0, file.metadata().unwrap().len()) } else { (start * SECTOR, (start + length.min(1 << 20)) * SECTOR) };
    let mut image = vec![0u8; (to - from) as usize];
    read_at(&mut file, from, &mut image);
    let random = Random::new(seed);
    let volume_start = if mode == "random" { start } else { 0 };
    damage_bytes(&mut image, volume_start, length, mode, &random, count);
    write_at(&mut file, from, &image);
}

// ---------------------------------------------------------------------------
// Fuzzing the kernel's code
// ---------------------------------------------------------------------------

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// What one damaged image did to the kernel's code.
#[derive(Default)]
struct Tally {
    refused: u64,
    unmountable: u64,
    mounted: u64,
    errors: u64,
}

/// Everything /data's users do, against one image. Errors are expected and
/// counted; the only failure is a panic, which the caller catches.
fn exercise(image: Vec<u8>, tally: &mut Tally) {
    let mut blocks = Memory { data: image, start: Instant::now() };
    let probe = match volume::probe(&mut blocks) {
        Ok(probe) => probe,
        Err(_) => {
            tally.refused += 1;
            return;
        }
    };
    let state = disk::share(blocks);
    let mut volume = match Volume::mount(state, probe.geometry, Clock { now: unix_now }) {
        Ok(volume) => volume,
        Err(_) => {
            tally.unmountable += 1;
            return;
        }
    };
    tally.mounted += 1;
    let mut errors = 0u64;
    let mut note = |ok: bool| {
        if !ok {
            errors += 1;
        }
    };
    note(volume.boot_file().is_ok());
    volume.allow_writes();

    let mut files = Vec::new();
    let mut dirs = vec![String::new()];
    let mut at = 0;
    while at < dirs.len() && at < 64 {
        let dir = dirs[at].clone();
        at += 1;
        match volume.list(&dir) {
            Ok(entries) => {
                for entry in entries.into_iter().take(256) {
                    let path = volume::join(&dir, &entry.name);
                    if entry.is_dir {
                        dirs.push(path);
                    } else {
                        files.push((path, entry.len));
                    }
                }
            }
            Err(_) => note(false),
        }
    }
    let mut buf = vec![0u8; 70_000];
    for (path, len) in files.iter().take(64) {
        for offset in [0u64, *len as u64 / 2, (*len as u64).saturating_sub(10)] {
            note(volume.read(path, offset, &mut buf).is_ok());
        }
    }
    let target = dirs.get(1).cloned().unwrap_or_default();
    note(volume.create("", "fuzz new file.txt").is_ok());
    note(volume.write("fuzz new file.txt", 0, &buf[..3000]).is_ok());
    note(volume.write("fuzz new file.txt", 90_000, b"far past the end").is_ok());
    note(volume.truncate("fuzz new file.txt", 10).is_ok());
    note(volume.rename("", "fuzz new file.txt", &target, "Fuzz Renamed.TXT").is_ok());
    note(volume.rename(&target, "Fuzz Renamed.TXT", &target, "fuzz renamed.txt").is_ok());
    note(volume.mkdir("", "fuzzdir").is_ok());
    note(volume.rename("", "fuzzdir", &target, "fuzzdir").is_ok());
    for (path, _) in files.iter().take(8) {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        note(volume.remove(dir, name, false).is_ok());
    }
    for dir in dirs.iter().skip(1).take(8) {
        let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
        note(volume.remove(parent, name, true).is_ok());
    }
    if let Some((path, _)) = files.get(8) {
        note(volume.truncate(path, 0).is_ok());
        note(volume.fsync(path).is_ok());
    }
    note(volume.stats().is_ok());
    note(volume.sync().is_ok());
    note(volume.list("").is_ok());
    tally.errors += errors;
}

fn fuzz(first: u64, last: u64) {
    // The template: a 40 MiB volume holding the content `fill` writes. What
    // is damaged is the volume itself; the partition table in front of it is
    // the kernel's `partition.rs`, which the QEMU cases exercise.
    let dir = std::env::temp_dir().join(format!("fatdisk-fuzz-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let template = dir.join("template.img");
    let template = template.to_str().unwrap();
    make_whole(template, 40, "CLAUDEDATA");
    fill(template, "CLAUDEDATA");
    let mut file = open(template, false);
    let (start, length) = volumes(&mut file)[0];
    let mut volume_bytes = vec![0u8; (length * SECTOR) as usize];
    read_at(&mut file, start * SECTOR, &mut volume_bytes);
    drop(file);
    // A second template whose `loopdir` and `loop.bin` chains loop, which
    // every seventh seed starts from before its own damage.
    damage_loops(&mut open(template, true), 0);
    let mut file = open(template, false);
    let mut looped_bytes = vec![0u8; (length * SECTOR) as usize];
    read_at(&mut file, start * SECTOR, &mut looped_bytes);
    drop(file);
    let _ = std::fs::remove_dir_all(&dir);

    let panics = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = panics.clone();
    std::panic::set_hook(Box::new(move |info| {
        seen.lock().unwrap().push(format!("{}", info));
    }));

    let modes = ["boot", "meta", "fat", "random", "meta", "fat"];
    let mut tally = Tally::default();
    let mut failed = Vec::new();
    let mut slowest = (0u128, 0u64);
    let started = Instant::now();
    for seed in first..=last {
        let mode = modes[(seed % modes.len() as u64) as usize];
        let random = Random::new(seed);
        let mut image = if seed % 7 == 0 { looped_bytes.clone() } else { volume_bytes.clone() };
        let count = match mode {
            "boot" => 1 + random.below(8),
            "fat" => 1 + random.below(32),
            "meta" => 1 + random.below(256),
            _ => 1 + random.below(4096),
        };
        damage_bytes(&mut image, 0, length, mode, &random, count);
        let before = panics.lock().unwrap().len();
        let one = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(image, &mut tally)));
        let took = one.elapsed().as_millis();
        if took > slowest.0 {
            slowest = (took, seed);
        }
        if result.is_err() {
            let message = panics.lock().unwrap().get(before).cloned().unwrap_or_default();
            println!("seed {} ({} x{}): PANIC: {}", seed, mode, count, message);
            failed.push(seed);
        }
    }
    println!(
        "fuzz: seeds {} to {}: {} panics; {} refused by probe, {} not mountable, {} mounted with {} operations failing cleanly; slowest seed {} took {} ms; {} s in all",
        first,
        last,
        failed.len(),
        tally.refused,
        tally.unmountable,
        tally.mounted,
        tally.errors,
        slowest.1,
        slowest.0,
        started.elapsed().as_secs()
    );
    if !failed.is_empty() {
        exit(1);
    }
}

// ---------------------------------------------------------------------------

/// The entry at `path`, looked up the way the kernel's path walk does it: one
/// lookup per component, each in the directory the one before it named. So a
/// path of depth d costs d lookups, as `stat` or `open` of it does on /data.
fn resolve(volume: &mut Volume<Memory>, path: &str) -> Result<volume::Entry, volume::FsError> {
    let mut dir = String::new();
    let mut last = None;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let entry = volume.lookup(&dir, part)?;
        dir = volume::join(&dir, &entry.name);
        last = Some(entry);
    }
    last.ok_or(volume::FsError::NotFound)
}

/// Walk the volume labelled `label` the way `find` walks /data, through the
/// kernel's volume code and with paths resolved as the kernel resolves them:
/// open a directory by its path and list it, look up every entry in it by
/// its path, and go into every directory, until a path under /data would
/// reach the kernel's 4096-byte limit. The volume is read into memory, so
/// nothing is written to the image.
fn walk(path: &str, label: &str) {
    let (mut file, start, length) = find(path, label, false);
    let mut image = vec![0u8; (length * SECTOR) as usize];
    read_at(&mut file, start * SECTOR, &mut image);
    let mut blocks = Memory { data: image, start: Instant::now() };
    let probe = volume::probe(&mut blocks).unwrap_or_else(|why| die(&why));
    let state = disk::share(blocks);
    let mut volume = Volume::mount(state, probe.geometry, Clock { now: unix_now })
        .unwrap_or_else(|e| die(&format!("mounting {}: {:?}", label, e)));
    let started = Instant::now();
    let (mut operations, mut failed, mut deepest) = (0u64, 0u64, 0usize);
    let mut slowest = (0u128, String::new());
    let mut time = |what: String, took: u128| {
        operations += 1;
        if took > slowest.0 {
            slowest = (took, what);
        }
    };
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        let one = Instant::now();
        let listed = if dir.is_empty() { volume.list(&dir) } else { resolve(&mut volume, &dir).and_then(|_| volume.list(&dir)) };
        time(format!("listing /{}", dir), one.elapsed().as_millis());
        let Ok(entries) = listed else {
            failed += 1;
            continue;
        };
        for entry in entries {
            let child = volume::join(&dir, &entry.name);
            if "/data/".len() + child.len() >= 4096 {
                continue;
            }
            let one = Instant::now();
            let found = resolve(&mut volume, &child);
            time(format!("looking up /{}", child), one.elapsed().as_millis());
            match found {
                Ok(found) if found.is_dir => {
                    deepest = deepest.max(child.split('/').count());
                    pending.push(child);
                }
                Ok(_) => {}
                Err(_) => failed += 1,
            }
        }
    }
    let shown: String = slowest.1.chars().take(120).collect();
    println!(
        "walk: {} operations, {} failed, {} levels at the deepest, {} ms in all; the slowest took {} ms: {}",
        operations,
        failed,
        deepest,
        started.elapsed().as_millis(),
        slowest.0,
        shown
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_else(|| die("missing argument; see the top of tools/fatdisk/src/main.rs"));
    let number = |i: usize| arg(i).parse::<u64>().unwrap_or_else(|_| die(&format!("{} is not a number", arg(i))));
    match arg(1).as_str() {
        "mbr" => make_mbr(&arg(2), number(3), &args[4..]),
        "whole" => make_whole(&arg(2), number(3), &arg(4)),
        "fill" => fill(&arg(2), &arg(3)),
        "put" => {
            let fs = mount(&arg(2), &arg(3), true);
            let path = arg(4);
            let path = path.trim_start_matches('/');
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            let mut parent = fs.root_dir();
            for part in dir.split('/').filter(|p| !p.is_empty()) {
                parent = match parent.open_dir(part) {
                    Ok(d) => d,
                    Err(_) => parent.create_dir(part).unwrap(),
                };
            }
            write_file(&parent, name, arg(5).as_bytes());
            drop(parent);
            fs.unmount().unwrap();
        }
        "cat" => cat(&arg(2), &arg(3), &arg(4)),
        "ls" => ls(&arg(2), &arg(3), &arg(4)),
        "check" => check(&arg(2), &arg(3)),
        "span" => {
            let (_, start, length) = find(&arg(2), &arg(3), false);
            println!("{} {}", start, length);
        }
        "damage" => damage(&arg(2), &arg(3), &arg(4), number(5), args.get(6).and_then(|c| c.parse().ok()).unwrap_or(64)),
        "fuzz" => fuzz(number(2), number(3)),
        "walk" => walk(&arg(2), &arg(3)),
        other => die(&format!("{} is not a command; see the top of tools/fatdisk/src/main.rs", other)),
    }
}
