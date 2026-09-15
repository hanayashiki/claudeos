//! fatdisk: SD card images for testing /data, made and read on the Mac without
//! root, and the kernel's FAT32 code run against them.
//!
//! ```text
//! fatdisk mbr IMAGE MIB PART...       an MBR card of MIB MiB with one FAT32
//!                                     partition per PART, LABEL:MIB or
//!                                     LABEL:rest, and +boot after either to
//!                                     put a boot partition's files in it
//! fatdisk whole IMAGE MIB LABEL[+boot] one FAT32 volume across the whole card
//! fatdisk fill IMAGE LABEL            the content the damage tests start from
//! fatdisk put IMAGE LABEL PATH TEXT   write TEXT to PATH in the volume
//! fatdisk cat IMAGE LABEL PATH        print a file
//! fatdisk ls IMAGE LABEL PATH         list a directory, directories with a /
//! fatdisk check IMAGE LABEL           read everything, look for clusters two
//!                                     files share, and run fsck_msdos -n
//! fatdisk span IMAGE LABEL            the volume's first sector and length
//! fatdisk damage IMAGE LABEL MODE SEED [COUNT]
//!                                     boot, meta, fat, dirent or random: COUNT
//!                                       changes of that kind (see damage.rs)
//!                                     loops: loopdir and loop.bin's chains
//!                                       made to loop (after fill)
//!                                     cycle: www/css pointed at the root
//!                                       directory (after fill)
//! fatdisk fuzz FIRST LAST             the kernel's FAT code against damaged
//!                                     images, one per seed, in this process
//! fatdisk fuzz --seeds FILE           the same for the seeds FILE lists
//! fatdisk walk IMAGE LABEL            walk the volume as `find` does, through
//!                                     the kernel's code, and time it
//! fatdisk cuts                        cut power after each write of each
//!                                     operation and run fsck_msdos -n on what
//!                                     is left (see cut.rs)
//! ```
//!
//! Volumes are made by macOS's newfs_msdos, through hdiutil. Every image is a
//! sparse file the size given, so QEMU can take it as a card: QEMU wants a
//! card's size to be a power of two.

use fatdisk::damage::{self, Random};
use fatdisk::fat::{join, Entry, FsError, Volume};
use fatdisk::image::{self, Memory, MIB, SECTOR};
use fatdisk::{cut, macos};
use std::path::Path;
use std::process::exit;
use std::time::Instant;

fn die(message: &str) -> ! {
    eprintln!("fatdisk: {}", message);
    exit(1)
}

fn or_die<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| die(&e.to_string()))
}

fn fs_or_die<T>(what: &str, result: Result<T, FsError>) -> T {
    result.unwrap_or_else(|e| die(&format!("{}: {:?}", what, e)))
}

/// The volume labelled `label` in the image, read into memory and mounted.
fn open(path: &str, label: &str) -> (Volume<Memory>, u64) {
    let path = Path::new(path);
    let (start, length) = or_die(image::find(path, label));
    let data = or_die(image::read_region(path, start, length));
    (or_die(image::mount(Memory { data })), start)
}

/// Put the volume back into the image it came from.
fn save(path: &str, volume: Volume<Memory>, start: u64) {
    or_die(image::write_region(Path::new(path), start, &volume.into_device().data));
}

fn split_part(spec: &str) -> (&str, bool) {
    match spec.strip_suffix("+boot") {
        Some(rest) => (rest, true),
        None => (spec, false),
    }
}

/// A boot partition's files, for the guard that refuses to mount one.
fn put_boot_files(path: &str, label: &str) {
    let (mut volume, start) = open(path, label);
    for (name, text) in [("start4.elf", "not firmware: a boot partition's file, for the test"), ("kernel8.img", "not a kernel"), ("config.txt", "arm_64bit=1\n")] {
        write_file(&mut volume, "", name, text.as_bytes());
    }
    fs_or_die("sync", volume.sync());
    save(path, volume, start);
}

fn write_file(volume: &mut Volume<Memory>, dir: &str, name: &str, data: &[u8]) {
    match volume.create(dir, name) {
        Ok(_) | Err(FsError::Exists) => {}
        Err(e) => die(&format!("creating {}: {:?}", join(dir, name), e)),
    }
    let path = join(dir, name);
    fs_or_die(&path, volume.truncate(&path, 0));
    let mut done = 0;
    while done < data.len() {
        done += fs_or_die(&path, volume.write(&path, done as u64, &data[done..]));
    }
}

