//! File and descriptor system calls.

use crate::abi::*;
use crate::fs::{self, pipe, FileBacking, Node, NodeKind, OpenFile};
use crate::sched;
use crate::uaccess;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// Largest single transfer accepted, to bound kernel buffers.
const MAX_IO: usize = 16 * 1024 * 1024;

/// Turn a (dirfd, path) pair into an absolute, normalised path.
pub fn resolve_at(dirfd: i64, path_addr: u64) -> Result<String, Errno> {
    let path = uaccess::read_cstr(path_addr, 4096)?;
    resolve_str(dirfd, &path)
}

pub fn resolve_str(dirfd: i64, path: &str) -> Result<String, Errno> {
    if path.starts_with('/') {
        return Ok(fs::normalize("/", path));
    }
    if dirfd == AT_FDCWD {
        let cwd = sched::current().cwd.clone();
        return Ok(fs::normalize(&cwd, path));
    }
    let file = sched::current().fds.get(dirfd as i32)?;
    if file.path.is_empty() {
        return Err(Errno::EBADF);
    }
    Ok(fs::normalize(&file.path, path))
}

/// Paths under /proc that the kernel answers with something else.
///
/// `/proc/self` names the calling process's own directory, and a few entries
/// under it are links whose target lies outside /proc.
fn procfs_override(path: &str) -> Option<String> {
    if !path.starts_with("/proc/") && path != "/proc/self" {
        return None;
    }
    let task = sched::current();
    let pid = task.pid;

    let rewritten = if path == "/proc/self" {
        Some(alloc::format!("/proc/{}", pid))
    } else {
        path.strip_prefix("/proc/self/")
            .map(|rest| alloc::format!("/proc/{}/{}", pid, rest))
    };
    let effective: &str = rewritten.as_deref().unwrap_or(path);

    let own_prefix = alloc::format!("/proc/{}/", pid);
    if let Some(entry) = effective.strip_prefix(&own_prefix) {
        match entry {
            "exe" => return Some(task.exe_path.clone()),
            "cwd" => return Some(task.cwd.clone()),
            _ => {}
        }
        if let Some(number) = entry.strip_prefix("fd/") {
            if let Ok(fd) = number.parse::<i32>() {
                if let Ok(file) = task.fds.get(fd) {
                    return Some(file.path.clone());
                }
            }
        }
    } else if let Some(rest) = effective.strip_prefix("/proc/") {
        // Another process's exe link.
        if let Some((number, "exe")) = rest.split_once('/') {
            if let Ok(other) = number.parse::<u32>() {
                if let Some(target) = sched::find(other) {
                    return Some(target.exe_path.clone());
                }
            }
        }
    }

    rewritten
}

pub fn read(fd: i32, buf_addr: u64, len: u64) -> SysResult {
    let len = (len as usize).min(MAX_IO);
    if len == 0 {
        return Ok(0);
    }
    let file = sched::current().fds.get(fd)?;
    // Validate up front so a bad pointer fails before any data is consumed.
    uaccess::validate(buf_addr, len as u64, true)?;
    let mut buf = vec![0u8; len];
    let n = file.read(&mut buf)?;
    uaccess::write_bytes(buf_addr, &buf[..n])?;
    Ok(n as u64)
}

pub fn write(fd: i32, buf_addr: u64, len: u64) -> SysResult {
    let len = (len as usize).min(MAX_IO);
    if len == 0 {
        return Ok(0);
    }
    let file = sched::current().fds.get(fd)?;
    let mut buf = vec![0u8; len];
    uaccess::read_bytes(buf_addr, &mut buf)?;
    let n = file.write(&buf)?;
    Ok(n as u64)
}

pub fn readv(fd: i32, iov_addr: u64, count: usize) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let iovs = uaccess::read_iovecs(iov_addr, count)?;
    let mut total = 0u64;
    for iov in iovs {
        if iov.len == 0 {
            continue;
        }
        let len = (iov.len as usize).min(MAX_IO);
        let mut buf = vec![0u8; len];
        let n = file.read(&mut buf)?;
        uaccess::write_bytes(iov.base, &buf[..n])?;
        total += n as u64;
        if n < len {
            break;
        }
    }
    Ok(total)
}

