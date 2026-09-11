//! Reader for the cpio "newc" archive format used for the initramfs.

use super::{link_node, lookup, mkdir_p, Node, NodeKind};
use crate::abi::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use alloc::string::String;

const HEADER_SIZE: usize = 110;
const MAGIC: &[u8; 6] = b"070701";

fn hex_field(bytes: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for &b in bytes {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | digit as u64;
    }
    Some(value)
}

#[inline]
fn align4(value: usize) -> usize {
    (value + 3) & !3
}

pub struct Stats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes: usize,
}

/// Unpack `archive` into the filesystem rooted at `/`.
pub fn extract(archive: &[u8]) -> Result<Stats, &'static str> {
    let mut stats = Stats { files: 0, dirs: 0, symlinks: 0, bytes: 0 };
    let mut pos = 0usize;

    while pos + HEADER_SIZE <= archive.len() {
        let header = &archive[pos..pos + HEADER_SIZE];
        if &header[0..6] != MAGIC {
            return Err("bad cpio magic");
        }
        let mode = hex_field(&header[14..22]).ok_or("bad mode")? as u32;
        let filesize = hex_field(&header[54..62]).ok_or("bad filesize")? as usize;
        let namesize = hex_field(&header[94..102]).ok_or("bad namesize")? as usize;

        let name_start = pos + HEADER_SIZE;
        let name_end = name_start + namesize;
        if name_end > archive.len() {
            return Err("truncated cpio name");
        }
        // namesize counts the trailing NUL.
        let name_bytes = &archive[name_start..name_end - 1];
        let name = core::str::from_utf8(name_bytes).map_err(|_| "non-utf8 cpio name")?;

        if name == "TRAILER!!!" {
            break;
        }

        let data_start = pos + align4(HEADER_SIZE + namesize);
        let data_end = data_start + filesize;
        if data_end > archive.len() {
            return Err("truncated cpio data");
        }
        let data = &archive[data_start..data_end];

        // Archives name entries relative to the root: "bin/sh", "." etc.
        let path = if name == "." {
            String::from("/")
        } else if let Some(rest) = name.strip_prefix("./") {
            alloc::format!("/{}", rest)
        } else if name.starts_with('/') {
            String::from(name)
        } else {
            alloc::format!("/{}", name)
        };

        match mode & S_IFMT {
            S_IFDIR => {
                if path != "/" {
                    mkdir_p(&path).map_err(|_| "mkdir failed")?;
                    stats.dirs += 1;
                }
            }
            S_IFREG => {
                if let Some(parent) = parent_of(&path) {
                    let _ = mkdir_p(&parent);
                }
                let node = Node::new_file(mode & 0o7777);
                node.inner.lock().data.extend_from_slice(data);
                link_node(&path, node).map_err(|_| "link failed")?;
                stats.files += 1;
                stats.bytes += filesize;
            }
            S_IFLNK => {
                if let Some(parent) = parent_of(&path) {
                    let _ = mkdir_p(&parent);
                }
                let target = core::str::from_utf8(data).map_err(|_| "bad symlink target")?;
                let node = Node::new(NodeKind::Symlink, S_IFLNK | 0o777);
                node.inner.lock().data.extend_from_slice(target.as_bytes());
                link_node(&path, node).map_err(|_| "link failed")?;
                stats.symlinks += 1;
            }
            _ => {
                // Device nodes and sockets in the archive are ignored; /dev is
                // populated by the kernel.
            }
        }

        pos = align4(data_end);
    }

    // Make sure the standard directories exist even if the archive omitted them.
    for dir in ["/dev", "/proc", "/tmp", "/bin", "/etc"] {
        if lookup(dir).is_err() {
            let _ = mkdir_p(dir);
        }
    }
    Ok(stats)
}

fn parent_of(path: &str) -> Option<String> {
    let idx = path.rfind('/')?;
    if idx == 0 {
        return None;
    }
    Some(String::from(&path[..idx]))
}
