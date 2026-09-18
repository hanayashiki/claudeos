//! Memory-related system calls.
//!
//! Each of these changes the record of regions and the page tables together.
//! What decides where a mapping goes, what checks the range is free, and what
//! writes the record down are one step under the address space's lock; so are
//! reading a descriptor and storing the one that replaces it. Apart, a sibling
//! thread runs between the halves and sees a state neither call ever meant to
//! exist: a range checked free and not yet claimed, a page mapped with no
//! region over it, a descriptor written for a frame the space no longer owns.
//! Those were findings 5, 8 and 9 of docs/audit-2026-09-18-unsafe.md.
//!
//! The record goes first and the pages follow, which is what lets a range be
//! walked in pieces. Once the region is gone, or the break is below the range,
//! there is nothing left for a fault to publish into, so the lock is let go
//! between pieces and the sweep cannot be raced from behind. Holding it for
//! the whole of a range instead masks interrupts for as long as the largest
//! range takes, which on a 64 MiB one was milliseconds.
//!
//! What the lock may not be held across at all is anything that allocates,
//! zeroes or reads a file, because it masks interrupts. A call that needs a
//! page therefore makes it first and publishes it under the lock, and gives
//! back what the publish did not take once the lock is let go.

use crate::abi::*;
use crate::arch::paging::{is_user_addr, COW, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::space::{Mm, Vma};
use crate::mm::tables::Prepared;
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE};
use crate::sched;
use crate::uaccess;
use alloc::sync::Arc;

/// Upper bound on a single mapping, so a bogus length fails fast.
const MAX_MAPPING: u64 = 1 << 40;

/// How many pages one turn of the lock retags. One last-level table's worth,
/// for the reason `Mm::unmap_pages` gives for taking a range away in pieces.
const PAGES_PER_TURN: u64 = crate::mm::walk::ENTRIES as u64;

/// True when `[addr, addr + len)` lies entirely in the half a program owns.
///
/// Every address in these calls comes from the program, and an address in the
/// other half names the kernel's own memory. Those tables are shared by
/// reference with every address space, so a mapping taken away there is taken
/// away from the kernel as well and the frame goes back to the allocator while
/// the kernel is still reading it. The comparison against `USER_MMAP_BASE`
/// that the hint used to be given does not stand in for this: it is unsigned,
/// and every kernel address is above it.
fn in_user_space(addr: u64, len: u64) -> bool {
    match addr.checked_add(len) {
        Some(end) => is_user_addr(addr) && is_user_addr(end.saturating_sub(1)),
        None => false,
    }
}

/// The address space of the task making the call, held for the length of it.
///
/// A task that entered a system call from a program is running in one. A
/// reference of the call's own is what keeps the tables it walks alive while
/// it is preempted, whatever the task's threads do meanwhile.
fn current_mm() -> Result<Arc<Mm>, Errno> {
    sched::current().mm().ok_or(Errno::EFAULT)
}

fn prot_to_flags(prot: u64) -> u64 {
    let mut bits = PRESENT | USER;
    if prot & PROT_WRITE != 0 {
        bits |= WRITABLE;
    }
    if prot & PROT_EXEC == 0 {
        bits |= NO_EXECUTE;
    }
    bits
}

pub fn brk(request: u64) -> SysResult {
    let mm = current_mm()?;
    let current_brk = {
        let mut space = mm.lock();
        let (brk_start, current_brk) = (space.brk_start, space.brk);
        if request == 0 || request < brk_start {
            return Ok(current_brk);
        }
        let new_brk = page_align_up(request);
        if new_brk > USER_MMAP_BASE {
            return Ok(current_brk);
        }
        // The bound moves first, in the same step that reads it. A sibling
        // faulting above the new break is then refused, rather than being
        // served a page the sweep below has already walked past and so leaves
        // mapped at an address the program was told nothing is at.
        // Growth is lazy: pages are faulted in on first touch.
        space.brk = new_brk;
        if new_brk >= current_brk {
            return Ok(new_brk);
        }
        current_brk
    };
    let new_brk = page_align_up(request);
    mm.unmap_pages(new_brk, current_brk);
    Ok(new_brk)
}

