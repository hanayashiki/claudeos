//! The boot sector, its BIOS parameter block, and FSInfo.
//!
//! Field offsets and rules are Microsoft's "FAT32 File System Specification"
//! (fatgen103), sections "Boot Sector and BPB", "FAT Type Determination" and
//! "FAT32 FSInfo Sector Structure and Backup Boot Sector".
//!
//! Every field is checked here, before any other code computes with it, and
//! the checks are the ones that make the rest of the arithmetic safe: after
//! `parse` succeeds, a cluster number from 2 to `clusters + 1` gives a block
//! inside the volume and a FAT entry inside every FAT copy.

use super::cache::{Blocks, BLOCK};
use super::{le16, le32, put32};
use alloc::format;
use alloc::string::String;
use alloc::vec;

/// The lowest cluster count of a FAT32 volume, and the highest cluster number
/// FAT32 can name before the values it reserves (fatgen103, "FAT Type
/// Determination", and "FAT Data Structure").
const MIN_CLUSTERS: u64 = 65_525;
const MAX_CLUSTERS: u64 = 0x0FFF_FFF5;

/// BS_BootSig, which says the three fields after it are present.
const EXTENDED_BOOT_SIGNATURE: u8 = 0x29;

/// Offset of Linux's `fat32.state` byte, fatgen103's BS_Reserved1, and the bit
/// Linux sets in it while a volume is mounted for writing (`FAT_STATE_DIRTY`
/// in include/uapi/linux/msdos_fs.h, `fat_set_state` in fs/fat/inode.c).
pub const LINUX_STATE: u64 = 65;
pub const LINUX_STATE_DIRTY: u8 = 0x01;

/// FSInfo's three signatures and its two hints (fatgen103, "FAT32 FSInfo
/// Sector Structure").
const FSI_LEAD_SIG: u32 = 0x4161_5252;
const FSI_STRUC_SIG: u32 = 0x6141_7272;
const FSI_TRAIL_SIG: u32 = 0xAA55_0000;
const FSI_FREE_COUNT: usize = 488;
const FSI_NXT_FREE: usize = 492;
/// The value of either hint that means nothing is known.
pub const FSI_UNKNOWN: u32 = 0xFFFF_FFFF;

pub type Sector = [u8; BLOCK];

/// Where things are in a FAT32 volume, all of it checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// BPB_TotSec32: the volume's length in sectors.
    pub sectors: u32,
    /// BPB_SecPerClus: 1, 2, 4 and so on to 128.
    pub cluster_blocks: u32,
    pub cluster_bytes: u32,
    /// BPB_RsvdSecCnt: the first sector of the first FAT.
    pub fat_start: u32,
    /// BPB_FATSz32: sectors per FAT copy.
    pub fat_blocks: u32,
    /// BPB_NumFATs: 1 or 2.
    pub fats: u32,
    /// The first sector of cluster 2.
    pub data_start: u32,
    /// The count of data clusters, numbered from 2 to `clusters + 1`.
    pub clusters: u32,
    /// BPB_RootClus.
    pub root_cluster: u32,
    /// BPB_FSInfo, when it names a sector inside the reserved region.
    pub fsinfo: Option<u32>,
    /// BPB_ExtFlags: nothing when every FAT copy is kept the same (bit 7
    /// clear), or the one copy in use (bits 0 to 3) when bit 7 is set.
    pub active_fat: Option<u32>,
}

impl Layout {
    /// Whether `cluster` names a data cluster of this volume.
    pub fn is_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster - 2 < self.clusters
    }

    /// The byte offset of `cluster` in the volume, which `is_cluster` must
    /// have allowed; `None` otherwise. With the checks in `parse` the result
    /// is below the volume's length: data_start + clusters * cluster_blocks
    /// is at most BPB_TotSec32.
    pub fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        if !self.is_cluster(cluster) {
            return None;
        }
        let block = self.data_start as u64 + (cluster - 2) as u64 * self.cluster_blocks as u64;
        Some(block * BLOCK as u64)
    }

    /// The byte offset of FAT entry `entry` in FAT copy `copy`. `entry` is at
    /// most `clusters + 1` and `copy` below `fats`, which `parse` has checked
    /// fit in the FAT and in the reserved and FAT regions.
    pub fn fat_offset(&self, copy: u32, entry: u32) -> u64 {
        (self.fat_start as u64 + copy as u64 * self.fat_blocks as u64) * BLOCK as u64 + entry as u64 * 4
    }

    /// The FAT copies a change is written to: every copy when they are
    /// mirrored, and only the active one when BPB_ExtFlags says only one is.
    pub fn written_copies(&self) -> core::ops::Range<u32> {
        match self.active_fat {
            Some(copy) => copy..copy + 1,
            None => 0..self.fats,
        }
    }

    /// The FAT copy that is read: the first when they are mirrored.
    pub fn read_copy(&self) -> u32 {
        self.active_fat.unwrap_or(0)
    }
}

