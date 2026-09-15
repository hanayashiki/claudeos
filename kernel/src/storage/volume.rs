//! The FAT32 volume behind /data: rust-fatfs, and what it needs around it.
//!
//! Everything here is in terms of paths inside the volume, `""` for its root
//! and `"www/index.html"` below it, and of `Blocks`, so it knows nothing of
//! the kernel. `kernel/src/storage/vfs.rs` turns it into nodes, and
//! `tools/fatdisk` compiles the same file on the Mac to run it against images
//! damaged on purpose.
//!
//! **Checked before mounting.** `probe` reads the boot sector itself and
//! refuses anything but FAT32 on 512-byte sectors, with sizes that fit the
//! blocks it was given and a FAT that does not reach the cluster numbers FAT32
//! reserves. rust-fatfs checks some of this too; what it does not check it
//! computes with, and those checks are the ones here.
//!
//! **Open files.** rust-fatfs keeps a file's position in the cluster chain in
//! a `File` value, and finding a position from the start of the chain walks
//! every cluster before it. So up to `MAX_OPEN` files stay open between
//! operations, keyed by path, and the least recently used is closed when
//! another is opened. A `File` also remembers where its directory entry is,
//! which a rename or an unlink moves or frees, so both close every open file
//! under the path first; the next use opens it again by its new name.
//!
//! **Operations.** Each public call is one operation: it gets a budget of
//! calls and a deadline (see `disk.rs`), and its writes reach the device
//! before it returns. If the device failed or the budget ran out, what
//! rust-fatfs holds in memory may be half of a change, so the open files and
//! the cache are thrown away and the volume is mounted again from what the
//! device holds.
//!
//! **What FAT cannot do, and what is done instead.** There are no hard links,
//! symbolic links, owners or permission bits. A rename onto an existing name
//! removes the old entry and then renames, in two steps, so a power cut
//! between them leaves the file under its old name only. A rename that changes
//! only the case of a name goes through a temporary name, because rust-fatfs
//! treats the two names as one entry and would do nothing. A directory moved
//! to another parent has its `..` entry pointed at the new parent, which
//! rust-fatfs leaves pointing at the old one. FAT has no holes, so writing past
//! the end of a file, or truncating it longer, writes zeros up to the new
//! bytes. A file is at most 4 GiB less one byte.

use super::disk::{Blocks, Disk, DiskError, Shared, BLOCK};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use fatfs::{Read as FatRead, Seek as FatSeek, Write as FatWrite};

/// Files kept open between operations.
pub const MAX_OPEN: usize = 32;
/// Directories remembered between operations, by path. The kernel resolves a
/// path one component at a time, each a lookup in the directory the one before
/// it named, and without these each lookup found that directory again from the
/// root, so a path d directories deep cost about d²/2 directory scans instead of
/// d. On a damaged test card whose directories form a cycle, which `find`
/// follows until paths reach the length limit, `fatdisk walk` resolving paths
/// the kernel's way took 84 s on the Mac without these and 0.7 s with them.
pub const MAX_DIRS: usize = 32;
/// Entries one listing returns. A FAT directory holds at most 65536 entries,
/// fewer with long names, so a valid one never reaches this.
pub const MAX_LIST: usize = 65536;
/// Zeros written per call when a file is extended.
const ZERO_CHUNK: usize = 64 * 1024;

type Fs<B> = fatfs::FileSystem<Disk<B>, Clock, fatfs::LossyOemCpConverter>;
type FatFile<B> = fatfs::File<'static, Disk<B>, Clock, fatfs::LossyOemCpConverter>;
type FatDir<B> = fatfs::Dir<'static, Disk<B>, Clock, fatfs::LossyOemCpConverter>;
type FatEntry<B> = fatfs::DirEntry<'static, Disk<B>, Clock, fatfs::LossyOemCpConverter>;

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// The clock rust-fatfs stamps entries with: seconds since 1970, UTC.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub now: fn() -> i64,
}

/// 1980-01-01 00:00:00 and 2107-12-31 23:59:58, the range a FAT timestamp
/// can hold.
const FAT_EPOCH: i64 = 315_532_800;
const FAT_END: i64 = 4_354_819_198;

impl fatfs::TimeProvider for Clock {
    fn get_current_date(&self) -> fatfs::Date {
        to_fat((self.now)()).date
    }

