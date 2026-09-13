//! Memory-related system calls.

use crate::abi::*;
use crate::arch::paging::{is_user_addr, FreshPage, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE};
use crate::sched;
use crate::sync::without_interrupts;
use crate::uaccess;

/// Upper bound on a single mapping, so a bogus length fails fast.
const MAX_MAPPING: u64 = 1 << 40;

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
    let task = sched::current();
    let (brk_start, current_brk) = (task.brk_start(), task.brk());
    if request == 0 || request < brk_start {
        return Ok(current_brk);
    }
    let new_brk = page_align_up(request);
    if new_brk > USER_MMAP_BASE {
        return Ok(current_brk);
    }

    if new_brk < current_brk {
        // Shrinking: give the frames back. Taking the mapping away hands
        // back the reference the entry held, and dropping it is the release.
        let mut page = new_brk;
        while page < current_brk {
            without_interrupts(|irq| drop(task.space().unmap(page, irq)));
            page += PAGE_SIZE_U64;
        }
    }
    // Growth is lazy: pages are faulted in on first touch.
    task.set_brk(new_brk);
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

    let base = if flags & MAP_FIXED != 0 {
        if addr == 0 || addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, len) {
            return Err(Errno::EINVAL);
        }
        // Replace whatever was there.
        unmap_range(addr, len);
        addr
    } else {
        let task = sched::current();
        let hint = page_align_down(addr);
        // A hint is a suggestion, so one that cannot be honoured is passed
        // over rather than reported. What has to be free is the whole range
        // the mapping will occupy: asking only about the page the hint names
        // takes a hint that sits just below a region already there and lays
        // the rest of the new mapping across it.
        if hint != 0
            && hint >= USER_MMAP_BASE
            && in_user_space(hint, len)
            && task.range_is_free(hint, hint + len)
        {
            hint
        } else {
            task.find_free_region(len)
        }
    };

    let anonymous = flags & MAP_ANONYMOUS != 0 || fd < 0;

    if anonymous {
        // Record the region; pages arrive on demand.
        sched::current().add_vma(base, base + len, prot, flags);
    } else {
        let file = sched::current().fds.get(fd as i32)?;
        let node = file.node().ok_or(Errno::ENODEV)?.clone();
        let task = sched::current();

        let offset = crate::fs::Offset::new(offset);
        if let Err(err) = populate_from_file(&node, base, len, offset, prot_to_flags(prot)) {
            // Pages already published are reachable and pages not yet
            // published are not, so a failure part of the way through leaves a
            // range that is partly the file. Take back what this call put in,
            // and record nothing: a mapping that fails leaves no mapping. What
            // MAP_FIXED took away above is gone either way, because replacing
            // a mapping destroys it before there is anything to put in its
            // place.
            let mut page = base;
            while page < base + len {
                without_interrupts(|irq| drop(task.space().unmap(page, irq)));
                page += PAGE_SIZE_U64;
            }
            return Err(err);
        }
        // Last, so that a thread sharing this address space that touches the
        // range while the pages are going in finds no region rather than one
        // whose pages are not there yet: a fault on the latter is served an
        // anonymous page of zeroes, which is neither the file's contents nor
        // something this call could then publish over.
        task.add_vma(base, base + len, prot, flags);
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
/// the whole read, so the timer is held off by a page's copy at a time. What
/// that costs is that the file can change between one page and the next, so a
/// mapping can come out part old and part new. A page the file no longer
/// reaches is read short and the rest of the frame stays zero, which is what a
/// page of a mapping that arrives later from a fault already does.
fn populate_from_file(
    node: &crate::fs::NodeRef,
    base: u64,
    len: u64,
    offset: crate::fs::Offset,
    bits: u64,
) -> Result<(), Errno> {
    let task = sched::current();
    let mut page = base;
    while page < base + len {
        let mut fresh = FreshPage::new().ok_or(Errno::ENOMEM)?;
        let at = offset.advanced(page - base)?;
        let n = node.read_at(at, fresh.bytes())?;
        // These bytes were written through the direct map rather than the
        // address they will be fetched from, and an executable mapping is
        // what a program maps a shared library with.
        if n > 0 && bits & NO_EXECUTE == 0 {
            crate::arch::sync_instruction_cache(fresh.bytes().as_ptr() as u64, n);
        }
        task.space().publish(page, fresh, bits).map_err(|_| Errno::ENOMEM)?;
        page += PAGE_SIZE_U64;
    }
    Ok(())
}

fn unmap_range(addr: u64, len: u64) {
    // Every path into here has already refused an address outside user space;
    // this is the seam they all cross, so it is checked once more where a new
    // caller cannot miss it.
    if !in_user_space(addr, len) {
        return;
    }
    let task = sched::current();
    let start = page_align_down(addr);
    let end = page_align_up(addr + len);
    // The region goes first. A thread sharing the address space that touches
    // this range while the pages are being taken away faults, and a fault
    // inside a region that is still recorded is served a fresh page of zeroes:
    // one that this call has already walked past and so leaves behind, at an
    // address the program was told nothing is at. With the region gone first
    // there is nothing here to fault into, which is what an unmapped range is.
    task.remove_vma_range(start, end);
    let mut page = start;
    while page < end {
        // One page at a time rather than one section around the loop: a range
        // is as long as a program asks for, and the timer may not be held off
        // for as long as it takes to walk one. What has to be inside one
        // section is the emptying of a table and the decision to free it,
        // which is the whole of what `unmap` does.
        without_interrupts(|irq| drop(task.space().unmap(page, irq)));
        page += PAGE_SIZE_U64;
    }
}

pub fn munmap(addr: u64, length: u64) -> SysResult {
    if length == 0 || addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, length) {
        return Err(Errno::EINVAL);
    }
    unmap_range(addr, length);
    Ok(0)
}

