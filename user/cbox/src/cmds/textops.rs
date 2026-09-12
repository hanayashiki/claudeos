//! Text-processing applets.

use super::{fail, read_inputs, split_flags};
use std::io::{Read, Write};

pub fn echo(args: &[String]) -> i32 {
    let mut items = &args[1..];
    let mut newline = true;
    if !items.is_empty() && items[0] == "-n" {
        newline = false;
        items = &items[1..];
    }
    let text = items.join(" ");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(text.as_bytes());
    if newline {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
    0
}

pub fn wc(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let show_all = !flags.contains('l') && !flags.contains('w') && !flags.contains('c');
    let (inputs, mut status) = read_inputs("wc", &operands);

    let mut totals = (0usize, 0usize, 0usize);
    for (name, text) in &inputs {
        let lines = text.lines().count();
        let words = text.split_whitespace().count();
        let bytes = text.len();
        totals = (totals.0 + lines, totals.1 + words, totals.2 + bytes);
        print_counts(lines, words, bytes, &flags, show_all, if name == "-" { "" } else { name });
    }
    if inputs.len() > 1 {
        print_counts(totals.0, totals.1, totals.2, &flags, show_all, "total");
    }
    if inputs.is_empty() {
        status = 1;
    }
    status
}

fn print_counts(lines: usize, words: usize, bytes: usize, flags: &str, all: bool, name: &str) {
    let mut parts = Vec::new();
    if all || flags.contains('l') {
        parts.push(format!("{:>7}", lines));
    }
    if all || flags.contains('w') {
        parts.push(format!("{:>7}", words));
    }
    if all || flags.contains('c') {
        parts.push(format!("{:>7}", bytes));
    }
    if name.is_empty() {
        // A single requested count prints unpadded, the way a pipeline expects.
        if parts.len() == 1 {
            println!("{}", parts[0].trim());
        } else {
            println!("{}", parts.join(" "));
        }
    } else {
        println!("{} {}", parts.join(" "), name);
    }
}

fn count_argument(args: &[String], default: usize) -> (usize, Vec<String>) {
    let mut count = default;
    let mut operands = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-n" && i + 1 < args.len() {
            count = args[i + 1].parse().unwrap_or(default);
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-n") {
            count = rest.parse().unwrap_or(default);
            i += 1;
        } else if arg.len() > 1 && arg.starts_with('-') && arg[1..].chars().all(|c| c.is_ascii_digit()) {
            count = arg[1..].parse().unwrap_or(default);
            i += 1;
        } else {
            operands.push(arg.clone());
            i += 1;
        }
    }
    (count, operands)
}

pub fn head(args: &[String]) -> i32 {
    if let Some(position) = args.iter().position(|a| a == "-c" || a.starts_with("-c")) {
        let inline = args[position].strip_prefix("-c").filter(|r| !r.is_empty());
        let bytes: usize = match inline {
            Some(value) => value.parse().unwrap_or(0),
            None => args.get(position + 1).and_then(|v| v.parse().ok()).unwrap_or(0),
        };
        let skip = if inline.is_some() { 1 } else { 2 };
        let paths: Vec<String> = args[1..]
            .iter()
            .enumerate()
            .filter(|(index, _)| *index + 1 < position || *index + 1 >= position + skip)
            .map(|(_, value)| value.clone())
            .collect();
        return head_bytes(bytes, &paths);
    }
    let (count, operands) = count_argument(args, 10);
    let (inputs, status) = read_inputs("head", &operands);
    let many = inputs.len() > 1;
    for (index, (name, text)) in inputs.iter().enumerate() {
        if many {
            if index > 0 {
                println!();
            }
            println!("==> {} <==", name);
        }
        for line in text.lines().take(count) {
            println!("{}", line);
        }
    }
    status
}

