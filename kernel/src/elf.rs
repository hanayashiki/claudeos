//! ELF64 program loader for static executables.

use crate::abi::Errno;
use crate::mm::paging::{AddressSpace, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64};
use alloc::collections::BTreeMap;

pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const EM_X86_64: u16 = 0x3E;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

/// Load address given to position-independent executables.
pub const DYN_BASE: u64 = 0x2000_0000;

#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub entry: u64,
    pub phdr_addr: u64,
    pub phent: u64,
    pub phnum: u64,
    pub base: u64,
    /// First page-aligned address past every loaded segment; brk starts here.
    pub brk_start: u64,
    pub interp_offset: Option<(usize, usize)>,
    pub tls_vaddr: u64,
    pub tls_filesz: u64,
    pub tls_memsz: u64,
    pub tls_align: u64,
    /// The mapped segments, as (start, end, PROT_* bits), so the task can
    /// record them alongside its other regions.
    pub segments: alloc::vec::Vec<(u64, u64, u64)>,
}

fn rd16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}
fn rd32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn rd64(data: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    u64::from_le_bytes(b)
}

pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// Check that `data` is a 64-bit little-endian x86-64 ELF we can run.
pub fn validate(data: &[u8]) -> Result<u16, Errno> {
    if data.len() < 64 || &data[0..4] != b"\x7FELF" {
        return Err(Errno::ENOEXEC);
    }
    if data[4] != 2 || data[5] != 1 {
        return Err(Errno::ENOEXEC); // not 64-bit little-endian
    }
    let e_type = rd16(data, 16);
    let e_machine = rd16(data, 18);
    if e_machine != EM_X86_64 {
        return Err(Errno::ENOEXEC);
    }
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(Errno::ENOEXEC);
    }
    Ok(e_type)
}

pub fn program_headers(data: &[u8]) -> Result<alloc::vec::Vec<ProgramHeader>, Errno> {
    let phoff = rd64(data, 32) as usize;
    let phentsize = rd16(data, 54) as usize;
    let phnum = rd16(data, 56) as usize;
    if phentsize < 56 || phoff + phnum * phentsize > data.len() {
        return Err(Errno::ENOEXEC);
    }
    let mut out = alloc::vec::Vec::with_capacity(phnum);
    for i in 0..phnum {
        let base = phoff + i * phentsize;
        out.push(ProgramHeader {
            p_type: rd32(data, base),
            p_flags: rd32(data, base + 4),
            p_offset: rd64(data, base + 8),
            p_vaddr: rd64(data, base + 16),
            p_filesz: rd64(data, base + 32),
            p_memsz: rd64(data, base + 40),
            p_align: rd64(data, base + 48),
        });
    }
    Ok(out)
}

/// Map `data`'s PT_LOAD segments into `space`, which must be the active
/// address space so the segment contents can be written directly.
pub fn load(space: &AddressSpace, data: &[u8]) -> Result<LoadedImage, Errno> {
    let e_type = validate(data)?;
    let phdrs = program_headers(data)?;
    let e_entry = rd64(data, 24);
    let phoff = rd64(data, 32);
    let phentsize = rd16(data, 54) as u64;
    let phnum = rd16(data, 56) as u64;

    let base = if e_type == ET_DYN { DYN_BASE } else { 0 };

    // Collect the final protection for every page first: two segments may
    // share a page when the linker did not pad them apart.
    let mut page_flags: BTreeMap<u64, u32> = BTreeMap::new();
    let mut brk_start = 0u64;
    let mut any_load = false;

    for ph in &phdrs {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }
        any_load = true;
        let start = page_align_down(base + ph.p_vaddr);
        let end = page_align_up(base + ph.p_vaddr + ph.p_memsz);
        if end > crate::mm::USER_MMAP_BASE {
            return Err(Errno::ENOMEM);
        }
        let mut page = start;
        while page < end {
            let entry = page_flags.entry(page).or_insert(0);
            *entry |= ph.p_flags;
            page += PAGE_SIZE_U64;
        }
        brk_start = brk_start.max(end);
    }
    if !any_load {
        return Err(Errno::ENOEXEC);
    }

    // Map every page writable so the contents can be copied in, then tighten.
    for page in page_flags.keys() {
        space
            .map_new(*page, PRESENT | WRITABLE | USER)
            .map_err(|_| Errno::ENOMEM)?;
    }

    for ph in &phdrs {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }
        let dest = base + ph.p_vaddr;
        let file_end = (ph.p_offset + ph.p_filesz) as usize;
        if file_end > data.len() {
            return Err(Errno::ENOEXEC);
        }
        unsafe {
            let src = data.as_ptr().add(ph.p_offset as usize);
            core::ptr::copy_nonoverlapping(src, dest as *mut u8, ph.p_filesz as usize);
            // .bss and any partial tail page must read as zero.
            let zero_from = dest + ph.p_filesz;
            let zero_len = ph.p_memsz - ph.p_filesz;
            if zero_len > 0 {
                core::ptr::write_bytes(zero_from as *mut u8, 0, zero_len as usize);
            }
        }
    }

    for (page, flags) in &page_flags {
        let mut bits = PRESENT | USER;
        if flags & PF_W != 0 {
            bits |= WRITABLE;
        }
        if flags & PF_X == 0 {
            bits |= NO_EXECUTE;
        }
        space.set_flags(*page, bits);
    }

    // AT_PHDR must point at the program headers as they sit in memory.
    let mut phdr_addr = 0u64;
    for ph in &phdrs {
        if ph.p_type == PT_PHDR {
            phdr_addr = base + ph.p_vaddr;
        }
    }
    if phdr_addr == 0 {
        for ph in &phdrs {
            if ph.p_type == PT_LOAD
                && phoff >= ph.p_offset
                && phoff + phnum * phentsize <= ph.p_offset + ph.p_filesz
            {
                phdr_addr = base + ph.p_vaddr + (phoff - ph.p_offset);
                break;
            }
        }
    }

    let mut interp_offset = None;
    let mut tls = (0u64, 0u64, 0u64, 0u64);
    for ph in &phdrs {
        if ph.p_type == PT_INTERP {
            interp_offset = Some((ph.p_offset as usize, ph.p_filesz as usize));
        }
        if ph.p_type == PT_TLS {
            tls = (base + ph.p_vaddr, ph.p_filesz, ph.p_memsz, ph.p_align.max(1));
        }
    }

    let mut segments = alloc::vec::Vec::new();
    for ph in &phdrs {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }
        let mut prot = 0u64;
        if ph.p_flags & PF_R != 0 {
            prot |= crate::abi::PROT_READ;
        }
        if ph.p_flags & PF_W != 0 {
            prot |= crate::abi::PROT_WRITE;
        }
        if ph.p_flags & PF_X != 0 {
            prot |= crate::abi::PROT_EXEC;
        }
        segments.push((
            page_align_down(base + ph.p_vaddr),
            page_align_up(base + ph.p_vaddr + ph.p_memsz),
            prot,
        ));
    }

    Ok(LoadedImage {
        entry: base + e_entry,
        phdr_addr,
        phent: phentsize,
        phnum,
        base,
        brk_start: page_align_up(brk_start),
        interp_offset,
        tls_vaddr: tls.0,
        tls_filesz: tls.1,
        tls_memsz: tls.2,
        tls_align: tls.3,
        segments,
    })
}
