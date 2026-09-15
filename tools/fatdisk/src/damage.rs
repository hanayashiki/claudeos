//! Volumes damaged on purpose, the same for the same seed on every run.

use crate::image::SECTOR;

/// xorshift64*: the same bytes for the same seed on every run.
pub struct Random(u64);

impl Random {
    pub fn new(seed: u64) -> Random {
        Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// The FAT32 layout a boot sector gives, read without the kernel's checks:
/// the first data sector, sectors per cluster, the first FAT sector, sectors
/// per FAT, FATs, and the root's cluster.
pub struct RawLayout {
    pub data_start: u64,
    pub per_cluster: u64,
    pub fat_start: u64,
    pub fat_sectors: u64,
    pub fats: u64,
    pub root: u64,
}

pub fn layout(volume: &[u8]) -> RawLayout {
    let le16 = |at: usize| u16::from_le_bytes([volume[at], volume[at + 1]]) as u64;
    let le32 = |at: usize| u32::from_le_bytes([volume[at], volume[at + 1], volume[at + 2], volume[at + 3]]) as u64;
    let (per_cluster, reserved, fats, fat_sectors, root) = (volume[13] as u64, le16(14), volume[16] as u64, le32(36), le32(44));
    RawLayout { data_start: reserved + fats * fat_sectors, per_cluster: per_cluster.max(1), fat_start: reserved, fat_sectors, fats, root }
}

/// The damage modes `damage` knows.
pub const MODES: [&str; 5] = ["boot", "meta", "fat", "dirent", "random"];

/// `count` changes of kind `mode` to the volume in `volume`, laid out as its
/// undamaged boot sector says:
///
/// - boot: random bytes over the boot sector's fields.
/// - meta: random bytes over the boot sector, FSInfo, the FATs and the first
///   16 clusters, where the directories of a small volume are.
/// - fat: entries of clusters in use pointed at other clusters, which makes
///   loops, shared clusters and chains that end nowhere.
/// - dirent: random bytes inside the directory entries of the first 64
///   clusters: names, attributes, first clusters, sizes, long-name ordinals and
///   checksums.
/// - random: random bytes anywhere in the volume.
pub fn damage(volume: &mut [u8], mode: &str, random: &mut Random, count: u64) {
    let raw = layout(volume);
    let len = volume.len() as u64;
    match mode {
        "boot" => {
            for _ in 0..count.max(1) {
                let at = 3 + random.below(87) as usize;
                volume[at] = random.next() as u8;
            }
        }
        "meta" => {
            let end = ((raw.data_start + 16 * raw.per_cluster) * SECTOR).min(len);
            for _ in 0..count {
                let at = random.below(end) as usize;
                volume[at] = random.next() as u8;
            }
        }
        "fat" => {
            let used = raw.fat_sectors * SECTOR / 4;
            for _ in 0..count.max(1) {
                let cluster = 2 + random.below(512.min(used.saturating_sub(2)));
                let target = random.below(600) as u32;
                for copy in 0..raw.fats.max(1) {
                    let at = ((raw.fat_start + copy * raw.fat_sectors) * SECTOR + cluster * 4) as usize;
                    if at + 4 <= volume.len() {
                        volume[at..at + 4].copy_from_slice(&target.to_le_bytes());
                    }
                }
            }
        }
        "dirent" => {
            let per_cluster = raw.per_cluster * SECTOR / 32;
            for _ in 0..count.max(1) {
                let cluster = 2 + random.below(64);
                let entry = random.below(per_cluster);
                let byte = random.below(32);
                let at = ((raw.data_start + (cluster - 2) * raw.per_cluster) * SECTOR + entry * 32 + byte) as usize;
                if at < volume.len() {
                    volume[at] = random.next() as u8;
                }
            }
        }
        "random" => {
            for _ in 0..count {
                let at = random.below(len) as usize;
                volume[at] = random.next() as u8;
            }
        }
        _ => {}
    }
}

fn fat_get(volume: &[u8], raw: &RawLayout, cluster: u64) -> u64 {
    let at = (raw.fat_start * SECTOR + cluster * 4) as usize;
    u32::from_le_bytes([volume[at], volume[at + 1], volume[at + 2], volume[at + 3]]) as u64 & 0x0FFF_FFFF
}

fn cluster_offset(raw: &RawLayout, cluster: u64) -> usize {
    ((raw.data_start + (cluster - 2) * raw.per_cluster) * SECTOR) as usize
}

/// The short entry called `name` in the first cluster of the directory at
/// `cluster`: its byte offset and the first cluster it names.
fn entry_in(volume: &[u8], raw: &RawLayout, cluster: u64, name: &[u8; 11]) -> Option<(usize, u64)> {
    let start = cluster_offset(raw, cluster);
    let data = &volume[start..start + (raw.per_cluster * SECTOR) as usize];
    let at = data.chunks_exact(32).position(|entry| &entry[..11] == name)?;
    let entry = &data[at * 32..at * 32 + 32];
    let first = (u16::from_le_bytes([entry[20], entry[21]]) as u64) << 16 | u16::from_le_bytes([entry[26], entry[27]]) as u64;
    Some((start + at * 32, first))
}

/// The chains of `loopdir` and `loop.bin`, in the root, made to loop back to
/// their first cluster. False if `fill` did not make them.
pub fn loops(volume: &mut [u8]) -> bool {
    let raw = layout(volume);
    let mut done = 0;
    for name in [b"LOOPDIR    ", b"LOOP    BIN"] {
        let Some((_, first)) = entry_in(volume, &raw, raw.root, name) else { continue };
        let mut last = first;
        for _ in 0..1_000_000 {
            let next = fat_get(volume, &raw, last);
            if !(2..0x0FFF_FFF8).contains(&next) {
                break;
            }
            last = next;
        }
        for copy in 0..raw.fats {
            let at = ((raw.fat_start + copy * raw.fat_sectors) * SECTOR + last * 4) as usize;
            volume[at..at + 4].copy_from_slice(&(first as u32).to_le_bytes());
        }
        done += 1;
    }
    done == 2
}

/// The entry of `www/css` pointed at the root directory's first cluster, so
/// that /www/css is the root again and the tree under it has no bottom. False
/// if `fill` did not make them.
pub fn cycle(volume: &mut [u8]) -> bool {
    let raw = layout(volume);
    let Some((_, www)) = entry_in(volume, &raw, raw.root, b"WWW        ") else { return false };
    let Some((css, _)) = entry_in(volume, &raw, www, b"CSS        ") else { return false };
    volume[css + 20..css + 22].copy_from_slice(&((raw.root >> 16) as u16).to_le_bytes());
    volume[css + 26..css + 28].copy_from_slice(&(raw.root as u16).to_le_bytes());
    true
}
