//! Directories: their 32-byte entries, reading names out of them, and finding
//! room in them (fatgen103, "FAT Directory Structure" and "FAT Long Directory
//! Entries").
//!
//! A directory is a cluster chain of 32-byte entries. The first byte of an
//! entry is 0xE5 for a free entry and 0x00 for the end of the directory, after
//! which every entry is free. An entry whose attribute byte, masked with 0x3F,
//! is 0x0F holds 13 UTF-16 units of a long name; the short entry that owns a
//! run of them follows the run, and each carries the short name's checksum.
//! The root has no `.` or `..`; every other directory starts with both.
//!
//! **Bounds.** fatgen103 limits a directory to 65536 entries, so a scan stops
//! with an error when a chain goes on past that many, which is also what ends a
//! scan along a directory chain that loops.
//!
//! **Damaged long names.** A run of long-name entries is used only when its
//! ordinals count down from the one flagged 0x40 to 1 without a gap, every
//! entry carries the same checksum, the checksum is the short entry's, and the
//! units are valid UTF-16. Otherwise the entry is shown by its short name, as
//! fatgen103 says of an orphaned long name.

use super::boot::Layout;
use super::cache::{Blocks, Cache};
use super::name::{self, Short, LAST_LONG_ENTRY, MAX_LONG_ENTRIES, MAX_UNITS, UNITS_PER_ENTRY};
use super::table::Fat;
use super::time::{self, Stamp};
use super::{le16, le32, put16, put32, put8, FsError};
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const ENTRY_BYTES: u64 = 32;
pub const MAX_ENTRIES: u32 = 65_536;
/// Entries in one 512-byte sector. A cluster is a whole number of sectors, so
/// a sector boundary falls at every sixteenth index of a directory.
pub const SLOTS_PER_SECTOR: u32 = 16;

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;
const ATTR_LONG_NAME_MASK: u8 = ATTR_LONG_NAME | ATTR_DIRECTORY | ATTR_ARCHIVE;

pub const FREE_MARK: u8 = 0xE5;
pub const END_MARK: u8 = 0x00;
pub const DOT: Short = *b".          ";
pub const DOTDOT: Short = *b"..         ";

/// One 32-byte entry as it is on the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raw(pub [u8; 32]);

impl Raw {
    /// A short entry. Creation, modification and access are all `stamp`.
    pub fn new(short: Short, attr: u8, cluster: u32, size: u32, stamp: Stamp) -> Raw {
        let mut raw = Raw([0u8; 32]);
        raw.set_short(short, 0);
        put8(&mut raw.0, 11, attr);
        put8(&mut raw.0, 13, stamp.hundredths);
        put16(&mut raw.0, 14, stamp.time);
        put16(&mut raw.0, 16, stamp.date);
        put16(&mut raw.0, 18, stamp.date);
        raw.set_cluster(cluster);
        raw.set_modified(stamp);
        raw.set_size(size);
        raw
    }

    fn byte(&self, at: usize) -> u8 {
        self.0.get(at).copied().unwrap_or(0)
    }

    /// DIR_Name.
    pub fn short(&self) -> Short {
        let mut short = [0u8; 11];
        for (slot, &byte) in short.iter_mut().zip(self.0.iter()) {
            *slot = byte;
        }
        short
    }

    /// DIR_Name, and DIR_NTRes, whose case flags belong to the name.
    pub fn set_short(&mut self, short: Short, lower: u8) {
        for (slot, &byte) in self.0.iter_mut().zip(short.iter()) {
            *slot = byte;
        }
        put8(&mut self.0, 12, lower);
    }

    pub fn first_byte(&self) -> u8 {
        self.byte(0)
    }

    pub fn attr(&self) -> u8 {
        self.byte(11)
    }

    pub fn set_attr(&mut self, attr: u8) {
        put8(&mut self.0, 11, attr);
    }

    /// DIR_NTRes.
    pub fn lower(&self) -> u8 {
        self.byte(12)
    }

    pub fn checksum_field(&self) -> u8 {
        self.byte(13)
    }

    pub fn is_long(&self) -> bool {
        self.attr() & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME
    }

    pub fn is_volume(&self) -> bool {
        !self.is_long() && self.attr() & ATTR_VOLUME_ID != 0
    }

    pub fn is_dir(&self) -> bool {
        self.attr() & ATTR_DIRECTORY != 0
    }

