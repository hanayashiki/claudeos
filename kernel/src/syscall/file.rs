//! File and descriptor system calls.

use super::{deadline_in, deadline_in_ms};
use crate::abi::*;
use crate::fs::{self, pipe, FileBacking, Node, NodeKind, Offset, OpenFile};
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
        let cwd = sched::current().cwd();
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
/// Rewrite a `/proc/self` path, and resolve the magic links under it.
///
/// `for_link` is set by readlink, which wants the label a descriptor carries
/// even when it is not a name. Everything else wants a path it can open or
/// stat, so a descriptor on a pipe or a socket is left to the entry in the
/// directory, which is a node of the right kind.
fn procfs_override_for(path: &str, for_link: bool) -> Option<String> {
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
            "exe" => return Some(task.exe_path()),
            "cwd" => return Some(task.cwd()),
            _ => {}
        }
        if let Some(number) = entry.strip_prefix("fd/") {
            if let Ok(fd) = number.parse::<i32>() {
                if let Ok(file) = task.fds.get(fd) {
                    if for_link {
                        return Some(file.path.clone());
                    }
                }
            }
        }
    } else if let Some(rest) = effective.strip_prefix("/proc/") {
        // Another process's exe link.
        if let Some((number, "exe")) = rest.split_once('/') {
            if let Ok(other) = number.parse::<u32>() {
                if let Some(path) = sched::with_task(other, |target| target.exe_path()) {
                    return Some(path);
                }
            }
        }
    }

    rewritten
}

fn procfs_override(path: &str) -> Option<String> {
    procfs_override_for(path, false)
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
    let n = node.read_at(Offset::new(offset), &mut buf)?;
    uaccess::write_bytes(buf_addr, &buf[..n])?;
    Ok(n as u64)
}

pub fn pwrite(fd: i32, buf_addr: u64, len: u64, offset: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ESPIPE)?;
    let len = (len as usize).min(MAX_IO);
    let mut buf = vec![0u8; len];
    uaccess::read_bytes(buf_addr, &mut buf)?;
    let n = node.write_at(Offset::new(offset), &buf)?;
    Ok(n as u64)
}

/// The descriptor `path` names, when it is one of the calling process's own
/// entries under /proc/<pid>/fd.
///
/// The table is what such an entry stands for, so it is asked directly rather
/// than through the directory that lists it: that directory is rebuilt on
/// demand and is not there at all for the first moments of a task's life, so
/// a child that redirects to one of its own descriptors before the fork that
/// made it has finished would otherwise be told the name does not exist.
fn own_descriptor(path: &str) -> Option<i32> {
    let rest = path.strip_prefix("/proc/")?;
    let (owner, rest) = rest.split_once('/')?;
    if owner.parse::<u32>().ok()? != sched::current().pid {
        return None;
    }
    rest.strip_prefix("fd/")?.parse::<i32>().ok()
}

/// Another handle on the open file a descriptor already holds.
fn open_descriptor(fd: i32, flags: u32) -> SysResult {
    // A descriptor that is not open has no entry, so the answer is the one a
    // missing name gets rather than the one a bad descriptor gets.
    let file = sched::current().fds.get(fd).map_err(|_| Errno::ENOENT)?;
    let new = sched::current().fds.alloc(file, flags & O_CLOEXEC != 0)?;
    Ok(new as u64)
}

/// Where a trailing symbolic link points, when `path` ends in one.
fn final_link_target(path: &str) -> Option<String> {
    let node = fs::lookup_nofollow(path).ok()?;
    if node.kind != fs::NodeKind::Symlink {
        return None;
    }
    let target = node.symlink_target()?;
    let (dir, _) = path.rsplit_once('/')?;
    Some(fs::normalize(if dir.is_empty() { "/" } else { dir }, &target))
}

