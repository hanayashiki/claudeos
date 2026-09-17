//! Digests for the boot-time integrity check in kernel/src/integrity.rs.
//!
//! The manifest, /etc/claudeos/checksums, has one line per item in the form
//! shasum prints: `kernel`, for the kernel's code and read-only data, and then
//! every file of the image declared `checked()` in the tree.
//!
//! The kernel's checked bytes run from the symbol __integrity_start to the
//! symbol __integrity_end. The linker scripts define both, and the kernel
//! hashes the bytes between them in memory. Here they are read out of the
//! ELF's loadable segments at the addresses they are loaded at, so both ends
//! hash the same bytes from the same definition.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use sha1::{Digest, Sha1};

use crate::cache::hex;

const PT_LOAD: u32 = 1;
const PF_W: u32 = 2;
const SHT_SYMTAB: u32 = 2;
const STT_FUNC: u8 = 2;

pub fn sha1_hex(data: &[u8]) -> String {
    hex(&Sha1::digest(data))
}

struct Segment {
    vaddr: u64,
    paddr: u64,
    offset: u64,
    filesz: u64,
    flags: u32,
}

/// The loadable segments and the symbols of a little-endian ELF file, 32-bit
/// or 64-bit. The x86-64 kernel QEMU boots is converted to ELF32, and its
/// addresses are truncated to 32 bits there, symbols and segments alike, so
/// they still agree with each other.
pub struct Elf {
    path: String,
    data: Vec<u8>,
    segments: Vec<Segment>,
    symbols: HashMap<String, u64>,
    /// (address, size) of every function.
    functions: Vec<(u64, u64)>,
}

fn u16_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as u64)
}

fn u32_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as u64)
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

impl Elf {
    pub fn read(path: &Path) -> Result<Elf, String> {
        let name = path.display().to_string();
        let data = fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        Elf::parse(name.clone(), data).ok_or_else(|| format!("{name} is not a well-formed little-endian ELF file"))
    }

    fn parse(path: String, data: Vec<u8>) -> Option<Elf> {
        if data.get(..4)? != b"\x7fELF" || *data.get(5)? != 1 {
            return None;
        }
        let wide = *data.get(4)? == 2;
        let (phoff, shoff, phentsize, phnum, shentsize, shnum) = if wide {
            (u64_at(&data, 32)?, u64_at(&data, 40)?, u16_at(&data, 54)?, u16_at(&data, 56)?, u16_at(&data, 58)?, u16_at(&data, 60)?)
        } else {
            (u32_at(&data, 28)?, u32_at(&data, 32)?, u16_at(&data, 42)?, u16_at(&data, 44)?, u16_at(&data, 46)?, u16_at(&data, 48)?)
        };

        let mut segments = Vec::new();
        for index in 0..phnum {
            let at = (phoff + index * phentsize) as usize;
            let kind = u32_at(&data, at)? as u32;
            if kind != PT_LOAD {
                continue;
            }
            segments.push(if wide {
                Segment {
                    flags: u32_at(&data, at + 4)? as u32,
                    offset: u64_at(&data, at + 8)?,
                    vaddr: u64_at(&data, at + 16)?,
                    paddr: u64_at(&data, at + 24)?,
                    filesz: u64_at(&data, at + 32)?,
                }
            } else {
                Segment {
                    offset: u32_at(&data, at + 4)?,
                    vaddr: u32_at(&data, at + 8)?,
                    paddr: u32_at(&data, at + 12)?,
                    filesz: u32_at(&data, at + 16)?,
                    flags: u32_at(&data, at + 24)? as u32,
                }
            });
        }

        // (type, offset, size, link, entsize) of every section.
        let mut sections = Vec::new();
        for index in 0..shnum {
            let at = (shoff + index * shentsize) as usize;
            sections.push(if wide {
                (u32_at(&data, at + 4)? as u32, u64_at(&data, at + 24)?, u64_at(&data, at + 32)?, u32_at(&data, at + 40)?, u64_at(&data, at + 56)?)
            } else {
                (u32_at(&data, at + 4)? as u32, u32_at(&data, at + 16)?, u32_at(&data, at + 20)?, u32_at(&data, at + 24)?, u32_at(&data, at + 36)?)
            });
        }

        let mut symbols = HashMap::new();
        let mut functions = Vec::new();
        for &(kind, offset, size, link, entsize) in &sections {
            if kind != SHT_SYMTAB || entsize == 0 {
                continue;
            }
            let names = sections.get(link as usize)?.1 as usize;
            let mut at = offset;
            while at < offset + size {
                let a = at as usize;
                let (name, info, value, length) = if wide {
                    (u32_at(&data, a)?, *data.get(a + 4)?, u64_at(&data, a + 8)?, u64_at(&data, a + 16)?)
                } else {
                    (u32_at(&data, a)?, *data.get(a + 12)?, u32_at(&data, a + 4)?, u32_at(&data, a + 8)?)
                };
                let start = names + name as usize;
                let end = start + data.get(start..)?.iter().position(|&b| b == 0)?;
                symbols.insert(String::from_utf8_lossy(&data[start..end]).into_owned(), value);
                if info & 0xF == STT_FUNC {
                    functions.push((value, length));
                }
                at += entsize;
            }
        }
        Some(Elf { path, data, segments, symbols, functions })
    }