    fn get_current_date_time(&self) -> fatfs::DateTime {
        to_fat((self.now)())
    }
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date `days`
/// after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// The inverse, `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// A FAT timestamp for `unix`, held to the range FAT has.
pub fn to_fat(unix: i64) -> fatfs::DateTime {
    let unix = unix.clamp(FAT_EPOCH, FAT_END);
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    let seconds = unix.rem_euclid(86_400);
    fatfs::DateTime::new(
        fatfs::Date::new(year as u16, month as u16, day as u16),
        fatfs::Time::new((seconds / 3600) as u16, (seconds / 60 % 60) as u16, (seconds % 60) as u16, 0),
    )
}

/// Seconds since 1970 for a FAT timestamp read off the card. The fields come
/// straight from the directory entry, so a month of 13 or a day of 0 is
/// possible; anything out of range reads as the start of FAT's epoch.
pub fn from_fat(stamp: fatfs::DateTime) -> i64 {
    let (date, time) = (stamp.date, stamp.time);
    if !(1..=12).contains(&date.month) || !(1..=31).contains(&date.day) || time.hour > 23 || time.min > 59 || time.sec > 59 {
        return FAT_EPOCH;
    }
    days_from_civil(date.year as i64, date.month as u32, date.day as u32) * 86_400
        + time.hour as i64 * 3600
        + time.min as i64 * 60
        + time.sec as i64
}

// ---------------------------------------------------------------------------
// Errors and entries
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NoSpace,
    NameTooLong,
    /// A character FAT does not allow in a name.
    BadName,
    /// Past FAT's 4 GiB limit on a file.
    TooBig,
    /// A directory moved inside itself, or an argument rust-fatfs refused.
    Invalid,
    /// The device failed, the volume's structures are damaged, or the
    /// operation ran out of budget.
    Io,
    /// The volume could not be mounted again after an error.
    Offline,
    /// A write before `allow_writes`.
    ReadOnly,
}

fn fat_error(error: fatfs::Error<DiskError>) -> FsError {
    match error {
        fatfs::Error::Io(DiskError::ReadOnly) => FsError::ReadOnly,
        fatfs::Error::Io(_) => FsError::Io,
        fatfs::Error::UnexpectedEof | fatfs::Error::CorruptedFileSystem => FsError::Io,
        fatfs::Error::WriteZero => FsError::TooBig,
        fatfs::Error::InvalidInput => FsError::Invalid,
        fatfs::Error::NotFound => FsError::NotFound,
        fatfs::Error::AlreadyExists => FsError::Exists,
        fatfs::Error::DirectoryIsNotEmpty => FsError::NotEmpty,
        fatfs::Error::NotEnoughSpace => FsError::NoSpace,
        fatfs::Error::InvalidFileNameLength => FsError::NameTooLong,
        fatfs::Error::UnsupportedFileNameCharacter => FsError::BadName,
        _ => FsError::Io,
    }
}

/// One directory entry, as the volume has it.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The name on the card: the long name, or the short one where there is
    /// no long name.
    pub name: String,
    pub is_dir: bool,
    pub len: u32,
    /// Last modification, seconds since 1970.
    pub mtime: i64,
}

fn entry_of<B: Blocks + 'static>(entry: &FatEntry<B>) -> Entry {
    Entry {
        name: entry.file_name(),
        is_dir: entry.is_dir(),
        len: if entry.is_dir() { 0 } else { entry.len() as u32 },
        mtime: from_fat(entry.modified()),
    }
}

/// `path` with `name` under it.
pub fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", path, name)
    }
}

/// Whether `name` names `entry` the way rust-fatfs's own lookups decide it:
/// the long name compared with ASCII letters folded to upper case, or else the
/// short name the same way.
fn names<B: Blocks + 'static>(entry: &FatEntry<B>, name: &str) -> bool {
    fn same(a: &str, b: &str) -> bool {
        a.chars().map(|c| c.to_ascii_uppercase()).eq(b.chars().map(|c| c.to_ascii_uppercase()))
    }
    if let Some(units) = entry.long_file_name_as_ucs2_units() {
        if let Ok(long) = String::from_utf16(units) {
            if same(&long, name) {
                return true;
            }
        }
    }
    same(&entry.short_file_name(), name)
}

/// A name the volume can be asked to create.
fn check_name(name: &str) -> Result<(), FsError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::Invalid);
    }
    if name.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The boot sector, read before anything trusts it
// ---------------------------------------------------------------------------

/// Where things are in a FAT32 volume.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// Sectors the boot sector says the volume has.
    pub sectors: u32,
    pub cluster_blocks: u32,
    pub fat_start: u32,
    pub fat_blocks: u32,
    pub fats: u32,
    /// The first sector of cluster 2.
    pub data_start: u32,
    pub clusters: u32,
    pub root_cluster: u32,
}

/// What `probe` found.
#[derive(Clone, Debug)]
pub struct Probe {
    pub geometry: Geometry,
    /// The label in the boot sector, trimmed, if the extended boot signature
    /// says the field is there.
    pub boot_label: Option<String>,
    /// The label entry in the root directory's first cluster, trimmed.
    pub root_label: Option<String>,
}

impl Probe {
    /// Whether either label is `label`, ignoring the case of ASCII letters.
    pub fn labelled(&self, label: &str) -> bool {
        [&self.boot_label, &self.root_label].iter().any(|found| found.as_deref().is_some_and(|found| found.eq_ignore_ascii_case(label)))
    }