pub fn openat(dirfd: i64, path_addr: u64, flags: u32, mode: u32) -> SysResult {
    let mut path = resolve_at(dirfd, path_addr)?;
    let mut found = None;

    // A trailing symbolic link whose target is not there is followed here
    // rather than left to the lookup, because what the name stands for is
    // what it points at: O_CREAT has to make the target, and a link to a
    // descriptor still has to reach that descriptor. Opening the link itself
    // would replace the path it holds with whatever was written to it.
    for _ in 0..fs::SYMLINK_DEPTH {
        if let Some(target) = procfs_override(&path) {
            path = target;
        }
        if let Some(fd) = own_descriptor(&path) {
            return open_descriptor(fd, flags);
        }
        match fs::lookup(&path) {
            Ok(node) => {
                found = Some(node);
                break;
            }
            Err(Errno::ENOENT) => match final_link_target(&path) {
                Some(target) => path = target,
                None => break,
            },
            Err(err) => return Err(err),
        }
    }

    let node = match found {
        Some(node) => {
            if flags & O_EXCL != 0 && flags & O_CREAT != 0 {
                return Err(Errno::EEXIST);
            }
            node
        }
        None if flags & O_CREAT != 0 => {
            let umask = sched::current().umask.get();
            fs::create(&path, mode & !umask)?
        }
        None => return Err(Errno::ENOENT),
    };

    // An entry in /proc/<pid>/fd is that descriptor. Opening it hands back
    // another handle on the same open file, which is what makes a redirection
    // to /dev/stdout reach the terminal or the pipe stdout is attached to.
    if let fs::NodeKind::Fd(owner, fd) = node.kind {
        if owner == sched::current().pid {
            return open_descriptor(fd, flags);
        }
        return Err(Errno::EACCES);
    }

    if flags & O_DIRECTORY != 0 && !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    if node.is_dir() && (flags & O_ACCMODE) != O_RDONLY {
        return Err(Errno::EISDIR);
    }
    if flags & O_TRUNC != 0 && !node.is_dir() {
        node.truncate(Offset::START)?;
    }

    if node.kind == fs::NodeKind::Fifo {
        let file = fs::pipe::open_fifo(node.ino, flags, &path)?;
        let cloexec = flags & O_CLOEXEC != 0;
        let fd = sched::current().fds.alloc(file, cloexec)?;
        return Ok(fd as u64);
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
    let table = sched::current().fds.share();
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
    uaccess::write_struct(out, &StatAbi::from(&stat))?;
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
    uaccess::write_struct(out, &StatAbi::from(&node.stat()))?;
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
    // The device the file is on, which is how a program such as `mv` or
    // `find -xdev` tells /data from the ram filesystem.
    put32(&mut buf, 136, (stat.st_dev >> 8) as u32);
    put32(&mut buf, 140, (stat.st_dev & 0xFF) as u32);

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

    // The listing is taken when a reading starts, at offset 0, and later calls
    // continue it, so entries removed or added in between do not move the
    // offsets. Otherwise a program that removes what it has read, as `rm -r`
    // does, skips an entry for every one it removed. It is taken without the
    // offset lock held: on /data a listing reads the card.
    let start = *file.offset.lock();
    let kept = if start == 0 { None } else { file.listing.lock().clone() };
    let entries = match kept {
        Some(entries) => entries,
        None => {
            let fresh = Arc::new(fs::readdir(&node)?);
            *file.listing.lock() = Some(fresh.clone());
            fresh
        }
    };
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
    // The end of the listing: nothing more is read from it, so it is not kept.
    if buf.is_empty() {
        *file.listing.lock() = None;
    }
    uaccess::write_bytes(out, &buf)?;
    Ok(buf.len() as u64)
}

pub fn getcwd(out: u64, len: usize) -> SysResult {
    let cwd = sched::current().cwd();
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
    sched::current().set_cwd(path);
    Ok(0)
}

pub fn fchdir(fd: i32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let node = file.node().ok_or(Errno::ENOTDIR)?;
    if !node.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    sched::current().set_cwd(file.path.clone());
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

/// A second name for an existing file. Nodes are reference counted and the
/// directory entry is the reference, so this is the same node under two names.
pub fn linkat(
    old_dirfd: i64,
    old_addr: u64,
    new_dirfd: i64,
    new_addr: u64,
    flags: u32,
) -> SysResult {
    let old = resolve_at(old_dirfd, old_addr)?;
    let new = resolve_at(new_dirfd, new_addr)?;
    let node = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        fs::lookup_nofollow(&old)?
    } else {
        fs::lookup(&old)?
    };
    if node.is_dir() {
        return Err(Errno::EPERM);
    }
    if fs::lookup_nofollow(&new).is_ok() {
        return Err(Errno::EEXIST);
    }
    fs::hard_link(&new, node)?;
    Ok(0)
}

pub fn mknodat(dirfd: i64, path_addr: u64, mode: u32, _dev: u64) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    match mode & S_IFMT {
        S_IFIFO => {
            fs::mkfifo(&path, mode)?;
            Ok(0)
        }
        0 | S_IFREG => {
            if fs::lookup_nofollow(&path).is_ok() {
                return Err(Errno::EEXIST);
            }
            fs::create(&path, mode)?;
            Ok(0)
        }
        // Character and block devices are the kernel's to hand out, not a
        // program's to invent.
        _ => Err(Errno::EPERM),
    }
}

