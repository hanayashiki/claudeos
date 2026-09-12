//! File and directory applets.

use super::{fail, split_flags};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const BLUE: &str = "\x1b[1;34m";
const GREEN: &str = "\x1b[1;32m";
const CYAN: &str = "\x1b[1;36m";
const RESET: &str = "\x1b[0m";

pub fn pwd(_args: &[String]) -> i32 {
    match std::env::current_dir() {
        Ok(dir) => {
            println!("{}", dir.display());
            0
        }
        Err(err) => fail("pwd", ".", err),
    }
}

fn mode_string(mode: u32, is_dir: bool, is_link: bool) -> String {
    let kind = if is_link {
        'l'
    } else if is_dir {
        'd'
    } else if mode & 0o170000 == 0o020000 {
        'c'
    } else {
        '-'
    };
    let bit = |shift: u32, ch: char| if mode >> shift & 1 == 1 { ch } else { '-' };
    format!(
        "{}{}{}{}{}{}{}{}{}{}",
        kind,
        bit(8, 'r'),
        bit(7, 'w'),
        bit(6, 'x'),
        bit(5, 'r'),
        bit(4, 'w'),
        bit(3, 'x'),
        bit(2, 'r'),
        bit(1, 'w'),
        bit(0, 'x'),
    )
}

pub fn ls(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let long = flags.contains('l');
    let all = flags.contains('a');
    let one_per_line = flags.contains('1') || long;

    let targets: Vec<String> =
        if operands.is_empty() { vec![".".to_string()] } else { operands };
    let mut status = 0;
    let show_headers = targets.len() > 1;

    for (index, target) in targets.iter().enumerate() {
        let metadata = match fs::symlink_metadata(target) {
            Ok(m) => m,
            Err(err) => {
                status = fail("ls", target, err);
                continue;
            }
        };

        if !metadata.is_dir() {
            print_entry(target, target, &metadata, long, one_per_line);
            if !one_per_line {
                println!();
            }
            continue;
        }

        if show_headers {
            if index > 0 {
                println!();
            }
            println!("{}:", target);
        }

        let mut names: Vec<String> = match fs::read_dir(target) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|name| all || !name.starts_with('.'))
                .collect(),
            Err(err) => {
                status = fail("ls", target, err);
                continue;
            }
        };
        names.sort();

        let mut printed = 0;
        for name in &names {
            let full = if target == "." {
                name.clone()
            } else {
                format!("{}/{}", target.trim_end_matches('/'), name)
            };
            let metadata = match fs::symlink_metadata(&full) {
                Ok(m) => m,
                Err(_) => continue,
            };
            print_entry(name, &full, &metadata, long, one_per_line);
            printed += 1;
            if !one_per_line && printed % 4 == 0 {
                println!();
            }
        }
        if !one_per_line && printed % 4 != 0 {
            println!();
        }
    }
    status
}

fn print_entry(name: &str, full: &str, metadata: &fs::Metadata, long: bool, one: bool) {
    let is_dir = metadata.is_dir();
    let is_link = metadata.file_type().is_symlink();
    let executable = metadata.mode() & 0o111 != 0 && !is_dir;

    let colour = if is_dir {
        BLUE
    } else if is_link {
        CYAN
    } else if executable {
        GREEN
    } else {
        ""
    };
    let reset = if colour.is_empty() { "" } else { RESET };

    if long {
        let mut suffix = String::new();
        if is_link {
            if let Ok(target) = fs::read_link(full) {
                suffix = format!(" -> {}", target.display());
            }
        }
        println!(
            "{} {:>3} {:>8} {}{}{}{}",
            mode_string(metadata.mode(), is_dir, is_link),
            metadata.nlink(),
            metadata.len(),
            colour,
            name,
            reset,
            suffix
        );
    } else if one {
        println!("{}{}{}", colour, name, reset);
    } else {
        print!("{}{:<18}{}", colour, name, reset);
    }
}

