//! The mounted volume: the operations /data is made of, and the order in
//! which their writes reach the card.
//!
//! Everything here is in terms of paths inside the volume, `""` for its root
//! and `www/index.html` below it. The kernel's `storage/vfs.rs` turns the
//! volume into nodes and always passes names as this code returned them, so
//! a path names each directory by its name on the card.
//!
//! **Write ordering.** There is no journal, so what a power cut leaves depends
//! on the order of the writes, and the order is chosen so that a cut leaves
//! clusters marked in use that nothing names, which fsck_msdos reclaims, and
//! never an entry that names a cluster whose contents were not written or that
//! is free. Each step below ends in a flush, so it is on the card before the
//! next begins:
//!
//! - Writing a file: the new contents; then the FAT entries of new clusters,
//!   the last one ending the chain; then the entry of the chain's old last
//!   cluster, pointing at the first new one (in the same flush when both
//!   entries are in one FAT sector); then the directory entry's first cluster,
//!   length and modification time. Growing by more than 1024 clusters repeats
//!   the first three steps per 1024.
//! - Shrinking a file: the directory entry's length, and its first cluster
//!   cleared when nothing is kept; then the new last cluster's end of chain;
//!   then the rest of the chain freed.
//! - Creating a file or a directory: for a directory, its first cluster's
//!   contents (`.`, `..`, zeros), then its FAT entry; when the parent needs
//!   another cluster, that cluster zeroed, then linked as a file's is; then the
//!   parent's long-name entries and short entry.
//! - Removing: the entries marked free; then the clusters freed.
//! - Renaming to a name not in use: the entries under the new name, naming the
//!   same clusters; then, for a directory moved to another parent, its `..`;
//!   then the old entries marked free.
//! - Renaming over an existing name: the existing short entry rewritten to
//!   describe the renamed file, keeping its name, as Linux's `vfat_rename`
//!   does; then `..` for a moved directory; then the old entries marked free;
//!   then the replaced file's clusters freed.
//!
//! What `fsck_msdos -n` reports after a cut at each step is recorded by
//! `tools/fatdisk`'s cut test and described in README.md.
//!
//! **Marking the volume in use.** Before the first write after mounting, or
//! after a sync, the clean-shutdown flag in FAT[1] is cleared in every FAT copy
//! that is written (fatgen103, "FAT Data Structure", ClnShutBitMask), which is
//! what Windows and macOS read, and Linux's state byte in the boot sector gets
//! its dirty bit (`fat_set_state` in Linux's fs/fat/inode.c), which is what
//! Linux's fsck.fat reads. `sync` sets the flag and clears the bit again. A
//! volume whose FAT[1] said it was not dismounted cleanly when it was mounted
//! is never marked clean here; fsck_msdos on the Mac does that when it repairs
//! it. The hard-error flag beside it is kept as found.
//!
//! **What is kept between operations.** Up to `MAX_OPEN` files with the length
//! of their chains and the last cluster reached, so reading on does not walk
//! the chain from its start, and up to `MAX_DIRS` directories by path, because
//! the kernel resolves a path one component at a time and each component would
//! otherwise be found again from the root. An operation that fails after it
//! began writing drops everything it had not yet flushed, and every one of
//! these, and the next operation finds what the card holds.

use super::boot::{self, Layout};
use super::cache::{Blocks, Cache, BLOCK};
use super::dir::{self, Found, Item, Raw, Scan};
use super::name;
use super::table::{Fat, END, FREE};
use super::time;
use super::{join, FsError};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// Files kept open between operations.
pub const MAX_OPEN: usize = 32;
/// Directories remembered between operations, by path.
pub const MAX_DIRS: usize = 32;
/// New clusters found, filled and linked at a time when a file grows, which
/// bounds the list of them held in memory.
const ROUND_CLUSTERS: u32 = 1024;
/// The most bytes one read of file contents covers through adjacent clusters.
const RUN_BYTES: u64 = 1 << 20;
/// The zeros written at a time when a file is extended.
const ZERO_BYTES: usize = 128 * 1024;
/// FAT entries in one sector.
const ENTRIES_PER_BLOCK: u32 = (BLOCK / 4) as u32;
/// The largest file FAT holds: DIR_FileSize is 32 bits.
const MAX_FILE: u64 = u32::MAX as u64;

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

fn entry_of(found: &Found) -> Entry {
    let is_dir = found.raw.is_dir();
    Entry { name: found.name.clone(), is_dir, len: if is_dir { 0 } else { found.raw.size() }, mtime: found.raw.modified() }
}

struct Open {
    path: String,
    first: u32,
    size: u32,
    /// The position of its short entry.
    entry_at: u64,
    /// The length of its chain, and its last cluster when it has one.
    clusters: u32,
    last: u32,
    /// The last cluster found by its index in the chain.
    pos: Option<(u32, u32)>,
}

struct Known {
    path: String,
    cluster: u32,
}

/// What a write puts in a file: zeros, or bytes from a program.
enum Piece<'a> {
    Zeros(u64),
    Data(&'a [u8]),
}

impl Piece<'_> {
    fn len(&self) -> u64 {
        match self {
            Piece::Zeros(len) => *len,
            Piece::Data(data) => data.len() as u64,
        }
    }
}

