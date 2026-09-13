//! Memory-related system calls.

use crate::abi::*;
use crate::arch::paging::{is_user_addr, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE};
use crate::sched;
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
            drop(task.space.unmap(page));
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
        // over rather than reported.
        if hint != 0
            && hint >= USER_MMAP_BASE
            && in_user_space(hint, len)
            && task.find_vma(hint).is_none()
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
        task.add_vma(base, base + len, prot, flags);

        // File-backed pages are populated up front from the file contents.
        let mut page = base;
        while page < base + len {
            task.space
                .map_new(page, PRESENT | WRITABLE | USER)
                .map_err(|_| Errno::ENOMEM)?;
            page += PAGE_SIZE_U64;
        }
        let mut buf = alloc::vec![0u8; len as usize];
        let n = node.read_at(offset, &mut buf)?;
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), base as *mut u8, n);
        }
        // Apply the requested protection now that the contents are in place.
        let bits = prot_to_flags(prot);
        let mut page = base;
        while page < base + len {
            task.space.set_flags(page, bits);
            page += PAGE_SIZE_U64;
        }
    }

    Ok(base)
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
    let mut page = start;
    while page < end {
        drop(task.space.unmap(page));
        page += PAGE_SIZE_U64;
    }
    task.remove_vma_range(start, end);
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
        if let Some(existing) = task.space.flags_of(page) {
            let bits = if existing & crate::arch::paging::COW != 0 {
                (bits & !WRITABLE) | crate::arch::paging::COW
            } else {
                bits
            };
            task.space.set_flags(page, bits);
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
    let copy_len = old_size.min(new_size) as usize;
    uaccess::validate(old_addr, copy_len as u64, false)?;
    uaccess::validate(base, copy_len as u64, true)?;
    unsafe {
        core::ptr::copy_nonoverlapping(old_addr as *const u8, base as *mut u8, copy_len);
    }
    unmap_range(old_addr, old_size);
    Ok(base)
}
