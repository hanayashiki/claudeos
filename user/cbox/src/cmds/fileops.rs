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

/// Width of the terminal, or None when output is not going to one.
fn terminal_width() -> Option<usize> {
    // struct winsize { u16 row; u16 col; u16 xpixel; u16 ypixel; }
    let mut size = [0u16; 4];
    let ok = crate::sys::ioctl_ptr(
        crate::sys::STDOUT,
        crate::sys::TIOCGWINSZ,
        size.as_mut_ptr() as u64,
    );
    if ok < 0 {
        return None;
    }
    Some(if size[1] >= 20 { size[1] as usize } else { 80 })
}

struct Entry {
    name: String,
    path: String,
    metadata: fs::Metadata,
}

pub fn ls(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let long = flags.contains('l');
    let all = flags.contains('a');
    // Columns are for a person reading a terminal; a pipe gets one per line,
    // which is what every caller that parses the output expects.
    let one_per_line = flags.contains('1') || terminal_width().is_none();
    let classify = flags.contains('F');
    let reverse = flags.contains('r');
    let by_time = flags.contains('t');
    let by_size = flags.contains('S');
    let human = flags.contains('h');
    let recurse = flags.contains('R');
    let directory_itself = flags.contains('d');

    let targets: Vec<String> =
        if operands.is_empty() { vec![".".to_string()] } else { operands };
    let mut status = 0;

    // Operands that are not directories are listed together, first.
    let mut files = Vec::new();
    let mut directories = Vec::new();
    for target in &targets {
        match fs::symlink_metadata(target) {
            // -d names the directory itself rather than its contents.
            Ok(metadata) if metadata.is_dir() && !directory_itself => {
                directories.push(target.clone())
            }
            Ok(metadata) => files.push(Entry {
                name: target.clone(),
                path: target.clone(),
                metadata,
            }),
            Err(err) => status = fail("ls", target, err),
        }
    }

    let options = ListOptions {
        colour: terminal_width().is_some(),
        long,
        one_per_line,
        classify,
        human,
        reverse,
        by_time,
        by_size,
    };

    if !files.is_empty() {
        sort_entries(&mut files, &options);
        // Named files get no "total" line; that belongs to a directory listing.
        print_entries(&files, &options, false);
    }

    let show_headers = directories.len() > 1 || !files.is_empty() || recurse;
    let mut first = files.is_empty();
    for directory in &directories {
        if list_directory(directory, all, recurse, &options, show_headers, &mut first).is_err() {
            status = 1;
        }
    }
    status
}

struct ListOptions {
    /// Colour is for a person at a terminal; a pipe gets plain text.
    colour: bool,
    long: bool,
    one_per_line: bool,
    classify: bool,
    human: bool,
    reverse: bool,
    by_time: bool,
    by_size: bool,
}

fn list_directory(
    path: &str,
    all: bool,
    recurse: bool,
    options: &ListOptions,
    show_headers: bool,
    first: &mut bool,
) -> Result<(), ()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) => {
            fail("ls", path, err);
            return Err(());
        }
    };

    if show_headers {
        if !*first {
            println!();
        }
        println!("{}:", path);
    }
    *first = false;

    let mut items: Vec<Entry> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !all && name.starts_with('.') {
            continue;
        }
        let full = join(path, &name);
        if let Ok(metadata) = fs::symlink_metadata(&full) {
            items.push(Entry { name, path: full, metadata });
        }
    }
    sort_entries(&mut items, options);
    print_entries(&items, options, true);

    if recurse {
        let subdirectories: Vec<String> = items
            .iter()
            .filter(|e| e.metadata.is_dir())
            .map(|e| e.path.clone())
            .collect();
        for subdirectory in subdirectories {
            let _ = list_directory(&subdirectory, all, true, options, true, first);
        }
    }
    Ok(())
}

fn join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", directory.trim_end_matches('/'), name)
    }
}

fn sort_entries(items: &mut [Entry], options: &ListOptions) {
    if options.by_time {
        items.sort_by(|a, b| b.metadata.mtime().cmp(&a.metadata.mtime()));
    } else if options.by_size {
        items.sort_by(|a, b| b.metadata.len().cmp(&a.metadata.len()));
    } else {
        items.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if options.reverse {
        items.reverse();
    }
}

fn suffix_for(metadata: &fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        "/"
    } else if metadata.file_type().is_symlink() {
        "@"
    } else if metadata.mode() & 0o111 != 0 {
        "*"
    } else {
        ""
    }
}