/// Whether `path` is below `ancestor`.
fn is_under(path: &str, ancestor: &str) -> bool {
    if ancestor.is_empty() {
        return !path.is_empty();
    }
    path.len() > ancestor.len() && path.starts_with(ancestor) && path.as_bytes().get(ancestor.len()) == Some(&b'/')
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Whether every one of these byte positions is in the same sector, so one
/// write of that sector changes all of them at once.
fn one_sector(mut positions: impl Iterator<Item = u64>) -> bool {
    let Some(first) = positions.next() else { return true };
    positions.all(|at| at / BLOCK as u64 == first / BLOCK as u64)
}

pub struct Volume<B: Blocks> {
    cache: Cache<B>,
    fat: Fat,
    /// Seconds since 1970, UTC.
    clock: fn() -> i64,
    open: Vec<Open>,
    dirs: Vec<Known>,
    /// FAT[1] said the volume was not dismounted cleanly when it was mounted.
    dirty_at_mount: bool,
    /// The volume is marked in use on the card by this code.
    marked: bool,
}

impl<B: Blocks> Volume<B> {
    /// Mount the volume `layout` describes, on `blocks`, refusing writes until
    /// `allow_writes`. Reads FSInfo for where to look for free clusters and
    /// FAT[1] for whether the volume was dismounted cleanly; writes nothing.
    pub fn mount(blocks: B, layout: Layout, clock: fn() -> i64) -> Result<Volume<B>, FsError> {
        let mut cache = Cache::new(blocks);
        let mut hint = boot::FSI_UNKNOWN;
        if let Some(sector) = layout.fsinfo {
            let mut bytes = [0u8; BLOCK];
            cache.read_at(sector as u64 * BLOCK as u64, &mut bytes)?;
            if let Some((_, next)) = boot::fsinfo_hints(&bytes) {
                hint = next;
            }
        }
        let fat = Fat::new(layout, hint);
        let clean = fat.clean(&mut cache)?;
        Ok(Volume { cache, fat, clock, open: Vec::new(), dirs: Vec::new(), dirty_at_mount: !clean, marked: false })
    }

    pub fn layout(&self) -> Layout {
        self.fat.layout
    }

    /// Whether FAT[1] said, when the volume was mounted, that it had not been
    /// dismounted cleanly.
    pub fn dirty_at_mount(&self) -> bool {
        self.dirty_at_mount
    }

    pub fn allow_writes(&mut self) {
        self.cache.set_writable(true);
    }

    pub fn device(&self) -> &B {
        self.cache.device()
    }

    pub fn device_mut(&mut self) -> &mut B {
        self.cache.device_mut()
    }

    /// The blocks, with nothing unflushed: every operation flushes before it
    /// returns.
    pub fn into_device(self) -> B {
        self.cache.into_device()
    }

    fn cluster_bytes(&self) -> u64 {
        self.fat.layout.cluster_bytes as u64
    }

    fn offset_of(&self, cluster: u32) -> Result<u64, FsError> {
        self.fat.layout.cluster_offset(cluster).ok_or(FsError::Corrupt)
    }

    /// Run one operation, put what it wrote on the device, and after a failure
    /// drop what it had not flushed and everything remembered.
    fn run<T>(&mut self, op: impl FnOnce(&mut Self) -> Result<T, FsError>) -> Result<T, FsError> {
        let result = op(self);
        let result = match result {
            Ok(value) => self.cache.flush().map(|()| value).map_err(FsError::from),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            if self.cache.has_dirty() || error == FsError::Io {
                self.cache.discard();
                self.fat.forget_free();
                self.forget();
            } else if error == FsError::Corrupt {
                self.forget();
            }
        }
        result
    }

    fn forget(&mut self) {
        self.open.clear();
        self.dirs.clear();
    }

    /// Before an operation's first write: refuse it on a volume not allowed
    /// writes, and mark the volume in use if it is not marked.
    fn begin_write(&mut self) -> Result<(), FsError> {
        if !self.cache.writable() {
            return Err(FsError::ReadOnly);
        }
        if !self.marked {
            if !self.dirty_at_mount {
                self.fat.set_clean(&mut self.cache, false)?;
            }
            self.set_linux_state(true)?;
            self.cache.flush()?;
            self.marked = true;
        }
        Ok(())
    }

    fn set_linux_state(&mut self, dirty: bool) -> Result<(), FsError> {
        let mut byte = [0u8; 1];
        self.cache.read_at(boot::LINUX_STATE, &mut byte)?;
        let [old] = byte;
        let new = if dirty { old | boot::LINUX_STATE_DIRTY } else { old & !boot::LINUX_STATE_DIRTY };
        if new != old {
            self.cache.write_at(boot::LINUX_STATE, &[new])?;
        }
        Ok(())
    }

    // ---- directories by path -------------------------------------------

    /// The first cluster of the directory at `path`, found from the deepest
    /// remembered directory on the way to it, and remembered with every
    /// directory passed.
    fn dir_cluster(&mut self, path: &str) -> Result<u32, FsError> {
        let root = self.fat.layout.root_cluster;
        if path.is_empty() {
            return Ok(root);
        }
        if let Some(at) = self.dirs.iter().position(|known| known.path == path) {
            let known = self.dirs.remove(at);
            let cluster = known.cluster;
            self.dirs.push(known);
            return Ok(cluster);
        }
        let (mut walked, mut cluster) = self
            .dirs
            .iter()
            .filter(|known| is_under(path, &known.path))
            .max_by_key(|known| known.path.len())
            .map(|known| (known.path.clone(), known.cluster))
            .unwrap_or((String::new(), root));
        let rest = path.get(walked.len()..).unwrap_or("");
        for part in rest.split('/').filter(|part| !part.is_empty()) {
            let found = dir::find(&mut self.cache, &self.fat, cluster, part)?.ok_or(FsError::NotFound)?;
            if !found.raw.is_dir() {
                return Err(FsError::NotDir);
            }
            cluster = self.subdir_cluster(&found)?;
            walked = join(&walked, &found.name);
            self.remember(walked.clone(), cluster);
        }
        Ok(cluster)
    }

    /// The first cluster of a directory entry, which must be a cluster.
    fn subdir_cluster(&self, found: &Found) -> Result<u32, FsError> {
        let cluster = found.raw.cluster();
        if self.fat.layout.is_cluster(cluster) {
            Ok(cluster)
        } else {
            Err(FsError::Corrupt)
        }
    }

    fn remember(&mut self, path: String, cluster: u32) {
        self.dirs.retain(|known| known.path != path);
        if self.dirs.len() >= MAX_DIRS {
            self.dirs.remove(0);
        }
        self.dirs.push(Known { path, cluster });
    }

    /// The clusters of the root and of each directory along `path`.
    fn path_clusters(&mut self, path: &str) -> Result<Vec<u32>, FsError> {
        let mut out = vec![self.fat.layout.root_cluster];
        let mut walked = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            walked = join(&walked, part);
            out.push(self.dir_cluster(&walked)?);
        }
        Ok(out)
    }

    /// Forget every open file and remembered directory at or under `path`,
    /// before the entry there is removed or moved.
    fn close_under(&mut self, path: &str) {
        self.open.retain(|open| open.path != path && !is_under(&open.path, path));
        self.dirs.retain(|known| known.path != path && !is_under(&known.path, path));
    }

    // ---- open files ------------------------------------------------------

    fn opened(&self, slot: usize) -> Result<&Open, FsError> {
        self.open.get(slot).ok_or(FsError::Corrupt)
    }

    fn opened_mut(&mut self, slot: usize) -> Result<&mut Open, FsError> {
        self.open.get_mut(slot).ok_or(FsError::Corrupt)
    }

    /// The open file for `path`, opening it if it is not open. Opening walks
    /// its whole chain once, so a loop or a chain shorter than the file is
    /// found here rather than part way through a read.
    fn handle(&mut self, path: &str) -> Result<usize, FsError> {
        if let Some(at) = self.open.iter().position(|open| open.path == path) {
            let open = self.open.remove(at);
            self.open.push(open);
            return Ok(self.open.len() - 1);
        }
        let (parent, name) = split(path);
        let dir = self.dir_cluster(parent)?;
        let found = dir::find(&mut self.cache, &self.fat, dir, name)?.ok_or(FsError::NotFound)?;
        if found.raw.is_dir() {
            return Err(FsError::IsDir);
        }
        let (first, size) = (found.raw.cluster(), found.raw.size());
        let (clusters, last) = if first == 0 { (0, 0) } else { self.fat.walk(&mut self.cache, first)? };
        if clusters as u64 * self.cluster_bytes() < size as u64 {
            return Err(FsError::Corrupt);
        }
        if self.open.len() >= MAX_OPEN {
            self.open.remove(0);
        }
        self.open.push(Open { path: path.to_string(), first, size, entry_at: found.at, clusters, last, pos: None });
        Ok(self.open.len() - 1)
    }

    /// The cluster at `index` in the open file's chain.
    fn cluster_at(&mut self, slot: usize, index: u32) -> Result<u32, FsError> {
        let open = self.opened(slot)?;
        if index >= open.clusters {
            return Err(FsError::Corrupt);
        }
        if index + 1 == open.clusters {
            return Ok(open.last);
        }
        let (mut at, mut cluster) = match open.pos {
            Some((at, cluster)) if at <= index => (at, cluster),
            _ => (0, open.first),
        };
        // `index` is below the chain's length, which `walk` bounded by the
        // volume's cluster count.
        while at < index {
            cluster = self.fat.next(&mut self.cache, cluster)?.ok_or(FsError::Corrupt)?;
            at += 1;
        }
        self.opened_mut(slot)?.pos = Some((index, cluster));
        Ok(cluster)
    }

    /// From cluster `index` of the open file: that cluster, and how many bytes
    /// from `within` it lie in clusters that follow it on the volume, up to
    /// `remaining` and `RUN_BYTES`, so they can be moved in one call.
    fn run_at(&mut self, slot: usize, index: u32, within: u64, remaining: u64) -> Result<(u32, u64), FsError> {
        let cluster_bytes = self.cluster_bytes();
        let clusters = self.opened(slot)?.clusters;
        let first = self.cluster_at(slot, index)?;
        let (mut count, mut last) = (1u32, first);
        while (count as u64 * cluster_bytes - within) < remaining && (count as u64 * cluster_bytes) < RUN_BYTES && index + count < clusters {
            let next = self.cluster_at(slot, index + count)?;
            if Some(next) != last.checked_add(1) {
                break;
            }
            last = next;
            count += 1;
        }
        Ok((first, (count as u64 * cluster_bytes - within).min(remaining)))
    }

    // ---- writing file contents ---------------------------------------------

    fn write_piece(&mut self, piece: &Piece, zeros: &[u8], at: u64, from: u64, len: u64) -> Result<(), FsError> {
        match piece {
            Piece::Data(data) => {
                let from = usize::try_from(from).map_err(|_| FsError::TooBig)?;
                let end = from.checked_add(usize::try_from(len).map_err(|_| FsError::TooBig)?).ok_or(FsError::TooBig)?;
                self.cache.write_at(at, data.get(from..end).ok_or(FsError::Corrupt)?)?;
            }
            Piece::Zeros(_) => {
                let mut done = 0u64;
                while done < len {
                    let n = (len - done).min(zeros.len() as u64);
                    if n == 0 {
                        return Err(FsError::Corrupt);
                    }
                    self.cache.write_at(at + done, zeros.get(..n as usize).ok_or(FsError::Corrupt)?)?;
                    done += n;
                }
            }
        }
        Ok(())
    }

    /// Link clusters whose contents are written: their own FAT entries first,
    /// the last one ending the chain, and then the entry of `prev`, the chain's
    /// old last cluster, pointing at the first of them. The two go to the card
    /// in separate flushes unless every entry is in one FAT sector, so that no
    /// cut leaves the old chain running into an entry not yet written.
    fn link_new(&mut self, prev: Option<u32>, found: &[u32]) -> Result<(), FsError> {
        let (Some(&first), Some(&last)) = (found.first(), found.last()) else {
            return Ok(());
        };
        for pair in found.windows(2) {
            if let [from, to] = *pair {
                self.fat.set(&mut self.cache, from, to)?;
            }
        }
        self.fat.set(&mut self.cache, last, END)?;
        if let Some(prev) = prev {
            let sector = prev / ENTRIES_PER_BLOCK;
            if found.iter().any(|&cluster| cluster / ENTRIES_PER_BLOCK != sector) {
                self.cache.flush()?;
            }
            self.fat.set(&mut self.cache, prev, first)?;
        }
        self.cache.flush()?;
        self.fat.took(found.len() as u32);
        Ok(())
    }

    /// Put `piece` in the open file from byte `start`, which is at most its
    /// length: into the clusters it has, then into new ones, which are found,
    /// written and linked a round at a time. Every byte is on the card when
    /// this returns; the directory entry is not changed.
    fn fill(&mut self, slot: usize, start: u64, piece: Piece) -> Result<(), FsError> {
        let cluster_bytes = self.cluster_bytes();
        let total = piece.len();
        let zeros = match piece {
            Piece::Zeros(_) => vec![0u8; ZERO_BYTES],
            Piece::Data(_) => Vec::new(),
        };
        let mut done = 0u64;
        while done < total {
            let pos = start + done;
            let index = u32::try_from(pos / cluster_bytes).map_err(|_| FsError::TooBig)?;
            let within = pos % cluster_bytes;
            let open = self.opened(slot)?;
            let (clusters, last) = (open.clusters, open.last);
            if index < clusters {
                let (cluster, n) = self.run_at(slot, index, within, total - done)?;
                let at = self.offset_of(cluster)? + within;
                self.write_piece(&piece, &zeros, at, done, n)?;
                done += n;
                continue;
            }
            // Past the chain's end, which is where the file's length is.
            if index != clusters || within != 0 {
                return Err(FsError::Corrupt);
            }
            let want = (total - done).div_ceil(cluster_bytes).min(ROUND_CLUSTERS as u64) as u32;
            let mut found = Vec::new();
            self.fat.find_free(&mut self.cache, want, &mut found)?;
            if found.is_empty() {
                return Err(FsError::NoSpace);
            }
            let mut written = 0u64;
            let mut i = 0usize;
            while let Some(&first) = found.get(i) {
                let mut count = 1usize;
                while found.get(i + count).copied() == first.checked_add(count as u32) {
                    count += 1;
                }
                let n = (count as u64 * cluster_bytes).min(total - done - written);
                let at = self.offset_of(first)?;
                self.write_piece(&piece, &zeros, at, done + written, n)?;
                written += n;
                i += count;
            }
            // The contents are on the card before a FAT entry names them.
            self.cache.flush()?;
            self.link_new(if clusters > 0 { Some(last) } else { None }, &found)?;
            let open = self.opened_mut(slot)?;
            if clusters == 0 {
                open.first = found.first().copied().unwrap_or(0);
            }
            open.clusters += found.len() as u32;
            open.last = found.last().copied().unwrap_or(0);
            done += written;
        }
        self.cache.flush()?;
        Ok(())
    }

    /// Point the open file's directory entry at its first cluster, give it
    /// `size`, and stamp it modified now.
    fn update_entry(&mut self, slot: usize, size: u32) -> Result<(), FsError> {
        let now = (self.clock)();
        let open = self.opened(slot)?;
        let (entry_at, first) = (open.entry_at, open.first);
        let mut bytes = [0u8; 32];
        self.cache.read_at(entry_at, &mut bytes)?;
        let mut raw = Raw(bytes);
        if raw.is_long() || raw.is_dir() || matches!(raw.first_byte(), dir::FREE_MARK | dir::END_MARK) {
            return Err(FsError::Corrupt);
        }
        raw.set_cluster(first);
        raw.set_size(size);
        raw.set_modified(time::encode(now));
        raw.set_attr(raw.attr() | dir::ATTR_ARCHIVE);
        if raw.0 != bytes {
            self.cache.write_at(entry_at, &raw.0)?;
            self.cache.flush()?;
        }
        self.opened_mut(slot)?.size = size;
        Ok(())
    }

    /// How far a file of `clusters` clusters can reach with the free clusters
    /// the volume has, in bytes.
    fn room(&mut self, clusters: u32) -> Result<u64, FsError> {
        let free = self.fat.free_count(&mut self.cache)?;
        Ok((clusters as u64 + free as u64) * self.cluster_bytes())
    }

    // ---- operations ----------------------------------------------------------

    /// The entry called `name` in the directory `dir`. A directory found is
    /// remembered, because the kernel's path walk asks for it next.
    pub fn lookup(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        self.run(|volume| {
            let cluster = volume.dir_cluster(dir)?;
            if name::key(name).is_empty() {
                return Err(FsError::NotFound);
            }
            let found = dir::find(&mut volume.cache, &volume.fat, cluster, name)?.ok_or(FsError::NotFound)?;
            if found.raw.is_dir() {
                if let Ok(child) = volume.subdir_cluster(&found) {
                    volume.remember(join(dir, &found.name), child);
                }
            }
            Ok(entry_of(&found))
        })
    }

    /// Every entry in the directory `dir` but `.`, `..`, the label, and any
    /// whose name no path could spell.
    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>, FsError> {
        self.run(|volume| {
            let cluster = volume.dir_cluster(dir)?;
            let mut scan = Scan::new(&volume.fat, cluster)?;
            let mut out = Vec::new();
            while let Some(item) = scan.next(&mut volume.cache, &volume.fat)? {
                match item {
                    Item::Entry(found) => out.push(entry_of(&found)),
                    Item::End { .. } => break,
                    _ => {}
                }
            }
            Ok(out)
        })
    }

    pub fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.run(|volume| {
            let slot = volume.handle(path)?;
            let size = volume.opened(slot)?.size as u64;
            if offset >= size || buf.is_empty() {
                return Ok(0);
            }
            let want = (size - offset).min(buf.len() as u64);
            let cluster_bytes = volume.cluster_bytes();
            let mut done = 0u64;
            while done < want {
                let pos = offset + done;
                let index = u32::try_from(pos / cluster_bytes).map_err(|_| FsError::Corrupt)?;
                let (cluster, n) = volume.run_at(slot, index, pos % cluster_bytes, want - done)?;
                let at = volume.offset_of(cluster)? + pos % cluster_bytes;
                let part = buf.get_mut(done as usize..(done + n) as usize).ok_or(FsError::Corrupt)?;
                volume.cache.read_at(at, part)?;
                done += n;
            }
            Ok(done as usize)
        })
    }

    /// Write `data` at `offset`, with zeros before it from the file's end when
    /// `offset` is past it: FAT has no holes. Returns the bytes written, fewer
    /// than all when the volume fills part way.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let end = offset.checked_add(data.len() as u64).ok_or(FsError::TooBig)?;
        if end > MAX_FILE {
            return Err(FsError::TooBig);
        }
        if data.is_empty() {
            return Ok(0);
        }
        self.run(|volume| {
            let slot = volume.handle(path)?;
            let open = volume.opened(slot)?;
            let (size, clusters) = (open.size as u64, open.clusters);
            let mut end = end;
            if end.div_ceil(volume.cluster_bytes()) > clusters as u64 {
                let room = volume.room(clusters)?;
                if room <= offset {
                    return Err(FsError::NoSpace);
                }
                end = end.min(room);
            }
            let data = data.get(..(end - offset) as usize).ok_or(FsError::Corrupt)?;
            volume.begin_write()?;
            if offset > size {
                volume.fill(slot, size, Piece::Zeros(offset - size))?;
            }
            volume.fill(slot, offset, Piece::Data(data))?;
            volume.update_entry(slot, size.max(end) as u32)?;
            Ok(data.len())
        })
    }

    pub fn truncate(&mut self, path: &str, len: u64) -> Result<(), FsError> {
        if len > MAX_FILE {
            return Err(FsError::TooBig);
        }
        self.run(|volume| {
            let slot = volume.handle(path)?;
            let open = volume.opened(slot)?;
            let (size, clusters) = (open.size as u64, open.clusters);
            let keep = len.div_ceil(volume.cluster_bytes()) as u32;
            if len > size {
                if keep > clusters && volume.room(clusters)? < len {
                    return Err(FsError::NoSpace);
                }
                volume.begin_write()?;
                volume.fill(slot, size, Piece::Zeros(len - size))?;
                return volume.update_entry(slot, len as u32);
            }
            volume.begin_write()?;
            if keep >= clusters {
                return volume.update_entry(slot, len as u32);
            }
            let new_last = if keep > 0 { Some(volume.cluster_at(slot, keep - 1)?) } else { None };
            let rest = match new_last {
                Some(cluster) => volume.fat.next(&mut volume.cache, cluster)?.ok_or(FsError::Corrupt)?,
                None => volume.opened(slot)?.first,
            };
            // The entry first, so that no cut leaves it naming freed clusters.
            if keep == 0 {
                volume.opened_mut(slot)?.first = 0;
            }
            volume.update_entry(slot, len as u32)?;
            if let Some(cluster) = new_last {
                volume.fat.set(&mut volume.cache, cluster, END)?;
                volume.cache.flush()?;
            }
            volume.fat.free_chain(&mut volume.cache, rest, clusters - keep)?;
            volume.cache.flush()?;
            let open = volume.opened_mut(slot)?;
            open.clusters = keep;
            open.last = new_last.unwrap_or(0);
            open.pos = None;
            Ok(())
        })
    }

    /// Add entries for `name` to the directory at `dir`, describing what
    /// `template` describes, and return their positions. `except` is the
    /// position of an entry that does not count as the name being taken: the
    /// one a rename is respelling. The entries are left in the cache for the
    /// caller to flush, so that a rename can put them on the card in the same
    /// write as the removal of the old ones when both are in one sector.
    fn add_entry(&mut self, dir: u32, name: &str, template: Raw, except: Option<u64>) -> Result<Vec<u64>, FsError> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let long_slots = units.len().div_ceil(name::UNITS_PER_ENTRY) as u32;
        let survey = dir::survey(&mut self.cache, &self.fat, dir, name, long_slots + 1, except)?;
        if survey.found.is_some() {
            return Err(FsError::Exists);
        }
        let (short, long) = name::short_name(name, |short| survey.taken(short)).ok_or(FsError::NoSpace)?;
        let count = if long { long_slots + 1 } else { 1 };
        let (start, missing) = survey.place(count);
        if start as u64 + count as u64 > dir::MAX_ENTRIES as u64 {
            return Err(FsError::NoSpace);
        }
        let per_cluster = (self.cluster_bytes() / dir::ENTRY_BYTES) as u32;
        if missing > 0 && self.fat.free_count(&mut self.cache)? < missing.div_ceil(per_cluster) {
            return Err(FsError::NoSpace);
        }
        self.begin_write()?;
        if missing > 0 {
            self.grow(survey.last_cluster, missing)?;
        }
        // Entries placed past the directory's end mark, with slots between:
        // those slots are marked free rather than ending the directory, first,
        // or a reader would stop before the new entries. Until the entries are
        // written, a reader then goes on to zeros, which end the directory.
        if let Some(end) = survey.end.filter(|&end| start > end) {
            for at in dir::positions(&mut self.cache, &self.fat, dir, end, start - end)? {
                self.cache.write_at(at, &[dir::FREE_MARK])?;
            }
            self.cache.flush()?;
        }
        let positions = dir::positions(&mut self.cache, &self.fat, dir, start, count + 1)?;
        if (positions.len() as u32) < count {
            return Err(FsError::Corrupt);
        }
        let mut raw = template;
        raw.set_short(short, 0);
        let mut entries = if long { name::long_entries(&units, name::checksum(&short)) } else { Vec::new() };
        entries.push(raw.0);
        for (&at, entry) in positions.iter().zip(entries.iter()) {
            self.cache.write_at(at, entry)?;
        }
        // Entries written at or past the directory's end mark: the slot after
        // them carries the mark, whatever was left in it.
        if survey.end.is_some_and(|end| start + count > end) {
            if let Some(&after) = positions.get(count as usize) {
                let mut first = [0u8; 1];
                self.cache.read_at(after, &mut first)?;
                if first != [dir::END_MARK] {
                    self.cache.write_at(after, &[dir::END_MARK])?;
                }
            }
        }
        Ok(positions.into_iter().take(count as usize).collect())
    }

    /// Add clusters to a directory whose chain ends at `last`, enough for
    /// `slots` more entries: found, zeroed, then linked.
    fn grow(&mut self, last: u32, slots: u32) -> Result<(), FsError> {
        let cluster_bytes = self.cluster_bytes();
        let want = slots.div_ceil((cluster_bytes / dir::ENTRY_BYTES) as u32);
        let mut found = Vec::new();
        self.fat.find_free(&mut self.cache, want, &mut found)?;
        if (found.len() as u32) < want {
            return Err(FsError::NoSpace);
        }
        let zeros = vec![0u8; cluster_bytes as usize];
        for &cluster in &found {
            let at = self.offset_of(cluster)?;
            self.cache.write_at(at, &zeros)?;
        }
        self.cache.flush()?;
        self.link_new(Some(last), &found)
    }

    pub fn create(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        let name = name::check(name)?;
        self.run(|volume| {
            let cluster = volume.dir_cluster(dir)?;
            let stamp = time::encode((volume.clock)());
            let raw = Raw::new([b' '; 11], dir::ATTR_ARCHIVE, 0, 0, stamp);
            volume.add_entry(cluster, name, raw, None)?;
            volume.cache.flush()?;
            Ok(Entry { name: name.to_string(), is_dir: false, len: 0, mtime: raw.modified() })
        })
    }

    pub fn mkdir(&mut self, dir: &str, name: &str) -> Result<Entry, FsError> {
        let name = name::check(name)?;
        self.run(|volume| {
            let parent = volume.dir_cluster(dir)?;
            if dir::find(&mut volume.cache, &volume.fat, parent, name)?.is_some() {
                return Err(FsError::Exists);
            }
            if volume.fat.free_count(&mut volume.cache)? == 0 {
                return Err(FsError::NoSpace);
            }
            volume.begin_write()?;
            let stamp = time::encode((volume.clock)());
            let mut found = Vec::new();
            volume.fat.find_free(&mut volume.cache, 1, &mut found)?;
            let own = found.first().copied().ok_or(FsError::NoSpace)?;
            let layout = volume.fat.layout;
            let parent_field = if parent == layout.root_cluster { 0 } else { parent };
            let at = volume.offset_of(own)?;
            volume.cache.write_at(at, &dir::new_directory(&layout, own, parent_field, stamp))?;
            volume.cache.flush()?;
            volume.link_new(None, &[own])?;
            let raw = Raw::new([b' '; 11], dir::ATTR_DIRECTORY, own, 0, stamp);
            if let Err(error) = volume.add_entry(parent, name, raw, None) {
                // Nothing names the new cluster yet: give it back rather than
                // leave it lost, unless the failure left writes unflushed.
                if !volume.cache.has_dirty() && error != FsError::Io {
                    volume.fat.set(&mut volume.cache, own, FREE)?;
                    volume.cache.flush()?;
                    volume.fat.gave(1);
                }
                return Err(error);
            }
            volume.cache.flush()?;
            Ok(Entry { name: name.to_string(), is_dir: true, len: 0, mtime: raw.modified() })
        })
    }

    /// The chain an entry naming `first` owns, measured: nothing when there is
    /// none, when it starts at the root directory, which no entry owns, or when
    /// it is damaged. Removing the entry then frees nothing, and fsck_msdos
    /// finds the clusters.
    fn owned_chain(&mut self, first: u32) -> Result<Option<(u32, u32)>, FsError> {
        let layout = self.fat.layout;
        if first == 0 || first == layout.root_cluster || !layout.is_cluster(first) {
            return Ok(None);
        }
        match self.fat.walk(&mut self.cache, first) {
            Ok((count, _)) => Ok(Some((first, count))),
            Err(FsError::Corrupt) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Mark an entry's slots free, its long-name entries before its short
    /// entry. When they are not all in one sector the long-name entries are
    /// flushed first: a cut between the two then leaves the short entry alone,
    /// which fsck_msdos accepts, where the other order leaves long-name
    /// entries that no short entry owns, which it reports. Left in the cache
    /// for the caller to flush.
    fn mark_free(&mut self, found: &Found) -> Result<(), FsError> {
        for at in found.positions().filter(|&at| at != found.at) {
            self.cache.write_at(at, &[dir::FREE_MARK])?;
        }
        if !one_sector(found.positions()) {
            self.cache.flush()?;
        }
        self.cache.write_at(found.at, &[dir::FREE_MARK])?;
        Ok(())
    }

    fn free_owned(&mut self, chain: Option<(u32, u32)>) -> Result<(), FsError> {
        if let Some((first, count)) = chain {
            self.fat.free_chain(&mut self.cache, first, count)?;
            self.cache.flush()?;
        }
        Ok(())
    }

    /// Remove the entry `name` in `dir_path`, which must be a directory when
    /// `want_dir` and a file otherwise; a directory must be empty.
    pub fn remove(&mut self, dir_path: &str, name: &str, want_dir: bool) -> Result<(), FsError> {
        self.run(|volume| {
            let parent = volume.dir_cluster(dir_path)?;
            let found = dir::find(&mut volume.cache, &volume.fat, parent, name)?.ok_or(FsError::NotFound)?;
            let is_dir = found.raw.is_dir();
            if is_dir != want_dir {
                return Err(if is_dir { FsError::IsDir } else { FsError::NotDir });
            }
            let first = found.raw.cluster();
            if is_dir && volume.fat.layout.is_cluster(first) && !dir::is_empty(&mut volume.cache, &volume.fat, first)? {
                return Err(FsError::NotEmpty);
            }
            let chain = volume.owned_chain(first)?;
            volume.close_under(&join(dir_path, &found.name));
            volume.begin_write()?;
            volume.mark_free(&found)?;
            volume.cache.flush()?;
            volume.free_owned(chain)
        })
    }

    /// The `..` entry of the directory starting at `first`, which must be
    /// there for the directory to be moved to another parent.
    fn dotdot(&mut self, first: u32) -> Result<(u64, Raw), FsError> {
        let at = self.offset_of(first)? + dir::ENTRY_BYTES;
        let mut bytes = [0u8; 32];
        self.cache.read_at(at, &mut bytes)?;
        let raw = Raw(bytes);
        if raw.short() != dir::DOTDOT || !raw.is_dir() {
            return Err(FsError::Corrupt);
        }
        Ok((at, raw))
    }

    /// Point `..` of the directory starting at `first` at `parent`: 0 for the
    /// root, as fatgen103 says, and the parent's first cluster otherwise.
    fn point_dotdot(&mut self, first: u32, parent: u32) -> Result<(), FsError> {
        let (at, mut raw) = self.dotdot(first)?;
        raw.set_cluster(if parent == self.fat.layout.root_cluster { 0 } else { parent });
        self.cache.write_at(at, &raw.0)?;
        self.cache.flush()?;
        Ok(())
    }

    /// Rename, replacing an existing entry of the same kind at the destination.
    /// Returns the name the entry now has on the card.
    pub fn rename(&mut self, from_dir: &str, from_name: &str, to_dir: &str, to_name: &str) -> Result<String, FsError> {
        let to_name = name::check(to_name)?;
        self.run(|volume| {
            let from_cluster = volume.dir_cluster(from_dir)?;
            let source = dir::find(&mut volume.cache, &volume.fat, from_cluster, from_name)?.ok_or(FsError::NotFound)?;
            let source_path = join(from_dir, &source.name);
            let is_dir = source.raw.is_dir();
            let to_cluster = volume.dir_cluster(to_dir)?;
            let moves = is_dir && from_cluster != to_cluster;
            if is_dir {
                if to_dir == source_path || is_under(to_dir, &source_path) {
                    return Err(FsError::Invalid);
                }
                // By cluster as well as by path, for a damaged tree where two
                // paths lead to one directory.
                if volume.path_clusters(to_dir)?.contains(&source.raw.cluster()) {
                    return Err(FsError::Invalid);
                }
            }
            if moves {
                volume.dotdot(volume.subdir_cluster(&source)?)?;
            }
            let target = dir::find(&mut volume.cache, &volume.fat, to_cluster, to_name)?;
            match target {
                Some(target) if target.at == source.at => {
                    if source.name == to_name {
                        return Ok(source.name);
                    }
                    // The same entry under a new spelling.
                    volume.close_under(&source_path);
                    let written = volume.add_entry(to_cluster, to_name, source.raw, Some(source.at))?;
                    if !one_sector(written.iter().copied().chain(source.positions())) {
                        volume.cache.flush()?;
                    }
                    volume.mark_free(&source)?;
                    volume.cache.flush()?;
                    Ok(to_name.to_string())
                }
                Some(target) => {
                    let target_dir = target.raw.is_dir();
                    if is_dir && !target_dir {
                        return Err(FsError::NotDir);
                    }
                    if !is_dir && target_dir {
                        return Err(FsError::IsDir);
                    }
                    let target_first = target.raw.cluster();
                    if target_dir && volume.fat.layout.is_cluster(target_first) && !dir::is_empty(&mut volume.cache, &volume.fat, target_first)? {
                        return Err(FsError::NotEmpty);
                    }
                    let replaced = volume.owned_chain(target_first)?;
                    volume.close_under(&source_path);
                    volume.close_under(&join(to_dir, &target.name));
                    volume.begin_write()?;
                    let mut raw = target.raw;
                    raw.take_contents(&source.raw);
                    volume.cache.write_at(target.at, &raw.0)?;
                    if moves || !one_sector(core::iter::once(target.at).chain(source.positions())) {
                        volume.cache.flush()?;
                    }
                    if moves {
                        volume.point_dotdot(source.raw.cluster(), to_cluster)?;
                    }
                    volume.mark_free(&source)?;
                    volume.cache.flush()?;
                    volume.free_owned(replaced)?;
                    Ok(target.name)
                }
                None => {
                    volume.close_under(&source_path);
                    let written = volume.add_entry(to_cluster, to_name, source.raw, None)?;
                    if moves || !one_sector(written.iter().copied().chain(source.positions())) {
                        volume.cache.flush()?;
                    }
                    if moves {
                        volume.point_dotdot(source.raw.cluster(), to_cluster)?;
                    }
                    volume.mark_free(&source)?;
                    volume.cache.flush()?;
                    Ok(to_name.to_string())
                }
            }
        })
    }

    /// Nothing an operation writes waits in memory after it returns, so there
    /// is nothing more to put on the card for one file.
    pub fn fsync(&mut self, _path: &str) -> Result<(), FsError> {
        self.run(|_| Ok(()))
    }

    /// Write FSInfo's free count and next-free hint from the FAT, and mark
    /// the volume dismounted cleanly, unless it was not clean when mounted.
    /// Unmounting is the same: nothing else is held in memory.
    pub fn sync(&mut self) -> Result<(), FsError> {
        self.run(|volume| {
            if !volume.marked {
                return Ok(());
            }
            let free = volume.fat.free_count(&mut volume.cache)?;
            let next = volume.fat.next_free_hint(&mut volume.cache)?;
            if let Some(sector) = volume.fat.layout.fsinfo {
                let offset = sector as u64 * BLOCK as u64;
                let mut bytes = [0u8; BLOCK];
                volume.cache.read_at(offset, &mut bytes)?;
                if boot::fsinfo_hints(&bytes).is_some_and(|hints| hints != (free, next)) {
                    boot::set_fsinfo_hints(&mut bytes, free, next);
                    volume.cache.write_at(offset, &bytes)?;
                    volume.cache.flush()?;
                }
            }
            if !volume.dirty_at_mount {
                volume.fat.set_clean(&mut volume.cache, true)?;
                volume.set_linux_state(false)?;
                volume.cache.flush()?;
                volume.marked = false;
            }
            Ok(())
        })
    }

    /// Cluster size in bytes, total clusters and free clusters.
    pub fn stats(&mut self) -> Result<(u32, u32, u32), FsError> {
        self.run(|volume| {
            let layout = volume.fat.layout;
            Ok((layout.cluster_bytes, layout.clusters, volume.fat.free_count(&mut volume.cache)?))
        })
    }

    /// The name of a file in the root directory that only a boot partition
    /// holds, if there is one.
    pub fn boot_file(&mut self) -> Result<Option<String>, FsError> {
        self.run(|volume| {
            let mut scan = Scan::new(&volume.fat, volume.fat.layout.root_cluster)?;
            while let Some(item) = scan.next(&mut volume.cache, &volume.fat)? {
                match item {
                    Item::Entry(found) if !found.raw.is_dir() && (name::same(&found.name, "start4.elf") || name::same(&found.name, "kernel8.img")) => {
                        return Ok(Some(found.name));
                    }
                    Item::End { .. } => break,
                    _ => {}
                }
            }
            Ok(None)
        })
    }

    /// The clusters of the entry `name` in `dir`, in chain order, for
    /// `tools/fatdisk`'s check that no two files share one.
    pub fn clusters_of(&mut self, dir: &str, name: &str) -> Result<Vec<u32>, FsError> {
        self.run(|volume| {
            let parent = volume.dir_cluster(dir)?;
            let found = dir::find(&mut volume.cache, &volume.fat, parent, name)?.ok_or(FsError::NotFound)?;
            let first = found.raw.cluster();
            let mut out = Vec::new();
            if first == 0 {
                return Ok(out);
            }
            let (count, _) = volume.fat.walk(&mut volume.cache, first)?;
            let mut cluster = first;
            for i in 0..count {
                out.push(cluster);
                if i + 1 < count {
                    cluster = volume.fat.next(&mut volume.cache, cluster)?.ok_or(FsError::Corrupt)?;
                }
            }
            Ok(out)
        })
    }
}