pub fn writev(fd: i32, iov_addr: u64, count: usize) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let iovs = uaccess::read_iovecs(iov_addr, count)?;
    let mut total = 0u64;
    for iov in iovs {
        if iov.len == 0 {
            continue;
        }
        let len = (iov.len as usize).min(MAX_IO);
        let mut buf = vec![0u8; len];
        uaccess::read_bytes(iov.base, &mut buf)?;
        let n = file.write(&buf)?;
        total += n as u64;
        if n < len {
            break;
        }
    }
    Ok(total)
}

pub fn pread(fd: i32, buf_addr: u64, len: u64, offset: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ESPIPE)?;
    let len = (len as usize).min(MAX_IO);
    let mut buf = vec![0u8; len];
    let n = node.read_at(offset, &mut buf)?;
    uaccess::write_bytes(buf_addr, &buf[..n])?;
    Ok(n as u64)
}

pub fn pwrite(fd: i32, buf_addr: u64, len: u64, offset: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ESPIPE)?;
    let len = (len as usize).min(MAX_IO);
    let mut buf = vec![0u8; len];
    uaccess::read_bytes(buf_addr, &mut buf)?;
    let n = node.write_at(offset, &buf)?;
    Ok(n as u64)
}

pub fn openat(dirfd: i64, path_addr: u64, flags: u32, mode: u32) -> SysResult {
    let mut path = resolve_at(dirfd, path_addr)?;
    if let Some(target) = procfs_override(&path) {
        path = target;
    }

    let node = match fs::lookup(&path) {
        Ok(node) => {
            if flags & O_EXCL != 0 && flags & O_CREAT != 0 {
                return Err(Errno::EEXIST);
            }
            node
        }
        Err(Errno::ENOENT) if flags & O_CREAT != 0 => {
            let umask = sched::current().umask;
            fs::create(&path, mode & !umask)?
        }
        Err(err) => return Err(err),
    };

    if flags & O_DIRECTORY != 0 && !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    if node.is_dir() && (flags & O_ACCMODE) != O_RDONLY {
        return Err(Errno::EISDIR);
    }
    if flags & O_TRUNC != 0 && !node.is_dir() {
        node.truncate(0)?;
    }

    let file = OpenFile::from_node_at(node, flags, &path);
    let cloexec = flags & O_CLOEXEC != 0;
    let fd = sched::current().fds.alloc(file, cloexec)?;
    Ok(fd as u64)
}

pub fn close(fd: i32) -> SysResult {
    sched::current().fds.close(fd)?;
    Ok(0)
}

pub fn close_range(first: u32, last: u32) -> SysResult {
    let table = &mut sched::current().fds;
    for fd in first..=last.min(fs::MAX_FDS as u32 - 1) {
        let _ = table.close(fd as i32);
    }
    Ok(0)
}

pub fn lseek(fd: i32, offset: i64, whence: u32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    file.seek(offset, whence)
}

pub fn fstat(fd: i32, out: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let stat = file.stat();
    uaccess::write_struct(out, &stat)?;
    Ok(0)
}

pub fn stat_path(dirfd: i64, path_addr: u64, out: u64, flags: u32) -> SysResult {
    // fstatat with an empty path and AT_EMPTY_PATH means "stat the fd".
    let path = uaccess::read_cstr(path_addr, 4096)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return fstat(dirfd as i32, out);
    }
    let mut resolved = resolve_str(dirfd, &path)?;
    if let Some(target) = procfs_override(&resolved) {
        resolved = target;
    }
    let node = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        fs::lookup_nofollow(&resolved)?
    } else {
        fs::lookup(&resolved)?
    };
    uaccess::write_struct(out, &node.stat())?;
    Ok(0)
}

const STATX_SIZE: usize = 256;