/// Eleven bytes of a label with the padding taken off, or nothing when they
/// are all padding. Bytes outside printable ASCII are shown as `?`.
pub fn label_text(raw: &[u8]) -> Option<String> {
    let end = raw.iter().rposition(|&b| b != b' ' && b != 0)?;
    let raw = raw.get(..=end)?;
    Some(raw.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '?' }).collect())
}

fn bytes_at<const N: usize>(sector: &Sector, at: usize) -> Option<[u8; N]> {
    sector.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// Whether block 0 of a card holds a FAT boot sector of any kind rather than a
/// partition table: the 55 AA signature, a jump instruction, and one of the
/// type strings every formatter writes. The type strings do not decide the
/// type (fatgen103 says not to use them for that; `parse` counts clusters);
/// they only tell a boot sector from an MBR whose boot code starts with a
/// jump.
pub fn looks_like_boot_sector(sector: &Sector) -> bool {
    let signature = bytes_at::<2>(sector, 510) == Some([0x55, 0xAA]);
    let jump = matches!(sector.first(), Some(0xEB) | Some(0xE9));
    let fat32 = bytes_at::<8>(sector, 82) == Some(*b"FAT32   ");
    let fat16 = bytes_at::<8>(sector, 54).is_some_and(|t| t.starts_with(b"FAT12") || t.starts_with(b"FAT16") || t.starts_with(b"FAT  "));
    let exfat = bytes_at::<8>(sector, 3) == Some(*b"EXFAT   ");
    signature && jump && (fat32 || fat16 || exfat)
}

/// The volume's layout from its boot sector, or why it is not a FAT32 volume
/// this code mounts. `blocks` is the length of the partition it is on.
pub fn parse(sector: &Sector, blocks: u64) -> Result<Layout, String> {
    if bytes_at::<2>(sector, 510) != Some([0x55, 0xAA]) {
        return Err(String::from("the volume's boot sector has no 55 AA signature"));
    }
    if bytes_at::<8>(sector, 3) == Some(*b"EXFAT   ") {
        return Err(String::from("the volume is exFAT, and only FAT32 is mounted"));
    }
    let sector_size = le16(sector, 11) as u64;
    let per_cluster = bytes_at::<1>(sector, 13).map_or(0, |[b]| b) as u64;
    let reserved = le16(sector, 14) as u64;
    let fats = bytes_at::<1>(sector, 16).map_or(0, |[b]| b) as u64;
    let root_entries = le16(sector, 17) as u64;
    let sectors_16 = le16(sector, 19) as u64;
    let media = bytes_at::<1>(sector, 21).map_or(0, |[b]| b);
    let fat_16 = le16(sector, 22) as u64;
    let sectors_32 = le32(sector, 32) as u64;
    let fat_32 = le32(sector, 36) as u64;
    let ext_flags = le16(sector, 40);
    let version = le16(sector, 42);
    let root_cluster = le32(sector, 44);
    let fsinfo = le16(sector, 48) as u64;

    if sector_size != BLOCK as u64 {
        return Err(format!("the volume's sectors are {} bytes, and only 512 is read", sector_size));
    }
    if per_cluster == 0 || !per_cluster.is_power_of_two() {
        return Err(format!("the boot sector gives {} sectors per cluster", per_cluster));
    }
    if reserved == 0 {
        return Err(String::from("the boot sector gives no reserved sectors"));
    }
    if !(1..=2).contains(&fats) {
        return Err(format!("the boot sector gives {} FATs, where 1 or 2 are mounted", fats));
    }
    // fatgen103, "FAT Type Determination": the type is decided by the count of
    // clusters, computed the same way for every type.
    let fat_size = if fat_16 != 0 { fat_16 } else { fat_32 };
    let total = if sectors_16 != 0 { sectors_16 } else { sectors_32 };
    let root_sectors = (root_entries * 32).div_ceil(BLOCK as u64);
    let data_start = reserved + fats * fat_size + root_sectors;
    let clusters = total.saturating_sub(data_start) / per_cluster;
    if root_entries != 0 || fat_16 != 0 || sectors_16 != 0 || clusters < MIN_CLUSTERS {
        let kind = if clusters < 4085 { "FAT12" } else if clusters < MIN_CLUSTERS { "FAT16" } else { "FAT12 or FAT16 in its fields" };
        return Err(format!("the volume is {} ({} clusters), and only FAT32 is mounted", kind, clusters));
    }
    if media != 0xF0 && media < 0xF8 {
        return Err(format!("the boot sector's media byte {:#04x} is not one FAT defines", media));
    }
    if version != 0 {
        return Err(format!("the volume is FAT32 version {:#06x}, and only version 0 is defined", version));
    }
    if fat_32 == 0 {
        return Err(String::from("the boot sector gives a FAT of no sectors"));
    }
    if total > blocks {
        return Err(format!("the boot sector claims {} sectors and the partition has {}", total, blocks));
    }
    if data_start >= total {
        return Err(String::from("the boot sector's FATs do not fit in the volume"));
    }
    if clusters > MAX_CLUSTERS {
        return Err(format!("{} clusters is more than FAT32 can number", clusters));
    }
    // Every cluster needs an entry, and entries 0 and 1 are reserved.
    if fat_32 * (BLOCK as u64 / 4) < clusters + 2 {
        return Err(format!("a FAT of {} sectors does not have entries for {} clusters", fat_32, clusters));
    }
    if (root_cluster as u64) < 2 || root_cluster as u64 >= clusters + 2 {
        return Err(format!("the root directory's cluster {} is not in the volume", root_cluster));
    }
    let active_fat = if ext_flags & 0x80 != 0 { Some((ext_flags & 0x0F) as u64) } else { None };
    if let Some(copy) = active_fat {
        if copy >= fats {
            return Err(format!("the boot sector makes FAT {} the only active one, and there are {}", copy, fats));
        }
    }
    // BPB_FSInfo is usually 1. Zero and 0xFFFF mean there is none; anything
    // else outside the reserved sectors is taken to mean the same, so that
    // nothing is ever written outside them for it.
    let fsinfo = if fsinfo >= 1 && fsinfo < reserved { Some(fsinfo as u32) } else { None };

    // Each value is below 2^32: total fits in BPB_TotSec32, data_start and
    // clusters are below total, per_cluster is at most 128.
    Ok(Layout {
        sectors: total as u32,
        cluster_blocks: per_cluster as u32,
        cluster_bytes: (per_cluster * BLOCK as u64) as u32,
        fat_start: reserved as u32,
        fat_blocks: fat_32 as u32,
        fats: fats as u32,
        data_start: data_start as u32,
        clusters: clusters as u32,
        root_cluster,
        fsinfo,
        active_fat: active_fat.map(|copy| copy as u32),
    })
}

/// What `probe` found.
#[derive(Clone, Debug)]
pub struct Probe {
    pub layout: Layout,
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

    /// The label to report: the root directory's, which is what a label change
    /// on another system updates, or else the boot sector's.
    pub fn label(&self) -> String {
        self.root_label.clone().or_else(|| self.boot_label.clone()).unwrap_or_else(|| String::from("(no label)"))
    }
}

/// Read and check the boot sector at block 0 of `blocks`, and the label in
/// the root directory's first cluster. Only reads.
pub fn probe<B: Blocks>(blocks: &mut B) -> Result<Probe, String> {
    let mut sector = [0u8; BLOCK];
    blocks.read(0, &mut sector).map_err(|_| String::from("the card failed to read the volume's boot sector"))?;
    let layout = parse(&sector, blocks.count())?;
    let boot_label = if bytes_at::<1>(&sector, 66) == Some([EXTENDED_BOOT_SIGNATURE]) {
        sector.get(71..82).and_then(label_text)
    } else {
        None
    };

    let mut root = vec![0u8; layout.cluster_bytes as usize];
    let first = layout.cluster_offset(layout.root_cluster).unwrap_or(0) / BLOCK as u64;
    blocks.read(first, &mut root).map_err(|_| String::from("the card failed to read the root directory"))?;
    let mut root_label = None;
    for raw in root.chunks_exact(32) {
        let (name, attributes) = (raw.first().copied().unwrap_or(0), raw.get(11).copied().unwrap_or(0));
        if name == 0 {
            break;
        }
        // Not deleted, not a long-name entry, and marked as the volume ID.
        if name != 0xE5 && attributes & 0x3F != 0x0F && attributes & 0x08 != 0 {
            root_label = raw.get(..11).and_then(label_text);
            break;
        }
    }
    Ok(Probe { layout, boot_label, root_label })
}

/// FSInfo's free-cluster count and next-free hint, if the sector carries all
/// three signatures.
pub fn fsinfo_hints(sector: &Sector) -> Option<(u32, u32)> {
    if le32(sector, 0) != FSI_LEAD_SIG || le32(sector, 484) != FSI_STRUC_SIG || le32(sector, 508) != FSI_TRAIL_SIG {
        return None;
    }
    Some((le32(sector, FSI_FREE_COUNT), le32(sector, FSI_NXT_FREE)))
}

/// Put the two hints in a sector that `fsinfo_hints` accepted.
pub fn set_fsinfo_hints(sector: &mut Sector, free: u32, next: u32) {
    put32(sector, FSI_FREE_COUNT, free);
    put32(sector, FSI_NXT_FREE, next);
}