    fn symbol(&self, name: &str) -> Result<u64, String> {
        self.symbols
            .get(name)
            .copied()
            .ok_or_else(|| format!("{} has no symbol {name}; the kernel's linker script defines it", self.path))
    }

    fn segment_at(&self, address: u64) -> Option<&Segment> {
        self.segments.iter().find(|s| s.vaddr <= address && address < s.vaddr + s.filesz)
    }

    fn checked_range(&self) -> Result<(u64, u64), String> {
        let start = self.symbol("__integrity_start")?;
        let end = self.symbol("__integrity_end")?;
        if start >= end {
            return Err(format!("{}: __integrity_start {start:#x} is not below __integrity_end {end:#x}", self.path));
        }
        Ok((start, end))
    }

    /// The bytes from `start` to `end` as the loader puts them in memory.
    fn loaded_bytes(&self, start: u64, end: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut address = start;
        while address < end {
            let segment = self
                .segment_at(address)
                .ok_or_else(|| format!("{}: {address:#x}, inside the checked range, has no contents in the file", self.path))?;
            if segment.flags & PF_W != 0 {
                return Err(format!("{}: {address:#x}, inside the checked range, is in a writable segment", self.path));
            }
            let stop = end.min(segment.vaddr + segment.filesz);
            let from = (segment.offset + address - segment.vaddr) as usize;
            let to = (segment.offset + stop - segment.vaddr) as usize;
            out.extend_from_slice(
                self.data.get(from..to).ok_or_else(|| format!("{}: a segment runs past the end of the file", self.path))?,
            );
            address = stop;
        }
        Ok(out)
    }

    /// The SHA-1 of the kernel's checked bytes.
    pub fn kernel_digest(&self) -> Result<String, String> {
        let (start, end) = self.checked_range()?;
        Ok(sha1_hex(&self.loaded_bytes(start, end)?))
    }
}

/// Copy the image at `image` to `out` with one byte of the kernel's code
/// changed, for the test that the check notices. The byte is the last one of
/// .text, past the end of the last function, so no instruction changes and the
/// kernel still boots. `image` is the ELF itself or the flat image made from
/// it.
pub fn flip_code_byte(elf: &Elf, image: &Path, out: &Path) -> Result<String, String> {
    let text_start = elf.symbol("__text_start")?;
    let text_end = elf.symbol("__text_end")?;
    let (start, end) = elf.checked_range()?;
    let address = text_end - 1;
    let code_end = elf
        .functions
        .iter()
        .filter(|(value, _)| text_start <= *value && *value < text_end)
        .map(|(value, length)| value + length)
        .max()
        .unwrap_or(text_start);
    if code_end > address {
        return Err(format!(
            "{}: .text ends inside a function, so no byte of it can be changed without changing an instruction",
            elf.path
        ));
    }
    if !(start <= address && address < end) {
        return Err(format!("{}: the end of .text is outside the checked range", elf.path));
    }

    let segment = elf.segment_at(address).ok_or_else(|| format!("{}: the end of .text has no contents in the file", elf.path))?;
    let in_elf = (segment.offset + address - segment.vaddr) as usize;
    let mut bytes = fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
    let position = if bytes.starts_with(b"\x7fELF") {
        in_elf
    } else {
        // llvm-objcopy -O binary starts the flat image at the lowest load
        // address of anything that has contents in the file.
        let base = elf.segments.iter().filter(|s| s.filesz > 0).map(|s| s.paddr).min().unwrap_or(0);
        (segment.paddr + address - segment.vaddr - base) as usize
    };
    if position >= bytes.len() || bytes[position] != elf.data[in_elf] {
        return Err(format!("{} does not hold the bytes of {} where they belong", image.display(), elf.path));
    }
    let before = bytes[position];
    bytes[position] ^= 0xFF;
    fs::write(out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    let name = image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(format!(
        "changed the byte at {address:#x}, offset {position:#x} of {name}, from {before:#04x} to {:#04x}; functions in .text end at {code_end:#x}",
        bytes[position]
    ))
}