fn head_bytes(count: usize, paths: &[String]) -> i32 {
    use std::io::Read;
    let mut buffer = vec![0u8; count];
    let (data, status) = if paths.is_empty() {
        let mut handle = std::io::stdin();
        let mut filled = 0;
        while filled < count {
            match handle.read(&mut buffer[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        (buffer[..filled].to_vec(), 0)
    } else {
        match std::fs::read(&paths[0]) {
            Ok(bytes) => {
                let n = bytes.len().min(count);
                (bytes[..n].to_vec(), 0)
            }
            Err(err) => (Vec::new(), fail("head", &paths[0], err)),
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&data);
    let _ = out.flush();
    status
}

pub fn tail(args: &[String]) -> i32 {
    let (count, operands) = count_argument(args, 10);
    let (inputs, status) = read_inputs("tail", &operands);
    let many = inputs.len() > 1;
    for (index, (name, text)) in inputs.iter().enumerate() {
        if many {
            if index > 0 {
                println!();
            }
            println!("==> {} <==", name);
        }
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(count);
        for line in &lines[start..] {
            println!("{}", line);
        }
    }
    status
}

pub fn grep(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    if operands.is_empty() {
        eprintln!("usage: grep [-cilnqrv] pattern [file...]");
        return 2;
    }
    let pattern = &operands[0];
    let ignore_case = flags.contains('i');
    let invert = flags.contains('v');
    let number = flags.contains('n');
    let count_only = flags.contains('c');
    let quiet = flags.contains('q');
    let names_only = flags.contains('l');
    let recursive = flags.contains('r') || flags.contains('R');

    // -r turns each directory operand into the files beneath it.
    let mut paths: Vec<String> = operands[1..].to_vec();
    if recursive {
        let mut expanded = Vec::new();
        for path in &paths {
            collect_files(path, &mut expanded);
        }
        paths = expanded;
    }
    let paths = &paths[..];

    let needle = if ignore_case { pattern.to_lowercase() } else { pattern.clone() };
    let (inputs, mut status) = read_inputs("grep", paths);
    let show_names = inputs.len() > 1 || recursive;
    let mut matched_any = false;

    for (name, text) in &inputs {
        let mut count = 0;
        for (index, line) in text.lines().enumerate() {
            let haystack = if ignore_case { line.to_lowercase() } else { line.to_string() };
            let hit = haystack.contains(&needle);
            if hit == invert {
                continue;
            }
            count += 1;
            matched_any = true;
            if count_only || quiet {
                continue;
            }
            if names_only {
                break;
            }
            let prefix = if show_names { format!("{}:", name) } else { String::new() };
            if number {
                println!("{}{}:{}", prefix, index + 1, line);
            } else {
                println!("{}{}", prefix, line);
            }
        }
        if names_only {
            if count > 0 {
                println!("{}", name);
            }
        } else if count_only {
            if show_names {
                println!("{}:{}", name, count);
            } else {
                println!("{}", count);
            }
        }
    }
    if !matched_any && status == 0 {
        status = 1;
    }
    status
}

/// Expand a path into the regular files beneath it, for grep -r.
fn collect_files(path: &str, out: &mut Vec<String>) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            let Ok(entries) = std::fs::read_dir(path) else { return };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            for name in names {
                let child = format!("{}/{}", path.trim_end_matches('/'), name);
                collect_files(&child, out);
            }
        }
        Ok(_) => out.push(path.to_string()),
        Err(_) => out.push(path.to_string()),
    }
}

pub fn sort(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let (inputs, status) = read_inputs("sort", &operands);
    let mut lines: Vec<String> = Vec::new();
    for (_, text) in &inputs {
        lines.extend(text.lines().map(|l| l.to_string()));
    }
    if flags.contains('n') {
        lines.sort_by_key(|l| l.trim().parse::<i64>().unwrap_or(i64::MIN));
    } else {
        lines.sort();
    }
    if flags.contains('r') {
        lines.reverse();
    }
    if flags.contains('u') {
        lines.dedup();
    }
    for line in lines {
        println!("{}", line);
    }
    status
}

pub fn uniq(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let (inputs, status) = read_inputs("uniq", &operands);
    let count = flags.contains('c');

    let mut previous: Option<String> = None;
    let mut repeats = 0usize;
    let emit = |line: &str, repeats: usize| {
        if count {
            println!("{:>7} {}", repeats, line);
        } else {
            println!("{}", line);
        }
    };
    for (_, text) in &inputs {
        for line in text.lines() {
            match &previous {
                Some(prev) if prev == line => repeats += 1,
                Some(prev) => {
                    emit(prev, repeats);
                    previous = Some(line.to_string());
                    repeats = 1;
                }
                None => {
                    previous = Some(line.to_string());
                    repeats = 1;
                }
            }
        }
    }
    if let Some(prev) = previous {
        emit(&prev, repeats);
    }
    status
}

pub fn cut(args: &[String]) -> i32 {
    let mut delimiter = '\t';
    let mut fields: Vec<usize> = Vec::new();
    let mut operands = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-d" && i + 1 < args.len() {
            delimiter = args[i + 1].chars().next().unwrap_or('\t');
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-d") {
            delimiter = rest.chars().next().unwrap_or('\t');
            i += 1;
        } else if arg == "-f" && i + 1 < args.len() {
            fields = parse_fields(&args[i + 1]);
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-f") {
            fields = parse_fields(rest);
            i += 1;
        } else {
            operands.push(arg.clone());
            i += 1;
        }
    }
    if fields.is_empty() {
        eprintln!("usage: cut -f LIST [-d DELIM] [file...]");
        return 2;
    }
    let (inputs, status) = read_inputs("cut", &operands);
    for (_, text) in &inputs {
        for line in text.lines() {
            let parts: Vec<&str> = line.split(delimiter).collect();
            let selected: Vec<&str> = fields
                .iter()
                .filter_map(|f| parts.get(f.saturating_sub(1)).copied())
                .collect();
            println!("{}", selected.join(&delimiter.to_string()));
        }
    }
    status
}

