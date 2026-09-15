//! Which part of the card /data is, and the only thing that can reach it.
//!
//! The filesystem is given a `Partition`: the card, moved in, with the first
//! block and the length of the one volume chosen, and block numbers that count
//! from that volume's first block. The card's block functions are private to
//! `card` and to this module, and a `Partition` has no way to give the card
//! back or to name a block by its number on the card. `absolute` is the one
//! place a block of the volume becomes a block of the card, and it refuses a
//! block at or past the volume's length. So no damaged number in the FAT and
//! no mistake above this file can reach the boot partition or any other part
//! of the card.
//!
//! **Choosing.** Block 0 of the card is either a FAT32 boot sector, for a card
//! formatted as one volume with no partition table, or an MBR. Of the MBR's
//! four primary entries, those of type 0x0B or 0x0C (FAT32) that lie inside the
//! card are looked at in order, and the first whose label matches is taken.
//! Looking at one means reading its boot sector and the first cluster of its
//! root directory, for the label entry there; a volume that does not match is
//! not mounted and nothing is written to it. A GPT card is refused whole.

use super::{Card, Identity};
use crate::storage::disk::{Blocks, DeviceError, BLOCK};
use crate::storage::volume::{self, Probe};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// MBR partition types that hold FAT32: with CHS addressing, and with LBA.
const FAT32_TYPES: [u8; 2] = [0x0B, 0x0C];
/// The type a GPT disk's protective MBR gives its one entry.
const GPT_PROTECTIVE: u8 = 0xEE;
/// Card errors printed one line each; the rest are counted.
const ERRORS_LOGGED: u32 = 16;

pub struct Partition {
    card: Card,
    /// The card block that is block 0 of the volume.
    first: u64,
    /// The volume's length in blocks.
    count: u64,
    errors: u32,
    /// The last card error, which choosing a volume adds to its reason for
    /// passing one over.
    last: Option<String>,
}

impl Partition {
    /// The card block for `bytes` bytes at block `block` of the volume, or
    /// nothing if any of them lie outside it.
    fn absolute(&self, block: u64, bytes: usize) -> Result<u64, DeviceError> {
        let count = (bytes / BLOCK) as u64;
        if bytes % BLOCK != 0 || block.checked_add(count).map_or(true, |end| end > self.count) {
            return Err(DeviceError);
        }
        Ok(self.first + block)
    }

    pub fn first(&self) -> u64 {
        self.first
    }

    pub fn identity(&self) -> &Identity {
        &self.card.identity
    }

    fn report(&mut self, what: &str, block: u64, why: String) -> DeviceError {
        self.errors = self.errors.saturating_add(1);
        self.last = Some(format!("card {} at block {} of the volume failed: {}", what, block, why));
        // Until /data is mounted, the one line bring-up prints says what went
        // wrong, and nothing else about the card is printed before the shell.
        if crate::storage::vfs::mounted() && self.errors <= ERRORS_LOGGED {
            crate::println!("data: card {} at block {} of the volume failed: {}", what, block, why);
            if self.errors == ERRORS_LOGGED {
                crate::println!("data: {} card errors; further ones are not printed", ERRORS_LOGGED);
            }
        }
        DeviceError
    }
}

impl Blocks for Partition {
    fn count(&self) -> u64 {
        self.count
    }

    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        let lba = self.absolute(block, buf.len())?;
        match self.card.read_blocks(lba, buf) {
            Ok(()) => Ok(()),
            Err(why) => Err(self.report("read", block, why)),
        }
    }

    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError> {
        let lba = self.absolute(block, data.len())?;
        match self.card.write_blocks(lba, data) {
            Ok(()) => Ok(()),
            Err(why) => Err(self.report("write", block, why)),
        }
    }

    fn now_ms(&self) -> u64 {
        crate::mmc::delay::now_us() / 1000
    }
}

/// The volume chosen, what its boot sector says, and which part of the card
/// it is, for the log.
pub struct Chosen {
    pub partition: Partition,
    pub probe: Probe,
    pub what: String,
    /// The MBR slot, counted from 1, or nothing for a volume across the whole
    /// card.
    pub slot: Option<usize>,
}

fn le32(data: &[u8], at: usize) -> u64 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as u64
}

/// The first FAT32 volume on `card` labelled `label`.
pub fn choose(mut card: Card, label: &str) -> Result<Chosen, String> {
    let mut sector = [0u8; BLOCK];
    card.read_blocks(0, &mut sector).map_err(|why| format!("reading block 0 of the card: {}", why))?;
    let blocks = card.identity.blocks;

    let mut candidates: Vec<(u64, u64, String, Option<usize>)> = Vec::new();
    if volume::looks_like_fat32(&sector) {
        candidates.push((0, blocks, String::from("the volume across the whole card"), None));
    } else if sector[510] == 0x55 && sector[511] == 0xAA {
        let entries: Vec<&[u8]> = sector[446..510].chunks_exact(16).collect();
        // `msdos_partition` in Linux's `block/partitions/msdos.c` refuses a
        // table with a boot indicator other than 0 or 0x80.
        if entries.iter().any(|entry| entry[0] != 0 && entry[0] != 0x80) {
            return Err(String::from("block 0 of the card has the 55 AA signature but no valid partition table"));
        }
        for (slot, entry) in entries.iter().enumerate() {
            let (kind, start, length) = (entry[4], le32(entry, 8), le32(entry, 12));
            if kind == GPT_PROTECTIVE {
                return Err(String::from("the card has a GPT partition table, and /data is only looked for in an MBR"));
            }
            if FAT32_TYPES.contains(&kind) && start > 0 && length > 0 && start + length <= blocks {
                candidates.push((start, length, format!("partition {}", slot + 1), Some(slot + 1)));
            }
        }
    } else {
        return Err(String::from("block 0 of the card holds neither a partition table nor a FAT32 volume"));
    }

    let mut seen: Vec<String> = Vec::new();
    for (first, count, what, slot) in candidates {
        let mut partition = Partition { card, first, count, errors: 0, last: None };
        match volume::probe(&mut partition) {
            Ok(probe) if probe.labelled(label) => return Ok(Chosen { partition, probe, what, slot }),
            Ok(probe) => seen.push(format!("{} is labelled {}", what, probe.label())),
            Err(why) => match partition.last.take() {
                Some(card_error) => seen.push(format!("{}: {} ({})", what, why, card_error)),
                None => seen.push(format!("{}: {}", what, why)),
            },
        }
        card = partition.card;
    }
    if seen.is_empty() {
        Err(String::from("the card's partition table has no FAT32 partition"))
    } else {
        Err(format!("no FAT32 volume on the card is labelled {} ({})", label, seen.join("; ")))
    }
}