pub fn readlinkat(dirfd: i64, path_addr: u64, out: u64, len: usize) -> SysResult {
    let path = resolve_at(dirfd, path_addr)?;
    let target = match procfs_override_for(&path, true) {
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
    set_mode(&fs::lookup(&path)?, mode)
}

pub fn fchmod(fd: i32, mode: u32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    set_mode(file.node().ok_or(Errno::EBADF)?, mode)
}

/// Give `node` the permission bits `mode`.
///
/// FAT keeps no permission bits, and every file and directory on /data
/// reports 0777. Asking for that mode succeeds and asking for any other is
/// EPERM, so a chmod that returns 0 has left the file with the mode it asked
/// for. Linux's vfat, mounted without `quiet`, answers EPERM in `fat_setattr`
/// for bits it has no place for, setuid, setgid and sticky, and a mode that
/// clears every write bit sets the FAT read-only attribute; a change to the
/// other bits it cannot store it lets through as success without effect
/// ("We don't return -EPERM here. Yes, strange, but this is too old
/// behavior."). Here those are EPERM as well, and the read-only attribute is
/// not used.
fn set_mode(node: &fs::NodeRef, mode: u32) -> SysResult {
    if node.is_stored() {
        return if mode & 0o7777 == node.mode() & 0o7777 { Ok(0) } else { Err(Errno::EPERM) };
    }
    let mut inner = node.inner.lock();
    inner.mode = (inner.mode & S_IFMT) | (mode & 0o7777);
    Ok(0)
}

pub fn truncate(path_addr: u64, len: u64) -> SysResult {
    let path = resolve_at(AT_FDCWD, path_addr)?;
    fs::lookup(&path)?.truncate(Offset::new(len))?;
    Ok(0)
}

pub fn ftruncate(fd: i32, len: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    file.node().ok_or(Errno::EINVAL)?.truncate(Offset::new(len))?;
    Ok(0)
}

/// `fsync` and `fdatasync`. For a file on the data volume, its directory entry
/// goes to the card, and the call fails if that write does; its contents are
/// there already, because every write reaches the card before it returns.
/// Everything else lives in memory and has nowhere further to go.
pub fn fsync(fd: i32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    if let Some(node) = file.node() {
        if node.is_stored() {
            fs::data::fsync(node)?;
        }
    }
    Ok(0)
}

/// `sync`: the data volume unmounted and mounted again, which writes its
/// count of free clusters and marks it clean, so another system finds it
/// unmounted properly. The call returns nothing on Linux, so a failure is only
/// reported in the log.
pub fn sync() -> SysResult {
    if let Err(error) = fs::data::sync() {
        println!("data: sync failed: {:?}", error);
    }
    Ok(0)
}

/// `syncfs`: the same, for the filesystem `fd` is on, with the error returned.
pub fn syncfs(fd: i32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    if file.node().is_some_and(|node| node.is_stored()) {
        fs::data::sync()?;
    }
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
    let table = sched::current().fds.share();
    let _ = table.close(new);
    table.insert_at(new as usize, file, flags & O_CLOEXEC != 0);
    Ok(new as u64)
}

pub fn pipe2(out: u64, flags: u32) -> SysResult {
    let (read_end, write_end) = pipe::create_pair(flags & !O_CLOEXEC);
    let cloexec = flags & O_CLOEXEC != 0;
    let table = sched::current().fds.share();
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
            let new = sched::current()
                .fds
                .alloc_at_least(arg as usize, file, cmd == F_DUPFD_CLOEXEC)?;
            Ok(new as u64)
        }
        F_GETFD => {
            let set = sched::current().fds.is_cloexec(fd);
            Ok(if set { FD_CLOEXEC as u64 } else { 0 })
        }
        F_SETFD => {
            sched::current().fds.set_cloexec(fd, arg as u32 & FD_CLOEXEC != 0);
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
                FileBacking::Socket(socket) => socket.rx.available() as u64,
                FileBacking::Inet(handle) => handle.socket.available() as u64,
                FileBacking::EventFd(_) | FileBacking::Epoll(_) => 0,
            };
            uaccess::write_u32(arg, available as u32)?;
            return Ok(0);
        }
        FIOCLEX | FIONCLEX => {
            sched::current().fds.set_cloexec(fd, request == FIOCLEX);
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
/// Descriptors an epoll set or a poll call can wait on.
pub fn ready_to_write(file: &Arc<OpenFile>) -> bool {
    match &file.backing {
        FileBacking::Pipe(pipe, end) => !end.writes() || pipe.writable_now(),
        FileBacking::Socket(socket) => socket.tx.writable_now(),
        FileBacking::Inet(handle) => handle.socket.writable(),
        _ => true,
    }
}

pub fn ready_to_read(file: &Arc<OpenFile>) -> bool {
    match &file.backing {
        FileBacking::EventFd(event) => event.readable(),
        FileBacking::Socket(socket) => socket.readable(),
        FileBacking::Inet(handle) => handle.socket.readable(),
        FileBacking::Epoll(_) => false,
        FileBacking::Pipe(pipe, end) => {
            if !end.reads() {
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

/// What a descriptor would report right now for the events asked about.
fn poll_state(file: &Arc<OpenFile>, events: i16) -> i16 {
    let mut revents = 0i16;
    if events & POLLIN != 0 && file.readable() && ready_to_read(file) {
        revents |= POLLIN;
    }
    if events & POLLOUT != 0 && file.writable() && ready_to_write(file) {
        revents |= POLLOUT;
    }
    revents
}

/// `ppoll` says how long to wait with a `timespec` rather than a count of
/// milliseconds, and a null pointer means no limit. It is the only form
/// aarch64 has, so a program that calls `poll` reaches the kernel through
/// this.
pub fn ppoll(fds_addr: u64, count: usize, timeout_addr: u64) -> SysResult {
    let timeout_ms = if timeout_addr == 0 {
        -1
    } else {
        let spec: Timespec = uaccess::read_struct(timeout_addr)?;
        if spec.tv_sec < 0 || spec.tv_nsec < 0 || spec.tv_nsec >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        spec.tv_sec.saturating_mul(1_000).saturating_add(spec.tv_nsec / 1_000_000)
    };
    poll(fds_addr, count, timeout_ms)
}

pub fn poll(fds_addr: u64, count: usize, timeout_ms: i64) -> SysResult {
    if count > 1024 {
        return Err(Errno::EINVAL);
    }
    let deadline = deadline_in_ms(timeout_ms);

    // Read the request once and hold the descriptors it names. Readiness has
    // to be testable from inside the sleep, where user memory must not be
    // touched, and the set being waited on cannot change while this task is
    // the one waiting.
    let mut requests: Vec<(i16, Option<Arc<OpenFile>>)> = Vec::with_capacity(count);
    for i in 0..count {
        let base = fds_addr + (i * 8) as u64;
        let fd = uaccess::read_u32(base)? as i32;
        let events = uaccess::read_u32(base + 4)? as i16;
        let file = if fd < 0 { None } else { Some(sched::current().fds.get(fd)) };
        match file {
            None => requests.push((events, None)),
            Some(Ok(file)) => requests.push((events, Some(file))),
            // POLLNVAL, reported without waiting for anything.
            Some(Err(_)) => requests.push((-1, None)),
        }
    }

    let current = |request: &(i16, Option<Arc<OpenFile>>)| -> i16 {
        match (&request.1, request.0) {
            (Some(file), events) => poll_state(file, events),
            (None, -1) => 0x020,
            (None, _) => 0,
        }
    };

    loop {
        // A socket whose other end is on this machine has its traffic handed
        // over by whichever task is not holding a lock, and this is one.
        crate::net::poll();
        let mut ready = 0u64;
        for (i, request) in requests.iter().enumerate() {
            let revents = current(request);
            if revents != 0 {
                ready += 1;
            }
            let events = if request.0 < 0 { 0 } else { request.0 };
            // revents is the high half of the second word.
            let packed = ((revents as u16 as u32) << 16) | (events as u16 as u32);
            uaccess::write_u32(fds_addr + (i * 8) as u64 + 4, packed)?;
        }
        if ready > 0 {
            return Ok(ready);
        }
        if sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        let woke = sched::IO_READY.wait_until_or_at(deadline, || {
            requests.iter().any(|request| current(request) != 0) || sched::has_pending_signal()
        });
        if !woke {
            return Ok(0);
        }
    }
}

/// `timeout` points at two 64-bit numbers: whole seconds, then a fraction in
/// units of `fraction_ns` nanoseconds. select counts microseconds there and
/// pselect6 counts nanoseconds, which is the only difference between them.
pub fn select(
    nfds: i32,
    readfds: u64,
    writefds: u64,
    _exceptfds: u64,
    timeout: u64,
    fraction_ns: u64,
) -> SysResult {
    if nfds < 0 || nfds > 1024 {
        return Err(Errno::EINVAL);
    }
    let deadline = if timeout == 0 {
        u64::MAX
    } else {
        let seconds = uaccess::read_u64(timeout)?;
        let fraction = uaccess::read_u64(timeout + 8)?;
        let nanos = seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(fraction.saturating_mul(fraction_ns));
        deadline_in(nanos)
    };
    let words = ((nfds as usize) + 63) / 64;

    // Read both sets once, and hold the descriptors they name, so readiness
    // can be tested from inside the sleep without touching user memory.
    let mut watched: Vec<(i32, i16, Arc<OpenFile>)> = Vec::new();
    for fd in 0..nfds {
        let word = (fd / 64) as usize;
        let bit = 1u64 << (fd % 64);
        let mut events = 0i16;
        if readfds != 0 && uaccess::read_u64(readfds + (word * 8) as u64)? & bit != 0 {
            events |= POLLIN;
        }
        if writefds != 0 && uaccess::read_u64(writefds + (word * 8) as u64)? & bit != 0 {
            events |= POLLOUT;
        }
        if events == 0 {
            continue;
        }
        // A descriptor that is not open is an error for the whole call.
        let file = sched::current().fds.get(fd)?;
        watched.push((fd, events, file));
    }

    loop {
        crate::net::poll();
        let mut ready = 0u64;
        let mut read_result = vec![0u64; words.max(1)];
        let mut write_result = vec![0u64; words.max(1)];
        for (fd, events, file) in watched.iter() {
            let word = (*fd / 64) as usize;
            let bit = 1u64 << (*fd % 64);
            let revents = poll_state(file, *events);
            if revents & POLLIN != 0 {
                read_result[word] |= bit;
                ready += 1;
            }
            if revents & POLLOUT != 0 {
                write_result[word] |= bit;
                ready += 1;
            }
        }

        if ready == 0 {
            if sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            let woke = sched::IO_READY.wait_until_or_at(deadline, || {
                watched
                    .iter()
                    .any(|(_, events, file)| poll_state(file, *events) != 0)
                    || sched::has_pending_signal()
            });
            if woke {
                continue;
            }
        }

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
}

/// `struct statfs`: 120 bytes of 64-bit words, the asm-generic layout that
/// x86-64 and aarch64 both take unchanged.
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
    let node = fs::lookup(&path)?;
    if node.is_stored() {
        return fill_statfs_data(&node, out);
    }
    fill_statfs(out)
}

pub fn fstatfs(fd: i32, out: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    if let Some(node) = file.node() {
        if node.is_stored() {
            return fill_statfs_data(node, out);
        }
    }
    fill_statfs(out)
}

/// `struct statfs` for the data volume: vfat's magic number, and its sizes in
/// clusters.
fn fill_statfs_data(node: &Node, out: u64) -> SysResult {
    const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
    let (cluster, total, free) = fs::data::statfs(node)?;
    let mut buf = [0u8; 120];
    let put = |buf: &mut [u8], offset: usize, value: u64| {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };
    put(&mut buf, 0, MSDOS_SUPER_MAGIC); // f_type
    put(&mut buf, 8, cluster); // f_bsize
    put(&mut buf, 16, total); // f_blocks
    put(&mut buf, 24, free); // f_bfree
    put(&mut buf, 32, free); // f_bavail
    put(&mut buf, 64, 255); // f_namelen
    put(&mut buf, 72, cluster); // f_frsize
    uaccess::write_bytes(out, &buf)?;
    Ok(0)
}

/// A counter two tasks can wait on. EFD_SEMAPHORE is bit 0 of the flags.
pub fn eventfd(initial: u32, flags: u32) -> SysResult {
    const EFD_SEMAPHORE: u32 = 1;
    const EFD_NONBLOCK: u32 = 0o4000;
    const EFD_CLOEXEC: u32 = 0o2000;
    let event = fs::chan::EventFd::new(initial, flags & EFD_SEMAPHORE != 0);
    let file = Arc::new(OpenFile {
        backing: FileBacking::EventFd(event),
        offset: crate::sync::Spinlock::new(0),
        flags: crate::sync::Spinlock::new(
            O_RDWR | if flags & EFD_NONBLOCK != 0 { O_NONBLOCK } else { 0 },
        ),
        path: alloc::string::String::from("anon_inode:[eventfd]"),
        listing: crate::sync::Spinlock::new(None),
    });
    let fd = sched::current().fds.alloc(file, flags & EFD_CLOEXEC != 0)?;
    Ok(fd as u64)
}

/// Two connected endpoints. Only AF_UNIX byte streams exist here; there is no
/// network, so nothing else has anywhere to go.
pub fn socketpair(domain: u32, kind: u32, _protocol: u32, out: u64) -> SysResult {
    const AF_UNIX: u32 = 1;
    const SOCK_STREAM: u32 = 1;
    const SOCK_SEQPACKET: u32 = 5;
    const SOCK_CLOEXEC: u32 = 0o2000000;
    const SOCK_NONBLOCK: u32 = 0o4000;
    if domain != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    let base = kind & 0xF;
    if base != SOCK_STREAM && base != SOCK_SEQPACKET {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let flags = O_RDWR | if kind & SOCK_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let cloexec = kind & SOCK_CLOEXEC != 0;
    let (one, two) = fs::chan::Socket::pair();
    let make = |socket| {
        Arc::new(OpenFile {
            backing: FileBacking::Socket(socket),
            offset: crate::sync::Spinlock::new(0),
            flags: crate::sync::Spinlock::new(flags),
            path: alloc::string::String::from("socket:[unix]"),
            listing: crate::sync::Spinlock::new(None),
        })
    };
    let first = sched::current().fds.alloc(make(one), cloexec)?;
    let second = match sched::current().fds.alloc(make(two), cloexec) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = sched::current().fds.close(first);
            return Err(err);
        }
    };
    uaccess::write_u32(out, first as u32)?;
    uaccess::write_u32(out + 4, second as u32)?;
    Ok(0)
}

pub fn epoll_create(flags: u32) -> SysResult {
    const EPOLL_CLOEXEC: u32 = 0o2000000;
    let file = Arc::new(OpenFile {
        backing: FileBacking::Epoll(fs::chan::Epoll::new()),
        offset: crate::sync::Spinlock::new(0),
        flags: crate::sync::Spinlock::new(0),
        path: alloc::string::String::from("anon_inode:[eventpoll]"),
        listing: crate::sync::Spinlock::new(None),
    });
    let fd = sched::current().fds.alloc(file, flags & EPOLL_CLOEXEC != 0)?;
    Ok(fd as u64)
}

pub fn epoll_ctl(epfd: i32, op: u32, fd: i32, event_addr: u64) -> SysResult {
    const EPOLL_CTL_ADD: u32 = 1;
    const EPOLL_CTL_DEL: u32 = 2;
    const EPOLL_CTL_MOD: u32 = 3;

    let file = sched::current().fds.get(epfd)?;
    let set = match &file.backing {
        FileBacking::Epoll(set) => set.clone(),
        _ => return Err(Errno::EINVAL),
    };
    if epfd == fd {
        return Err(Errno::EINVAL);
    }
    // The descriptor has to exist, whatever is being done with it.
    sched::current().fds.get(fd)?;

    if op == EPOLL_CTL_DEL {
        return set.remove(fd).map(|_| 0);
    }
    let events = uaccess::read_u32(event_addr)?;
    let data = uaccess::read_u64(event_addr + EPOLL_EVENT_DATA)?;
    let watch = fs::chan::Watch { fd, events, data };
    match op {
        EPOLL_CTL_ADD => set.add(watch).map(|_| 0),
        EPOLL_CTL_MOD => set.modify(watch).map(|_| 0),
        _ => Err(Errno::EINVAL),
    }
}

pub fn epoll_wait(epfd: i32, events_addr: u64, max: i32, timeout_ms: i64) -> SysResult {
    const EPOLLIN: u32 = 0x001;
    const EPOLLOUT: u32 = 0x004;
    const EPOLLERR: u32 = 0x008;
    const EPOLLHUP: u32 = 0x010;

    if max <= 0 {
        return Err(Errno::EINVAL);
    }
    let file = sched::current().fds.get(epfd)?;
    let set = match &file.backing {
        FileBacking::Epoll(set) => set.clone(),
        _ => return Err(Errno::EINVAL),
    };
    let deadline = deadline_in_ms(timeout_ms);

    /// What one watched descriptor would report right now.
    fn state(file: &Result<Arc<OpenFile>, Errno>, events: u32) -> u32 {
        let file = match file {
            Ok(file) => file,
            Err(_) => return EPOLLERR,
        };
        let mut ready = 0u32;
        if events & EPOLLIN != 0 && file.readable() && ready_to_read(file) {
            ready |= EPOLLIN;
        }
        if events & EPOLLOUT != 0 && file.writable() && ready_to_write(file) {
            ready |= EPOLLOUT;
        }
        if let FileBacking::Pipe(pipe, end) = &file.backing {
            // An end that holds both sides keeps either count from reaching
            // zero, so it never hangs up on itself.
            let readers = pipe.readers.load(core::sync::atomic::Ordering::Acquire);
            let writers = pipe.writers.load(core::sync::atomic::Ordering::Acquire);
            let gone = (end.reads() && writers == 0) || (end.writes() && readers == 0);
            if gone {
                ready |= EPOLLHUP;
            }
        }
        if let FileBacking::Inet(handle) = &file.backing {
            if handle.socket.hung_up() {
                ready |= EPOLLHUP;
            }
        }
        ready
    }

    loop {
        crate::net::poll();
        // Hold the descriptors being watched, so readiness can be tested from
        // inside the sleep without going near user memory or the fd table.
        let watches = set.watches.lock().clone();
        let held: Vec<(u64, u32, Result<Arc<OpenFile>, Errno>)> = watches
            .iter()
            .map(|watch| (watch.data, watch.events, sched::current().fds.get(watch.fd)))
            .collect();

        let mut written = 0i32;
        for (data, events, file) in held.iter() {
            if written >= max {
                break;
            }
            let ready = state(file, *events);
            if ready == 0 {
                continue;
            }
            let base = events_addr + written as u64 * EPOLL_EVENT_SIZE;
            uaccess::write_u32(base, ready)?;
            uaccess::write_u64(base + EPOLL_EVENT_DATA, *data)?;
            written += 1;
        }
        if written > 0 {
            return Ok(written as u64);
        }
        if sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        let woke = sched::IO_READY.wait_until_or_at(deadline, || {
            held.iter().any(|(_, events, file)| state(file, *events) != 0)
                || sched::has_pending_signal()
        });
        if !woke {
            return Ok(0);
        }
    }
}

/// `sendto` and `recvfrom` on a connected socket are a write and a read; the
/// address arguments have nowhere to point, since these sockets are pairs
/// rather than names.
pub fn sendto(fd: i32, buf: u64, len: usize, addr: u64) -> SysResult {
    if addr != 0 {
        return Err(Errno::EISCONN);
    }
    write(fd, buf, len as u64)
}

pub fn recvfrom(fd: i32, buf: u64, len: usize, addr: u64, addrlen: u64) -> SysResult {
    let n = read(fd, buf, len as u64)?;
    if addr != 0 {
        super::net::put_name(&[], addr, addrlen)?;
    }
    Ok(n)
}

/// Close one direction of a socket. Shutting down writing is what makes the
/// other end see end of file while this one stays open for reading.
pub fn shutdown(fd: i32, how: u32) -> SysResult {
    const SHUT_RD: u32 = 0;
    const SHUT_WR: u32 = 1;
    const SHUT_RDWR: u32 = 2;
    let file = sched::current().fds.get(fd)?;
    let socket = match &file.backing {
        FileBacking::Socket(socket) => socket.clone(),
        _ => return Err(Errno::ENOTSOCK),
    };
    if how == SHUT_WR || how == SHUT_RDWR {
        socket.shutdown_write();
    }
    if how == SHUT_RD || how == SHUT_RDWR {
        socket.shutdown_read();
    }
    Ok(0)
}

/// A socket pair has no address. Report the family and an empty path, which
/// is what a program that asks gets for an unnamed AF_UNIX socket.
pub fn getsockname(fd: i32, addr: u64, addrlen: u64) -> SysResult {
    const AF_UNIX: u16 = 1;
    let file = sched::current().fds.get(fd)?;
    if !matches!(file.backing, FileBacking::Socket(_)) {
        return Err(Errno::ENOTSOCK);
    }
    super::net::put_name(&AF_UNIX.to_le_bytes(), addr, addrlen)?;
    Ok(0)
}

pub fn getsockopt(fd: i32, value: u64, len: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    if !matches!(file.backing, FileBacking::Socket(_)) {
        return Err(Errno::ENOTSOCK);
    }
    // Nothing here has an error to report or a size to negotiate.
    if value != 0 {
        uaccess::write_u32(value, 0)?;
    }
    if len != 0 {
        uaccess::write_u32(len, 4)?;
    }
    Ok(0)
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
        let offset = Offset::new(uaccess::read_u64(offset_addr)?);
        let node = input.node().ok_or(Errno::EINVAL)?;
        let n = node.read_at(offset, &mut buf)?;
        uaccess::write_u64(offset_addr, offset.advanced(n as u64)?.raw())?;
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
