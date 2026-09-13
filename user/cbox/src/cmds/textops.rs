//! Text-processing applets.
//!
//! What these move is bytes. A tool that counts lines or takes the first few
//! of them has no say in whether its input was text, so an encoding that does
//! not decode is carried through rather than refused or replaced.

use super::{fail, lines, read_inputs, split_flags, without_newline};
use std::io::{Read, Write};

/// The bytes C calls whitespace, which is what separates one word from the
/// next and what `tr [:space:]` names.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

pub fn echo(args: &[String]) -> i32 {
    let mut items = &args[1..];
    let mut newline = true;
    let mut escapes = false;
    while let Some(first) = items.first() {
        match first.as_str() {
            "-n" => newline = false,
            "-e" => escapes = true,
            "-E" => escapes = false,
            _ => break,
        }
        items = &items[1..];
    }
    let joined = items.join(" ");
    let text = if escapes { unescape(&joined) } else { joined };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // A write that fails has to be reported, or `cmd > file || handler` can
    // never fire.
    if out.write_all(text.as_bytes()).is_err() {
        eprintln!("echo: write error");
        return 1;
    }
    if newline && out.write_all(b"\n").is_err() {
        eprintln!("echo: write error");
        return 1;
    }
    if out.flush().is_err() {
        eprintln!("echo: write error");
        return 1;
    }
    0
}

pub fn wc(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let show_all = !flags.contains('l')
        && !flags.contains('w')
        && !flags.contains('c')
        && !flags.contains('L');
    let (inputs, mut status) = read_inputs("wc", &operands);

    let mut totals = (0usize, 0usize, 0usize, 0usize);
    for (name, data) in &inputs {
        // A line is a newline, not a decoded one: a file whose last line has
        // no newline after it has as many lines as it has newlines, which is
        // what every other wc reports.
        let line_count = data.iter().filter(|byte| **byte == b'\n').count();
        let words = data.split(|byte| is_space(*byte)).filter(|word| !word.is_empty()).count();
        let bytes = data.len();
        // The longest line, in bytes. How wide it would be on a terminal is a
        // different question, and one a file of arbitrary bytes has no answer
        // to.
        let longest =
            lines(data).iter().map(|line| without_newline(line).len()).max().unwrap_or(0);
        totals = (
            totals.0 + line_count,
            totals.1 + words,
            totals.2 + bytes,
            totals.3.max(longest),
        );
        print_counts(
            line_count,
            words,
            bytes,
            longest,
            &flags,
            show_all,
            if name == "-" { "" } else { name },
        );
    }
    if inputs.len() > 1 {
        print_counts(totals.0, totals.1, totals.2, totals.3, &flags, show_all, "total");
    }
    if inputs.is_empty() {
        status = 1;
    }
    status
}