pub fn mprotect(addr: u64, length: u64, prot: u64) -> SysResult {
    if addr & (PAGE_SIZE_U64 - 1) != 0 || !in_user_space(addr, length) {
        return Err(Errno::EINVAL);
    }
    let task = sched::current();
    let start = page_align_down(addr);
    let end = page_align_up(addr + length);
    let bits = prot_to_flags(prot);

    let mut page = start;
    while page < end {
        // Only pages that exist are retagged; the rest inherit the new
        // protection when they fault in. A page still shared after a fork
        // keeps its copy-on-write mark and stays read-only whatever is asked
        // for: the copy happens when it is written to, as before.
        if let Some(existing) = task.space().flags_of(page) {
            let bits = if existing & crate::arch::paging::COW != 0 {
                (bits & !WRITABLE) | crate::arch::paging::COW
            } else {
                bits
            };
            task.space().set_flags(page, bits);
        }
        page += PAGE_SIZE_U64;
    }

    task.set_vma_prot(start, end, prot);
    Ok(0)
}

pub fn mremap(old_addr: u64, old_size: u64, new_size: u64, _flags: u64) -> SysResult {
    let old_size = page_align_up(old_size);
    let new_size = page_align_up(new_size);
    if !in_user_space(old_addr, old_size.max(new_size)) {
        return Err(Errno::EINVAL);
    }
    if new_size <= old_size {
        if new_size < old_size {
            unmap_range(old_addr + new_size, old_size - new_size);
        }
        return Ok(old_addr);
    }

    let task = sched::current();
    let vma = task.find_vma(old_addr).ok_or(Errno::EFAULT)?;

    // Grow in place when the space directly above is free.
    let tail_start = old_addr + old_size;
    let tail_end = old_addr + new_size;
    let blocked = task
        .snapshot_vmas()
        .iter()
        .any(|v| v.start < tail_end && tail_start < v.end && v.start != vma.start);
    if !blocked {
        task.add_vma(tail_start, tail_end, vma.prot, vma.flags);
        return Ok(old_addr);
    }

    // Otherwise relocate.
    let base = task.find_free_region(new_size);
    task.add_vma(base, base + new_size, vma.prot, vma.flags);
    // Through the checked path, which takes a page at a time with interrupts
    // off: a sibling thread that forks between the check and the copy takes
    // write permission away from every page of the address space, the
    // destination among them, and a bare copy is then a kernel store into a
    // read-only page.
    uaccess::copy_within_user_in(&task, base, old_addr, old_size.min(new_size))?;
    unmap_range(old_addr, old_size);
    Ok(base)
}
