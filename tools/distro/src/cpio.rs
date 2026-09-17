//! The initramfs: a cpio archive in the "newc" format of a directory tree, as
//! kernel/src/fs/cpio.rs reads it.
//!
//! Every directory, regular file and symbolic link under the directory gets an
//! entry, named by its path from the directory and in the order of those
//! names; anything else is left out. A directory has mode 0755, a link 0777
//! with its target as its data, and a file 0755 when any execute bit is set
//! and 0644 otherwise. Owners are root and inode numbers count from 1.
//!
//! Each entry carries the time its file was last written. A board with no clock
//! of its own takes the latest of those as the earliest it can possibly be, so
//! an archive with no dates leaves such a machine believing it is 1970.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const TRAILER: &str = "TRAILER!!!";
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

struct Entry {
    name: String,
    mode: u32,
    data: Vec<u8>,
    mtime: i64,
}

/// What `pack` wrote.
pub struct Packed {
    pub entries: usize,
    pub data_bytes: usize,
}

/// Write the cpio archive of the tree under `source` to `target`.
pub fn pack(source: &Path, target: &Path) -> Result<Packed, String> {
    let mut entries = Vec::new();
    collect(source, source, &mut entries)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        write_entry(&mut out, &entry.name, entry.mode, &entry.data, index as u32 + 1, entry.mtime);
    }
    write_entry(&mut out, TRAILER, 0, &[], 0, 0);

    let mut file = fs::File::create(target).map_err(|e| format!("{}: {e}", target.display()))?;
    file.write_all(&out).map_err(|e| format!("{}: {e}", target.display()))?;
    Ok(Packed { entries: entries.len(), data_bytes: entries.iter().map(|e| e.data.len()).sum() })
}

fn collect(top: &Path, dir: &Path, entries: &mut Vec<Entry>) -> Result<(), String> {
    let read = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for item in read {
        let item = item.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = item.path();
        let info = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .strip_prefix(top)
            .unwrap()
            .to_str()
            .ok_or_else(|| format!("{}: the name is not UTF-8", path.display()))?
            .to_string();
        let kind = info.file_type();
        if kind.is_dir() {
            entries.push(Entry { name, mode: S_IFDIR | 0o755, data: Vec::new(), mtime: info.mtime() });
            collect(top, &path, entries)?;
        } else if kind.is_symlink() {
            let target = fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let data = target
                .to_str()
                .ok_or_else(|| format!("{}: the link target is not UTF-8", path.display()))?
                .as_bytes()
                .to_vec();
            entries.push(Entry { name, mode: S_IFLNK | 0o777, data, mtime: info.mtime() });
        } else if kind.is_file() {
            let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let executable = info.permissions().mode() & 0o111 != 0;
            let mode = S_IFREG | if executable { 0o755 } else { 0o644 };
            entries.push(Entry { name, mode, data, mtime: info.mtime() });
        }
    }
    Ok(())
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn write_entry(out: &mut Vec<u8>, name: &str, mode: u32, data: &[u8], ino: u32, mtime: i64) {
    out.extend_from_slice(b"070701");
    let fields = [
        ino,
        mode,
        0, // uid
        0, // gid
        1, // nlink
        mtime as u32,
        data.len() as u32,
        0, // devmajor
        0, // devminor
        0, // rdevmajor
        0, // rdevminor
        name.len() as u32 + 1,
        0, // check
    ];
    for field in fields {
        out.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad4(out);
    if !data.is_empty() {
        out.extend_from_slice(data);
        pad4(out);
    }
}