fn colour_for(metadata: &fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        BLUE
    } else if metadata.file_type().is_symlink() {
        CYAN
    } else if metadata.mode() & 0o111 != 0 {
        GREEN
    } else {
        ""
    }
}

/// Owner and group names; only root exists on this system.
fn owner_name(id: u32) -> String {
    if id == 0 {
        "root".to_string()
    } else {
        id.to_string()
    }
}

/// "Mon DD HH:MM", the way ls shows a recent timestamp.
fn timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86400);
    let rest = seconds.rem_euclid(86400);
    let (_, month, day) = super::sysinfo::civil_from_days(days);
    format!(
        "{} {:>2} {:02}:{:02}",
        super::sysinfo::MONTHS[(month - 1) as usize],
        day,
        rest / 3600,
        (rest % 3600) / 60
    )
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}", bytes)
    } else if value < 10.0 {
        format!("{:.1}{}", value, UNITS[unit])
    } else {
        format!("{:.0}{}", value, UNITS[unit])
    }
}

fn print_entries(items: &[Entry], options: &ListOptions, show_total: bool) {
    if items.is_empty() {
        return;
    }

    if options.long {
        if show_total {
            let total: u64 = items.iter().map(|i| (i.metadata.len() + 1023) / 1024).sum();
            println!("total {}", total);
        }

        let widest = items
            .iter()
            .map(|i| {
                if options.human {
                    human_size(i.metadata.len()).len()
                } else {
                    i.metadata.len().to_string().len()
                }
            })
            .max()
            .unwrap_or(1);

        for item in items {
            let is_link = item.metadata.file_type().is_symlink();
            let size = if options.human {
                human_size(item.metadata.len())
            } else {
                item.metadata.len().to_string()
            };
            let mut trailer = String::new();
            if is_link {
                if let Ok(target) = fs::read_link(&item.path) {
                    trailer = format!(" -> {}", target.display());
                }
            }
            let colour = if options.colour { colour_for(&item.metadata) } else { "" };
            let reset = if colour.is_empty() { "" } else { RESET };
            println!(
                "{} {:>3} {:<8} {:<8} {:>width$} {} {}{}{}{}{}",
                mode_string(item.metadata.mode(), item.metadata.is_dir(), is_link),
                item.metadata.nlink(),
                owner_name(item.metadata.uid()),
                owner_name(item.metadata.gid()),
                size,
                timestamp(item.metadata.mtime()),
                colour,
                item.name,
                reset,
                if options.classify { suffix_for(&item.metadata) } else { "" },
                trailer,
                width = widest,
            );
        }
        return;
    }

    // Decorated names, and the display width of each without escape codes.
    let cells: Vec<(String, usize)> = items
        .iter()
        .map(|item| {
            let suffix = if options.classify { suffix_for(&item.metadata) } else { "" };
            let colour = if options.colour { colour_for(&item.metadata) } else { "" };
            let reset = if colour.is_empty() { "" } else { RESET };
            let text = format!("{}{}{}{}", colour, item.name, reset, suffix);
            (text, item.name.chars().count() + suffix.len())
        })
        .collect();

    if options.one_per_line {
        for (text, _) in &cells {
            println!("{}", text);
        }
        return;
    }

    // Lay the names out in as many columns as the terminal will take, filling
    // down each column the way ls does.
    let width = terminal_width().unwrap_or(80);
    let widest = cells.iter().map(|(_, w)| *w).max().unwrap_or(1);
    let column_width = widest + 2;
    let columns = ((width / column_width).max(1)).min(cells.len());
    let rows = (cells.len() + columns - 1) / columns;

    for row in 0..rows {
        let mut line = String::new();
        for column in 0..columns {
            let index = column * rows + row;
            if index >= cells.len() {
                continue;
            }
            let (text, display) = &cells[index];
            line.push_str(text);
            let last = column == columns - 1 || index + rows >= cells.len();
            if !last {
                for _ in *display..column_width {
                    line.push(' ');
                }
            }
        }
        println!("{}", line.trim_end());
    }
}