    /// The label to report: the root directory's, which is what a label
    /// change on another system updates, or else the boot sector's.
    pub fn label(&self) -> String {
        self.root_label.clone().or_else(|| self.boot_label.clone()).unwrap_or_else(|| String::from("(no label)"))
    }
}

fn le16(data: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([data[at], data[at + 1]]) as u32
}

fn le32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Eleven bytes of a label with the padding taken off, or nothing when they
/// are all padding. Bytes outside printable ASCII are shown as `?`.
fn label_text(raw: &[u8]) -> Option<String> {
    let end = raw.iter().rposition(|&b| b != b' ' && b != 0).map(|p| p + 1)?;
    Some(raw[..end].iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '?' }).collect())
}

/// Whether block 0 of a card or a partition holds a FAT32 boot sector: the
/// signature, a jump, 512-byte sectors, the FAT32 layout of the fields, and
/// the type string mkfs tools write. Nothing here is trusted beyond deciding
/// whether to look further.
pub fn looks_like_fat32(sector: &[u8]) -> bool {
    sector.len() >= BLOCK
        && sector[510] == 0x55
        && sector[511] == 0xAA
        && (sector[0] == 0xEB || sector[0] == 0xE9)
        && le16(sector, 11) == 512
        && le16(sector, 22) == 0
        && &sector[82..90] == b"FAT32   "
}

/// Read and check the boot sector at block 0 of `blocks`, and the label in
/// the root directory. Only reads, and only the boot sector and one cluster.
pub fn probe<B: Blocks>(blocks: &mut B) -> Result<Probe, String> {
    let mut sector = [0u8; BLOCK];
    blocks.read(0, &mut sector).map_err(|_| String::from("the card failed to read the volume's boot sector"))?;
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(String::from("the volume's boot sector has no 55 AA signature"));
    }
    let sector_size = le16(&sector, 11);
    let per_cluster = sector[13] as u32;
    let reserved = le16(&sector, 14);
    let fats = sector[16] as u32;
    let root_entries = le16(&sector, 17);
    let sectors_16 = le16(&sector, 19);
    let fat_16 = le16(&sector, 22);
    let sectors = le32(&sector, 32);
    let fat_blocks = le32(&sector, 36);
    let version = le16(&sector, 42);
    let root_cluster = le32(&sector, 44);
    let fs_info = le16(&sector, 48);
    let backup = le16(&sector, 50);

    if sector_size != 512 {
        return Err(format!("the volume's sectors are {} bytes, and only 512 is read", sector_size));
    }
    if fat_16 != 0 || root_entries != 0 || sectors_16 != 0 {
        return Err(String::from("the volume is FAT12 or FAT16, not FAT32"));
    }
    if per_cluster == 0 || !per_cluster.is_power_of_two() {
        return Err(format!("the boot sector gives {} sectors per cluster", per_cluster));
    }
    if reserved == 0 || fats == 0 || fats > 2 || fat_blocks == 0 || version != 0 {
        return Err(String::from("the boot sector's reserved sectors, FAT count, FAT size or version are invalid"));
    }
    if fs_info >= reserved || backup >= reserved {
        return Err(String::from("the boot sector puts FSInfo or its backup outside the reserved sectors"));
    }
    if sectors == 0 || sectors as u64 > blocks.count() {
        return Err(format!("the boot sector claims {} sectors and the partition has {}", sectors, blocks.count()));
    }
    let data_start = reserved as u64 + fats as u64 * fat_blocks as u64;
    if data_start >= sectors as u64 {
        return Err(String::from("the boot sector's FATs do not fit in the volume"));
    }
    let clusters = (sectors as u64 - data_start) / per_cluster as u64;
    if !(65_525..=0x0FFF_FFF5).contains(&clusters) {
        return Err(format!("{} clusters is not a FAT32 volume", clusters));
    }
    // A FAT entry past the table is a read past it, which fails. One inside a
    // table big enough to reach the numbers FAT32 reserves for end of chain
    // would be set free by rust-fatfs's code for freeing a chain, which
    // refuses those numbers by panicking.
    let entries = fat_blocks as u64 * (BLOCK as u64 / 4);
    if entries < clusters + 2 || entries >= 0x0FFF_FFF7 {
        return Err(format!("a FAT of {} sectors does not fit {} clusters", fat_blocks, clusters));
    }
    if root_cluster < 2 || root_cluster as u64 >= clusters + 2 {
        return Err(format!("the root directory's cluster {} is not in the volume", root_cluster));
    }
    let geometry = Geometry {
        sectors,
        cluster_blocks: per_cluster,
        fat_start: reserved,
        fat_blocks,
        fats,
        data_start: data_start as u32,
        clusters: clusters as u32,
        root_cluster,
    };
    let boot_label = if sector[66] == 0x29 { label_text(&sector[71..82]) } else { None };

    let mut root = vec![0u8; per_cluster as usize * BLOCK];
    let first = data_start + (root_cluster as u64 - 2) * per_cluster as u64;
    blocks.read(first, &mut root).map_err(|_| String::from("the card failed to read the root directory"))?;
    let mut root_label = None;
    for raw in root.chunks_exact(32) {
        if raw[0] == 0 {
            break;
        }
        let attributes = raw[11];
        if raw[0] != 0xE5 && attributes != 0x0F && attributes & 0x08 != 0 {
            root_label = label_text(&raw[..11]);
            break;
        }
    }
    Ok(Probe { geometry, boot_label, root_label })
}