pub fn statx(dirfd: i64, path_addr: u64, flags: u32, _mask: u32, out: u64) -> SysResult {
    let path = uaccess::read_cstr(path_addr, 4096)?;
    let stat = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        sched::current().fds.get(dirfd as i32)?.stat()
    } else {
        let mut resolved = resolve_str(dirfd, &path)?;
        if let Some(target) = procfs_override(&resolved) {
            resolved = target;
        }
        let node = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs::lookup_nofollow(&resolved)?
        } else {
            fs::lookup(&resolved)?
        };
        node.stat()
    };

    let mut buf = [0u8; STATX_SIZE];
    let put32 = |buf: &mut [u8], off: usize, v: u32| {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let put64 = |buf: &mut [u8], off: usize, v: u64| {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    };

    const STATX_BASIC_STATS: u32 = 0x7FF;
    put32(&mut buf, 0, STATX_BASIC_STATS);
    put32(&mut buf, 4, stat.st_blksize as u32);
    put32(&mut buf, 16, stat.st_nlink as u32);
    put32(&mut buf, 20, stat.st_uid);
    put32(&mut buf, 24, stat.st_gid);
    buf[28..30].copy_from_slice(&(stat.st_mode as u16).to_le_bytes());
    put64(&mut buf, 32, stat.st_ino);
    put64(&mut buf, 40, stat.st_size as u64);
    put64(&mut buf, 48, stat.st_blocks as u64);
    // atime, btime, ctime, mtime timestamps at 64, 80, 96, 112.
    for off in [64usize, 80, 96, 112] {
        put64(&mut buf, off, stat.st_mtime as u64);
    }
    put32(&mut buf, 128, (stat.st_rdev >> 8) as u32);
    put32(&mut buf, 132, (stat.st_rdev & 0xFF) as u32);
    put32(&mut buf, 136, 0);
    put32(&mut buf, 140, 1);

    uaccess::write_bytes(out, &buf)?;
    Ok(0)
}

pub fn access(dirfd: i64, path_addr: u64) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    fs::lookup(&path)?;
    Ok(0)
}

pub fn getdents64(fd: i32, out: u64, len: usize) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ENOTDIR)?.clone();
    if !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }

    let entries = fs::readdir(&node);
    let mut offset = file.offset.lock();
    let mut index = *offset as usize;
    let mut buf: Vec<u8> = Vec::new();

    while index < entries.len() {
        let entry = &entries[index];
        let name = entry.name.as_bytes();
        // d_ino(8) d_off(8) d_reclen(2) d_type(1) name + NUL, padded to 8.
        let reclen = (19 + name.len() + 1 + 7) & !7;
        if buf.len() + reclen > len {
            break;
        }
        let start = buf.len();
        buf.resize(start + reclen, 0);
        buf[start..start + 8].copy_from_slice(&entry.ino.to_le_bytes());
        buf[start + 8..start + 16].copy_from_slice(&((index + 1) as u64).to_le_bytes());
        buf[start + 16..start + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        buf[start + 18] = entry.kind;
        buf[start + 19..start + 19 + name.len()].copy_from_slice(name);
        index += 1;
    }

    if buf.is_empty() && index < entries.len() {
        return Err(Errno::EINVAL); // buffer too small for even one entry
    }
    *offset = index as u64;
    drop(offset);
    uaccess::write_bytes(out, &buf)?;
    Ok(buf.len() as u64)
}

pub fn getcwd(out: u64, len: usize) -> SysResult {
    let cwd = sched::current().cwd.clone();
    let bytes = cwd.as_bytes();
    if bytes.len() + 1 > len {
        return Err(Errno::ERANGE);
    }
    let mut buf = bytes.to_vec();
    buf.push(0);
    uaccess::write_bytes(out, &buf)?;
    Ok(buf.len() as u64)
}

pub fn chdir(path_addr: u64) -> SysResult {
    let path = resolve_at(AT_FDCWD, path_addr)?;
    let node = fs::lookup(&path)?;
    if !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    sched::current().cwd = path;
    Ok(0)
}

pub fn fchdir(fd: i32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ENOTDIR)?;
    if !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    sched::current().cwd = file.path.clone();
    Ok(0)
}

pub fn mkdirat(dirfd: i64, path_addr: u64, mode: u32) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    fs::mkdir(&path, mode)?;
    Ok(0)
}

pub fn unlinkat(dirfd: i64, path_addr: u64, flags: u32) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    fs::unlink(&path, flags & AT_REMOVEDIR != 0)?;
    Ok(0)
}

pub fn rename(old_dirfd: i64, old_addr: u64, new_dirfd: i64, new_addr: u64) -> SysResult {
    let from = resolve_at(old_dirfd, old_addr)?;
    let to = resolve_at(new_dirfd, new_addr)?;
    fs::rename(&from, &to)?;
    Ok(0)
}

pub fn symlinkat(target_addr: u64, dirfd: i64, path_addr: u64) -> SysResult {
    let target = uaccess::read_cstr(target_addr, 4096)?;
    let path = resolve_at(dirfd, path_addr)?;
    fs::symlink(&path, &target)?;
    Ok(0)
}