pub fn cat(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let number = flags.contains('n');
    let mut status = 0;
    let mut line_number = 1;

    let mut emit = |text: &str| {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if number {
            for line in text.lines() {
                let _ = writeln!(out, "{:>6}  {}", line_number, line);
                line_number += 1;
            }
        } else {
            let _ = out.write_all(text.as_bytes());
        }
    };

    if operands.is_empty() {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_ok() {
            emit(&text);
        }
        return 0;
    }
    for path in &operands {
        match fs::read(path) {
            Ok(bytes) => emit(&String::from_utf8_lossy(&bytes)),
            Err(err) => status = fail("cat", path, err),
        }
    }
    let _ = std::io::stdout().flush();
    status
}

pub fn cp(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let recursive = flags.contains('r') || flags.contains('R');
    if operands.len() < 2 {
        eprintln!("usage: cp [-r] source... destination");
        return 2;
    }
    let destination = operands.last().unwrap();
    let sources = &operands[..operands.len() - 1];
    let dest_is_dir = fs::metadata(destination).map(|m| m.is_dir()).unwrap_or(false);

    let mut status = 0;
    for source in sources {
        let target = if dest_is_dir {
            let name = Path::new(source).file_name().map(|n| n.to_string_lossy().to_string());
            match name {
                Some(name) => format!("{}/{}", destination.trim_end_matches('/'), name),
                None => destination.clone(),
            }
        } else {
            destination.clone()
        };
        if let Err(err) = copy_one(source, &target, recursive) {
            status = fail("cp", source, err);
        }
    }
    status
}

fn copy_one(source: &str, target: &str, recursive: bool) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        if !recursive {
            return Err(std::io::Error::other("is a directory"));
        }
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            copy_one(&format!("{}/{}", source, name), &format!("{}/{}", target, name), true)?;
        }
        return Ok(());
    }
    let data = fs::read(source)?;
    fs::write(target, data)
}

pub fn mv(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    if operands.len() < 2 {
        eprintln!("usage: mv source... destination");
        return 2;
    }
    let destination = operands.last().unwrap();
    let sources = &operands[..operands.len() - 1];
    let dest_is_dir = fs::metadata(destination).map(|m| m.is_dir()).unwrap_or(false);

    let mut status = 0;
    for source in sources {
        let target = if dest_is_dir {
            match Path::new(source).file_name() {
                Some(name) => {
                    format!("{}/{}", destination.trim_end_matches('/'), name.to_string_lossy())
                }
                None => destination.clone(),
            }
        } else {
            destination.clone()
        };
        if let Err(err) = fs::rename(source, &target) {
            status = fail("mv", source, err);
        }
    }
    status
}

pub fn rm(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let recursive = flags.contains('r') || flags.contains('R');
    let force = flags.contains('f');
    let mut status = 0;

    for path in &operands {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) => {
                if !force {
                    status = fail("rm", path, err);
                }
                continue;
            }
        };
        let result = if metadata.is_dir() {
            if recursive {
                fs::remove_dir_all(path)
            } else {
                Err(std::io::Error::other("is a directory"))
            }
        } else {
            fs::remove_file(path)
        };
        if let Err(err) = result {
            if !force {
                status = fail("rm", path, err);
            }
        }
    }
    status
}

pub fn mkdir(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let parents = flags.contains('p');
    let mut status = 0;
    for path in &operands {
        let result = if parents { fs::create_dir_all(path) } else { fs::create_dir(path) };
        if let Err(err) = result {
            status = fail("mkdir", path, err);
        }
    }
    status
}

pub fn rmdir(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let mut status = 0;
    for path in &operands {
        if let Err(err) = fs::remove_dir(path) {
            status = fail("rmdir", path, err);
        }
    }
    status
}

pub fn touch(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let mut status = 0;
    for path in &operands {
        if fs::metadata(path).is_ok() {
            continue;
        }
        if let Err(err) = fs::write(path, b"") {
            status = fail("touch", path, err);
        }
    }
    status
}

pub fn ln(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    if operands.len() != 2 {
        eprintln!("usage: ln -s target linkname");
        return 2;
    }
    if !flags.contains('s') {
        eprintln!("ln: only symbolic links are supported");
        return 1;
    }
    match std::os::unix::fs::symlink(&operands[0], &operands[1]) {
        Ok(()) => 0,
        Err(err) => fail("ln", &operands[1], err),
    }
}