fn parse_fields(spec: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start.parse().unwrap_or(1);
            let end: usize = end.parse().unwrap_or(start);
            for f in start..=end {
                out.push(f);
            }
        } else if let Ok(value) = part.parse() {
            out.push(value);
        }
    }
    out
}

pub fn tr(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let delete = flags.contains('d');
    let squeeze = flags.contains('s');
    let complement = flags.contains('c') || flags.contains('C');
    if operands.is_empty() {
        eprintln!("usage: tr [-dsc] SET1 [SET2]");
        return 2;
    }
    let set1 = expand_set(&operands[0]);
    let set2 = operands.get(1).map(|s| expand_set(s)).unwrap_or_default();

    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        return 1;
    }

    let in_set1 = |c: char| set1.contains(&c) != complement;

    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if delete {
            if !in_set1(c) {
                out.push(c);
            }
            continue;
        }
        match set1.iter().position(|&s| s == c) {
            Some(index) if !complement => {
                out.push(*set2.get(index).or(set2.last()).unwrap_or(&c))
            }
            _ => out.push(c),
        }
    }

    if squeeze {
        // Runs of a character from the squeeze set collapse to one. The set is
        // SET2 when translating, SET1 otherwise.
        let squeeze_set = if set2.is_empty() { &set1 } else { &set2 };
        let mut collapsed = String::with_capacity(out.len());
        let mut previous: Option<char> = None;
        for c in out.chars() {
            let repeat = previous == Some(c) && squeeze_set.contains(&c);
            if !repeat {
                collapsed.push(c);
            }
            previous = Some(c);
        }
        out = collapsed;
    }

    print!("{}", out);
    let _ = std::io::stdout().flush();
    0
}

/// Expand `a-z` style ranges and the common character classes.
fn expand_set(spec: &str) -> Vec<char> {
    let mut out = Vec::new();
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' && spec[i..].starts_with("[:") {
            if let Some(end) = spec[i..].find(":]") {
                let class = &spec[i + 2..i + end];
                match class {
                    "alpha" => out.extend(('a'..='z').chain('A'..='Z')),
                    "lower" => out.extend('a'..='z'),
                    "upper" => out.extend('A'..='Z'),
                    "digit" => out.extend('0'..='9'),
                    "space" => out.extend([' ', '\t', '\n', '\r']),
                    _ => {}
                }
                i += end + 2;
                continue;
            }
        }
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] >= chars[i] {
            let (start, end) = (chars[i], chars[i + 2]);
            for c in start..=end {
                out.push(c);
            }
            i += 3;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn tee(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let append = flags.contains('a');
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        return 1;
    }
    print!("{}", text);
    let _ = std::io::stdout().flush();

    let mut status = 0;
    for path in &operands {
        let result = if append {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .and_then(|mut f| f.write_all(text.as_bytes()))
        } else {
            std::fs::write(path, text.as_bytes())
        };
        if let Err(err) = result {
            status = fail("tee", path, err);
        }
    }
    status
}

pub fn seq(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let numbers: Vec<i64> = operands.iter().filter_map(|a| a.parse().ok()).collect();
    let (start, step, end) = match numbers.len() {
        1 => (1, 1, numbers[0]),
        2 => (numbers[0], 1, numbers[1]),
        3 => (numbers[0], numbers[1], numbers[2]),
        _ => {
            eprintln!("usage: seq [first [increment]] last");
            return 2;
        }
    };
    if step == 0 {
        eprintln!("seq: increment must not be zero");
        return 1;
    }
    let mut value = start;
    while (step > 0 && value <= end) || (step < 0 && value >= end) {
        println!("{}", value);
        value += step;
    }
    0
}

pub fn rev(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let (inputs, status) = read_inputs("rev", &operands);
    for (_, text) in &inputs {
        for line in text.lines() {
            println!("{}", line.chars().rev().collect::<String>());
        }
    }
    status
}
