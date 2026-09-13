//! ELF64 program loader for static executables.

use crate::abi::Errno;
use crate::arch::paging::{AddressSpace, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_down, PAGE_SIZE_U64};
use alloc::collections::{BTreeMap, BTreeSet};

pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;

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
/// Load address given to a program interpreter (the dynamic linker).
pub const INTERP_BASE: u64 = 0x7000_0000;

#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub entry: u64,
    pub phdr_addr: u64,
    pub phent: u64,
    pub phnum: u64,
    pub base: u64,
    /// First page-aligned address past every loaded segment; brk starts here.
    pub brk_start: u64,
    /// Path of the program interpreter, for a dynamically linked executable.
    pub interp: Option<alloc::string::String>,
    pub tls_vaddr: u64,
    pub tls_filesz: u64,
    pub tls_memsz: u64,
    pub tls_align: u64,
    /// The mapped segments, so the task can record them alongside its other
    /// regions. A segment with a file mapping has not been read in yet: its
    /// pages arrive as the program reaches them.
    pub segments: alloc::vec::Vec<Segment>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub file: Option<crate::task::FileMap>,
}

/// A field of the file, or ENOEXEC if the file does not reach that far.
/// Indexing would do instead, but an index past the end of a slice is a panic
/// in every build, and a panic here is a machine that stops rather than a
/// program that is refused.
fn field(data: &[u8], off: usize, len: usize) -> Result<&[u8], Errno> {
    off.checked_add(len)
        .and_then(|end| data.get(off..end))
        .ok_or(Errno::ENOEXEC)
}