fn make_mbr(path: &str, mib: u64, parts: &[String]) {
    let mut specs = Vec::new();
    for spec in parts {
        let (spec, boot) = split_part(spec);
        let (label, size) = spec.split_once(':').unwrap_or_else(|| die(&format!("{} is not LABEL:MIB", spec)));
        let size = if size == "rest" { None } else { Some(size.parse::<u64>().unwrap_or_else(|_| die(size))) };
        specs.push((label.to_string(), size, boot));
    }
    let labels: Vec<(String, Option<u64>)> = specs.iter().map(|(label, size, _)| (label.clone(), *size)).collect();
    or_die(macos::newfs_mbr(Path::new(path), mib, &labels));
    for (label, _, boot) in specs {
        if boot {
            put_boot_files(path, &label);
        }
    }
}

fn make_whole(path: &str, mib: u64, spec: &str) {
    let (label, boot) = split_part(spec);
    or_die(macos::newfs_whole(Path::new(path), mib, label, None));
    if boot {
        put_boot_files(path, label);
    }
}

/// What the damage tests start from: directories, long names, a file of
/// several clusters, a directory of exactly two clusters of entries for
/// `loops`, and a file of three clusters for it.
fn fill_volume(volume: &mut Volume<Memory>) {
    fs_or_die("www", volume.mkdir("", "www"));
    fs_or_die("css", volume.mkdir("www", "css"));
    write_file(volume, "www", "index.html", b"<h1>hello from the card</h1>\n");
    write_file(volume, "www", "A page with a long name, spaces and (brackets).html", b"long\n");
    write_file(volume, "www/css", "site.css", b"body { color: black }\n");
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    write_file(volume, "", "big.bin", &big);
    let cluster = volume.layout().cluster_bytes as usize;
    let per_cluster = cluster / 32;
    fs_or_die("loopdir", volume.mkdir("", "loopdir"));
    // `.` and `..`, and names of two entries each: a long name and a short one.
    for i in 0..(2 * per_cluster - 2) / 2 {
        write_file(volume, "loopdir", &format!("f{:04}.txt", i), b"x");
    }
    write_file(volume, "", "loop.bin", &vec![7u8; cluster * 3]);
    fs_or_die("sync", volume.sync());
}

fn fill(path: &str, label: &str) {
    let (mut volume, start) = open(path, label);
    fill_volume(&mut volume);
    save(path, volume, start);
}

fn cat(path: &str, label: &str, file: &str) {
    let (mut volume, _) = open(path, label);
    let file = file.trim_matches('/');
    let data = read_all(&mut volume, file).unwrap_or_else(|e| die(&format!("{}: {:?}", file, e)));
    use std::io::Write;
    or_die(std::io::stdout().write_all(&data));
}

/// The entry at `path`, looked up the way the kernel's path walk does it: one
/// lookup per component, each in the directory the one before named.
fn resolve(volume: &mut Volume<Memory>, path: &str) -> Result<Entry, FsError> {
    let mut dir = String::new();
    let mut last = None;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let entry = volume.lookup(&dir, part)?;
        dir = join(&dir, &entry.name);
        last = Some(entry);
    }
    last.ok_or(FsError::NotFound)
}

