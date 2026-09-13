//! Copying between kernel and user memory.
//!
//! Every access is validated first: the range must be canonical user memory,
//! and any page that a region covers but has not been faulted in yet is
//! allocated here, so the kernel never takes a page fault on a user pointer.
//!
//! Each call comes in two forms. The plain one is for a caller that holds no
//! reference to the running task and reads it back out of the scheduler; the
//! `_in` one takes the task it was given. Signal delivery, `rt_sigreturn` and
//! the stack an exec builds all reach here while already holding a reference
//! to the task, and taking a second one to the same task alongside theirs is a
//! claim about uniqueness that is not true.

use crate::abi::Errno;
use crate::arch::paging::{is_user_addr, COW, WRITABLE};
use crate::mm::{page_align_down, PAGE_SIZE_U64};
use crate::task::Task;
use alloc::string::String;
use alloc::vec::Vec;

/// Check that `[addr, addr + len)` is usable user memory, faulting pages in.
pub fn validate(addr: u64, len: u64, write: bool) -> Result<(), Errno> {
    validate_in(&crate::sched::current(), addr, len, write)
}

/// The same, against the address space of a task the caller already holds.
pub fn validate_in(task: &Task, addr: u64, len: u64, write: bool) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    let end = addr.checked_add(len).ok_or(Errno::EFAULT)?;
    if !is_user_addr(addr) || !is_user_addr(end.saturating_sub(1)) {
        return Err(Errno::EFAULT);
    }

    let mut page = page_align_down(addr);
    while page < end {
        match task.space().flags_of(page) {
            Some(flags) => {
                // A page shared after a fork is read-only until someone writes
                // to it. The kernel writing on the task's behalf counts, so
                // take the private copy here rather than reporting a bad
                // address.
                //
                // The mark decides, not the write permission. aarch64 keeps
                // the permission the caller asked for and derives read-only
                // from the mark, so a shared page there reads back as writable
                // while the hardware refuses the store; asking the permission
                // alone would let the copy through and fault in the kernel.
                if write && (flags & COW != 0 || flags & WRITABLE == 0) {
                    if !task.handle_cow(page) {
                        return Err(Errno::EFAULT);
                    }
                }
            }
            None => {
                if !task.fault_in(page) {
                    return Err(Errno::EFAULT);
                }
                if write && !writable(task, page) {
                    return Err(Errno::EFAULT);
                }
            }
        }
        page += PAGE_SIZE_U64;
    }
    Ok(())
}

fn writable(task: &Task, page: u64) -> bool {
    matches!(task.space().flags_of(page), Some(flags) if flags & WRITABLE != 0)
}

pub fn read_bytes(addr: u64, buf: &mut [u8]) -> Result<(), Errno> {
    read_bytes_in(&crate::sched::current(), addr, buf)
}

pub fn read_bytes_in(task: &Task, addr: u64, buf: &mut [u8]) -> Result<(), Errno> {
    validate_in(task, addr, buf.len() as u64, false)?;
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
    Ok(())
}

pub fn write_bytes(addr: u64, buf: &[u8]) -> Result<(), Errno> {
    write_bytes_in(&crate::sched::current(), addr, buf)
}

/// Copy into user memory a page at a time, checking each page and writing it
/// with nothing else able to run in between.
///
/// A fork takes write permission away from every page of the address space it
/// copies, the parent's included, so a thread that forks while a sibling is
/// inside a write turns pages that sibling has already checked read-only. The
/// store then faults in the kernel, which is fatal. Checking and copying under
/// the same block closes that, and a page at a time is what keeps the block
/// from being as long as whatever buffer the program passed.
pub fn write_bytes_in(task: &Task, addr: u64, buf: &[u8]) -> Result<(), Errno> {
    if buf.is_empty() {
        return Ok(());
    }
    let end = addr.checked_add(buf.len() as u64).ok_or(Errno::EFAULT)?;
    let mut at = addr;
    while at < end {
        let chunk_end = (page_align_down(at) + PAGE_SIZE_U64).min(end);
        let from = (at - addr) as usize;
        let len = (chunk_end - at) as usize;
        crate::sync::without_interrupts(|_irq| -> Result<(), Errno> {
            validate_in(task, at, len as u64, true)?;
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr().add(from), at as *mut u8, len) };
            Ok(())
        })?;
        at = chunk_end;
    }
    Ok(())
}

pub fn read_u64(addr: u64) -> Result<u64, Errno> {
    let mut bytes = [0u8; 8];
    read_bytes(addr, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub fn write_u64(addr: u64, value: u64) -> Result<(), Errno> {
    write_bytes(addr, &value.to_le_bytes())
}

pub fn write_u64_in(task: &Task, addr: u64, value: u64) -> Result<(), Errno> {
    write_bytes_in(task, addr, &value.to_le_bytes())
}

pub fn read_u64_in(task: &Task, addr: u64) -> Result<u64, Errno> {
    let mut bytes = [0u8; 8];
    read_bytes_in(task, addr, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub fn read_u32(addr: u64) -> Result<u32, Errno> {
    let mut bytes = [0u8; 4];
    read_bytes(addr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn write_u32(addr: u64, value: u32) -> Result<(), Errno> {
    write_bytes(addr, &value.to_le_bytes())
}

pub fn write_u32_in(task: &Task, addr: u64, value: u32) -> Result<(), Errno> {
    write_bytes_in(task, addr, &value.to_le_bytes())
}

pub fn read_struct<T: Copy>(addr: u64) -> Result<T, Errno> {
    let size = core::mem::size_of::<T>();
    validate(addr, size as u64, false)?;
    Ok(unsafe { core::ptr::read_unaligned(addr as *const T) })
}

pub fn write_struct<T: Copy>(addr: u64, value: &T) -> Result<(), Errno> {
    let size = core::mem::size_of::<T>();
    let task = crate::sched::current();
    // Checked and written together, for the same reason `write_bytes_in` is.
    crate::sync::without_interrupts(|_irq| -> Result<(), Errno> {
        validate_in(&task, addr, size as u64, true)?;
        unsafe { core::ptr::write_unaligned(addr as *mut T, *value) };
        Ok(())
    })
}

/// Read a NUL-terminated string, at most `max` bytes long.
pub fn read_cstr(addr: u64, max: usize) -> Result<String, Errno> {
    let mut out = Vec::new();
    let mut cursor = addr;
    loop {
        if out.len() >= max {
            return Err(Errno::ENAMETOOLONG);
        }
        // Validate a page at a time so a string near the end of a mapping
        // does not require the next page to exist.
        let chunk_end = (page_align_down(cursor) + PAGE_SIZE_U64).min(addr + max as u64);
        validate(cursor, chunk_end - cursor, false)?;
        while cursor < chunk_end {
            let byte = unsafe { *(cursor as *const u8) };
            cursor += 1;
            if byte == 0 {
                return String::from_utf8(out).map_err(|_| Errno::EINVAL);
            }
            out.push(byte);
        }
    }
}

/// Read the iovec array for readv/writev.
pub fn read_iovecs(addr: u64, count: usize) -> Result<Vec<crate::abi::IoVec>, Errno> {
    if count > 1024 {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(read_struct::<crate::abi::IoVec>(addr + (i * 16) as u64)?);
    }
    Ok(out)
}
