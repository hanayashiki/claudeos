//! Copying between kernel and user memory.
//!
//! Every access is validated first: the range must be canonical user memory,
//! and any page that a region covers but has not been faulted in yet is
//! allocated here, so the kernel never takes a page fault on a user pointer.
//! The check and the access that follows it are one block with interrupts off,
//! a page at a time, because a sibling thread on the same address space can
//! unmap what was just checked or take its write permission away, and the
//! access is then a kernel fault on a page the check said was there.
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
                //
                // Taking the copy is one step from the entry it reads to the
                // entry it writes, so it asks for interrupts to be off for the
                // whole of it. Opening the section here rather than around the
                // loop keeps it to one page: the caller may be validating a
                // buffer megabytes long, and the pages it has not reached yet
                // are no part of this.
                if write && (flags & COW != 0 || flags & WRITABLE == 0) {
                    let copied =
                        crate::sync::without_interrupts(|irq| task.handle_cow(page, irq));
                    if !copied {
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

/// Copy out of user memory a page at a time, checking each page and reading it
/// with nothing else able to run in between.
///
/// A sibling thread that unmaps the buffer between the check and the read
/// leaves the kernel reading a page that is not present, which faults in the
/// kernel and is fatal to the machine rather than to the program. Checking and
/// copying under the same block closes that, and a page at a time is what
/// keeps the block from being as long as whatever buffer the program passed.
///
/// A range that goes bad partway is a bad address with the pages before it
/// already copied, which is what the write side does as well.
pub fn read_bytes_in(task: &Task, addr: u64, buf: &mut [u8]) -> Result<(), Errno> {
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
            validate_in(task, at, len as u64, false)?;
            unsafe {
                core::ptr::copy_nonoverlapping(at as *const u8, buf.as_mut_ptr().add(from), len)
            };
            Ok(())
        })?;
        at = chunk_end;
    }
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

/// Copy `len` bytes from one user address to another, a page at a time,
/// checking both and copying with nothing else able to run in between.
///
/// Moving a mapping is the one place the kernel reads through one user address
/// and writes through another. Checking the two and then copying left the same
/// window the two calls above close: a sibling thread that forks takes write
/// permission away from every page of the address space, the destination among
/// them, and the copy that follows is a kernel store into a page that is
/// read-only by then. A sibling that unmaps either range leaves the copy
/// reading or writing a page that is not present. Both are fatal to the
/// machine rather than to the program.
///
/// A chunk stops at the end of a page on either side, so the block is never
/// longer than one page of copying whatever the caller is moving. A range that
/// goes bad partway is a bad address with the bytes before it already copied,
/// which is what the two calls above do as well.
///
/// The two ranges must not overlap. The one caller moves a mapping into a
/// region that was free, so they never do.
///
/// Only the form that takes the task, because that caller is holding a
/// reference to it already.
pub fn copy_within_user_in(task: &Task, dst: u64, src: u64, len: u64) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    src.checked_add(len).ok_or(Errno::EFAULT)?;
    dst.checked_add(len).ok_or(Errno::EFAULT)?;
    let mut done = 0u64;
    while done < len {
        let from = src + done;
        let to = dst + done;
        let chunk = (page_align_down(from) + PAGE_SIZE_U64 - from)
            .min(page_align_down(to) + PAGE_SIZE_U64 - to)
            .min(len - done);
        crate::sync::without_interrupts(|_irq| -> Result<(), Errno> {
            validate_in(task, from, chunk, false)?;
            validate_in(task, to, chunk, true)?;
            unsafe {
                core::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, chunk as usize)
            };
            Ok(())
        })?;
        done += chunk;
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
    let task = crate::sched::current();
    // Checked and read together, for the same reason `read_bytes_in` is.
    crate::sync::without_interrupts(|_irq| -> Result<T, Errno> {
        validate_in(&task, addr, size as u64, false)?;
        Ok(unsafe { core::ptr::read_unaligned(addr as *const T) })
    })
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
///
/// The string is copied in pieces and the NUL looked for afterwards, because a
/// scan that pushed a byte at a time would be growing a vector inside the
/// block, and the heap it grows into maps pages. A piece stops at the end of
/// the page, so a string near the end of a mapping does not require the next
/// page to exist, and at `CHUNK` bytes, so the usual short path costs one
/// small allocation rather than a page of copying.
pub fn read_cstr(addr: u64, max: usize) -> Result<String, Errno> {
    const CHUNK: u64 = 256;
    let task = crate::sched::current();
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = addr;
    loop {
        if out.len() >= max {
            return Err(Errno::ENAMETOOLONG);
        }
        let chunk_end = (page_align_down(cursor) + PAGE_SIZE_U64)
            .min(addr + max as u64)
            .min(cursor + CHUNK);
        let len = (chunk_end - cursor) as usize;
        let start = out.len();
        // The room for the piece is taken before interrupts go off, for the
        // reason above.
        out.reserve(len);
        crate::sync::without_interrupts(|_irq| -> Result<(), Errno> {
            validate_in(&task, cursor, len as u64, false)?;
            let chunk = unsafe { core::slice::from_raw_parts(cursor as *const u8, len) };
            out.extend_from_slice(chunk);
            Ok(())
        })?;
        cursor = chunk_end;
        if let Some(at) = out[start..].iter().position(|byte| *byte == 0) {
            out.truncate(start + at);
            return String::from_utf8(out).map_err(|_| Errno::EINVAL);
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