// ---------------------------------------------------------------------------
// The mounted volume
// ---------------------------------------------------------------------------

struct Open<B: Blocks + 'static> {
    path: String,
    file: FatFile<B>,
    /// Where `file` is positioned, so a read that continues where the last
    /// one ended does not seek.
    pos: u32,
    len: u32,
}

pub struct Volume<B: Blocks + 'static> {
    state: Shared<B>,
    /// The filesystem, from `Box::into_raw`, or null once it could not be
    /// mounted again. Every `File` in `open` and every entry in `dirs` borrows
    /// it, and they are all dropped before it is freed; nothing else that
    /// borrows it outlives the call it was made in.
    fs: *mut Fs<B>,
    clock: Clock,
    geometry: Geometry,
    open: Vec<Open<B>>,
    /// Directory entries found by earlier operations, by the path inside the
    /// volume they are at, the most recently used last.
    dirs: Vec<(String, FatEntry<B>)>,
}

// The volume holds reference-counted handles and a raw pointer, none of which
// leave it: every `Rc` clone is inside rust-fatfs's filesystem or its files,
// which the volume owns. The kernel keeps it behind one lock, so one task at a
// time reaches any of it.
unsafe impl<B: Blocks + Send + 'static> Send for Volume<B> {}

/// The budget for an operation that moves `bytes`.
///
/// Calls: a walk along a cluster chain costs rust-fatfs two or three calls a
/// cluster, and one operation walks a chain or scans the FAT for a free
/// cluster at most a few times, so sixteen calls for every cluster on the
/// volume; a directory holds at most 65536 entries at about a dozen calls
/// each, so a million more; and 64 calls for every block the operation moves,
/// for the allocation that goes with it. A deadline of 30 seconds, plus a
/// second for every MiB moved.
fn budget(geometry: &Geometry, bytes: u64) -> (u64, u64) {
    let calls = 16 * (geometry.clusters as u64 + 2) + 64 * (bytes / BLOCK as u64) + 1_000_000;
    let millis = 30_000 + bytes / 1024;
    (calls, millis)
}

