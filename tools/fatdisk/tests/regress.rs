//! Regression tests: the rust-fatfs issues this code replaced, and the edges
//! of the FAT format. Every volume written here is checked with fsck_msdos -n,
//! and where names or contents matter, read back through a mount on the Mac.

mod common;

use common::*;
use fatdisk::fat::table::{CLEAN_SHUTDOWN, NO_HARD_ERROR};
use fatdisk::fat::{self, FsError, Volume};
use fatdisk::image::{self, Memory, Recorder};
use fatdisk::macos;

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn dotdot_of(volume: &Volume<Memory>, cluster: u32) -> u32 {
    let entry = raw_entries(volume, cluster)[1];
    assert_eq!(&entry[..11], b"..         ");
    ((u16::from_le_bytes([entry[20], entry[21]]) as u32) << 16) | u16::from_le_bytes([entry[26], entry[27]]) as u32
}

fn shown(short: &[u8; 11]) -> String {
    let base = String::from_utf8_lossy(&short[..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&short[8..]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{}.{}", base, ext)
    }
}

/// rust-fatfs #118: `create_file` panics on a name that starts with a
/// multibyte character. Here every valid UTF-8 name is either created or
/// refused with an error.
#[test]
fn multibyte_names() {
    let dir = scratch("multibyte");
    let (path, mut volume) = fresh(&dir, 40, None);
    let names = [
        "日本語.txt",
        "🎉",
        "🎉🎉🎉.txt",
        "中",
        "ü.txt",
        "Ωmega.TXT",
        "e\u{301}cole, decomposed.txt",
        "\u{e9}cole, composed.txt",
        "\u{301} starts with a combining mark.txt",
        "a\u{200d}b, a zero-width joiner.txt",
        "\u{feff}starts with a byte order mark.txt",
        "𝄞",
        "\u{7f}.txt",
        "𠮷𠮷𠮷𠮷𠮷𠮷𠮷𠮷𠮷𠮷.txt",
    ];
    for name in names {
        volume.create("", name).unwrap_or_else(|e| panic!("create {:?}: {:?}", name, e));
        write_all(&mut volume, name, name.as_bytes()).unwrap();
    }
    for name in names {
        assert_eq!(volume.lookup("", name).unwrap().name, name);
        assert_eq!(read_all(&mut volume, name).unwrap(), name.as_bytes());
    }
    // A composed and a decomposed spelling are two names, as on Linux.
    assert_eq!(volume.lookup("", "\u{e9}cole, decomposed.txt").unwrap_err(), FsError::NotFound);
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    let seen = walk_mac(&mounted.point);
    drop(mounted);
    let mut wrong = Vec::new();
    for name in names {
        let got = seen.get(&nfc_one(name));
        let expected = if nfc_one(name) != name {
            // macOS looks a name up precomposed, so a name stored decomposed
            // is in its listing and cannot be opened there.
            Some(Some(b"the Mac lists this file and cannot open it: No such file or directory (os error 2)".to_vec()))
        } else {
            Some(Some(name.as_bytes().to_vec()))
        };
        if got.cloned() != expected {
            wrong.push(format!("{:?}: {:?}", name, got.map(|data| data.as_ref().map(|bytes| String::from_utf8_lossy(bytes).to_string()))));
        }
    }
    assert!(wrong.is_empty(), "the Mac's view differs for:\n{}", wrong.join("\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// rust-fatfs #119: moving a directory does not update its `..`. Here `..`
/// names the new parent, or 0 for the root, and a directory is not moved into
/// itself.
#[test]
fn moved_directory_dotdot() {
    let dir = scratch("dotdot");
    let (path, mut volume) = fresh(&dir, 40, None);
    volume.mkdir("", "a").unwrap();
    volume.mkdir("a", "b").unwrap();
    volume.mkdir("a/b", "c").unwrap();
    volume.create("a/b/c", "file.txt").unwrap();
    write_all(&mut volume, "a/b/c/file.txt", b"inside").unwrap();
    volume.mkdir("", "x").unwrap();
    assert_eq!(volume.rename("a", "b", "x", "b").unwrap(), "b");
    let x = volume.clusters_of("", "x").unwrap()[0];
    let b = volume.clusters_of("x", "b").unwrap()[0];
    assert_eq!(dotdot_of(&volume, b), x);
    assert_eq!(volume.rename("x", "b", "", "b, moved to the root").unwrap(), "b, moved to the root");
    let b = volume.clusters_of("", "b, moved to the root").unwrap()[0];
    assert_eq!(dotdot_of(&volume, b), 0);
    let c = volume.clusters_of("b, moved to the root", "c").unwrap()[0];
    assert_eq!(dotdot_of(&volume, c), b);
    assert_eq!(volume.rename("", "b, moved to the root", "b, moved to the root/c", "b").unwrap_err(), FsError::Invalid);
    assert_eq!(volume.rename("", "b, moved to the root", "b, moved to the root", "b").unwrap_err(), FsError::Invalid);
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    assert_eq!(std::fs::read(mounted.point.join("b, moved to the root/c/file.txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read_dir(mounted.point.join("b, moved to the root/c/..")).unwrap().count(), 1);
    drop(mounted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Short names with `~N`: unique, the checksum in every long entry, found by
/// the short name as well, and an 8.3 name kept without a tail.
#[test]
fn numeric_tails() {
    let dir = scratch("tails");
    let (path, mut volume) = fresh(&dir, 40, None);
    for i in 0..30 {
        volume.create("", &format!("Long File Name {}.txt", i)).unwrap();
    }
    volume.create("", "readme.txt").unwrap();
    assert_eq!(volume.create("", "README.TXT").unwrap_err(), FsError::Exists);
    volume.create("", "read me.txt").unwrap();
    volume.create("", "UPPER.TXT").unwrap();
    let entries = raw_entries(&volume, volume.layout().root_cluster);
    let shorts = short_names(&entries);
    let mut unique = shorts.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), shorts.len(), "short names repeat");
    let names: Vec<String> = shorts.iter().map(shown).collect();
    for expected in ["LONGFI~1.TXT", "LONGFI~9.TXT", "LONGF~10.TXT", "LONGF~30.TXT", "README.TXT", "README~1.TXT", "UPPER.TXT"] {
        assert!(names.contains(&expected.to_string()), "{} is not among {:?}", expected, names);
    }
    // Each run of long entries carries its short entry's checksum.
    for (i, entry) in entries.iter().enumerate() {
        if entry[0] == 0 {
            break;
        }
        if entry[0] != 0xE5 && entry[11] & 0x3F == 0x0F && entry[0] & 0x3F == 1 {
            let short: [u8; 11] = entries[i + 1][..11].try_into().unwrap();
            assert_eq!(entry[13], fat::name::checksum(&short));
        }
    }
    assert_eq!(volume.lookup("", "LONGFI~1.TXT").unwrap().name, "Long File Name 0.txt");
    assert_eq!(volume.lookup("", "longf~10.txt").unwrap().name, "Long File Name 9.txt");
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    assert_eq!(walk_mac(&mounted.point).len(), 33);
    drop(mounted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory of 500 long names, far past its first cluster, with half
/// removed and the holes used again.
#[test]
fn directory_grows() {
    let dir = scratch("grows");
    let (path, mut volume) = fresh(&dir, 40, None);
    volume.mkdir("", "grow").unwrap();
    for i in 0..500 {
        volume.create("grow", &format!("entry number {} with a long name.txt", i)).unwrap();
    }
    assert_eq!(volume.list("grow").unwrap().len(), 500);
    assert!(volume.clusters_of("", "grow").unwrap().len() > 100);
    for i in (0..500).step_by(2) {
        volume.remove("grow", &format!("entry number {} with a long name.txt", i), false).unwrap();
    }
    let clusters = volume.clusters_of("", "grow").unwrap().len();
    for i in 0..250 {
        volume.create("grow", &format!("again {}.txt", i)).unwrap();
    }
    assert_eq!(volume.list("grow").unwrap().len(), 500);
    assert_eq!(volume.clusters_of("", "grow").unwrap().len(), clusters, "the holes were not used again");
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    assert_eq!(std::fs::read_dir(mounted.point.join("grow")).unwrap().count(), 500);
    drop(mounted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Truncating to zero frees every cluster; extending, by truncating longer or
/// writing past the end, reads as zeros.
#[test]
fn truncate_and_extend() {
    let dir = scratch("truncate");
    let (path, mut volume) = fresh(&dir, 40, None);
    let c = volume.layout().cluster_bytes as usize;
    volume.create("", "file.bin").unwrap();
    write_all(&mut volume, "file.bin", &pattern(5 * c, 1)).unwrap();
    let (_, _, free_before) = volume.stats().unwrap();
    volume.truncate("file.bin", 0).unwrap();
    assert_eq!(volume.lookup("", "file.bin").unwrap().len, 0);
    assert!(volume.clusters_of("", "file.bin").unwrap().is_empty());
    assert_eq!(volume.stats().unwrap().2, free_before + 5);
    volume.truncate("file.bin", (3 * c + 5) as u64).unwrap();
    assert_eq!(read_all(&mut volume, "file.bin").unwrap(), vec![0u8; 3 * c + 5]);
    volume.write("file.bin", (6 * c + 3) as u64, b"after a gap").unwrap();
    let mut expected = vec![0u8; 6 * c + 3];
    expected.extend_from_slice(b"after a gap");
    assert_eq!(read_all(&mut volume, "file.bin").unwrap(), expected);
    volume.truncate("file.bin", (c + 1) as u64).unwrap();
    expected.truncate(c + 1);
    assert_eq!(read_all(&mut volume, "file.bin").unwrap(), expected);
    assert_eq!(volume.clusters_of("", "file.bin").unwrap().len(), 2);
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    assert_eq!(std::fs::read(mounted.point.join("file.bin")).unwrap(), expected);
    drop(mounted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A full volume: a write stops at the last free cluster and says how much it
/// wrote, then everything that needs a cluster is ENOSPC, and removing the
/// file gives the clusters back.
#[test]
fn full_volume() {
    let dir = scratch("full");
    let (path, mut volume) = fresh(&dir, 36, None);
    let (cluster, _, free) = volume.stats().unwrap();
    volume.create("", "big.bin").unwrap();
    let chunk = vec![0xABu8; 1 << 20];
    let mut size = 0u64;
    loop {
        match volume.write("big.bin", size, &chunk) {
            Ok(n) => {
                size += n as u64;
                if n < chunk.len() {
                    break;
                }
            }
            Err(FsError::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        }
    }
    assert_eq!(size, free as u64 * cluster as u64);
    assert_eq!(volume.stats().unwrap().2, 0);
    assert_eq!(volume.lookup("", "big.bin").unwrap().len as u64, size);
    assert_eq!(volume.write("big.bin", size, b"x").unwrap_err(), FsError::NoSpace);
    assert_eq!(volume.truncate("big.bin", size + 1).unwrap_err(), FsError::NoSpace);
    assert_eq!(volume.mkdir("", "no room").unwrap_err(), FsError::NoSpace);
    let mut made = 0;
    loop {
        match volume.create("", &format!("name {}", made)) {
            Ok(_) => made += 1,
            Err(FsError::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        }
        assert!(made < 100_000);
    }
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let mut volume = image::mount(Memory { data: std::fs::read(&path).unwrap() }).unwrap();
    volume.remove("", "big.bin", false).unwrap();
    assert_eq!(volume.stats().unwrap().2, free);
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Files of exactly one cluster, and of one cluster and a byte, with 512-byte
/// and 4 KiB clusters.
#[test]
fn one_cluster_and_a_byte() {
    for (mib, sectors) in [(40u64, 1u32), (300, 8)] {
        let dir = scratch(&format!("cluster-{}", sectors));
        let (path, mut volume) = fresh(&dir, mib, Some(sectors));
        let c = volume.layout().cluster_bytes as usize;
        for (name, len) in [("one.bin", c), ("one and a byte.bin", c + 1), ("one less a byte.bin", c - 1)] {
            volume.create("", name).unwrap();
            write_all(&mut volume, name, &pattern(len, 9)).unwrap();
        }
        volume.create("", "in two writes.bin").unwrap();
        volume.write("in two writes.bin", 0, &pattern(c - 1, 11)[..]).unwrap();
        volume.write("in two writes.bin", (c - 1) as u64, &[1, 2]).unwrap();
        assert_eq!(volume.clusters_of("", "one.bin").unwrap().len(), 1);
        assert_eq!(volume.clusters_of("", "one and a byte.bin").unwrap().len(), 2);
        assert_eq!(volume.clusters_of("", "one less a byte.bin").unwrap().len(), 1);
        assert_eq!(volume.clusters_of("", "in two writes.bin").unwrap().len(), 2);
        let mut expected = pattern(c - 1, 11);
        expected.extend_from_slice(&[1, 2]);
        assert_eq!(read_all(&mut volume, "in two writes.bin").unwrap(), expected);
        volume.sync().unwrap();
        save(volume, &path);
        assert_fsck_clean(&path);
        let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
        assert_eq!(std::fs::read(mounted.point.join("one and a byte.bin")).unwrap(), pattern(c + 1, 9));
        assert_eq!(std::fs::read(mounted.point.join("in two writes.bin")).unwrap(), expected);
        drop(mounted);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Names FAT cannot hold are errors, never panics.
#[test]
fn names_fat_cannot_hold() {
    let dir = scratch("names");
    let (path, mut volume) = fresh(&dir, 40, None);
    for bad in ["a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b", "tab\there", "\u{1}", "...", ".", "..", " ", "   ", ". .", "ends with a space "] {
        assert_eq!(volume.create("", bad).unwrap_err(), FsError::BadName, "{:?}", bad);
        assert_eq!(volume.mkdir("", bad).unwrap_err(), FsError::BadName, "{:?}", bad);
    }
    volume.create("", "source").unwrap();
    assert_eq!(volume.rename("", "source", "", "a:b").unwrap_err(), FsError::BadName);
    assert_eq!(volume.create("", &"x".repeat(256)).unwrap_err(), FsError::NameTooLong);
    assert_eq!(volume.create("", &"😀".repeat(128)).unwrap_err(), FsError::NameTooLong);
    volume.create("", &("😀".repeat(127) + "x")).unwrap();
    volume.create("", &"y".repeat(255)).unwrap();
    assert_eq!(volume.create("", "a name with a period.").unwrap().name, "a name with a period");
    assert_eq!(volume.lookup("", "a name with a period...").unwrap().name, "a name with a period");
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ASCII letters match in either case and keep the case they were made with;
/// other letters match only themselves.
#[test]
fn case() {
    let dir = scratch("case");
    let (path, mut volume) = fresh(&dir, 40, None);
    volume.create("", "Index.HTML").unwrap();
    assert_eq!(volume.lookup("", "index.html").unwrap().name, "Index.HTML");
    assert_eq!(volume.create("", "INDEX.html").unwrap_err(), FsError::Exists);
    volume.create("", "École.txt").unwrap();
    assert_eq!(volume.lookup("", "école.txt").unwrap_err(), FsError::NotFound);
    volume.create("", "école.txt").unwrap();
    volume.create("", "A long name.html").unwrap();
    assert_eq!(volume.lookup("", "ALONGN~1.HTM").unwrap().name, "A long name.html");
    assert_eq!(volume.rename("", "Index.HTML", "", "index.html").unwrap(), "index.html");
    let names: Vec<String> = volume.list("").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"index.html".to_string()) && !names.contains(&"Index.HTML".to_string()), "{:?}", names);
    volume.sync().unwrap();
    save(volume, &path);
    assert_fsck_clean(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// FAT[1]'s clean-shutdown flag: cleared before the first write and set again
/// by sync; left cleared when it was cleared at mount; the hard-error flag
/// kept as found. Linux's boot-sector byte follows it.
#[test]
fn dirty_flag() {
    let dir = scratch("dirty");
    let (path, mut volume) = fresh(&dir, 40, None);
    let layout = volume.layout();
    let fat1 = |data: &[u8], copy: u32| u32_at(data, layout.fat_offset(copy, 1) as usize);
    assert!(!volume.dirty_at_mount());
    volume.create("", "written, not synced").unwrap();
    let data = volume.into_device().data;
    for copy in 0..2 {
        assert_eq!(fat1(&data, copy) & CLEAN_SHUTDOWN, 0);
    }
    assert_eq!(data[65] & 1, 1);
    std::fs::write(&path, &data).unwrap();
    let findings = fsck_findings(&path);
    assert!(findings.iter().any(|line| line.contains("LEFT MARKED AS DIRTY")), "{:?}", findings);

    let mut again = image::mount(Memory { data }).unwrap();
    assert!(again.dirty_at_mount());
    again.create("", "second").unwrap();
    again.sync().unwrap();
    let data = again.into_device().data;
    assert_eq!(fat1(&data, 0) & CLEAN_SHUTDOWN, 0);
    std::fs::write(&path, &data).unwrap();
    assert!(fsck_findings(&path).iter().any(|line| line.contains("LEFT MARKED AS DIRTY")));

    let other = scratch("dirty-clean");
    let (path, volume) = fresh(&other, 40, None);
    let mut data = volume.into_device().data;
    for copy in 0..2 {
        let at = layout.fat_offset(copy, 1) as usize;
        let value = u32_at(&data, at) & !NO_HARD_ERROR;
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut volume = image::mount(Memory { data }).unwrap();
    assert!(!volume.dirty_at_mount());
    volume.create("", "then synced").unwrap();
    volume.sync().unwrap();
    let data = volume.into_device().data;
    for copy in 0..2 {
        assert_eq!(fat1(&data, copy) & CLEAN_SHUTDOWN, CLEAN_SHUTDOWN);
        assert_eq!(fat1(&data, copy) & NO_HARD_ERROR, 0);
    }
    assert_eq!(data[65] & 1, 0);
    std::fs::write(&path, &data).unwrap();
    assert!(!fsck_findings(&path).iter().any(|line| line.contains("DIRTY")));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&other);
}

/// A card that fails a command: the operation is EIO, nothing panics, and
/// once the card answers again the volume is what the card holds.
#[test]
fn card_errors() {
    let dir = scratch("errors");
    let (path, volume) = fresh(&dir, 40, None);
    let mut volume = image::mount(Recorder::new(volume.into_device().data)).unwrap();
    let c = volume.layout().cluster_bytes as u64;
    volume.create("", "file.bin").unwrap();
    write_all(&mut volume, "file.bin", &pattern(2 * c as usize, 3)).unwrap();
    for fail_at in 0..8 {
        let done = volume.device().writes.len();
        volume.device_mut().fail_after = Some(done + fail_at);
        let result = volume.write("file.bin", 2 * c, &pattern(4 * c as usize, 4));
        volume.device_mut().fail_after = None;
        if result.is_ok() {
            volume.truncate("file.bin", 2 * c).unwrap();
            continue;
        }
        assert_eq!(result.unwrap_err(), FsError::Io);
        let len = volume.lookup("", "file.bin").unwrap().len as u64;
        assert!(len == 2 * c || len == 6 * c, "length {} after a failure at write {}", len, fail_at);
        assert_eq!(read_all(&mut volume, "file.bin").unwrap()[..2 * c as usize], pattern(2 * c as usize, 3)[..]);
        volume.truncate("file.bin", 2 * c).unwrap();
    }
    volume.device_mut().fail_reads = true;
    let mut buf = vec![0u8; 4096];
    assert_eq!(volume.read("file.bin", 0, &mut buf).unwrap_err(), FsError::Io);
    let _ = volume.list("");
    let _ = volume.mkdir("", "while failing");
    volume.device_mut().fail_reads = false;
    volume.sync().unwrap();
    std::fs::write(&path, volume.into_device().data).unwrap();
    let findings = fsck_findings(&path);
    assert!(findings.iter().all(|line| line.contains("orphan") || line.contains("Fix? no") || line.contains("Free space")), "{:?}", findings);
    let _ = std::fs::remove_dir_all(&dir);
}

/// BPB_ExtFlags: with bit 7 set only the FAT it names is read and written; a
/// FAT that does not exist is refused; mirrored FATs change together.
#[test]
fn fat_copies() {
    let dir = scratch("copies");
    let path = dir.join("volume.img");
    macos::newfs_whole(&path, 40, "COPIES", None).unwrap();
    let mut data = std::fs::read(&path).unwrap();
    data[40] = 0x81;
    data[41] = 0;
    let before = data.clone();
    let mut volume = image::mount(Memory { data }).unwrap();
    let layout = volume.layout();
    assert_eq!(layout.active_fat, Some(1));
    let fat = |copy: u32| {
        let start = layout.fat_offset(copy, 0) as usize;
        start..start + layout.fat_blocks as usize * 512
    };
    volume.create("", "file.bin").unwrap();
    write_all(&mut volume, "file.bin", &pattern(3 * 512, 2)).unwrap();
    volume.sync().unwrap();
    let after = volume.into_device().data;
    assert_eq!(after[fat(0)], before[fat(0)], "FAT 0 changed while only FAT 1 is active");
    assert_ne!(after[fat(1)], before[fat(1)]);

    let mut bad = before.clone();
    bad[40] = 0x83;
    let refusal = fat::probe(&mut Memory { data: bad }).unwrap_err();
    assert!(refusal.contains("only active"), "{}", refusal);

    let mut volume = image::mount(Memory { data: std::fs::read(&path).unwrap() }).unwrap();
    volume.create("", "file.bin").unwrap();
    write_all(&mut volume, "file.bin", &pattern(3 * 512, 2)).unwrap();
    volume.sync().unwrap();
    let after = volume.into_device().data;
    assert_eq!(after[fat(0)], after[fat(1)]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// FSInfo's hints are not trusted, and sync writes correct ones.
#[test]
fn fsinfo_hints() {
    let dir = scratch("fsinfo");
    let (path, volume) = fresh(&dir, 40, None);
    let mut data = volume.into_device().data;
    data[512 + 488..512 + 492].copy_from_slice(&5u32.to_le_bytes());
    data[512 + 492..512 + 496].copy_from_slice(&0x0FFF_FFF0u32.to_le_bytes());
    let mut volume = image::mount(Memory { data }).unwrap();
    let (_, total, free) = volume.stats().unwrap();
    assert!(free > 5 && free < total);
    volume.create("", "file.bin").unwrap();
    write_all(&mut volume, "file.bin", &pattern(10 * 512, 6)).unwrap();
    assert_eq!(volume.stats().unwrap().2, free - 10);
    volume.sync().unwrap();
    let data = volume.into_device().data;
    assert_eq!(u32_at(&data, 512 + 488), free - 10);
    std::fs::write(&path, &data).unwrap();
    assert_fsck_clean(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Timestamps are the kernel's clock as UTC. The Mac reads a FAT stamp as its
/// own local time, so it shows the same moment shifted by its offset from UTC.
#[test]
fn timestamps_are_utc() {
    let dir = scratch("stamps");
    let (path, volume) = fresh(&dir, 40, None);
    let mut blocks = Memory { data: volume.into_device().data };
    let probe = fat::probe(&mut blocks).unwrap();
    let mut volume = Volume::mount(blocks, probe.layout, || 1_700_000_001).unwrap();
    volume.allow_writes();
    // 2023-11-14 22:13:21 UTC, stored to FAT's two seconds.
    assert_eq!(volume.create("", "stamped.txt").unwrap().mtime, 1_700_000_000);
    let entries = raw_entries(&volume, probe.layout.root_cluster);
    let entry = entries.iter().find(|e| &e[..11] == b"STAMPED TXT").unwrap();
    assert_eq!(u16::from_le_bytes([entry[24], entry[25]]), ((2023 - 1980) << 9) | (11 << 5) | 14);
    assert_eq!(u16::from_le_bytes([entry[22], entry[23]]), (22 << 11) | (13 << 5) | 10);
    volume.sync().unwrap();
    save(volume, &path);
    let offset = std::process::Command::new("date").args(["-r", "1700000000", "+%z"]).output().unwrap();
    let offset = String::from_utf8(offset.stdout).unwrap();
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let hours: i64 = offset[1..3].parse().unwrap();
    let minutes: i64 = offset[3..5].trim().parse().unwrap();
    let local = sign * (hours * 3600 + minutes * 60);
    let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
    let modified = std::fs::metadata(mounted.point.join("stamped.txt")).unwrap().modified().unwrap();
    drop(mounted);
    let seconds = modified.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert_eq!(seconds, 1_700_000_000 - local, "the Mac's offset from UTC is {} s", local);
    let _ = std::fs::remove_dir_all(&dir);
}