fn print_counts(
    lines: usize,
    words: usize,
    bytes: usize,
    longest: usize,
    flags: &str,
    all: bool,
    name: &str,
) {
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
    if flags.contains('L') {
        parts.push(format!("{:>7}", longest));
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

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Read only as far as needed. Reading to the end first would never return
    // on an endless producer such as `yes`.
    if operands.is_empty() {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buffer = [0u8; 65536];
        let mut taken = 0;
        while taken < count {
            let filled = match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut upto = filled;
            for (index, byte) in buffer[..filled].iter().enumerate() {
                if *byte == b'\n' {
                    taken += 1;
                    if taken >= count {
                        upto = index + 1;
                        break;
                    }
                }
            }
            if out.write_all(&buffer[..upto]).is_err() {
                return 1;
            }
        }
        let _ = out.flush();
        return 0;
    }

    let (inputs, status) = read_inputs("head", &operands);
    let many = inputs.len() > 1;
    for (index, (name, data)) in inputs.iter().enumerate() {
        if many {
            if index > 0 {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "==> {} <==", name);
        }
        for line in lines(data).iter().take(count) {
            let _ = out.write_all(line);
        }
    }
    let _ = out.flush();
    status
}

fn head_bytes(count: usize, paths: &[String]) -> i32 {
    use std::io::Read;

    // Read exactly `count` bytes. Reading the whole file first would never
    // finish on a character device such as /dev/zero.
    fn take_bytes(reader: &mut dyn Read, count: usize) -> std::io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; count];
        let mut filled = 0;
        while filled < count {
            match reader.read(&mut buffer[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        buffer.truncate(filled);
        Ok(buffer)
    }

    let (data, status) = if paths.is_empty() {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        match take_bytes(&mut handle, count) {
            Ok(bytes) => (bytes, 0),
            Err(err) => (Vec::new(), fail("head", "-", err)),
        }
    } else {
        match std::fs::File::open(&paths[0]) {
            Ok(mut file) => match take_bytes(&mut file, count) {
                Ok(bytes) => (bytes, 0),
                Err(err) => (Vec::new(), fail("head", &paths[0], err)),
            },
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
    // -f keeps reading; take it out before the operands are worked out, or it
    // looks like a file name.
    let follow = args.iter().any(|a| a == "-f" || a == "-F");
    let kept: Vec<String> =
        args.iter().filter(|a| *a != "-f" && *a != "-F").cloned().collect();
    let (count, operands) = count_argument(&kept, 10);
    let (inputs, status) = read_inputs("tail", &operands);
    let many = inputs.len() > 1;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (index, (name, data)) in inputs.iter().enumerate() {
        if many {
            if index > 0 {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "==> {} <==", name);
        }
        let all = lines(data);
        let start = all.len().saturating_sub(count);
        for line in &all[start..] {
            let _ = out.write_all(line);
        }
    }
    let _ = out.flush();
    if follow {
        return follow_files(&operands);
    }
    status
}

/// `tail -f`: print what gets appended to each file, until interrupted.
fn follow_files(paths: &[String]) -> i32 {
    use std::io::Write;
    if paths.is_empty() {
        // Nothing to watch: standard input has already been read to the end.
        return 0;
    }
    let mut sizes: Vec<u64> = paths
        .iter()
        .map(|path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect();
    loop {
        let mut moved = false;
        for (index, path) in paths.iter().enumerate() {
            let size = match std::fs::metadata(path) {
                Ok(metadata) => metadata.len(),
                Err(_) => continue,
            };
            if size < sizes[index] {
                // Truncated: start again from the beginning.
                sizes[index] = 0;
            }
            if size == sizes[index] {
                continue;
            }
            if let Ok(text) = std::fs::read(path) {
                let from = sizes[index] as usize;
                if from < text.len() {
                    let _ = std::io::stdout().write_all(&text[from..]);
                    let _ = std::io::stdout().flush();
                }
            }
            sizes[index] = size;
            moved = true;
        }
        if !moved {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

pub fn grep(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    if operands.is_empty() {
        eprintln!("usage: grep [-cEFilnoqrvw] pattern [file...]");
        return 2;
    }
    let pattern = &operands[0];
    let ignore_case = flags.contains('i');
    let invert = flags.contains('v');
    let number = flags.contains('n');
    let count_only = flags.contains('c');
    let quiet = flags.contains('q');
    let names_only = flags.contains('l');
    let only_matching = flags.contains('o');
    let recursive = flags.contains('r') || flags.contains('R');

    // -r turns each directory operand into the files beneath it. Given none,
    // it walks the working directory: a recursive search of standard input is
    // not a thing that can be asked for, and waiting for a line to be typed
    // looks the same as having hung.
    let mut paths: Vec<String> = operands[1..].to_vec();
    if recursive {
        if paths.is_empty() {
            paths.push(".".to_string());
        }
        let mut expanded = Vec::new();
        for path in &paths {
            collect_files(path, &mut expanded);
        }
        paths = expanded;
    }
    let paths = &paths[..];

    // -i folds only the ASCII letters, on both sides. A byte outside them has
    // no case to fold, and folding it as though it were a character would move
    // it to some other byte.
    let source = if ignore_case { pattern.to_ascii_lowercase() } else { pattern.clone() };
    let matcher = if flags.contains('F') {
        crate::regex::Regex::literal(&source)
    } else {
        crate::regex::Regex::new(&source, flags.contains('E'))
    };
    let (inputs, mut status) = read_inputs("grep", paths);
    let show_names = inputs.len() > 1 || recursive;
    let mut matched_any = false;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (name, data) in &inputs {
        let mut count = 0;
        for (index, raw) in lines(data).iter().enumerate() {
            let line = without_newline(raw);
            let folded;
            let haystack: &[u8] = if ignore_case {
                folded = line.to_ascii_lowercase();
                &folded
            } else {
                line
            };
            let hit = matcher.is_match(haystack);
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
            let mut label = if show_names { format!("{}:", name) } else { String::new() };
            if number {
                label.push_str(&format!("{}:", index + 1));
            }
            // -o prints what matched rather than the line it was found on,
            // once per match.
            if only_matching {
                let mut at = 0usize;
                while let Some((start, end)) = matcher.find(haystack, at) {
                    if end > start {
                        let _ = out.write_all(label.as_bytes());
                        let _ = out.write_all(&line[start..end]);
                        let _ = out.write_all(b"\n");
                    }
                    at = if end > start { end } else { start + 1 };
                    if at > haystack.len() {
                        break;
                    }
                }
                continue;
            }
            let _ = out.write_all(label.as_bytes());
            let _ = out.write_all(line);
            let _ = out.write_all(b"\n");
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
    let _ = out.flush();
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
    let mut collected: Vec<Vec<u8>> = Vec::new();
    for (_, data) in &inputs {
        collected.extend(lines(data).iter().map(|line| without_newline(line).to_vec()));
    }
    if flags.contains('n') {
        // Compare the number a line starts with, ignoring whatever follows.
        collected.sort_by(|a, b| {
            leading_number(a)
                .partial_cmp(&leading_number(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        // Byte order, which is the order sort uses where no locale says
        // otherwise, and the only one a line of arbitrary bytes has.
        collected.sort();
    }
    if flags.contains('r') {
        collected.reverse();
    }
    if flags.contains('u') {
        collected.dedup();
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in &collected {
        let _ = out.write_all(line);
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
    status
}

/// The number a line begins with, for `sort -n`. Lines without one sort first.
fn leading_number(line: &[u8]) -> f64 {
    let text = match line.iter().position(|byte| !is_space(*byte)) {
        Some(at) => &line[at..],
        None => return f64::NEG_INFINITY,
    };
    let mut end = 0;
    if end < text.len() && (text[end] == b'-' || text[end] == b'+') {
        end += 1;
    }
    while end < text.len() && text[end].is_ascii_digit() {
        end += 1;
    }
    if end < text.len() && text[end] == b'.' {
        end += 1;
        while end < text.len() && text[end].is_ascii_digit() {
            end += 1;
        }
    }
    // Digits and a sign, so what was taken is always readable as text.
    std::str::from_utf8(&text[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(f64::NEG_INFINITY)
}

pub fn uniq(args: &[String]) -> i32 {
    let (flags, operands) = split_flags(args);
    let (inputs, status) = read_inputs("uniq", &operands);
    let count = flags.contains('c');
    let only_repeated = flags.contains('d');
    let only_unique = flags.contains('u');

    let mut previous: Option<Vec<u8>> = None;
    let mut repeats = 0usize;
    let emit = |out: &mut dyn Write, line: &[u8], repeats: usize| {
        if (only_repeated && repeats < 2) || (only_unique && repeats > 1) {
            return;
        }
        if count {
            let _ = write!(out, "{:>7} ", repeats);
        }
        let _ = out.write_all(line);
        let _ = out.write_all(b"\n");
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (_, data) in &inputs {
        for raw in lines(data) {
            let line = without_newline(raw);
            match &previous {
                Some(prev) if prev == line => repeats += 1,
                Some(prev) => {
                    emit(&mut out, prev, repeats);
                    previous = Some(line.to_vec());
                    repeats = 1;
                }
                None => {
                    previous = Some(line.to_vec());
                    repeats = 1;
                }
            }
        }
    }
    if let Some(prev) = previous {
        emit(&mut out, &prev, repeats);
    }
    let _ = out.flush();
    status
}

pub fn cut(args: &[String]) -> i32 {
    let mut delimiter = b'\t';
    let mut fields: Vec<usize> = Vec::new();
    let mut characters: Vec<usize> = Vec::new();
    let mut operands = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-d" && i + 1 < args.len() {
            delimiter = args[i + 1].as_bytes().first().copied().unwrap_or(b'\t');
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-d") {
            delimiter = rest.as_bytes().first().copied().unwrap_or(b'\t');
            i += 1;
        } else if arg == "-f" && i + 1 < args.len() {
            fields = parse_fields(&args[i + 1]);
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-f") {
            fields = parse_fields(rest);
            i += 1;
        } else if arg == "-c" && i + 1 < args.len() {
            characters = parse_fields(&args[i + 1]);
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("-c") {
            characters = parse_fields(rest);
            i += 1;
        } else {
            operands.push(arg.clone());
            i += 1;
        }
    }
    if fields.is_empty() && characters.is_empty() {
        eprintln!("usage: cut -f LIST [-d DELIM] | cut -c LIST  [file...]");
        return 2;
    }
    let (inputs, status) = read_inputs("cut", &operands);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (_, data) in &inputs {
        for raw in lines(data) {
            let line = without_newline(raw);
            let mut picked: Vec<u8> = Vec::new();
            if !characters.is_empty() {
                // -c selects by position in the line, which for a line that
                // is not text is a byte.
                picked.extend(
                    characters.iter().filter_map(|c| line.get(c.saturating_sub(1)).copied()),
                );
            } else {
                let parts: Vec<&[u8]> = line.split(|byte| *byte == delimiter).collect();
                for field in fields.iter().filter_map(|f| parts.get(f.saturating_sub(1))) {
                    if !picked.is_empty() {
                        picked.push(delimiter);
                    }
                    picked.extend_from_slice(field);
                }
            }
            let _ = out.write_all(&picked);
            let _ = out.write_all(b"\n");
        }
    }
    let _ = out.flush();
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

/// Turn backslash escapes into the characters they stand for.
fn unescape(spec: &str) -> String {
    let chars: Vec<char> = spec.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        i += 2;
        match next {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'f' => out.push('\u{0c}'),
            'v' => out.push('\u{0b}'),
            'a' => out.push('\u{07}'),
            'b' => out.push('\u{08}'),
            '\\' => out.push('\\'),
            '0'..='7' => {
                // An octal escape of up to three digits.
                let mut value = next.to_digit(8).unwrap();
                let mut taken = 1;
                while taken < 3 && i < chars.len() && chars[i].is_digit(8) {
                    value = value * 8 + chars[i].to_digit(8).unwrap();
                    i += 1;
                    taken += 1;
                }
                out.push(char::from_u32(value).unwrap_or('\0'));
            }
            other => out.push(other),
        }
    }
    out
}

/// Expand `a-z` style ranges and the common character classes.
fn expand_set(spec: &str) -> Vec<char> {
    let spec = &unescape(spec);
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
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (_, data) in &inputs {
        for raw in lines(data) {
            // Reversed byte by byte. Which bytes make up one character is a
            // question about an encoding the line need not be written in.
            let line = without_newline(raw);
            let mut reversed = line.to_vec();
            reversed.reverse();
            let _ = out.write_all(&reversed);
            if raw.len() != line.len() {
                let _ = out.write_all(b"\n");
            }
        }
    }
    let _ = out.flush();
    status
}