/// The whole of the file at `path`, looked up component by component.
fn read_all(volume: &mut Volume<Memory>, path: &str) -> Result<Vec<u8>, FsError> {
    let entry = resolve(volume, path)?;
    let (dir, _) = path.rsplit_once('/').unwrap_or(("", path));
    let canonical = join(&canonical_dir(volume, dir)?, &entry.name);
    let mut data = vec![0u8; entry.len as usize];
    let mut done = 0;
    while done < data.len() {
        let n = volume.read(&canonical, done as u64, &mut data[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    data.truncate(done);
    Ok(data)
}

/// A directory path as the card spells it.
fn canonical_dir(volume: &mut Volume<Memory>, dir: &str) -> Result<String, FsError> {
    let mut out = String::new();
    for part in dir.split('/').filter(|part| !part.is_empty()) {
        let entry = volume.lookup(&out, part)?;
        out = join(&out, &entry.name);
    }
    Ok(out)
}

fn ls(path: &str, label: &str, dir: &str) {
    let (mut volume, _) = open(path, label);
    let dir = fs_or_die(dir, canonical_dir(&mut volume, dir.trim_matches('/')));
    let mut names: Vec<String> = fs_or_die(&dir, volume.list(&dir)).into_iter().map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name }).collect();
    names.sort();
    for name in names {
        println!("{}", name);
    }
}

/// Read every file to its length, require no cluster to belong to two
/// entries, and run fsck_msdos -n on the volume.
fn check(path: &str, label: &str) {
    let (mut volume, start) = open(path, label);
    let mut owners: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let (mut files, mut dirs, mut shared) = (0, 0, 0);
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        for entry in fs_or_die(&format!("listing /{}", dir), volume.list(&dir)) {
            let full = join(&dir, &entry.name);
            for cluster in fs_or_die(&full, volume.clusters_of(&dir, &entry.name)) {
                if let Some(other) = owners.insert(cluster, full.clone()) {
                    eprintln!("fatdisk: {} and {} share cluster {}", other, full, cluster);
                    shared += 1;
                }
            }
            if entry.is_dir {
                dirs += 1;
                pending.push(full);
                continue;
            }
            files += 1;
            let data = fs_or_die(&format!("reading {}", full), read_all(&mut volume, &full));
            if data.len() != entry.len as usize {
                die(&format!("{} is {} bytes long and {} could be read", full, entry.len, data.len()));
            }
        }
    }
    let (_, total, free) = fs_or_die("statfs", volume.stats());
    let data = volume.into_device().data;
    let copy = std::env::temp_dir().join(format!("fatdisk-check-{}.img", std::process::id()));
    or_die(std::fs::write(&copy, &data));
    let (code, output) = macos::fsck(&copy);
    let _ = std::fs::remove_file(&copy);
    let findings = macos::findings(&output);
    println!(
        "{} files and {} directories read whole; {} shared clusters; {} of {} clusters free; fsck_msdos -n exit {}{}",
        files,
        dirs,
        shared,
        free,
        total,
        code,
        if findings.is_empty() { String::from(", nothing reported") } else { format!(": {}", findings.join(" / ")) }
    );
    let _ = start;
    if shared > 0 || code != 0 || !findings.is_empty() {
        exit(1);
    }
}

fn damage_image(path: &str, label: &str, mode: &str, seed: u64, count: u64) {
    let file = Path::new(path);
    if mode == "random" {
        // Anywhere in the image: the partition table and every volume.
        let mut data = or_die(std::fs::read(file));
        let mut random = Random::new(seed);
        damage::damage(&mut data, "random", &mut random, count);
        or_die(image::write_region(file, 0, &data));
        return;
    }
    let (start, length) = or_die(image::find(file, label));
    let mut data = or_die(image::read_region(file, start, length));
    let ok = match mode {
        "loops" => damage::loops(&mut data),
        "cycle" => damage::cycle(&mut data),
        mode if damage::MODES.contains(&mode) => {
            damage::damage(&mut data, mode, &mut Random::new(seed), count);
            true
        }
        other => die(&format!("{} is not a damage mode", other)),
    };
    if !ok {
        die("the entries this damage changes are not there; run fill first");
    }
    or_die(image::write_region(file, start, &data));
}

// ---------------------------------------------------------------------------
// Fuzzing the kernel's code
// ---------------------------------------------------------------------------

/// What the damaged images did to the kernel's code.
#[derive(Default)]
struct Tally {
    refused: u64,
    unmountable: u64,
    mounted: u64,
    errors: u64,
}

/// Names the random operations use: 8.3 names in either case, long names,
/// Japanese and emoji, trailing periods, and names FAT refuses.
const NAMES: [&str; 16] = ["a", "B.TXT", "readme.txt", "Long file name number 1.html", "日本語", "🎉.txt", "sub", "Sub", "𠮷", "e.", "...", "a:b", "x", "index.html", "www", "css"];