pub fn readlinkat(dirfd: i64, path_addr: u64, out: u64, len: usize) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    let target = match procfs_override(&path) {
        Some(target) => target,
        None => {
            let node = fs::lookup_nofollow(&path)?;
            node.symlink_target().ok_or(Errno::EINVAL)?
        }
    };
    let bytes = target.as_bytes();
    let n = bytes.len().min(len);
    uaccess::write_bytes(out, &bytes[..n])?;
    Ok(n as u64)
}

/// Change a file's permission bits, keeping its type bits.
pub fn chmod(dirfd: i64, path_addr: u64, mode: u32) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    let node = fs::lookup(&path)?;
    let mut inner = node.inner.lock();
    inner.mode = (inner.mode & S_IFMT) | (mode & 0o7777);
    Ok(0)
}

pub fn fchmod(fd: i32, mode: u32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::EBADF)?;
    let mut inner = node.inner.lock();
    inner.mode = (inner.mode & S_IFMT) | (mode & 0o7777);
    Ok(0)
}

pub fn truncate(path_addr: u64, len: u64) -> SysResult {
    let path = resolve_at(AT_FDCWD, path_addr)?;
    fs::lookup(&path)?.truncate(len)?;
    Ok(0)
}

pub fn ftruncate(fd: i32, len: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    file.node().ok_or(Errno::EINVAL)?.truncate(len)?;
    Ok(0)
}

pub fn dup(fd: i32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let new = sched::current().fds.alloc(file, false)?;
    Ok(new as u64)
}

pub fn dup2(old: i32, new: i32, flags: u32) -> SysResult {
    let file = sched::current().fds.get(old)?;
    if old == new {
        return Ok(new as u64);
    }
    if new < 0 || new as usize >= fs::MAX_FDS {
        return Err(Errno::EBADF);
    }
    let table = &mut sched::current().fds;
    let _ = table.close(new);
    table.insert_at(new as usize, file, flags & O_CLOEXEC != 0);
    Ok(new as u64)
}

pub fn pipe2(out: u64, flags: u32) -> SysResult {
    let (read_end, write_end) = pipe::create_pair(flags & !O_CLOEXEC);
    let cloexec = flags & O_CLOEXEC != 0;
    let table = &mut sched::current().fds;
    let read_fd = table.alloc(read_end, cloexec)?;
    let write_fd = match table.alloc(write_end, cloexec) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = table.close(read_fd);
            return Err(err);
        }
    };
    uaccess::write_u32(out, read_fd as u32)?;
    uaccess::write_u32(out + 4, write_fd as u32)?;
    Ok(0)
}

pub fn fcntl(fd: i32, cmd: u32, arg: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let table = &mut sched::current().fds;
            let new = table.alloc_at_least(arg as usize, file, cmd == F_DUPFD_CLOEXEC)?;
            Ok(new as u64)
        }
        F_GETFD => {
            let table = &sched::current().fds;
            let set = table.cloexec.get(fd as usize).copied().unwrap_or(false);
            Ok(if set { FD_CLOEXEC as u64 } else { 0 })
        }
        F_SETFD => {
            let table = &mut sched::current().fds;
            if (fd as usize) < table.cloexec.len() {
                table.cloexec[fd as usize] = arg as u32 & FD_CLOEXEC != 0;
            }
            Ok(0)
        }
        F_GETFL => Ok(file.flags() as u64),
        F_SETFL => {
            // Only the status flags may be changed.
            let keep = file.flags() & (O_ACCMODE | O_CREAT | O_EXCL | O_TRUNC);
            *file.flags.lock() = keep | (arg as u32 & (O_APPEND | O_NONBLOCK));
            Ok(0)
        }
        _ => Ok(0),
    }
}