impl<B: Blocks + 'static> Volume<B> {
    /// Mount the volume `probe` described, with writes refused until
    /// `allow_writes`.
    pub fn mount(state: Shared<B>, geometry: Geometry, clock: Clock) -> Result<Volume<B>, FsError> {
        let (calls, millis) = budget(&geometry, 0);
        state.borrow_mut().begin(calls, millis);
        let options = fatfs::FsOptions::new().time_provider(clock);
        let fs = fatfs::FileSystem::new(Disk::new(state.clone()), options).map_err(fat_error)?;
        Ok(Volume { state, fs: Box::into_raw(Box::new(fs)), clock, geometry, open: Vec::new(), dirs: Vec::new() })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn allow_writes(&mut self) {
        self.state.borrow_mut().allow_writes();
    }

    fn fs(&self) -> Result<&'static Fs<B>, FsError> {
        // See the field: nothing given this reference outlives the pointer.
        unsafe { self.fs.as_ref() }.ok_or(FsError::Offline)
    }

    /// Free the filesystem, which unmounts it. Every open file and every
    /// remembered directory goes first.
    fn drop_fs(&mut self) -> Option<Box<Fs<B>>> {
        self.open.clear();
        self.dirs.clear();
        if self.fs.is_null() {
            return None;
        }
        let fs = unsafe { Box::from_raw(self.fs) };
        self.fs = core::ptr::null_mut();
        Some(fs)
    }

    fn mount_again(&mut self) {
        let options = fatfs::FsOptions::new().time_provider(self.clock);
        if let Ok(fs) = fatfs::FileSystem::new(Disk::new(self.state.clone()), options) {
            self.fs = Box::into_raw(Box::new(fs));
        }
    }

    /// After the device failed or an operation ran out: drop what rust-fatfs
    /// holds in memory without letting it write, drop the cache, and mount
    /// again from what the device has.
    fn recover(&mut self) {
        let writable = self.state.borrow().writable();
        self.state.borrow_mut().set_writable(false);
        drop(self.drop_fs());
        {
            let mut state = self.state.borrow_mut();
            state.discard();
            state.set_writable(writable);
            let (calls, millis) = budget(&self.geometry, 0);
            state.begin(calls, millis);
        }
        self.mount_again();
    }

    /// Run one operation: set its budget, run it, put its writes on the
    /// device, and recover if the device failed or the budget ran out.
    fn run<T>(&mut self, bytes: u64, op: impl FnOnce(&mut Self) -> Result<T, FsError>) -> Result<T, FsError> {
        self.fs()?;
        let (calls, millis) = budget(&self.geometry, bytes);
        self.state.borrow_mut().begin(calls, millis);
        let result = op(self);
        let broke = {
            let state = self.state.borrow();
            state.failed || state.exhausted()
        };
        if broke {
            self.recover();
            return Err(match result {
                Err(FsError::ReadOnly) => FsError::ReadOnly,
                _ => FsError::Io,
            });
        }
        let flushed = self.state.borrow_mut().flush();
        if flushed.is_err() {
            self.recover();
            return Err(FsError::Io);
        }
        result
    }

    /// The directory at `path`, found from the deepest remembered directory on
    /// the way to it, and remembered with every directory passed on the way.
    fn dir(&mut self, path: &str) -> Result<FatDir<B>, FsError> {
        let root = self.fs()?.root_dir();
        if path.is_empty() {
            return Ok(root);
        }
        if let Some(at) = self.dirs.iter().position(|(known, _)| known == path) {
            let remembered = self.dirs.remove(at);
            let dir = remembered.1.to_dir();
            self.dirs.push(remembered);
            return Ok(dir);
        }
        let nearest = self
            .dirs
            .iter()
            .filter(|(known, _)| path.len() > known.len() && path.starts_with(known.as_str()) && path.as_bytes()[known.len()] == b'/')
            .max_by_key(|(known, _)| known.len())
            .map(|(known, entry)| (known.clone(), entry.to_dir()));
        let (mut walked, mut dir) = nearest.unwrap_or((String::new(), root));
        for part in path[walked.len()..].split('/').filter(|part| !part.is_empty()) {
            let entry = find(&dir, part)?.ok_or(FsError::NotFound)?;
            if !entry.is_dir() {
                return Err(FsError::NotDir);
            }
            walked = join(&walked, part);
            dir = entry.to_dir();
            self.remember(walked.clone(), entry);
        }
        Ok(dir)
    }

    /// Remember `entry` as the directory at `path`, forgetting the least
    /// recently used when `MAX_DIRS` are remembered.
    fn remember(&mut self, path: String, entry: FatEntry<B>) {
        self.dirs.retain(|(known, _)| *known != path);
        if self.dirs.len() >= MAX_DIRS {
            self.dirs.remove(0);
        }
        self.dirs.push((path, entry));
    }

    /// The open file for `path`, opening it if it is not open.
    fn handle(&mut self, path: &str) -> Result<usize, FsError> {
        if let Some(at) = self.open.iter().position(|open| open.path == path) {
            let open = self.open.remove(at);
            self.open.push(open);
            return Ok(self.open.len() - 1);
        }
        let (parent, name) = match path.rfind('/') {
            Some(at) => (&path[..at], &path[at + 1..]),
            None => ("", path),
        };
        let dir = self.dir(parent)?;
        let entry = find(&dir, name)?.ok_or(FsError::NotFound)?;
        if entry.is_dir() {
            return Err(FsError::IsDir);
        }
        let len = entry.len() as u32;
        let file = entry.to_file();
        if self.open.len() >= MAX_OPEN {
            self.open.remove(0);
        }
        self.open.push(Open { path: path.to_string(), file, pos: 0, len });
        Ok(self.open.len() - 1)
    }

    /// Close every open file, and forget every remembered directory, at or
    /// under `path`. Called before the entry at `path` is removed or moved,
    /// after which a remembered entry would name freed clusters or a path that
    /// no longer leads to it.
    fn close_under(&mut self, path: &str) {
        let prefix = format!("{}/", path);
        self.open.retain(|open| open.path != path && !open.path.starts_with(&prefix));
        self.dirs.retain(|(known, _)| known != path && !known.starts_with(&prefix));
    }

    /// Put the open file at `at` at `offset`, which must be inside the file.
    fn position(&mut self, at: usize, offset: u32) -> Result<(), FsError> {
        let open = &mut self.open[at];
        if open.pos != offset {
            let reached = FatSeek::seek(&mut open.file, fatfs::SeekFrom::Start(offset as u64)).map_err(fat_error)?;
            if reached != offset as u64 {
                // The chain ends before the length the entry gives.
                open.pos = reached as u32;
                return Err(FsError::Io);
            }
            open.pos = offset;
        }
        Ok(())
    }

    /// Write `count` zero bytes at the end of the open file at `at`.
    fn extend(&mut self, at: usize, count: u64) -> Result<(), FsError> {
        let len = self.open[at].len;
        self.position(at, len)?;
        let zeros = [0u8; ZERO_CHUNK];
        let mut left = count;
        while left > 0 {
            let piece = left.min(ZERO_CHUNK as u64) as usize;
            write_all(&mut self.open[at].file, &zeros[..piece])?;
            let open = &mut self.open[at];
            open.pos += piece as u32;
            open.len = open.pos;
            left -= piece as u64;
        }
        Ok(())
    }

    // ---- operations ----------------------------------------------------

    /// The entry called `name` in the directory `dir`. A directory found is
    /// remembered, because the kernel's path walk asks for it next.
    pub fn lookup(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        self.run(0, |volume| {
            let parent = volume.dir(dir)?;
            let entry = find(&parent, name)?.ok_or(FsError::NotFound)?;
            let found = entry_of(&entry);
            if found.is_dir {
                volume.remember(join(dir, &found.name), entry);
            }
            Ok(found)
        })
    }

    /// Every entry in the directory `dir` but `.` and `..`, and any whose name
    /// no path could spell.
    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>, FsError> {
        self.run(0, |volume| {
            let dir = volume.dir(dir)?;
            let mut out = Vec::new();
            for entry in dir.iter() {
                let entry = entry.map_err(fat_error)?;
                let name = entry.file_name();
                if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
                    continue;
                }
                if out.len() >= MAX_LIST {
                    return Err(FsError::Io);
                }
                out.push(entry_of(&entry));
            }
            Ok(out)
        })
    }

    pub fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.run(buf.len() as u64, |volume| {
            let at = volume.handle(path)?;
            let len = volume.open[at].len as u64;
            if offset >= len || buf.is_empty() {
                return Ok(0);
            }
            let want = (len - offset).min(buf.len() as u64) as usize;
            volume.position(at, offset as u32)?;
            let open = &mut volume.open[at];
            let mut done = 0;
            while done < want {
                let n = FatRead::read(&mut open.file, &mut buf[done..want]).map_err(fat_error)?;
                if n == 0 {
                    // The chain ended before the length the entry gives.
                    break;
                }
                done += n;
                open.pos += n as u32;
            }
            Ok(done)
        })
    }

    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let end = offset.checked_add(data.len() as u64).ok_or(FsError::TooBig)?;
        if end > u32::MAX as u64 {
            return Err(FsError::TooBig);
        }
        self.run(end, |volume| {
            let at = volume.handle(path)?;
            let len = volume.open[at].len as u64;
            if offset > len {
                volume.extend(at, offset - len)?;
            }
            volume.position(at, offset as u32)?;
            write_all(&mut volume.open[at].file, data)?;
            let open = &mut volume.open[at];
            open.pos = end as u32;
            open.len = open.len.max(end as u32);
            FatWrite::flush(&mut open.file).map_err(fat_error)?;
            Ok(data.len())
        })
    }

    pub fn truncate(&mut self, path: &str, len: u64) -> Result<(), FsError> {
        if len > u32::MAX as u64 {
            return Err(FsError::TooBig);
        }
        self.run(len, |volume| {
            let at = volume.handle(path)?;
            let current = volume.open[at].len as u64;
            if len > current {
                volume.extend(at, len - current)?;
            } else if len < current {
                volume.position(at, len as u32)?;
                let open = &mut volume.open[at];
                open.file.truncate().map_err(fat_error)?;
                open.len = len as u32;
            }
            FatWrite::flush(&mut volume.open[at].file).map_err(fat_error)?;
            Ok(())
        })
    }

    pub fn create(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        check_name(name)?;
        self.run(0, |volume| {
            let dir = volume.dir(dir)?;
            if find(&dir, name)?.is_some() {
                return Err(FsError::Exists);
            }
            drop(dir.create_file(name).map_err(fat_error)?);
            find(&dir, name)?.map(|entry| entry_of(&entry)).ok_or(FsError::Io)
        })
    }

    pub fn mkdir(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        check_name(name)?;
        self.run(0, |volume| {
            let dir = volume.dir(dir)?;
            if find(&dir, name)?.is_some() {
                return Err(FsError::Exists);
            }
            dir.create_dir(name).map_err(fat_error)?;
            find(&dir, name)?.map(|entry| entry_of(&entry)).ok_or(FsError::Io)
        })
    }

    pub fn remove(&mut self, dir_path: &str, name: &str, want_dir: bool) -> Result<(), FsError> {
        self.run(0, |volume| {
            let dir = volume.dir(dir_path)?;
            let entry = find(&dir, name)?.ok_or(FsError::NotFound)?;
            if entry.is_dir() != want_dir {
                return Err(if entry.is_dir() { FsError::IsDir } else { FsError::NotDir });
            }
            let on_card = entry.file_name();
            volume.close_under(&join(dir_path, &on_card));
            dir.remove(&on_card).map_err(fat_error)
        })
    }

    /// Rename, replacing an existing entry of the same kind at the
    /// destination. Returns the name the entry now has on the card.
    pub fn rename(&mut self, from_dir: &str, from_name: &str, to_dir: &str, to_name: &str) -> Result<String, FsError> {
        check_name(to_name)?;
        self.run(0, |volume| {
            let source_dir = volume.dir(from_dir)?;
            let source = find(&source_dir, from_name)?.ok_or(FsError::NotFound)?;
            let source_name = source.file_name();
            let source_path = join(from_dir, &source_name);
            let is_dir = source.is_dir();
            if is_dir && (to_dir == source_path || to_dir.starts_with(&format!("{}/", source_path))) {
                return Err(FsError::Invalid);
            }
            let target_dir = volume.dir(to_dir)?;
            let existing = find(&target_dir, to_name)?;
            volume.close_under(&source_path);
            if let Some(existing) = existing {
                let existing_name = existing.file_name();
                if to_dir == from_dir && existing_name == source_name {
                    if source_name == to_name {
                        return Ok(source_name);
                    }
                    // The same entry under a name differing only in case.
                    let temporary = temporary_name(&source_dir)?;
                    source_dir.rename(&source_name, &source_dir, &temporary).map_err(fat_error)?;
                    source_dir.rename(&temporary, &source_dir, to_name).map_err(fat_error)?;
                    return Ok(to_name.to_string());
                }
                if is_dir && !existing.is_dir() {
                    return Err(FsError::NotDir);
                }
                if !is_dir && existing.is_dir() {
                    return Err(FsError::IsDir);
                }
                volume.close_under(&join(to_dir, &existing_name));
                target_dir.remove(&existing_name).map_err(fat_error)?;
            }
            source_dir.rename(&source_name, &target_dir, to_name).map_err(fat_error)?;
            if is_dir && from_dir != to_dir {
                volume.point_dotdot(&join(to_dir, to_name), to_dir)?;
            }
            Ok(to_name.to_string())
        })
    }

    /// Put the open file's directory entry on the device. Its contents are
    /// there already: every write reaches the device before it returns.
    pub fn fsync(&mut self, path: &str) -> Result<(), FsError> {
        self.run(0, |volume| {
            if let Some(open) = volume.open.iter_mut().find(|open| open.path == path) {
                FatWrite::flush(&mut open.file).map_err(fat_error)?;
            }
            Ok(())
        })
    }

    /// Close every file, write FSInfo and clear the volume's dirty flag by
    /// unmounting, and mount again. What another system then finds is a volume
    /// that was unmounted cleanly, with a correct count of free clusters.
    pub fn sync(&mut self) -> Result<(), FsError> {
        self.fs()?;
        let (calls, millis) = budget(&self.geometry, 0);
        self.state.borrow_mut().begin(calls, millis);
        let unmounted = match self.drop_fs() {
            Some(fs) => fs.unmount().map_err(fat_error),
            None => Err(FsError::Offline),
        };
        let flushed = self.state.borrow_mut().flush();
        let failed = self.state.borrow().failed;
        if failed || flushed.is_err() {
            self.recover();
            return Err(FsError::Io);
        }
        self.mount_again();
        self.fs()?;
        unmounted
    }

    /// Cluster size in bytes, total clusters and free clusters.
    pub fn stats(&mut self) -> Result<(u32, u32, u32), FsError> {
        self.run(0, |volume| {
            let stats = volume.fs()?.stats().map_err(fat_error)?;
            Ok((stats.cluster_size(), stats.total_clusters(), stats.free_clusters()))
        })
    }

    /// The name of a file in the root directory that only a boot partition
    /// holds, if there is one.
    pub fn boot_file(&mut self) -> Result<Option<String>, FsError> {
        self.run(0, |volume| {
            for entry in volume.fs()?.root_dir().iter() {
                let entry = entry.map_err(fat_error)?;
                let name = entry.file_name();
                if !entry.is_dir() && (name.eq_ignore_ascii_case("start4.elf") || name.eq_ignore_ascii_case("kernel8.img")) {
                    return Ok(Some(name));
                }
            }
            Ok(None)
        })
    }

    // ---- the `..` entry --------------------------------------------------

    /// Point the `..` entry of the directory at `moved` at `parent`, the way a
    /// directory created there would have it: zero for the root, as the FAT
    /// specification says, and the parent's first cluster otherwise.
    fn point_dotdot(&mut self, moved: &str, parent: &str) -> Result<(), FsError> {
        let parent_cluster = if parent.is_empty() { 0 } else { self.cluster_of(parent)? };
        let cluster = self.cluster_of(moved)?;
        let g = self.geometry;
        let block = g.data_start as u64 + (cluster as u64 - 2) * g.cluster_blocks as u64;
        let offset = block * BLOCK as u64 + 32;
        let mut raw = [0u8; 32];
        let mut state = self.state.borrow_mut();
        if offset + 32 > state.bytes() {
            return Err(FsError::Io);
        }
        state.read_at(offset, &mut raw).map_err(|_| FsError::Io)?;
        if &raw[..11] != b"..         " || raw[11] & 0x10 == 0 {
            // Not the entry every FAT implementation writes second in a
            // directory, so there is nothing here to correct.
            return Ok(());
        }
        raw[20..22].copy_from_slice(&((parent_cluster >> 16) as u16).to_le_bytes());
        raw[26..28].copy_from_slice(&(parent_cluster as u16).to_le_bytes());
        state.write_at(offset, &raw).map_err(|_| FsError::Io)
    }

    /// The first cluster of the directory at `path`. rust-fatfs does not say,
    /// so each step finds the entry through rust-fatfs, for its short name,
    /// and then finds the entry with that short name in the raw directory,
    /// where short names are unique.
    fn cluster_of(&mut self, path: &str) -> Result<u32, FsError> {
        let mut cluster = self.geometry.root_cluster;
        let mut walked = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let dir = self.dir(&walked)?;
            let entry = find(&dir, part)?.ok_or(FsError::NotFound)?;
            let short = entry.short_file_name_as_bytes().to_vec();
            walked = join(&walked, &entry.file_name());
            cluster = self.raw_child(cluster, &short)?;
        }
        Ok(cluster)
    }

    /// The first cluster of the directory whose short name is `short` in the
    /// raw directory starting at `cluster`, following the FAT for at most as
    /// many clusters as the volume has.
    fn raw_child(&mut self, mut cluster: u32, short: &[u8]) -> Result<u32, FsError> {
        let g = self.geometry;
        let mut data = vec![0u8; g.cluster_blocks as usize * BLOCK];
        let mut state = self.state.borrow_mut();
        for _ in 0..g.clusters {
            if cluster < 2 || cluster as u64 >= g.clusters as u64 + 2 {
                return Err(FsError::Io);
            }
            let block = g.data_start as u64 + (cluster as u64 - 2) * g.cluster_blocks as u64;
            state.read_at(block * BLOCK as u64, &mut data).map_err(|_| FsError::Io)?;
            for raw in data.chunks_exact(32) {
                if raw[0] == 0 {
                    return Err(FsError::NotFound);
                }
                if raw[0] == 0xE5 || raw[11] == 0x0F || raw[11] & 0x08 != 0 || raw[11] & 0x10 == 0 {
                    continue;
                }
                if short_name(&raw[..11]) == short {
                    return Ok((le16(raw, 20) << 16) | le16(raw, 26));
                }
            }
            let mut next = [0u8; 4];
            state.read_at(g.fat_start as u64 * BLOCK as u64 + cluster as u64 * 4, &mut next).map_err(|_| FsError::Io)?;
            cluster = u32::from_le_bytes(next) & 0x0FFF_FFFF;
            if cluster >= 0x0FFF_FFF8 {
                return Err(FsError::NotFound);
            }
        }
        Err(FsError::Io)
    }
}