/// `count` operations picked by `random` on whatever the volume holds, results
/// counted and otherwise ignored: paths go stale as entries move and go.
fn random_ops(volume: &mut Volume<Memory>, random: &mut Random, dirs: &[String], files: &[(String, u32)], count: usize, note: &mut impl FnMut(bool)) {
    let mut dirs: Vec<String> = dirs.to_vec();
    let mut files: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    for _ in 0..count {
        let dir = dirs[random.below(dirs.len() as u64) as usize].clone();
        let name = NAMES[random.below(NAMES.len() as u64) as usize];
        let file = if files.is_empty() { None } else { Some(files[random.below(files.len() as u64) as usize].clone()) };
        let ok = match (random.below(9), file) {
            (0, _) => volume.create(&dir, name).map(|entry| files.push(join(&dir, &entry.name))).is_ok(),
            (1, _) => volume.mkdir(&dir, name).map(|entry| dirs.push(join(&dir, &entry.name))).is_ok(),
            (2, Some(file)) => {
                let offset = random.below(1 << 16);
                let data = vec![random.next() as u8; random.below(1 << 14) as usize];
                volume.write(&file, offset, &data).is_ok()
            }
            (3, Some(file)) => volume.truncate(&file, random.below(1 << 16)).is_ok(),
            (4, _) => volume.remove(&dir, name, random.below(2) == 0).is_ok(),
            (5, _) => {
                let target = dirs[random.below(dirs.len() as u64) as usize].clone();
                volume.rename(&dir, name, &target, NAMES[random.below(NAMES.len() as u64) as usize]).is_ok()
            }
            (6, Some(file)) => volume.read(&file, random.below(1 << 16), &mut [0u8; 3000]).is_ok(),
            (7, _) => volume.list(&dir).is_ok(),
            _ => volume.sync().is_ok(),
        };
        note(ok);
    }
}

/// Everything /data's users do, against one image, and then random
/// operations. Errors are expected and counted; the only failure is a panic,
/// which the caller catches.
fn exercise(data: Vec<u8>, seed: u64, tally: &mut Tally) {
    let mut blocks = Memory { data };
    let probe = match fatdisk::fat::probe(&mut blocks) {
        Ok(probe) => probe,
        Err(_) => {
            tally.refused += 1;
            return;
        }
    };
    let mut volume = match Volume::mount(blocks, probe.layout, image::unix_now) {
        Ok(volume) => volume,
        Err(_) => {
            tally.unmountable += 1;
            return;
        }
    };
    tally.mounted += 1;
    let mut errors = 0u64;
    let mut note = |ok: bool| {
        if !ok {
            errors += 1;
        }
    };
    note(volume.boot_file().is_ok());
    volume.allow_writes();

    let mut files = Vec::new();
    let mut dirs = vec![String::new()];
    let mut at = 0;
    while at < dirs.len() && at < 64 {
        let dir = dirs[at].clone();
        at += 1;
        match volume.list(&dir) {
            Ok(entries) => {
                for entry in entries.into_iter().take(256) {
                    let path = join(&dir, &entry.name);
                    note(volume.lookup(&dir, &entry.name).is_ok());
                    if entry.is_dir {
                        dirs.push(path);
                    } else {
                        files.push((path, entry.len));
                    }
                }
            }
            Err(_) => note(false),
        }
    }
    let mut buf = vec![0u8; 70_000];
    for (path, len) in files.iter().take(64) {
        for offset in [0u64, *len as u64 / 2, (*len as u64).saturating_sub(10)] {
            note(volume.read(path, offset, &mut buf).is_ok());
        }
    }
    let target = dirs.get(1).cloned().unwrap_or_default();
    note(volume.create("", "fuzz new file.txt").is_ok());
    note(volume.write("fuzz new file.txt", 0, &buf[..3000]).is_ok());
    note(volume.write("fuzz new file.txt", 90_000, b"far past the end").is_ok());
    note(volume.truncate("fuzz new file.txt", 10).is_ok());
    note(volume.create("", "日本語.txt").is_ok());
    note(volume.write("日本語.txt", 0, "こんにちは".as_bytes()).is_ok());
    note(volume.rename("", "fuzz new file.txt", &target, "Fuzz Renamed 🎉.TXT").is_ok());
    note(volume.rename(&target, "Fuzz Renamed 🎉.TXT", &target, "fuzz renamed 🎉.txt").is_ok());
    note(volume.mkdir("", "fuzzdir").is_ok());
    note(volume.mkdir("fuzzdir", "inner").is_ok());
    note(volume.rename("", "fuzzdir", &target, "fuzzdir").is_ok());
    note(volume.rename("", "fuzzdir", "fuzzdir/inner", "x").is_ok());
    if let Some((path, _)) = files.first() {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        note(volume.rename("", "日本語.txt", dir, name).is_ok());
    }
    for (path, _) in files.iter().skip(1).take(8) {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        note(volume.remove(dir, name, false).is_ok());
    }
    for dir in dirs.iter().skip(1).take(8) {
        let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
        note(volume.remove(parent, name, true).is_ok());
    }
    if let Some((path, _)) = files.get(9) {
        note(volume.truncate(path, 0).is_ok());
        note(volume.write(path, 5000, b"after").is_ok());
        note(volume.fsync(path).is_ok());
    }
    for i in 0..40 {
        note(volume.create("", &format!("many long names to grow the root {}", i)).is_ok());
    }
    random_ops(&mut volume, &mut Random::new(seed ^ 0x5eed_0f_0e5), &dirs, &files, 200, &mut note);
    note(volume.stats().is_ok());
    note(volume.sync().is_ok());
    note(volume.list("").is_ok());
    tally.errors += errors;
}

