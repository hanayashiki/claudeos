//! Images: blocks in memory, blocks recorded as they are written, and the FAT
//! volumes inside an image file.

use crate::fat::{self, Blocks, DeviceError, Volume};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SECTOR: u64 = 512;
pub const MIB: u64 = 1024 * 1024;

/// A volume held in memory.
pub struct Memory {
    pub data: Vec<u8>,
}

fn range(block: u64, len: usize) -> Result<std::ops::Range<usize>, DeviceError> {
    let at = usize::try_from(block.checked_mul(SECTOR).ok_or(DeviceError)?).map_err(|_| DeviceError)?;
    Ok(at..at.checked_add(len).ok_or(DeviceError)?)
}

impl Blocks for Memory {
    fn count(&self) -> u64 {
        self.data.len() as u64 / SECTOR
    }
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        buf.copy_from_slice(self.data.get(range(block, buf.len())?).ok_or(DeviceError)?);
        Ok(())
    }
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError> {
        self.data.get_mut(range(block, data.len())?).ok_or(DeviceError)?.copy_from_slice(data);
        Ok(())
    }
}

/// A volume in memory that keeps every block written, one entry per block in
/// the order written, so that a power cut after any of them can be replayed;
/// and that fails every write after `fail_after` of them, as a card that
/// stops answering does.
pub struct Recorder {
    pub data: Vec<u8>,
    pub writes: Vec<(u64, Vec<u8>)>,
    pub fail_after: Option<usize>,
    pub fail_reads: bool,
}

impl Recorder {
    pub fn new(data: Vec<u8>) -> Recorder {
        Recorder { data, writes: Vec::new(), fail_after: None, fail_reads: false }
    }
}

impl Blocks for Recorder {
    fn count(&self) -> u64 {
        self.data.len() as u64 / SECTOR
    }
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if self.fail_reads {
            return Err(DeviceError);
        }
        buf.copy_from_slice(self.data.get(range(block, buf.len())?).ok_or(DeviceError)?);
        Ok(())
    }
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), DeviceError> {
        if self.fail_after.is_some_and(|limit| self.writes.len() >= limit) {
            return Err(DeviceError);
        }
        self.data.get_mut(range(block, data.len())?).ok_or(DeviceError)?.copy_from_slice(data);
        for (i, chunk) in data.chunks(SECTOR as usize).enumerate() {
            self.writes.push((block + i as u64, chunk.to_vec()));
        }
        Ok(())
    }
}

/// Seconds since 1970 now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Probe and mount the volume on `blocks`, allowing writes.
pub fn mount<B: Blocks>(mut blocks: B) -> Result<Volume<B>, String> {
    let probe = fat::probe(&mut blocks)?;
    let mut volume = Volume::mount(blocks, probe.layout, unix_now).map_err(|e| format!("mounting: {:?}", e))?;
    volume.allow_writes();
    Ok(volume)
}

pub fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

pub fn write_at(file: &mut File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

/// The first sector and length of every FAT32 volume in the image: the whole
/// image when block 0 is a boot sector, else the MBR's FAT32 entries.
pub fn volumes(path: &Path) -> std::io::Result<Vec<(u64, u64)>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len() / SECTOR;
    let mut sector = [0u8; 512];
    read_at(&mut file, 0, &mut sector)?;
    if fat::looks_like_boot_sector(&sector) {
        return Ok(vec![(0, size)]);
    }
    let mut out = Vec::new();
    for entry in sector[446..510].chunks_exact(16) {
        let start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as u64;
        let length = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as u64;
        if (entry[4] == 0x0B || entry[4] == 0x0C) && start > 0 && length > 0 && start + length <= size {
            out.push((start, length));
        }
    }
    Ok(out)
}

/// The volume labelled `label` in the image, found the way the kernel finds
/// it: its first sector and length.
pub fn find(path: &Path, label: &str) -> Result<(u64, u64), String> {
    for (start, length) in volumes(path).map_err(|e| format!("{}: {}", path.display(), e))? {
        let mut blocks = Memory { data: read_region(path, start, length).map_err(|e| e.to_string())? };
        if fat::probe(&mut blocks).is_ok_and(|probe| probe.labelled(label)) {
            return Ok((start, length));
        }
    }
    Err(format!("{}: no FAT32 volume labelled {}", path.display(), label))
}

/// `sectors` sectors of the image from `start`.
pub fn read_region(path: &Path, start: u64, sectors: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut data = vec![0u8; (sectors * SECTOR) as usize];
    read_at(&mut file, start * SECTOR, &mut data)?;
    Ok(data)
}

/// Put `data` back at sector `start` of the image, writing only the sectors
/// that differ from what is there, so a sparse image stays sparse.
pub fn write_region(path: &Path, start: u64, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut old = vec![0u8; data.len()];
    read_at(&mut file, start * SECTOR, &mut old)?;
    for (i, (new, was)) in data.chunks(SECTOR as usize).zip(old.chunks(SECTOR as usize)).enumerate() {
        if new != was {
            write_at(&mut file, (start + i as u64) * SECTOR, new)?;
        }
    }
    Ok(())
}