impl<B: Blocks + 'static> Drop for Volume<B> {
    fn drop(&mut self) {
        drop(self.drop_fs());
    }
}

/// The first entry `name` names in `dir`.
fn find<B: Blocks + 'static>(dir: &FatDir<B>, name: &str) -> Result<Option<FatEntry<B>>, FsError> {
    for entry in dir.iter() {
        let entry = entry.map_err(fat_error)?;
        if names(&entry, name) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn write_all<B: Blocks + 'static>(file: &mut FatFile<B>, mut data: &[u8]) -> Result<(), FsError> {
    while !data.is_empty() {
        let n = FatWrite::write(file, data).map_err(fat_error)?;
        if n == 0 {
            return Err(FsError::TooBig);
        }
        data = &data[n..];
    }
    Ok(())
}

/// A name nothing in `dir` has, for a rename in two steps.
fn temporary_name<B: Blocks + 'static>(dir: &FatDir<B>) -> Result<String, FsError> {
    for n in 0..1000 {
        let name = format!(".claudeos-rename-{}", n);
        if find(dir, &name)?.is_none() {
            return Ok(name);
        }
    }
    Err(FsError::Exists)
}

/// Eleven raw bytes of a short name as rust-fatfs shows them: the name part
/// and the extension with their padding off, a dot between them when there is
/// an extension, and a leading 0x05 read as 0xE5.
fn short_name(raw: &[u8]) -> Vec<u8> {
    let name_len = raw[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let ext_len = raw[8..11].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let mut out = raw[..name_len].to_vec();
    if ext_len > 0 {
        out.push(b'.');
        out.extend_from_slice(&raw[8..8 + ext_len]);
    }
    if out.first() == Some(&0x05) {
        out[0] = 0xE5;
    }
    out
}