/// Seeds from a file of lines, each a seed or FIRST-LAST, with `#` comments.
fn read_seeds(path: &str) -> Vec<u64> {
    let text = or_die(std::fs::read_to_string(path));
    let mut seeds = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (first.trim().parse::<u64>().unwrap_or_else(|_| die(line)), last.trim().parse::<u64>().unwrap_or_else(|_| die(line)));
                seeds.extend(first..=last);
            }
            None => seeds.push(line.parse::<u64>().unwrap_or_else(|_| die(line))),
        }
    }
    seeds
}

fn fuzz(seeds: &[u64]) {
    // The template: a 40 MiB volume holding what `fill` writes. What is
    // damaged is the volume itself; the partition table in front of it is the
    // kernel's `partition.rs`, which the QEMU cases exercise.
    let dir = std::env::temp_dir().join(format!("fatdisk-fuzz-{}", std::process::id()));
    or_die(std::fs::create_dir_all(&dir));
    let template = dir.join("template.img");
    or_die(macos::newfs_whole(&template, 40, "CLAUDEDATA", None));
    let mut volume = or_die(image::mount(Memory { data: or_die(std::fs::read(&template)) }));
    fill_volume(&mut volume);
    let clean = volume.into_device().data;
    let mut looped = clean.clone();
    damage::loops(&mut looped);
    let mut cycled = clean.clone();
    damage::cycle(&mut cycled);
    let _ = std::fs::remove_dir_all(&dir);

    let panics = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = panics.clone();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut seen) = seen.lock() {
            seen.push(format!("{}", info));
        }
    }));

    let modes = ["boot", "meta", "fat", "dirent", "random", "fat", "dirent", "meta"];
    let mut tally = Tally::default();
    let mut failed = Vec::new();
    let mut slowest = (0u128, 0u64);
    let started = Instant::now();
    for &seed in seeds {
        let mode = modes[(seed % modes.len() as u64) as usize];
        let mut random = Random::new(seed);
        let mut data = match seed % 11 {
            0 => looped.clone(),
            5 => cycled.clone(),
            _ => clean.clone(),
        };
        let count = match mode {
            "boot" => 1 + random.below(8),
            "fat" => 1 + random.below(32),
            "dirent" => 1 + random.below(64),
            "meta" => 1 + random.below(256),
            _ => 1 + random.below(4096),
        };
        damage::damage(&mut data, mode, &mut random, count);
        let before = panics.lock().map(|p| p.len()).unwrap_or(0);
        let one = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise(data, seed, &mut tally)));
        let took = one.elapsed().as_millis();
        if took > slowest.0 {
            slowest = (took, seed);
        }
        if result.is_err() {
            let message = panics.lock().ok().and_then(|p| p.get(before).cloned()).unwrap_or_default();
            println!("seed {} ({} x{}): PANIC: {}", seed, mode, count, message);
            failed.push(seed);
        }
    }
    println!(
        "fuzz: {} seeds: {} panics; {} refused by probe, {} not mountable, {} mounted with {} operations failing cleanly; slowest seed {} took {} ms; {} s in all",
        seeds.len(),
        failed.len(),
        tally.refused,
        tally.unmountable,
        tally.mounted,
        tally.errors,
        slowest.1,
        slowest.0,
        started.elapsed().as_secs()
    );
    if !failed.is_empty() {
        exit(1);
    }
}