pub fn cat(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let number = flags.contains('n');
    let show_ends = flags.contains('e') || flags.contains('E') || flags.contains('A');
    let mut status = 0;
    let mut line_number = 1;

    let mut emit = |text: &str| -> std::io::Result<()> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if number || show_ends {
            for line in text.lines() {
                if number {
                    write!(out, "{:>6}  ", line_number)?;
                    line_number += 1;
                }
                out.write_all(line.as_bytes())?;
                if show_ends {
                    out.write_all(b"$")?;
                }
                out.write_all(b"\n")?;
            }
        } else {
            out.write_all(text.as_bytes())?;
        }
        Ok(())
    };

    if operands.is_empty() {
        // Copy standard input through in chunks, so a character device that
        // never ends still streams.
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buffer = [0u8; 65536];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if emit(&String::from_utf8_lossy(&buffer[..n])).is_err() {
                        eprintln!("cat: write error");
                        return 1;
                    }
                }
                Err(err) => {
                    return fail("cat", "-", err);
                }
            }
        }
        let _ = std::io::stdout().flush();
        return 0;
    }
    for path in &operands {
        match fs::read(path) {
            Ok(bytes) => {
                if emit(&String::from_utf8_lossy(&bytes)).is_err() {
                    eprintln!("cat: write error");
                    return 1;
                }
            }
            Err(err) => status = fail("cat", path, err),
        }
    }
    if std::io::stdout().flush().is_err() {
        eprintln!("cat: write error");
        return 1;
    }
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
        eprintln!("usage: ln [-sf] target linkname");
        return 2;
    }
    if !flags.contains('s') {
        eprintln!("ln: only symbolic links are supported");
        return 1;
    }
    if flags.contains('f') {
        // -f replaces an existing link or file.
        let _ = fs::remove_file(&operands[1]);
    }
    match std::os::unix::fs::symlink(&operands[0], &operands[1]) {
        Ok(()) => 0,
        Err(err) => fail("ln", &operands[1], err),
    }
}

pub fn readlink(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    if operands.is_empty() {
        eprintln!("usage: readlink [-f] file...");
        return 2;
    }
    let mut status = 0;
    for path in &operands {
        if flags.contains('f') {
            // -f resolves the whole chain.
            match fs::canonicalize(path) {
                Ok(resolved) => println!("{}", resolved.display()),
                Err(err) => status = fail("readlink", path, err),
            }
            continue;
        }
        match fs::read_link(path) {
            Ok(target) => println!("{}", target.display()),
            Err(err) => status = fail("readlink", path, err),
        }
    }
    status
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

/// What `find` should do with each entry it matches.
enum Action {
    Print,
    Exec(Vec<String>),
}

struct FindSpec {
    name: Option<String>,
    iname: Option<String>,
    kind: Option<char>,
    min_depth: usize,
    max_depth: usize,
    action: Action,
}

pub fn find(args: &[String]) -> i32 {
    let mut roots: Vec<String> = Vec::new();
    let mut spec = FindSpec {
        name: None,
        iname: None,
        kind: None,
        min_depth: 0,
        max_depth: usize::MAX,
        action: Action::Print,
    };

    let mut index = 1;
    // Everything before the first predicate is a path.
    while index < args.len() && !args[index].starts_with('-') {
        roots.push(args[index].clone());
        index += 1;
    }
    while index < args.len() {
        let take = |index: usize| args.get(index + 1).cloned();
        match args[index].as_str() {
            "-name" => {
                spec.name = take(index);
                index += 2;
            }
            "-iname" => {
                spec.iname = take(index).map(|p| p.to_lowercase());
                index += 2;
            }
            "-type" => {
                spec.kind = take(index).and_then(|t| t.chars().next());
                index += 2;
            }
            "-maxdepth" => {
                spec.max_depth = take(index).and_then(|d| d.parse().ok()).unwrap_or(usize::MAX);
                index += 2;
            }
            "-mindepth" => {
                spec.min_depth = take(index).and_then(|d| d.parse().ok()).unwrap_or(0);
                index += 2;
            }
            "-print" => {
                spec.action = Action::Print;
                index += 1;
            }
            "-exec" => {
                let mut command = Vec::new();
                index += 1;
                while index < args.len() && args[index] != ";" && args[index] != "\\;" {
                    command.push(args[index].clone());
                    index += 1;
                }
                index += 1; // the terminating ';'
                spec.action = Action::Exec(command);
            }
            "-a" | "-and" => index += 1,
            other => {
                eprintln!("find: unknown predicate: {}", other);
                return 2;
            }
        }
    }

    if roots.is_empty() {
        roots.push(".".to_string());
    }

    let mut status = 0;
    for root in &roots {
        if fs::symlink_metadata(root).is_err() {
            status = fail("find", root, std::io::Error::from(std::io::ErrorKind::NotFound));
            continue;
        }
        if visit_tree(root, 0, &spec).is_err() {
            status = 1;
        }
    }
    status
}

fn visit_tree(path: &str, depth: usize, spec: &FindSpec) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;

    if depth >= spec.min_depth && matches_spec(path, &metadata, spec) {
        match &spec.action {
            Action::Print => println!("{}", path),
            Action::Exec(command) if !command.is_empty() => {
                let argv: Vec<String> = command
                    .iter()
                    .map(|word| if word == "{}" { path.to_string() } else { word.clone() })
                    .collect();
                let _ = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
            }
            Action::Exec(_) => {}
        }
    }

    if !metadata.is_dir() || depth >= spec.max_depth {
        return Ok(());
    }
    let mut names: Vec<String> = fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    for name in names {
        let child = join(path, &name);
        let _ = visit_tree(&child, depth + 1, spec);
    }
    Ok(())
}