    /// DIR_FstClusHI and DIR_FstClusLO.
    pub fn cluster(&self) -> u32 {
        ((le16(&self.0, 20) as u32) << 16) | le16(&self.0, 26) as u32
    }

    pub fn set_cluster(&mut self, cluster: u32) {
        put16(&mut self.0, 20, (cluster >> 16) as u16);
        put16(&mut self.0, 26, cluster as u16);
    }

    /// DIR_FileSize.
    pub fn size(&self) -> u32 {
        le32(&self.0, 28)
    }

    pub fn set_size(&mut self, size: u32) {
        put32(&mut self.0, 28, size);
    }

    /// DIR_WrtDate and DIR_WrtTime as seconds since 1970.
    pub fn modified(&self) -> i64 {
        time::decode(le16(&self.0, 24), le16(&self.0, 22))
    }

    pub fn set_modified(&mut self, stamp: Stamp) {
        put16(&mut self.0, 22, stamp.time);
        put16(&mut self.0, 24, stamp.date);
    }

    /// Everything of `other` but its name and the case flags that go with the
    /// name: the attributes, the times, the first cluster and the size. What a
    /// rename that reuses an entry moves into it.
    pub fn take_contents(&mut self, other: &Raw) {
        for (at, (slot, &byte)) in self.0.iter_mut().zip(other.0.iter()).enumerate() {
            if at >= 11 && at != 12 {
                *slot = byte;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Walking the slots of a directory
// ---------------------------------------------------------------------------

/// The slots of one directory in order, with where each is on the volume.
pub struct Slots {
    cluster: u32,
    within: u32,
    per_cluster: u32,
    index: u32,
}

impl Slots {
    pub fn new(fat: &Fat, first: u32) -> Result<Slots, FsError> {
        if !fat.layout.is_cluster(first) {
            return Err(FsError::Corrupt);
        }
        Ok(Slots { cluster: first, within: 0, per_cluster: fat.layout.cluster_bytes / ENTRY_BYTES as u32, index: 0 })
    }

    /// The next slot: its index in the directory, its byte position on the
    /// volume, and its bytes. Nothing past the end of the chain.
    pub fn next<B: Blocks>(&mut self, cache: &mut Cache<B>, fat: &Fat) -> Result<Option<(u32, u64, Raw)>, FsError> {
        if self.within >= self.per_cluster {
            match fat.next(cache, self.cluster)? {
                None => return Ok(None),
                Some(next) => {
                    self.cluster = next;
                    self.within = 0;
                }
            }
        }
        if self.index >= MAX_ENTRIES {
            return Err(FsError::Corrupt);
        }
        let at = fat.layout.cluster_offset(self.cluster).ok_or(FsError::Corrupt)? + self.within as u64 * ENTRY_BYTES;
        let mut bytes = [0u8; 32];
        cache.read_at(at, &mut bytes)?;
        let index = self.index;
        self.index += 1;
        self.within += 1;
        Ok(Some((index, at, Raw(bytes))))
    }

    /// The cluster the last slot came from.
    pub fn cluster(&self) -> u32 {
        self.cluster
    }

    /// Slots returned so far.
    pub fn count(&self) -> u32 {
        self.index
    }
}

/// The positions of up to `count` slots from index `start` in the directory at
/// `first`; fewer when its chain ends first.
pub fn positions<B: Blocks>(cache: &mut Cache<B>, fat: &Fat, first: u32, start: u32, count: u32) -> Result<Vec<u64>, FsError> {
    let per_cluster = fat.layout.cluster_bytes / ENTRY_BYTES as u32;
    let mut cluster = first;
    // Clusters wholly before `start` are passed by their FAT links alone. A
    // directory has at most 65536 entries, so this is at most 4096 links.
    for _ in 0..start / per_cluster {
        match fat.next(cache, cluster)? {
            Some(next) => cluster = next,
            None => return Ok(Vec::new()),
        }
    }
    let mut out = Vec::new();
    for index in start..start.saturating_add(count) {
        if index != start && index % per_cluster == 0 {
            match fat.next(cache, cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }
        let base = fat.layout.cluster_offset(cluster).ok_or(FsError::Corrupt)?;
        out.push(base + (index % per_cluster) as u64 * ENTRY_BYTES);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Names out of slots
// ---------------------------------------------------------------------------

/// The long-name entries seen since the last short entry.
struct Long {
    units: [u16; MAX_LONG_ENTRIES * UNITS_PER_ENTRY],
    /// Entries in the run, from the ordinal flagged 0x40.
    count: u8,
    /// The ordinal the next entry must have; 0 once the run is complete.
    next: u8,
    checksum: u8,
    at: [u64; MAX_LONG_ENTRIES],
    seen: usize,
}

impl Long {
    fn new() -> Long {
        Long { units: [0; MAX_LONG_ENTRIES * UNITS_PER_ENTRY], count: 0, next: 0, checksum: 0, at: [0; MAX_LONG_ENTRIES], seen: 0 }
    }

    fn reset(&mut self) {
        self.count = 0;
        self.next = 0;
        self.seen = 0;
    }

    fn push(&mut self, raw: &Raw, at: u64) {
        let ord = raw.first_byte();
        if ord & LAST_LONG_ENTRY != 0 {
            let count = ord & !LAST_LONG_ENTRY;
            if count == 0 || count as usize > MAX_LONG_ENTRIES {
                self.reset();
                return;
            }
            self.count = count;
            self.next = count;
            self.checksum = raw.checksum_field();
            self.seen = 0;
        } else if self.next == 0 || ord != self.next || raw.checksum_field() != self.checksum {
            self.reset();
            return;
        }
        // `next` is from 1 to 20 here, so the units land inside the array.
        let start = (self.next as usize - 1) * UNITS_PER_ENTRY;
        for (k, unit) in name::entry_units(&raw.0).iter().enumerate() {
            if let Some(slot) = self.units.get_mut(start + k) {
                *slot = *unit;
            }
        }
        if let Some(slot) = self.at.get_mut(self.seen) {
            *slot = at;
        }
        self.seen += 1;
        self.next -= 1;
    }

    /// The run ending at the short entry named `short`, if it is complete and
    /// belongs to it: the positions of its entries, and the name, which is
    /// nothing when its units are not a name.
    fn take(&mut self, short: &Short) -> Option<(Option<String>, [u64; MAX_LONG_ENTRIES], usize)> {
        let belongs = self.count > 0 && self.next == 0 && self.checksum == name::checksum(short);
        let result = if belongs {
            let units = self.units.get(..self.count as usize * UNITS_PER_ENTRY).unwrap_or(&[]);
            let len = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
            let name = if len <= MAX_UNITS { units.get(..len).and_then(name::from_units) } else { None };
            Some((name, self.at, self.seen))
        } else {
            None
        };
        self.reset();
        result
    }
}

/// A file or directory in a directory.
#[derive(Clone, Debug)]
pub struct Found {
    /// The name shown: the long name, or the short one where there is no
    /// usable long name.
    pub name: String,
    pub raw: Raw,
    /// The short entry's index in the directory and position on the volume.
    pub index: u32,
    pub at: u64,
    /// The positions of the long-name entries before it, in order.
    long_at: [u64; MAX_LONG_ENTRIES],
    long_count: usize,
}

impl Found {
    /// Every slot the entry takes, the long-name ones first.
    pub fn positions(&self) -> impl Iterator<Item = u64> + '_ {
        self.long_at.iter().take(self.long_count).copied().chain(core::iter::once(self.at))
    }

    pub fn has_long(&self) -> bool {
        self.long_count > 0
    }
}

pub enum Item {
    Entry(Found),
    /// A short entry in use that no path names: `.`, `..`, the volume label,
    /// or one whose name no path can spell. Its short name is taken all the
    /// same.
    Other { short: Short, dot: bool, volume: bool },
    Free { index: u32 },
    /// The first slot marked as the end of the directory. Every slot after it
    /// is returned as free.
    End { index: u32 },
}

/// The entries of one directory, names assembled.
pub struct Scan {
    slots: Slots,
    long: Long,
    ended: bool,
}

impl Scan {
    pub fn new(fat: &Fat, first: u32) -> Result<Scan, FsError> {
        Ok(Scan { slots: Slots::new(fat, first)?, long: Long::new(), ended: false })
    }

    pub fn next<B: Blocks>(&mut self, cache: &mut Cache<B>, fat: &Fat) -> Result<Option<Item>, FsError> {
        loop {
            let Some((index, at, raw)) = self.slots.next(cache, fat)? else {
                return Ok(None);
            };
            if self.ended {
                return Ok(Some(Item::Free { index }));
            }
            match raw.first_byte() {
                END_MARK => {
                    self.ended = true;
                    self.long.reset();
                    return Ok(Some(Item::End { index }));
                }
                FREE_MARK => {
                    self.long.reset();
                    return Ok(Some(Item::Free { index }));
                }
                _ => {}
            }
            if raw.is_long() {
                self.long.push(&raw, at);
                continue;
            }
            let short = raw.short();
            let long = self.long.take(&short);
            let dot = short == DOT || short == DOTDOT;
            if dot || raw.is_volume() {
                return Ok(Some(Item::Other { short, dot, volume: !dot }));
            }
            let (long_name, long_at, long_count) = match long {
                Some((name, at, count)) => (name, at, count),
                None => (None, [0; MAX_LONG_ENTRIES], 0),
            };
            let shown = long_name.or_else(|| name::shown(&short, raw.lower()));
            return Ok(Some(match shown {
                Some(name) => Item::Entry(Found { name, raw, index, at, long_at, long_count }),
                None => Item::Other { short, dot: false, volume: false },
            }));
        }
    }

    pub fn cluster(&self) -> u32 {
        self.slots.cluster()
    }

    pub fn slots_seen(&self) -> u32 {
        self.slots.count()
    }
}

/// Whether `found` is the entry `wanted` names: by its name, or by its short
/// name when it also has a long one. `wanted_short` is `wanted` as a short
/// name, when it is one.
fn names(found: &Found, wanted: &str, wanted_short: Option<&Short>) -> bool {
    name::same(&found.name, wanted) || (found.has_long() && Some(&found.raw.short()) == wanted_short)
}

fn short_of(wanted: &str) -> Option<Short> {
    let key = name::key(wanted);
    if key.is_ascii() {
        name::exact_short(&key.to_ascii_uppercase())
    } else {
        None
    }
}

/// The first entry `wanted` names in the directory at `first`.
pub fn find<B: Blocks>(cache: &mut Cache<B>, fat: &Fat, first: u32, wanted: &str) -> Result<Option<Found>, FsError> {
    let wanted_short = short_of(wanted);
    let mut scan = Scan::new(fat, first)?;
    while let Some(item) = scan.next(cache, fat)? {
        match item {
            Item::Entry(found) if names(&found, wanted, wanted_short.as_ref()) => return Ok(Some(found)),
            Item::End { .. } => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}

/// Whether the directory at `first` holds nothing but `.`, `..` and a label.
pub fn is_empty<B: Blocks>(cache: &mut Cache<B>, fat: &Fat, first: u32) -> Result<bool, FsError> {
    let mut scan = Scan::new(fat, first)?;
    while let Some(item) = scan.next(cache, fat)? {
        match item {
            Item::Entry(_) | Item::Other { dot: false, volume: false, .. } => return Ok(false),
            Item::End { .. } => return Ok(true),
            _ => {}
        }
    }
    Ok(true)
}

/// What adding a name to a directory needs to know, from one scan: whether
/// the name is there, which short names are taken, and where there is room.
pub struct Survey {
    /// The entry the name names, unless it is the one at `except`.
    pub found: Option<Found>,
    shorts: BTreeSet<Short>,
    /// Long names of twelve bytes or fewer, upper-cased, which a short name
    /// could be read as.
    longs: BTreeSet<String>,
    /// Slots in the chain, and its last cluster.
    pub total: u32,
    pub last_cluster: u32,
    /// The first slot of the end-of-directory mark, if there is one.
    pub end: Option<u32>,
    first_free: Option<u32>,
    run_wanted: u32,
    /// The first run of `run_wanted` free slots, and the first inside one
    /// sector.
    first_run: Option<u32>,
    first_sector_run: Option<u32>,
    run_start: u32,
    run_len: u32,
    sector_run_start: u32,
    sector_run_len: u32,
}

impl Survey {
    /// Whether a short name is in use, or reads as a long name in use.
    pub fn taken(&self, short: &Short) -> bool {
        self.shorts.contains(short) || name::shown(short, 0).is_some_and(|shown| self.longs.contains(&shown))
    }

    /// Where `count` slots go: the first index of a run of free slots that
    /// long, and how many slots must be added to the directory for it, which
    /// is none when the run is already there.
    ///
    /// A name's entries are kept inside one sector whenever they fit in one,
    /// so that they reach the card in one write and a cut leaves all of them
    /// or none. A name of up to 195 UTF-16 units fits: 15 long-name entries
    /// and the short one. When no run inside a sector is free, the directory
    /// grows and the entries start its new cluster.
    pub fn place(&self, count: u32) -> (u32, u32) {
        if count <= 1 {
            if let Some(index) = self.first_free {
                return (index, 0);
            }
        } else if count <= self.run_wanted {
            if let Some(index) = self.first_sector_run {
                return (index, 0);
            }
            if count > SLOTS_PER_SECTOR {
                if let Some(index) = self.first_run {
                    return (index, 0);
                }
            }
        }
        if count <= SLOTS_PER_SECTOR {
            return (self.total, count);
        }
        // The free run at the end of the chain, if there is one, grown.
        let trailing = self.run_start.saturating_add(self.run_len) == self.total && self.run_len > 0;
        let (start, len) = if trailing { (self.run_start, self.run_len) } else { (self.total, 0) };
        (start, count.saturating_sub(len))
    }
}

/// Scan the directory at `first` for adding `wanted`, which takes `run_wanted`
/// slots with long-name entries or one without. The entry at position
/// `except`, when given, does not count as the name being there: it is the
/// entry a rename is about to replace with its new spelling.
pub fn survey<B: Blocks>(cache: &mut Cache<B>, fat: &Fat, first: u32, wanted: &str, run_wanted: u32, except: Option<u64>) -> Result<Survey, FsError> {
    let wanted_short = short_of(wanted);
    let mut survey = Survey {
        found: None,
        shorts: BTreeSet::new(),
        longs: BTreeSet::new(),
        total: 0,
        last_cluster: first,
        end: None,
        first_free: None,
        run_wanted,
        first_run: None,
        first_sector_run: None,
        run_start: 0,
        run_len: 0,
        sector_run_start: 0,
        sector_run_len: 0,
    };
    let mut scan = Scan::new(fat, first)?;
    while let Some(item) = scan.next(cache, fat)? {
        match item {
            Item::Entry(found) => {
                survey.shorts.insert(found.raw.short());
                if found.name.len() <= 12 {
                    survey.longs.insert(name::key(&found.name).to_ascii_uppercase());
                }
                if survey.found.is_none() && Some(found.at) != except && names(&found, wanted, wanted_short.as_ref()) {
                    survey.found = Some(found);
                }
            }
            Item::Other { short, .. } => {
                survey.shorts.insert(short);
            }
            Item::Free { index } | Item::End { index } => {
                if matches!(item, Item::End { .. }) && survey.end.is_none() {
                    survey.end = Some(index);
                }
                survey.first_free.get_or_insert(index);
                // Long-name entries are passed over without an item, so a run
                // continues only across adjacent indices.
                if survey.run_len == 0 || survey.run_start.saturating_add(survey.run_len) != index {
                    survey.run_start = index;
                    survey.run_len = 0;
                }
                survey.run_len += 1;
                if survey.run_len >= run_wanted && survey.first_run.is_none() {
                    survey.first_run = Some(survey.run_start);
                }
                if survey.sector_run_len == 0 || index % SLOTS_PER_SECTOR == 0 || survey.sector_run_start.saturating_add(survey.sector_run_len) != index {
                    survey.sector_run_start = index;
                    survey.sector_run_len = 0;
                }
                survey.sector_run_len += 1;
                if survey.sector_run_len >= run_wanted && survey.first_sector_run.is_none() {
                    survey.first_sector_run = Some(survey.sector_run_start);
                }
            }
        }
    }
    survey.total = scan.slots_seen();
    survey.last_cluster = scan.cluster();
    Ok(survey)
}

/// The first cluster of a new directory: `.` naming `own`, `..` naming
/// `parent` (0 for the root, as fatgen103 says), both with the directory's
/// own timestamp, and the rest zero, which marks the end of the directory.
pub fn new_directory(layout: &Layout, own: u32, parent: u32, stamp: Stamp) -> Vec<u8> {
    let mut data = vec![0u8; layout.cluster_bytes as usize];
    let dot = Raw::new(DOT, ATTR_DIRECTORY, own, 0, stamp);
    let dotdot = Raw::new(DOTDOT, ATTR_DIRECTORY, parent, 0, stamp);
    if let Some(slot) = data.get_mut(0..32) {
        slot.copy_from_slice(&dot.0);
    }
    if let Some(slot) = data.get_mut(32..64) {
        slot.copy_from_slice(&dotdot.0);
    }
    data
}