/// Walk the volume the way `find` walks /data, through the kernel's code and
/// with paths resolved as the kernel resolves them, until a path under /data
/// would reach the kernel's 4096-byte limit. Nothing is written to the image.
fn walk(path: &str, label: &str) {
    let (mut volume, _) = open(path, label);
    let started = Instant::now();
    let (mut operations, mut failed, mut deepest) = (0u64, 0u64, 0usize);
    let mut slowest = (0u128, String::new());
    let mut time = |what: String, took: u128| {
        operations += 1;
        if took > slowest.0 {
            slowest = (took, what);
        }
    };
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        let one = Instant::now();
        let listed = if dir.is_empty() { volume.list(&dir) } else { resolve(&mut volume, &dir).and_then(|_| volume.list(&dir)) };
        time(format!("listing /{}", dir), one.elapsed().as_millis());
        let Ok(entries) = listed else {
            failed += 1;
            continue;
        };
        for entry in entries {
            let child = join(&dir, &entry.name);
            if "/data/".len() + child.len() >= 4096 {
                continue;
            }
            let one = Instant::now();
            let found = resolve(&mut volume, &child);
            time(format!("looking up /{}", child), one.elapsed().as_millis());
            match found {
                Ok(found) if found.is_dir => {
                    deepest = deepest.max(child.split('/').count());
                    pending.push(child);
                }
                Ok(_) => {}
                Err(_) => failed += 1,
            }
        }
    }
    let shown: String = slowest.1.chars().take(120).collect();
    println!(
        "walk: {} operations, {} failed, {} levels at the deepest, {} ms in all; the slowest took {} ms: {}",
        operations,
        failed,
        deepest,
        started.elapsed().as_millis(),
        slowest.0,
        shown
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or_else(|| die("missing argument; see the top of tools/fatdisk/src/main.rs"));
    let number = |i: usize| arg(i).parse::<u64>().unwrap_or_else(|_| die(&format!("{} is not a number", arg(i))));
    match arg(1).as_str() {
        "mbr" => make_mbr(&arg(2), number(3), &args[4..]),
        "whole" => make_whole(&arg(2), number(3), &arg(4)),
        "fill" => fill(&arg(2), &arg(3)),
        "put" => {
            let (mut volume, start) = open(&arg(2), &arg(3));
            let target = arg(4);
            let target = target.trim_matches('/');
            let (dir, name) = target.rsplit_once('/').unwrap_or(("", target));
            let mut walked = String::new();
            for part in dir.split('/').filter(|p| !p.is_empty()) {
                let entry = match volume.lookup(&walked, part) {
                    Ok(entry) => entry,
                    Err(_) => fs_or_die(part, volume.mkdir(&walked, part)),
                };
                walked = join(&walked, &entry.name);
            }
            write_file(&mut volume, &walked, name, arg(5).as_bytes());
            fs_or_die("sync", volume.sync());
            save(&arg(2), volume, start);
        }
        "cat" => cat(&arg(2), &arg(3), &arg(4)),
        "ls" => ls(&arg(2), &arg(3), &arg(4)),
        "check" => check(&arg(2), &arg(3)),
        "span" => {
            let (start, length) = or_die(image::find(Path::new(&arg(2)), &arg(3)));
            println!("{} {}", start, length);
        }
        "damage" => damage_image(&arg(2), &arg(3), &arg(4), number(5), args.get(6).and_then(|c| c.parse().ok()).unwrap_or(64)),
        "fuzz" if arg(2) == "--seeds" => fuzz(&read_seeds(&arg(3))),
        "fuzz" => fuzz(&(number(2)..=number(3)).collect::<Vec<u64>>()),
        "walk" => walk(&arg(2), &arg(3)),
        "cuts" => cut::report(),
        other => die(&format!("{} is not a command; see the top of tools/fatdisk/src/main.rs", other)),
    }
    let _ = (MIB, SECTOR);
}