fn rd16(data: &[u8], off: usize) -> Result<u16, Errno> {
    let b = field(data, off, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
fn rd32(data: &[u8], off: usize) -> Result<u32, Errno> {
    let b = field(data, off, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn rd64(data: &[u8], off: usize) -> Result<u64, Errno> {
    let mut b = [0u8; 8];
    b.copy_from_slice(field(data, off, 8)?);
    Ok(u64::from_le_bytes(b))
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

/// Check that `data` is a 64-bit little-endian ELF for this machine.
pub fn validate(data: &[u8]) -> Result<u16, Errno> {
    if data.len() < 64 || &data[0..4] != b"\x7FELF" {
        return Err(Errno::ENOEXEC);
    }
    if data[4] != 2 || data[5] != 1 {
        return Err(Errno::ENOEXEC); // not 64-bit little-endian
    }
    let e_type = rd16(data, 16)?;
    let e_machine = rd16(data, 18)?;
    if e_machine != crate::arch::ELF_MACHINE {
        return Err(Errno::ENOEXEC);
    }
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(Errno::ENOEXEC);
    }
    Ok(e_type)
}

pub fn program_headers(data: &[u8]) -> Result<alloc::vec::Vec<ProgramHeader>, Errno> {
    let phoff = rd64(data, 32)?;
    let phentsize = rd16(data, 54)? as u64;
    let phnum = rd16(data, 56)? as u64;
    if phentsize < 56 {
        return Err(Errno::ENOEXEC);
    }
    // e_phoff is a 64-bit number from the file, so the end of the table is a
    // sum that wraps: one near the top of the range plus a table of any size
    // is a small number, and a small number is inside the file.
    if sum(&[phoff, phnum * phentsize])? > data.len() as u64 {
        return Err(Errno::ENOEXEC);
    }
    let mut out = alloc::vec::Vec::with_capacity(phnum as usize);
    for i in 0..phnum {
        let base = (phoff + i * phentsize) as usize;
        out.push(ProgramHeader {
            p_type: rd32(data, base)?,
            p_flags: rd32(data, base + 4)?,
            p_offset: rd64(data, base + 8)?,
            p_vaddr: rd64(data, base + 16)?,
            p_filesz: rd64(data, base + 32)?,
            p_memsz: rd64(data, base + 40)?,
            p_align: rd64(data, base + 48)?,
        });
    }
    Ok(out)
}

/// The sum of numbers that came out of the file.
///
/// Every field of an ELF header is chosen by whoever wrote the file, and the
/// release build has overflow checks off, so an unchecked sum of two of them
/// wraps to a small number that passes whatever bound it is then checked
/// against. There is no useful saturating answer either: a header whose
/// arithmetic does not fit is a file to refuse.
fn sum(values: &[u64]) -> Result<u64, Errno> {
    let mut total: u64 = 0;
    for value in values {
        total = total.checked_add(*value).ok_or(Errno::ENOEXEC)?;
    }
    Ok(total)
}

/// Round a value from the file up to a page boundary. The rounding is itself
/// an addition, so a value in the last page of the address space wraps to zero.
fn page_up(value: u64) -> Result<u64, Errno> {
    Ok(page_align_down(sum(&[value, PAGE_SIZE_U64 - 1])?))
}

/// How many pages of segment the loader will describe. It keeps one map entry
/// per page while it works out the protection each page ends up with, so what
/// bounds a segment is the heap those entries live in rather than where user
/// space ends. The largest image here is a few thousand pages.
const MAX_IMAGE_PAGES: u64 = 64 * 1024;

/// Where one PT_LOAD segment lands, worked out once with checked arithmetic.
///
/// The three sums a segment needs -- its first address, its last, and the end
/// of its bytes in the file -- were each written out again at every place that
/// wanted them, which is five places to forget the check in. Deriving them
/// here instead means the rest of the loader has nothing left to add up.
struct Extent {
    /// The segment's first byte, and the page it falls in.
    vaddr: u64,
    start: u64,
    /// First page-aligned address past the segment.
    end: u64,
    /// Where the segment's bytes are in the file.
    file_offset: u64,
    file_len: u64,
}

impl Extent {
    fn of(ph: &ProgramHeader, base: u64, file_size: usize) -> Result<Extent, Errno> {
        // The ELF ABI has a segment's bytes fit inside it and start at the
        // same offset into a page as they do into the file. The loader reads a
        // page from the file at a fixed distance from the segment's start, and
        // subtracts that distance below, so a header that says otherwise has no
        // reading that works.
        if ph.p_filesz > ph.p_memsz {
            return Err(Errno::ENOEXEC);
        }
        if ph.p_offset % PAGE_SIZE_U64 != ph.p_vaddr % PAGE_SIZE_U64 {
            return Err(Errno::ENOEXEC);
        }
        if sum(&[ph.p_offset, ph.p_filesz])? > file_size as u64 {
            return Err(Errno::ENOEXEC);
        }
        let vaddr = sum(&[base, ph.p_vaddr])?;
        let end = page_up(sum(&[vaddr, ph.p_memsz])?)?;
        if end > crate::mm::USER_MMAP_BASE {
            return Err(Errno::ENOMEM);
        }
        Ok(Extent {
            vaddr,
            start: page_align_down(vaddr),
            end,
            file_offset: ph.p_offset,
            file_len: ph.p_filesz,
        })
    }

    fn pages(&self) -> u64 {
        (self.end - self.start) / PAGE_SIZE_U64
    }

    /// Past the segment's bytes from the file; from here to `end` is .bss.
    fn file_end(&self) -> u64 {
        self.vaddr + self.file_len
    }

    /// How far into its first page the segment's bytes begin.
    fn into_page(&self) -> u64 {
        self.vaddr - self.start
    }
}

/// Map `data`'s PT_LOAD segments into `space`, which must be the active
/// address space so the segment contents can be written directly.
pub fn load(space: &AddressSpace, data: &[u8]) -> Result<LoadedImage, Errno> {
    load_at(space, data, None, None)
}

/// As `load`, but with an explicit load address for a relocatable image, and
/// with the file the image came from so its pages can be read in on demand
/// rather than copied here.
pub fn load_at(
    space: &AddressSpace,
    data: &[u8],
    base_override: Option<u64>,
    node: Option<crate::fs::NodeRef>,
) -> Result<LoadedImage, Errno> {
    let e_type = validate(data)?;
    let phdrs = program_headers(data)?;
    let e_entry = rd64(data, 24)?;
    let phoff = rd64(data, 32)?;
    let phentsize = rd16(data, 54)? as u64;
    let phnum = rd16(data, 56)? as u64;

    let base = match base_override {
        Some(base) if e_type == ET_DYN => base,
        _ if e_type == ET_DYN => DYN_BASE,
        _ => 0,
    };

    // Where every segment lands, worked out before anything is mapped. A
    // header whose numbers do not add up is refused here rather than being
    // discovered halfway through the mapping.
    let mut loads: alloc::vec::Vec<(usize, Extent)> = alloc::vec::Vec::new();
    let mut pages = 0u64;
    for (index, ph) in phdrs.iter().enumerate() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        let extent = Extent::of(ph, base, data.len())?;
        if ph.p_memsz == 0 {
            continue;
        }
        pages = pages.checked_add(extent.pages()).ok_or(Errno::ENOMEM)?;
        if pages > MAX_IMAGE_PAGES {
            return Err(Errno::ENOMEM);
        }
        loads.push((index, extent));
    }
    if loads.is_empty() {
        return Err(Errno::ENOEXEC);
    }

    // Collect the final protection for every page first: two segments may
    // share a page when the linker did not pad them apart.
    let mut page_flags: BTreeMap<u64, u32> = BTreeMap::new();
    let mut brk_start = 0u64;
    for (index, extent) in &loads {
        let mut page = extent.start;
        while page < extent.end {
            let entry = page_flags.entry(page).or_insert(0);
            *entry |= phdrs[*index].p_flags;
            page += PAGE_SIZE_U64;
        }
        brk_start = brk_start.max(extent.end);
    }

    // Pages that cannot be left to a fault: a partial head or tail, anything
    // past the file's contents, and any page two segments share. A page in
    // one of those cases has to be assembled from more than one source, and
    // the fault handler only knows how to fill a page from one.
    let mut eager: BTreeSet<u64> = BTreeSet::new();
    for (_, extent) in &loads {
        eager.insert(extent.start);
        let mut page = page_align_down(extent.file_end());
        while page < extent.end {
            eager.insert(page);
            page += PAGE_SIZE_U64;
        }
    }
    // A page inside more than one segment has to be assembled here too.
    let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
    for (index, extent) in &loads {
        let mut page = extent.start;
        while page < extent.end {
            match seen.insert(page, *index) {
                Some(other) if other != *index => {
                    eager.insert(page);
                }
                _ => {}
            }
            page += PAGE_SIZE_U64;
        }
    }
    // Without a file to read from later, everything has to be read now.
    if node.is_none() {
        eager.extend(page_flags.keys().copied());
    }

    for page in page_flags.keys() {
        if !eager.contains(page) {
            continue;
        }
        space
            .map_new(*page, PRESENT | WRITABLE | USER)
            .map_err(|_| Errno::ENOMEM)?;
    }

    for (_, extent) in &loads {
        // Copy only what lands in a page that was mapped here. A fresh frame
        // is already zero, so .bss needs nothing written.
        let mut offset = 0u64;
        while offset < extent.file_len {
            let address = extent.vaddr + offset;
            let page = page_align_down(address);
            let chunk = (page + PAGE_SIZE_U64 - address).min(extent.file_len - offset);
            if eager.contains(&page) {
                unsafe {
                    let src = data.as_ptr().add((extent.file_offset + offset) as usize);
                    core::ptr::copy_nonoverlapping(src, address as *mut u8, chunk as usize);
                }
                // These bytes are about to be executed, and on some machines
                // writing them is not enough to make them fetchable.
                crate::arch::sync_instruction_cache(address, chunk as usize);
            }
            offset += chunk;
        }
    }

    for (page, flags) in &page_flags {
        if !eager.contains(page) {
            continue;
        }
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
            phdr_addr = sum(&[base, ph.p_vaddr])?;
        }
    }
    if phdr_addr == 0 {
        let table_end = sum(&[phoff, phnum * phentsize])?;
        for (index, extent) in &loads {
            let ph = &phdrs[*index];
            if phoff >= ph.p_offset && table_end <= extent.file_offset + extent.file_len {
                phdr_addr = extent.vaddr + (phoff - ph.p_offset);
                break;
            }
        }
    }

    let mut interp = None;
    let mut tls = (0u64, 0u64, 0u64, 0u64);
    for ph in &phdrs {
        if ph.p_type == PT_INTERP {
            let end = sum(&[ph.p_offset, ph.p_filesz])?;
            let name = data
                .get(ph.p_offset as usize..end as usize)
                .ok_or(Errno::ENOEXEC)?;
            if let Ok(text) = core::str::from_utf8(name) {
                interp = Some(alloc::string::String::from(text.trim_end_matches('\0')));
            }
        }
        if ph.p_type == PT_TLS {
            tls = (
                sum(&[base, ph.p_vaddr])?,
                ph.p_filesz,
                ph.p_memsz,
                ph.p_align.max(1),
            );
        }
    }

    let mut segments = alloc::vec::Vec::new();
    for (index, extent) in &loads {
        let flags = phdrs[*index].p_flags;
        let mut prot = 0u64;
        if flags & PF_R != 0 {
            prot |= crate::abi::PROT_READ;
        }
        if flags & PF_W != 0 {
            prot |= crate::abi::PROT_WRITE;
        }
        if flags & PF_X != 0 {
            prot |= crate::abi::PROT_EXEC;
        }
        // The region's file mapping is described from its page-aligned start,
        // which is where the fault handler measures from. The offset the
        // segment's bytes begin at within their page is the same in the file,
        // which is what makes the subtraction below sound.
        let file = node.as_ref().map(|node| crate::task::FileMap {
            node: node.clone(),
            offset: extent.file_offset - extent.into_page(),
            length: extent.file_len + extent.into_page(),
        });
        segments.push(Segment {
            start: extent.start,
            end: extent.end,
            prot,
            file,
        });
    }

    Ok(LoadedImage {
        entry: sum(&[base, e_entry])?,
        phdr_addr,
        phent: phentsize,
        phnum,
        base,
        brk_start,
        interp,
        tls_vaddr: tls.0,
        tls_filesz: tls.1,
        tls_memsz: tls.2,
        tls_align: tls.3,
        segments,
    })
}
