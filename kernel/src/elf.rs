//! ELF64 program loader for static executables.

use crate::abi::Errno;
use crate::arch::paging::{FreshPage, PageTables, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::fs::NodeKind;
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
    /// regions. A segment has not been read in yet beyond the pages the loader
    /// had to assemble: the rest arrive from the file as the program reaches
    /// them.
    pub segments: alloc::vec::Vec<Segment>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub file: crate::task::FileMap,
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

impl ProgramHeader {
    /// The entry that starts at `base` in `data`.
    fn parse(data: &[u8], base: usize) -> Result<ProgramHeader, Errno> {
        Ok(ProgramHeader {
            p_type: rd32(data, base)?,
            p_flags: rd32(data, base + 4)?,
            p_offset: rd64(data, base + 8)?,
            p_vaddr: rd64(data, base + 16)?,
            p_filesz: rd64(data, base + 32)?,
            p_memsz: rd64(data, base + 40)?,
            p_align: rd64(data, base + 48)?,
        })
    }
}

/// The largest program header table the loader reads, the bound Linux's
/// `elf_read_phdrs` puts on it. A table from memory is read in place, but one
/// on the card is copied out first, and its size is two numbers from the file.
const MAX_PHDR_BYTES: u64 = 65536;

/// The ELF header, and how long the file was when it was read.
///
/// The node's lock masks interrupts, so it is taken for these sixty-four bytes
/// and let go again. Everything done with the numbers in them happens without
/// it, which means the file can change in between: what is read here is a copy
/// and stays what it was.
fn head_of(node: &crate::fs::NodeRef) -> Result<([u8; 64], usize), Errno> {
    let mut head = [0u8; 64];
    if node.kind == NodeKind::DataFile {
        let size = node.size() as usize;
        if size < 64 || read_into(node, 0, &mut head)? < 64 {
            return Err(Errno::ENOEXEC);
        }
        return Ok((head, size));
    }
    let inner = node.inner.lock();
    if inner.data.len() < 64 {
        return Err(Errno::ENOEXEC);
    }
    head.copy_from_slice(&inner.data[..64]);
    Ok((head, inner.data.len()))
}

/// Refuse a file that is not an ELF64 executable for this machine.
///
/// exec asks this before it takes the caller's address space apart, so a file
/// that is not a program for this machine leaves the task on the one it has.
pub fn check(node: &crate::fs::NodeRef) -> Result<(), Errno> {
    validate(&head_of(node)?.0)?;
    Ok(())
}

/// Check that `data` is a 64-bit little-endian ELF for this machine.
fn validate(data: &[u8]) -> Result<u16, Errno> {
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

/// What the loader reads out of the file before it maps anything: the ELF
/// header, the program header table, and how long the file was.
///
/// Every offset the loader checks and then uses comes from here. The file's
/// lock masks interrupts and so cannot be held across the mapping and filling
/// of an image, which leaves the file free to change while a load runs; taking
/// the table once is what keeps the numbers the loader tested and the numbers
/// it uses the same ones.
struct Headers {
    /// The file's length when the table was read. A segment's bytes are
    /// checked against this and read against whatever the file holds later.
    size: usize,
    e_type: u16,
    e_entry: u64,
    phoff: u64,
    phent: u64,
    phnum: u64,
    phdrs: alloc::vec::Vec<ProgramHeader>,
}

impl Headers {
    fn read(node: &crate::fs::NodeRef) -> Result<Headers, Errno> {
        let (head, size) = head_of(node)?;
        let e_type = validate(&head)?;
        let e_entry = rd64(&head, 24)?;
        let phoff = rd64(&head, 32)?;
        let phent = rd16(&head, 54)? as u64;
        let phnum = rd16(&head, 56)? as u64;
        if phent < 56 {
            return Err(Errno::ENOEXEC);
        }
        // e_phoff is a 64-bit number from the file, so the end of the table is
        // a sum that wraps: one near the top of the range plus a table of any
        // size is a small number, and a small number is inside the file.
        if phnum * phent > MAX_PHDR_BYTES || sum(&[phoff, phnum * phent])? > size as u64 {
            return Err(Errno::ENOEXEC);
        }
        // Room for the table is asked for before the file is locked: the
        // allocator is the slow half of this, and it has a lock of its own.
        let mut phdrs = alloc::vec::Vec::with_capacity(phnum as usize);
        if node.kind == NodeKind::DataFile {
            // Copied out through the volume, whose lock sleeps, rather than
            // read under the node's.
            let mut table = alloc::vec![0u8; (phnum * phent) as usize];
            if read_into(node, phoff, &mut table)? < table.len() {
                return Err(Errno::ENOEXEC);
            }
            for i in 0..phnum {
                phdrs.push(ProgramHeader::parse(&table, (i * phent) as usize)?);
            }
            return Ok(Headers { size, e_type, e_entry, phoff, phent, phnum, phdrs });
        }
        let inner = node.inner.lock();
        for i in 0..phnum {
            // A file shortened since its length was read fails here, because
            // every field is taken from the buffer the file has now.
            phdrs.push(ProgramHeader::parse(&inner.data, (phoff + i * phent) as usize)?);
        }
        drop(inner);
        Ok(Headers { size, e_type, e_entry, phoff, phent, phnum, phdrs })
    }
}

/// Read the file's bytes at `off` into `dst`, and say how many arrived.
///
/// The node's lock is taken for this one piece and let go after it, so a page
/// is the most the timer is held off by. A file shortened since its headers
/// were read gives back less than was asked for, and the rest of the page
/// stays as the fresh frame was, which is zero. That is what the fault handler
/// does with a page of the same image that arrives later.
///
/// A file on the data volume is read through the volume instead, with no
/// node lock held, and a card that fails the read fails the load.
fn read_into(node: &crate::fs::NodeRef, off: u64, dst: &mut [u8]) -> Result<usize, Errno> {
    if node.kind == NodeKind::DataFile {
        return crate::fs::data::read(node, crate::fs::Offset::new(off), dst);
    }
    let inner = node.inner.lock();
    let start = off as usize;
    let n = inner.data.len().saturating_sub(start).min(dst.len());
    dst[..n].copy_from_slice(&inner.data[start..start + n]);
    Ok(n)
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

/// Map the PT_LOAD segments of the program in `node` into `space`.
/// `base_override` says where to put a relocatable image.
///
/// The file is read a piece at a time rather than held under its lock for the
/// length of the load. That lock masks interrupts, and what happens here is
/// proportional to the size of the program: a page table walk and a frame per
/// page of every segment, and a copy of the pages an image cannot leave to a
/// fault. The header and the program header table are read once and kept; the
/// bytes of a segment are copied a page at a time, with the lock taken for
/// each page and let go after it.
///
/// The pages this assembles are filled before they are published, so nothing
/// reaches an address of the image until it holds what it is going to hold.
/// That is also why `space` need not be the address space the processor is
/// on: the contents go in through the frames rather than through the
/// addresses they will be read at.
pub fn load_at(
    space: &PageTables,
    node: &crate::fs::NodeRef,
    base_override: Option<u64>,
) -> Result<LoadedImage, Errno> {
    let headers = Headers::read(node)?;
    let phdrs = &headers.phdrs;

    let base = match base_override {
        Some(base) if headers.e_type == ET_DYN => base,
        _ if headers.e_type == ET_DYN => DYN_BASE,
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
        let extent = Extent::of(ph, base, headers.size)?;
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

    // The frames those pages will be, held here until every source has been
    // copied into them. A page two segments share is written by both passes
    // and each finds it through this map, so there is no point at which the
    // image has an address a program could reach that is not finished.
    //
    // What is held at once is the assembled pages rather than the image: a
    // partial head and tail per segment, and any page two of them share. An
    // image of many small segments can make nearly every page one of those,
    // and MAX_IMAGE_PAGES allows 64K of them, which is 256 MiB. That is the
    // same memory either way -- mapping each page as the copy reached it
    // would commit exactly as much -- but until the end it is held here
    // instead of in the page tables.
    let mut fresh: BTreeMap<u64, FreshPage> = BTreeMap::new();
    for page in eager {
        fresh.insert(page, FreshPage::new().ok_or(Errno::ENOMEM)?);
    }

    for (_, extent) in &loads {
        // Copy only what lands in a page assembled here. A fresh frame is
        // already zero, so .bss needs nothing written.
        let mut offset = 0u64;
        while offset < extent.file_len {
            let address = extent.vaddr + offset;
            let page = page_align_down(address);
            let chunk = (page + PAGE_SIZE_U64 - address).min(extent.file_len - offset);
            if let Some(fresh) = fresh.get_mut(&page) {
                let at = (address - page) as usize;
                let end = at + chunk as usize;
                let n = read_into(node, extent.file_offset + offset, &mut fresh.bytes()[at..end])?;
                // These bytes are about to be executed, and on some machines
                // writing them is not enough to make them fetchable. What the
                // maintenance names is the address they were written through,
                // which is not the one they will be fetched from.
                crate::arch::sync_instruction_cache(fresh.bytes()[at..].as_ptr() as u64, n);
            }
            offset += chunk;
        }
    }

    for (page, flags) in &page_flags {
        let Some(fresh) = fresh.remove(page) else {
            continue;
        };
        let mut bits = PRESENT | USER;
        if flags & PF_W != 0 {
            bits |= WRITABLE;
        }
        if flags & PF_X == 0 {
            bits |= NO_EXECUTE;
        }
        space.publish(*page, fresh, bits).map_err(|_| Errno::ENOMEM)?;
    }

    // AT_PHDR must point at the program headers as they sit in memory.
    let mut phdr_addr = 0u64;
    for ph in phdrs {
        if ph.p_type == PT_PHDR {
            phdr_addr = sum(&[base, ph.p_vaddr])?;
        }
    }
    if phdr_addr == 0 {
        let table_end = sum(&[headers.phoff, headers.phnum * headers.phent])?;
        for (index, extent) in &loads {
            let ph = &phdrs[*index];
            if headers.phoff >= ph.p_offset && table_end <= extent.file_offset + extent.file_len {
                phdr_addr = extent.vaddr + (headers.phoff - ph.p_offset);
                break;
            }
        }
    }

    let mut interp = None;
    let mut tls = (0u64, 0u64, 0u64, 0u64);
    for ph in phdrs {
        if ph.p_type == PT_INTERP {
            if sum(&[ph.p_offset, ph.p_filesz])? > headers.size as u64 {
                return Err(Errno::ENOEXEC);
            }
            // p_filesz is a number from the file like any other, and what it
            // measures here is a path, so no more than a path's worth is read.
            let mut name = alloc::vec![0u8; (ph.p_filesz as usize).min(4096)];
            let read = read_into(node, ph.p_offset, &mut name)?;
            if let Ok(text) = core::str::from_utf8(&name[..read]) {
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
        segments.push(Segment {
            start: extent.start,
            end: extent.end,
            prot,
            file: crate::task::FileMap {
                node: node.clone(),
                offset: extent.file_offset - extent.into_page(),
                length: extent.file_len + extent.into_page(),
            },
        });
    }

    Ok(LoadedImage {
        entry: sum(&[base, headers.e_entry])?,
        phdr_addr,
        phent: headers.phent,
        phnum: headers.phnum,
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