pub fn mmap(
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
    offset: u64,
) -> SysResult {
    if length == 0 || length > MAX_MAPPING {
        return Err(Errno::EINVAL);
    }
    let len = page_align_up(length);
    let mm = current_mm()?;
    let anonymous = flags & MAP_ANONYMOUS != 0 || fd < 0;

    // The file is looked up before the lock, because a file descriptor table
    // is another lock and a node is an allocation.
    let file = if anonymous {
        None
    } else {
        let open = sched::current().fds.get(fd as i32)?;
        Some(open.node().ok_or(Errno::ENODEV)?.clone())
    };

    // Placement, the check that the range is free, and the region going in are
    // one step. Two threads asking for a mapping with no address of their own
    // otherwise both pass the check and both claim the range.
    //
    // A file-backed region is recorded with its file before its pages are
    // read, not after. A sibling that touches the range while they are going
    // in then faults into a region that says where the bytes come from and
    // fills the page it wants from the same file; recorded without one, it
    // would be served an anonymous page of zeroes, and recorded not at all,
    // the range would be free for another mapping to be placed across.
    let offset = crate::fs::Offset::new(offset);
    let map = file.map(|node| crate::mm::space::FileMap {
        node,
        offset: offset.raw(),
        length: len,
    });
    if flags & MAP_FIXED != 0 {
        if addr == 0 || addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, len) {
            return Err(Errno::EINVAL);
        }
        // Replace whatever was there: the region goes and so do its pages,
        // before anything of this mapping is recorded. The address came from
        // the program, so there is no placement to hold still around it.
        mm.unmap_recorded(page_align_down(addr), page_align_up(addr + len));
    }
    let base = {
        let mut space = mm.lock();
        let base = if flags & MAP_FIXED != 0 {
            addr
        } else {
            let hint = page_align_down(addr);
            // A hint is a suggestion, so one that cannot be honoured is passed
            // over rather than reported. What has to be free is the whole range
            // the mapping will occupy: asking only about the page the hint names
            // takes a hint that sits just below a region already there and lays
            // the rest of the new mapping across it.
            if hint != 0
                && hint >= USER_MMAP_BASE
                && in_user_space(hint, len)
                && space.range_is_free(hint, hint + len)
            {
                hint
            } else {
                space.find_free_region(len)
            }
        };
        space.vmas.push(Vma {
            start: base,
            end: base + len,
            prot,
            flags,
            file: map.clone(),
        });
        base
    };

    if let Some(map) = map {
        // The pages are read from the file and published one at a time, with
        // the reading outside the lock and the publish under it.
        let bits = prot_to_flags(prot);
        if let Err(err) = populate_from_file(&mm, &map.node, base, len, offset, bits) {
            // A mapping that fails leaves no mapping. What MAP_FIXED took away
            // above is gone either way, because replacing a mapping destroys
            // it before there is anything to put in its place.
            mm.unmap_recorded(base, base + len);
            return Err(err);
        }
    }

    Ok(base)
}

/// Fill `[base, base + len)` from the file at `offset` and publish each page
/// with `bits`.
///
/// A page is read into a frame the allocator has just handed over, through the
/// kernel's own view of memory, and goes into the address space once with the
/// protection the caller asked for. The mapping is visible to every thread
/// sharing the address space from the moment its first page goes in, so a page
/// mapped wide enough to be written through and narrowed afterwards is one a
/// sibling can read blank, and on aarch64, where execute permission is its own
/// bit, run: for the length of the whole file read, not one page of it.
///
/// The file's lock is taken and let go once per page rather than held across
/// the whole read, and the address space's lock is taken only for the publish.
/// What that costs is that the file can change between one page and the next,
/// so a mapping can come out part old and part new. A page the file no longer
/// reaches is read short and the rest of the frame stays zero, which is what a
/// page of a mapping that arrives later from a fault already does.
fn populate_from_file(
    mm: &Arc<Mm>,
    node: &crate::fs::NodeRef,
    base: u64,
    len: u64,
    offset: crate::fs::Offset,
    bits: u64,
) -> Result<(), Errno> {
    let mut page = base;
    while page < base + len {
        let mut fresh = Prepared::new(0).ok_or(Errno::ENOMEM)?;
        let at = offset.advanced(page - base)?;
        let n = node.read_at(at, fresh.bytes())?;
        // These bytes were written through the direct map rather than the
        // address they will be fetched from, and an executable mapping is
        // what a program maps a shared library with.
        if n > 0 && bits & NO_EXECUTE == 0 {
            crate::arch::sync_instruction_cache(fresh.bytes().as_ptr() as u64, n);
        }
        let done = mm.publish_page(page, &mut fresh, bits);
        // Outside the lock, whether it was taken or not.
        drop(fresh);
        match done {
            Ok(_) => {}
            // A sibling faulted on this page of the region while this call was
            // reading it, and filled it from the same file at the same offset.
            // What is there is what this would have put there.
            Err(crate::mm::space::Refused::Occupied) => {}
            Err(_) => return Err(Errno::ENOMEM),
        }
        page += PAGE_SIZE_U64;
    }
    Ok(())
}

