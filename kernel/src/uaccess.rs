//! Copying between kernel and user memory.
//!
//! Every access is validated first: the range must be canonical user memory,
//! and any page that a region covers but has not been faulted in yet is
//! allocated here, so the kernel never takes a page fault on a user pointer.

use crate::abi::Errno;
use crate::mm::paging::{is_user_addr, WRITABLE};
use crate::mm::{page_align_down, PAGE_SIZE_U64};
use alloc::string::String;
use alloc::vec::Vec;

/// Check that `[addr, addr + len)` is usable user memory, faulting pages in.
pub fn validate(addr: u64, len: u64, write: bool) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    let end = addr.checked_add(len).ok_or(Errno::EFAULT)?;
    if !is_user_addr(addr) || !is_user_addr(end.saturating_sub(1)) {
        return Err(Errno::EFAULT);
    }

    let task = crate::sched::current();
    let mut page = page_align_down(addr);
    while page < end {
        match task.space.flags_of(page) {
            Some(flags) => {
                if write && flags & WRITABLE == 0 {
                    return Err(Errno::EFAULT);
                }
            }
            None => {
                if !task.fault_in(page) {
                    return Err(Errno::EFAULT);
                }
                if write {
                    match task.space.flags_of(page) {
                        Some(flags) if flags & WRITABLE != 0 => {}
                        _ => return Err(Errno::EFAULT),
                    }
                }
            }
        }
        page += PAGE_SIZE_U64;
    }
    Ok(())
}

pub fn read_bytes(addr: u64, buf: &mut [u8]) -> Result<(), Errno> {
    validate(addr, buf.len() as u64, false)?;
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
    Ok(())
}

pub fn write_bytes(addr: u64, buf: &[u8]) -> Result<(), Errno> {
    validate(addr, buf.len() as u64, true)?;
    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, buf.len()) };
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

pub fn read_u32(addr: u64) -> Result<u32, Errno> {
    let mut bytes = [0u8; 4];
    read_bytes(addr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn write_u32(addr: u64, value: u32) -> Result<(), Errno> {
    write_bytes(addr, &value.to_le_bytes())
}

pub fn read_struct<T: Copy>(addr: u64) -> Result<T, Errno> {
    let size = core::mem::size_of::<T>();
    validate(addr, size as u64, false)?;
    Ok(unsafe { core::ptr::read_unaligned(addr as *const T) })
}

pub fn write_struct<T: Copy>(addr: u64, value: &T) -> Result<(), Errno> {
    let size = core::mem::size_of::<T>();
    validate(addr, size as u64, true)?;
    unsafe { core::ptr::write_unaligned(addr as *mut T, *value) };
    Ok(())
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
