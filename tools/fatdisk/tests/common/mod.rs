//! What the tests share: scratch directories, volumes newfs_msdos made and
//! this code mounted, the set of names and contents both sides write, and
//! walks of a volume through this code and through the Mac's mount.

#![allow(dead_code)]

use fatdisk::fat::{join, Blocks, FsError, Volume};
use fatdisk::image::{self, Memory};
use fatdisk::macos;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Names in Unicode normalisation form C, by Python's `unicodedata`. macOS
/// stores a FAT name precomposed and shows it to programs decomposed, so what
/// the Mac lists is composed again before it is compared.
pub fn nfc(names: &[String]) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let script = "import sys, unicodedata\nsys.stdout.buffer.write(unicodedata.normalize('NFC', sys.stdin.buffer.read().decode('utf-8')).encode('utf-8'))";
    let mut child = Command::new("python3").args(["-c", script]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(names.join("\n").as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout).unwrap().split('\n').map(String::from).collect()
}

pub fn nfc_one(name: &str) -> String {
    nfc(&[name.to_string()]).remove(0)
}

/// Whether a path holds a name macOS makes for itself on a volume it has
/// mounted read-write: its metadata directories at the root, and `._` files
/// that hold a file's extended attributes.
pub fn mac_made(path: &str) -> bool {
    path.split('/').enumerate().any(|(depth, name)| name.starts_with("._") || (depth == 0 && matches!(name, ".fseventsd" | ".Spotlight-V100" | ".Trashes" | ".TemporaryItems")))
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fatdisk-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A new image of `mib` MiB with one volume newfs_msdos made, and that volume
/// mounted by this code.
pub fn fresh(dir: &Path, mib: u64, cluster_sectors: Option<u32>) -> (PathBuf, Volume<Memory>) {
    let path = dir.join("volume.img");
    macos::newfs_whole(&path, mib, "TESTVOL", cluster_sectors).unwrap();
    let volume = image::mount(Memory { data: std::fs::read(&path).unwrap() }).unwrap();
    (path, volume)
}

pub fn save(volume: Volume<Memory>, path: &Path) {
    std::fs::write(path, volume.into_device().data).unwrap();
}

/// Require fsck_msdos -n to find nothing to report in the image.
pub fn assert_fsck_clean(path: &Path) {
    let (code, output) = macos::fsck(path);
    assert_eq!(macos::findings(&output), Vec::<String>::new(), "fsck_msdos -n said:\n{}", output);
    assert_eq!(code, 0, "fsck_msdos -n exit status; it said:\n{}", output);
}

/// fsck_msdos -n's findings on the image.
pub fn fsck_findings(path: &Path) -> Vec<String> {
    macos::findings(&macos::fsck(path).1)
}

pub fn write_all<B: Blocks>(volume: &mut Volume<B>, path: &str, data: &[u8]) -> Result<(), FsError> {
    let mut done = 0;
    while done < data.len() {
        let n = volume.write(path, done as u64, &data[done..])?;
        assert!(n > 0, "a write of {} made no progress", path);
        done += n;
    }
    Ok(())
}

pub fn read_all<B: Blocks>(volume: &mut Volume<B>, path: &str) -> Result<Vec<u8>, FsError> {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let len = volume.lookup(dir, name)?.len as usize;
    let mut data = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = volume.read(path, done as u64, &mut data[done..])?;
        assert!(n > 0, "a read of {} stopped at {} of {}", path, done, len);
        done += n;
    }
    Ok(data)
}

/// A pattern of `len` bytes that differs with `seed`.
pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i as u32).wrapping_mul(2_654_435_761).rotate_left(seed as u32 % 31) >> 13) as u8 ^ seed).collect()
}

/// A tree: directories as `None`, files with their contents, parents before
/// children.
pub type Tree = BTreeMap<String, Option<Vec<u8>>>;