pub fn munmap(addr: u64, length: u64) -> SysResult {
    if length == 0 || addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, length) {
        return Err(Errno::EINVAL);
    }
    let mm = current_mm()?;
    mm.unmap_recorded(page_align_down(addr), page_align_up(addr + length));
    Ok(0)
}

pub fn mprotect(addr: u64, length: u64, prot: u64) -> SysResult {
    if addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, length) {
        return Err(Errno::EINVAL);
    }
    let mm = current_mm()?;
    let start = page_align_down(addr);
    let end = page_align_up(addr + length);
    let bits = prot_to_flags(prot);

    // The record changes first, so a page faulted in while the pages already
    // there are being retagged arrives with the protection that was asked for
    // rather than the one being replaced. That is what lets the lock be let go
    // between pieces of a long range.
    mm.lock().set_vma_prot(start, end, prot);

    let mut page = start;
    while page < end {
        let stop = end.min(page + PAGES_PER_TURN * PAGE_SIZE_U64);
        let mut space = mm.lock();
        while page < stop {
            // Only pages that exist are retagged; the rest inherit the new
            // protection when they fault in. A page still shared after a fork
            // keeps its copy-on-write mark and stays read-only whatever is
            // asked for: the copy happens when it is written to, as before.
            //
            // What the descriptor says and what is stored back are one step
            // under the lock. Apart, a sibling's unmap between them stored a
            // descriptor for a frame this address space no longer owned, and a
            // sibling's copy-on-write break stored the shared frame back over
            // the private copy it had just made.
            if let Some(existing) = space.flags_of(page) {
                let bits = if existing & COW != 0 { (bits & !WRITABLE) | COW } else { bits };
                space.protect(page, bits);
            }
            page += PAGE_SIZE_U64;
        }
    }
    Ok(0)
}

pub fn mremap(old_addr: u64, old_size: u64, new_size: u64, _flags: u64) -> SysResult {
    let old_size = page_align_up(old_size);
    let new_size = page_align_up(new_size);
    if !in_user_space(old_addr, old_size.max(new_size)) {
        return Err(Errno::EINVAL);
    }
    let mm = current_mm()?;
    if new_size <= old_size {
        if new_size < old_size {
            mm.unmap_recorded(old_addr + new_size, old_addr + old_size);
        }
        return Ok(old_addr);
    }

    // Deciding where the mapping goes and recording it there are one step, so
    // that a sibling asking for a mapping of its own cannot be given the range
    // this has just chosen.
    let (base, in_place) = {
        let mut space = mm.lock();
        let vma = space.find_vma(old_addr).ok_or(Errno::EFAULT)?.clone();
        // Grow in place when the space directly above is free.
        let tail_start = old_addr + old_size;
        let tail_end = old_addr + new_size;
        let blocked = space
            .vmas
            .iter()
            .any(|v| v.start < tail_end && tail_start < v.end && v.start != vma.start);
        if !blocked {
            space.vmas.push(Vma {
                start: tail_start,
                end: tail_end,
                prot: vma.prot,
                flags: vma.flags,
                file: None,
            });
            (old_addr, true)
        } else {
            let base = space.find_free_region(new_size);
            space.vmas.push(Vma {
                start: base,
                end: base + new_size,
                prot: vma.prot,
                flags: vma.flags,
                file: None,
            });
            (base, false)
        }
    };
    if in_place {
        return Ok(old_addr);
    }

    // Through the checked path, which takes a page at a time with interrupts
    // off: a sibling thread that forks between the check and the copy takes
    // write permission away from every page of the address space, the
    // destination among them, and a bare copy is then a kernel store into a
    // read-only page.
    let task = sched::current();
    uaccess::copy_within_user_in(&task, base, old_addr, old_size.min(new_size))?;
    mm.unmap_recorded(old_addr, old_addr + old_size);
    Ok(base)
}