pub fn stat(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    if operands.is_empty() {
        eprintln!("usage: stat file...");
        return 2;
    }
    let mut status = 0;
    for path in &operands {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                let kind = if metadata.is_dir() {
                    "directory"
                } else if metadata.file_type().is_symlink() {
                    "symbolic link"
                } else {
                    "regular file"
                };
                println!("  File: {}", path);
                println!("  Size: {:<12} Type: {}", metadata.len(), kind);
                println!(
                    "  Mode: {:o}/{}  Links: {}  Inode: {}",
                    metadata.mode() & 0o7777,
                    mode_string(metadata.mode(), metadata.is_dir(), false),
                    metadata.nlink(),
                    metadata.ino()
                );
                println!("   Uid: {}   Gid: {}", metadata.uid(), metadata.gid());
            }
            Err(err) => status = fail("stat", path, err),
        }
    }
    status
}

pub fn find(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let roots: Vec<String> = if operands.is_empty() { vec![".".into()] } else { operands };
    let mut status = 0;
    for root in &roots {
        if let Err(err) = walk(root, &mut |path| println!("{}", path)) {
            status = fail("find", root, err);
        }
    }
    status
}

fn walk(path: &str, visit: &mut dyn FnMut(&str)) -> std::io::Result<()> {
    visit(path);
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut names: Vec<String> = fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    for name in names {
        let child = if path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", path.trim_end_matches('/'), name)
        };
        walk(&child, visit)?;
    }
    Ok(())
}

pub fn du(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let summarise = flags.contains('s');
    let roots: Vec<String> = if operands.is_empty() { vec![".".into()] } else { operands };
    let mut status = 0;
    for root in &roots {
        let mut total = 0u64;
        let result = walk(root, &mut |path| {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.is_file() {
                    total += metadata.len();
                    if !summarise {
                        println!("{:>8}  {}", (metadata.len() + 1023) / 1024, path);
                    }
                }
            }
        });
        if let Err(err) = result {
            status = fail("du", root, err);
            continue;
        }
        println!("{:>8}  {}", (total + 1023) / 1024, root);
    }
    status
}

pub fn df(_args: &[String]) -> i32 {
    match fs::read_to_string("/proc/meminfo") {
        Ok(text) => {
            let value = |key: &str| -> u64 {
                text.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let total = value("MemTotal:");
            let free = value("MemFree:");
            println!("{:<12} {:>10} {:>10} {:>10} {:>5} {}", "Filesystem", "1K-blocks", "Used", "Available", "Use%", "Mounted on");
            let used = total.saturating_sub(free);
            let percent = if total > 0 { used * 100 / total } else { 0 };
            println!(
                "{:<12} {:>10} {:>10} {:>10} {:>4}% {}",
                "rootfs", total, used, free, percent, "/"
            );
            0
        }
        Err(err) => fail("df", "/proc/meminfo", err),
    }
}

pub fn hexdump(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let mut status = 0;
    let dump = |label: &str, bytes: &[u8]| {
        let _ = label;
        for (offset, chunk) in bytes.chunks(16).enumerate() {
            print!("{:08x}  ", offset * 16);
            for i in 0..16 {
                match chunk.get(i) {
                    Some(byte) => print!("{:02x} ", byte),
                    None => print!("   "),
                }
                if i == 7 {
                    print!(" ");
                }
            }
            print!(" |");
            for byte in chunk {
                let c = *byte;
                print!("{}", if (0x20..0x7f).contains(&c) { c as char } else { '.' });
            }
            println!("|");
        }
        println!("{:08x}", bytes.len());
    };

    if operands.is_empty() {
        let mut bytes = Vec::new();
        if std::io::stdin().read_to_end(&mut bytes).is_ok() {
            dump("-", &bytes);
        }
        return 0;
    }
    for path in &operands {
        match fs::read(path) {
            Ok(bytes) => dump(path, &bytes),
            Err(err) => status = fail("hexdump", path, err),
        }
    }
    status
}