fn matches_spec(path: &str, metadata: &fs::Metadata, spec: &FindSpec) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if let Some(pattern) = &spec.name {
        if !crate::shell_glob(base, pattern) {
            return false;
        }
    }
    if let Some(pattern) = &spec.iname {
        if !crate::shell_glob(&base.to_lowercase(), pattern) {
            return false;
        }
    }
    if let Some(kind) = spec.kind {
        let actual = if metadata.is_dir() {
            'd'
        } else if metadata.file_type().is_symlink() {
            'l'
        } else {
            'f'
        };
        if actual != kind {
            return false;
        }
    }
    true
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
    let human = flags.contains('h');
    let roots: Vec<String> = if operands.is_empty() { vec![".".into()] } else { operands };
    let mut status = 0;
    for root in &roots {
        let mut total = 0u64;
        // A plain file gets one line, not a line and a summary.
        let is_file = fs::symlink_metadata(root).map(|m| !m.is_dir()).unwrap_or(false);
        let result = walk(root, &mut |path| {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.is_file() {
                    total += metadata.len();
                    if !summarise {
                        let size = if human {
                            human_size(metadata.len())
                        } else {
                            ((metadata.len() + 1023) / 1024).to_string()
                        };
                        println!("{:>8}  {}", size, path);
                    }
                }
            }
        });
        if let Err(err) = result {
            status = fail("du", root, err);
            continue;
        }
        if is_file && !summarise {
            continue;
        }
        let size = if human { human_size(total) } else { ((total + 1023) / 1024).to_string() };
        println!("{:>8}  {}", size, root);
    }
    status
}

pub fn df(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let human = flags.contains('h');
    let path = operands.first().map(|s| s.as_str()).unwrap_or("/");

    let Some(stats) = crate::sys::statfs(path) else {
        eprintln!("df: {}: cannot read filesystem statistics", path);
        return 1;
    };
    let kb = stats.block_size / 1024;
    let total = stats.blocks * kb;
    let free = stats.free * kb;
    let used = total.saturating_sub(free);
    let percent = if total > 0 { used * 100 / total } else { 0 };

    let show = |value: u64| -> String {
        if human {
            human_size(value * 1024)
        } else {
            value.to_string()
        }
    };

    println!(
        "{:<12} {:>10} {:>10} {:>10} {:>5} {}",
        "Filesystem", "1K-blocks", "Used", "Available", "Use%", "Mounted on"
    );
    println!(
        "{:<12} {:>10} {:>10} {:>10} {:>4}% {}",
        "rootfs",
        show(total),
        show(used),
        show(free),
        percent,
        "/"
    );
    0
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