/// What both sides write in the interoperability tests: names in Japanese,
/// with emoji and characters outside the Basic Multilingual Plane, long names
/// up to 255 UTF-16 units, 8.3 names in either case, a deep directory, a
/// directory of 300 long names, a large file, and files of exactly one
/// cluster and one cluster and a byte.
pub fn corpus(cluster: usize) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    let mut file = |path: &str, data: Vec<u8>| out.push((path.to_string(), Some(data)));
    file("日本語.txt", "こんにちは、世界\n".as_bytes().to_vec());
    file("🎉.txt", "starts with an emoji\n".as_bytes().to_vec());
    file("emoji 🎉🚀 party.txt", b"emoji in the middle\n".to_vec());
    file("𝄞 music and 𠮷野家.txt", b"outside the Basic Multilingual Plane\n".to_vec());
    file("MixedCase.Html", b"<p>mixed</p>\n".to_vec());
    file("README.TXT", b"an 8.3 name in upper case\n".to_vec());
    file("lower.txt", b"an 8.3 name in lower case\n".to_vec());
    file("Spaces In A Name.txt", b"spaces\n".to_vec());
    file("multiple.dots.in.a.name.tar.gz", b"dots\n".to_vec());
    file(".starts-with-a-dot", b"a leading dot\n".to_vec());
    file("empty.txt", Vec::new());
    let long: String = "L".repeat(250) + ".html";
    file(&long, b"255 UTF-16 units\n".to_vec());
    let japanese_long: String = "あ".repeat(120) + ".txt";
    file(&japanese_long, "124 units and 364 bytes of UTF-8\n".as_bytes().to_vec());
    file("big.bin", pattern(3 * 1024 * 1024 + 17, 3));
    file("one-cluster.bin", pattern(cluster, 5));
    file("one-cluster-and-a-byte.bin", pattern(cluster + 1, 7));
    let mut dirs = Vec::new();
    let mut path = String::from("deep");
    dirs.push(path.clone());
    for level in 1..=12 {
        path = format!("{}/level {} ディレクトリ", path, level);
        dirs.push(path.clone());
    }
    dirs.push(String::from("many"));
    let mut files = vec![(format!("{}/leaf.txt", path), b"twelve levels down\n".to_vec())];
    for i in 0..300 {
        files.push((format!("many/a file with a long name, number {}.txt", i), format!("{}\n", i).into_bytes()));
    }
    let mut all: Vec<(String, Option<Vec<u8>>)> = dirs.into_iter().map(|dir| (dir, None)).collect();
    all.extend(out);
    all.extend(files.into_iter().map(|(p, d)| (p, Some(d))));
    all
}

/// Everything under the volume's root, through this code.
pub fn walk_ours<B: Blocks>(volume: &mut Volume<B>) -> Tree {
    let mut tree = Tree::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        for entry in volume.list(&dir).unwrap_or_else(|e| panic!("listing /{}: {:?}", dir, e)) {
            let path = join(&dir, &entry.name);
            if entry.is_dir {
                tree.insert(path.clone(), None);
                pending.push(path);
            } else {
                let data = read_all(volume, &path).unwrap_or_else(|e| panic!("reading /{}: {:?}", path, e));
                tree.insert(path, Some(data));
            }
        }
    }
    tree
}

/// Everything under a directory the Mac has mounted, but what macOS makes for
/// itself, with names in normalisation form C.
pub fn walk_mac(root: &Path) -> Tree {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let path = join(&prefix, &name);
            if mac_made(&path) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                found.push((path.clone(), None));
                pending.push((entry.path(), path));
            } else {
                // A name the Mac lists but cannot open is recorded as such
                // rather than stopping the walk, so a test can say which.
                let data = std::fs::read(entry.path()).unwrap_or_else(|e| format!("the Mac lists this file and cannot open it: {}", e).into_bytes());
                found.push((path, Some(data)));
            }
        }
    }
    let names: Vec<String> = found.iter().map(|(path, _)| path.clone()).collect();
    nfc(&names).into_iter().zip(found.into_iter().map(|(_, data)| data)).collect()
}

/// The raw 32-byte entries of the directory whose chain starts at `cluster`,
/// read straight out of the image and following FAT 0, not through the code
/// under test.
pub fn raw_entries(volume: &Volume<Memory>, cluster: u32) -> Vec<[u8; 32]> {
    let layout = volume.layout();
    let data = &volume.device().data;
    let mut out = Vec::new();
    let mut at_cluster = cluster;
    for _ in 0..layout.clusters {
        let at = layout.cluster_offset(at_cluster).unwrap() as usize;
        for entry in data[at..at + layout.cluster_bytes as usize].chunks_exact(32) {
            out.push(entry.try_into().unwrap());
        }
        let fat = layout.fat_offset(0, at_cluster) as usize;
        let next = u32::from_le_bytes(data[fat..fat + 4].try_into().unwrap()) & 0x0FFF_FFFF;
        if next >= 0x0FFF_FFF8 {
            break;
        }
        at_cluster = next;
    }
    out
}

/// The short names in use among raw entries, up to the end mark.
pub fn short_names(entries: &[[u8; 32]]) -> Vec<[u8; 11]> {
    let mut out = Vec::new();
    for entry in entries {
        if entry[0] == 0 {
            break;
        }
        if entry[0] == 0xE5 || entry[11] & 0x3F == 0x0F || entry[11] & 0x08 != 0 {
            continue;
        }
        out.push(entry[..11].try_into().unwrap());
    }
    out
}

/// The differences between two trees, for an assertion message.
pub fn differences(expected: &Tree, actual: &Tree) -> Vec<String> {
    let mut out = Vec::new();
    for (path, data) in expected {
        match actual.get(path) {
            None => out.push(format!("missing: {:?}", path)),
            Some(found) if found != data => out.push(format!("different contents: {:?}", path)),
            _ => {}
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            out.push(format!("unexpected: {:?}", path));
        }
    }
    out
}