pub fn ioctl(fd: i32, request: u64, arg: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;

    // These apply to any descriptor, not just terminals.
    match request {
        FIONBIO => {
            let enable = uaccess::read_u32(arg)? != 0;
            let mut flags = file.flags.lock();
            if enable {
                *flags |= O_NONBLOCK;
            } else {
                *flags &= !O_NONBLOCK;
            }
            return Ok(0);
        }
        FIONREAD => {
            let available = match &file.backing {
                FileBacking::Pipe(pipe, _) => pipe.available() as u64,
                FileBacking::Node(node) => match node.kind {
                    NodeKind::Device(kind) if kind.is_tty() => {
                        crate::console::available() as u64
                    }
                    _ => {
                        let offset = *file.offset.lock();
                        node.size().saturating_sub(offset)
                    }
                },
            };
            uaccess::write_u32(arg, available as u32)?;
            return Ok(0);
        }
        FIOCLEX | FIONCLEX => {
            let table = &mut sched::current().fds;
            if (fd as usize) < table.cloexec.len() {
                table.cloexec[fd as usize] = request == FIOCLEX;
            }
            return Ok(0);
        }
        _ => {}
    }

    let is_tty = match file.node() {
        Some(node) => matches!(node.kind, NodeKind::Device(kind) if kind.is_tty()),
        None => false,
    };
    if !is_tty {
        return Err(Errno::ENOTTY);
    }
    match request {
        TCGETS => {
            let termios = *crate::console::TERMIOS.lock();
            uaccess::write_struct(arg, &termios)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let termios: Termios = uaccess::read_struct(arg)?;
            *crate::console::TERMIOS.lock() = termios;
            Ok(0)
        }
        TIOCGWINSZ => {
            let size = WinSize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
            uaccess::write_struct(arg, &size)?;
            Ok(0)
        }
        TIOCSWINSZ => Ok(0),
        TIOCGPGRP => {
            uaccess::write_u32(arg, sched::foreground())?;
            Ok(0)
        }
        TIOCSPGRP => {
            let pgid = uaccess::read_u32(arg)?;
            sched::set_foreground(pgid);
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// How many bytes a descriptor can supply without blocking.
fn ready_to_read(file: &Arc<OpenFile>) -> bool {
    match &file.backing {
        FileBacking::Pipe(pipe, is_write) => {
            if *is_write {
                true
            } else {
                pipe.available() > 0 || pipe.writers.load(core::sync::atomic::Ordering::Acquire) == 0
            }
        }
        FileBacking::Node(node) => match node.kind {
            NodeKind::Device(kind) if kind.is_tty() => crate::console::available() > 0,
            _ => true,
        },
    }
}

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;

pub fn poll(fds_addr: u64, count: usize, timeout_ms: i64) -> SysResult {
    if count > 1024 {
        return Err(Errno::EINVAL);
    }
    let deadline = if timeout_ms < 0 {
        u64::MAX
    } else {
        crate::trap::ticks() + crate::time::ns_to_ticks(timeout_ms as u64 * 1_000_000)
    };

    loop {
        let mut ready = 0u64;
        for i in 0..count {
            let base = fds_addr + (i * 8) as u64;
            let fd = uaccess::read_u32(base)? as i32;
            let events = uaccess::read_u32(base + 4)? as i16;
            let mut revents = 0i16;
            if fd >= 0 {
                match sched::current().fds.get(fd) {
                    Ok(file) => {
                        if events & POLLIN != 0 && file.readable() && ready_to_read(&file) {
                            revents |= POLLIN;
                        }
                        if events & POLLOUT != 0 && file.writable() {
                            revents |= POLLOUT;
                        }
                    }
                    Err(_) => revents |= 0x020, // POLLNVAL
                }
            }
            if revents != 0 {
                ready += 1;
            }
            // revents is the high half of the second word.
            let packed = ((revents as u16 as u32) << 16) | (events as u16 as u32);
            uaccess::write_u32(base + 4, packed)?;
        }
        if ready > 0 {
            return Ok(ready);
        }
        if crate::trap::ticks() >= deadline {
            return Ok(0);
        }
        if sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        sched::yield_or_sleep();
    }
}

pub fn select(nfds: i32, readfds: u64, writefds: u64, _exceptfds: u64) -> SysResult {
    if nfds < 0 || nfds > 1024 {
        return Err(Errno::EINVAL);
    }
    let words = ((nfds as usize) + 63) / 64;
    loop {
        let mut ready = 0u64;
        let mut read_result = vec![0u64; words.max(1)];
        let mut write_result = vec![0u64; words.max(1)];

        for fd in 0..nfds {
            let word = (fd / 64) as usize;
            let bit = 1u64 << (fd % 64);
            if readfds != 0 {
                let set = uaccess::read_u64(readfds + (word * 8) as u64)?;
                if set & bit != 0 {
                    if let Ok(file) = sched::current().fds.get(fd) {
                        if ready_to_read(&file) {
                            read_result[word] |= bit;
                            ready += 1;
                        }
                    }
                }
            }
            if writefds != 0 {
                let set = uaccess::read_u64(writefds + (word * 8) as u64)?;
                if set & bit != 0 {
                    write_result[word] |= bit;
                    ready += 1;
                }
            }
        }

        if ready > 0 {
            for w in 0..words {
                if readfds != 0 {
                    uaccess::write_u64(readfds + (w * 8) as u64, read_result[w])?;
                }
                if writefds != 0 {
                    uaccess::write_u64(writefds + (w * 8) as u64, write_result[w])?;
                }
            }
            return Ok(ready);
        }
        if sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        sched::yield_or_sleep();
    }
}

/// `struct statfs` as x86_64 Linux defines it (120 bytes).
fn fill_statfs(out: u64) -> SysResult {
    // The root filesystem lives in RAM, so its size is what memory allows and
    // its free space is what a file could still grow into.
    let (_, total) = crate::mm::frame::stats();
    let total_blocks = total as u64;
    let free_blocks = (crate::mm::file_available_bytes() / crate::mm::PAGE_SIZE) as u64;
    let mut buf = [0u8; 120];
    let put = |buf: &mut [u8], offset: usize, value: u64| {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    put(&mut buf, 0, TMPFS_MAGIC); // f_type
    put(&mut buf, 8, 4096); // f_bsize
    put(&mut buf, 16, total_blocks); // f_blocks
    put(&mut buf, 24, free_blocks); // f_bfree
    put(&mut buf, 32, free_blocks); // f_bavail
    put(&mut buf, 40, 1 << 20); // f_files
    put(&mut buf, 48, 1 << 19); // f_ffree
    put(&mut buf, 64, 255); // f_namelen
    put(&mut buf, 72, 4096); // f_frsize
    uaccess::write_bytes(out, &buf)?;
    Ok(0)
}

pub fn statfs(path_addr: u64, out: u64) -> SysResult {
    let path = resolve_at(AT_FDCWD, path_addr)?;
    fs::lookup(&path)?;
    fill_statfs(out)
}

pub fn fstatfs(fd: i32, out: u64) -> SysResult {
    sched::current().fds.get(fd)?;
    fill_statfs(out)
}

pub fn memfd_create(name_addr: u64, _flags: u32) -> SysResult {
    let name = uaccess::read_cstr(name_addr, 256)?;
    let node = Node::new_file(0o600);
    let file = OpenFile::from_node_at(node, O_RDWR, &alloc::format!("memfd:{}", name));
    let fd = sched::current().fds.alloc(file, false)?;
    Ok(fd as u64)
}

pub fn sendfile(out_fd: i32, in_fd: i32, offset_addr: u64, count: usize) -> SysResult {
    let input = sched::current().fds.get(in_fd)?;
    let output = sched::current().fds.get(out_fd)?;
    let count = count.min(MAX_IO);
    let mut buf = vec![0u8; count];

    let n = if offset_addr != 0 {
        let offset = uaccess::read_u64(offset_addr)?;
        let node = input.node().ok_or(Errno::EINVAL)?;
        let n = node.read_at(offset, &mut buf)?;
        uaccess::write_u64(offset_addr, offset + n as u64)?;
        n
    } else {
        input.read(&mut buf)?
    };
    let written = output.write(&buf[..n])?;
    Ok(written as u64)
}

/// Read a whole file into kernel memory.
pub fn read_file(path: &str) -> Result<Vec<u8>, Errno> {
    let node = fs::lookup(path)?;
    if node.is_dir() {
        return Err(Errno::EISDIR);
    }
    let data = node.inner.lock().data.clone();
    Ok(data)
}

pub fn dump_tree(path: &str, depth: usize) {
    let Ok(node) = fs::lookup(path) else { return };
    if !node.is_dir() {
        return;
    }
    // Snapshot the directory so the lock is not held while recursing.
    let children: Vec<(String, bool, u64)> = node
        .inner
        .lock()
        .children
        .iter()
        .map(|(name, child)| (name.clone(), child.is_dir(), child.size()))
        .collect();

    for (name, is_dir, size) in children {
        for _ in 0..depth {
            crate::print!("  ");
        }
        crate::println!("{}{}  {} bytes", name, if is_dir { "/" } else { "" }, size);
        if is_dir && depth < 4 {
            let sub = if path == "/" {
                alloc::format!("/{}", name)
            } else {
                alloc::format!("{}/{}", path, name)
            };
            dump_tree(&sub, depth + 1);
        }
    }
}

pub fn to_string(value: &str) -> String {
    value.to_string()
}
