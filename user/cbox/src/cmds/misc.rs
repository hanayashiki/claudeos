//! Small applets.

use super::split_flags;

pub fn yes(args: &[String]) -> i32 {
    use std::io::Write;
    let mut line = if args.len() > 1 { args[1..].join(" ") } else { "y".to_string() };
    line.push('\n');

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        // Stop when the reader has gone, rather than spinning.
        if out.write_all(line.as_bytes()).is_err() {
            return 1;
        }
    }
}

pub fn basename(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let Some(path) = operands.first() else {
        eprintln!("usage: basename path [suffix]");
        return 2;
    };
    let trimmed = path.trim_end_matches('/');
    let mut name = trimmed.rsplit('/').next().unwrap_or(trimmed).to_string();
    if let Some(suffix) = operands.get(1) {
        if name.ends_with(suffix.as_str()) && name.len() > suffix.len() {
            name.truncate(name.len() - suffix.len());
        }
    }
    println!("{}", if name.is_empty() { "/" } else { &name });
    0
}

pub fn dirname(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let Some(path) = operands.first() else {
        eprintln!("usage: dirname path");
        return 2;
    };
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => println!("/"),
        Some(index) => println!("{}", &trimmed[..index]),
        None => println!("."),
    }
    0
}

/// `test` / `[`: evaluate a condition, including -a, -o and !.
pub fn test(args: &[String]) -> i32 {
    let mut operands: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
    if args[0] == "[" {
        if operands.last() != Some(&"]") {
            eprintln!("[: missing ]");
            return 2;
        }
        operands.pop();
    }
    if evaluate(&operands) {
        0
    } else {
        1
    }
}

/// -o binds loosest, then -a, then a leading !, then the primaries.
fn evaluate(terms: &[&str]) -> bool {
    if terms.is_empty() {
        return false;
    }
    if let Some(index) = terms.iter().position(|t| *t == "-o") {
        return evaluate(&terms[..index]) || evaluate(&terms[index + 1..]);
    }
    if let Some(index) = terms.iter().position(|t| *t == "-a") {
        return evaluate(&terms[..index]) && evaluate(&terms[index + 1..]);
    }
    if terms[0] == "!" {
        return !evaluate(&terms[1..]);
    }
    primary(terms)
}

fn primary(terms: &[&str]) -> bool {
    match terms.len() {
        0 => false,
        1 => !terms[0].is_empty(),
        2 => {
            let value = terms[1];
            match terms[0] {
                "-n" => !value.is_empty(),
                "-z" => value.is_empty(),
                "-e" => std::fs::symlink_metadata(value).is_ok(),
                "-f" => std::fs::metadata(value).map(|m| m.is_file()).unwrap_or(false),
                "-d" => std::fs::metadata(value).map(|m| m.is_dir()).unwrap_or(false),
                "-L" | "-h" => std::fs::symlink_metadata(value)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "-s" => std::fs::metadata(value).map(|m| m.len() > 0).unwrap_or(false),
                "-r" | "-w" => std::fs::metadata(value).is_ok(),
                "-p" | "-c" | "-b" | "-S" => {
                    use std::os::unix::fs::FileTypeExt;
                    std::fs::metadata(value)
                        .map(|m| {
                            let kind = m.file_type();
                            match terms[0] {
                                "-p" => kind.is_fifo(),
                                "-c" => kind.is_char_device(),
                                "-b" => kind.is_block_device(),
                                _ => kind.is_socket(),
                            }
                        })
                        .unwrap_or(false)
                }
                "-x" => {
                    use std::os::unix::fs::MetadataExt;
                    std::fs::metadata(value).map(|m| m.mode() & 0o111 != 0).unwrap_or(false)
                }
                _ => false,
            }
        }
        3 => {
            let (left, operator, right) = (terms[0], terms[1], terms[2]);
            let numbers = (left.parse::<i64>(), right.parse::<i64>());
            match operator {
                "=" | "==" => left == right,
                "!=" => left != right,
                "-eq" => matches!(numbers, (Ok(a), Ok(b)) if a == b),
                "-ne" => matches!(numbers, (Ok(a), Ok(b)) if a != b),
                "-lt" => matches!(numbers, (Ok(a), Ok(b)) if a < b),
                "-le" => matches!(numbers, (Ok(a), Ok(b)) if a <= b),
                "-gt" => matches!(numbers, (Ok(a), Ok(b)) if a > b),
                "-ge" => matches!(numbers, (Ok(a), Ok(b)) if a >= b),
                _ => false,
            }
        }
        _ => false,
    }
}

/// `printf FORMAT [ARG...]` with the conversions scripts actually use.
pub fn printf(args: &[String]) -> i32 {
    use std::io::Write;
    let Some(format) = args.get(1) else {
        eprintln!("usage: printf FORMAT [ARGUMENT...]");
        return 2;
    };
    let operands = &args[2..];
    let chars: Vec<char> = format.chars().collect();
    let mut out = String::new();
    let mut next = 0usize;
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            out.push_str(&escape_char(&chars, &mut i));
            continue;
        }
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        i += 1;
        if chars.get(i) == Some(&'%') {
            out.push('%');
            i += 1;
            continue;
        }

        // [-][width][.precision]conversion
        let left = chars.get(i) == Some(&'-');
        if left {
            i += 1;
        }
        let mut width = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            width.push(chars[i]);
            i += 1;
        }
        let mut precision = String::new();
        if chars.get(i) == Some(&'.') {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                precision.push(chars[i]);
                i += 1;
            }
        }
        let Some(&conversion) = chars.get(i) else { break };
        i += 1;

        let argument = operands.get(next).cloned().unwrap_or_default();
        let rendered = match conversion {
            's' => {
                next += 1;
                match precision.parse::<usize>() {
                    Ok(limit) => argument.chars().take(limit).collect(),
                    Err(_) => argument,
                }
            }
            'd' | 'i' => {
                next += 1;
                argument.trim().parse::<i64>().unwrap_or(0).to_string()
            }
            'x' => {
                next += 1;
                format!("{:x}", argument.trim().parse::<i64>().unwrap_or(0))
            }
            'X' => {
                next += 1;
                format!("{:X}", argument.trim().parse::<i64>().unwrap_or(0))
            }
            'o' => {
                next += 1;
                format!("{:o}", argument.trim().parse::<i64>().unwrap_or(0))
            }
            'f' | 'F' => {
                next += 1;
                let value = argument.trim().parse::<f64>().unwrap_or(0.0);
                let places = precision.parse::<usize>().unwrap_or(6);
                format!("{:.*}", places, value)
            }
            'c' => {
                next += 1;
                argument.chars().next().map(String::from).unwrap_or_default()
            }
            other => {
                let mut literal = String::from("%");
                literal.push(other);
                literal
            }
        };

        match width.parse::<usize>() {
            Ok(width) if left => out.push_str(&format!("{:<1$}", rendered, width)),
            Ok(width) => out.push_str(&format!("{:>1$}", rendered, width)),
            Err(_) => out.push_str(&rendered),
        }
    }

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if handle.write_all(out.as_bytes()).is_err() || handle.flush().is_err() {
        eprintln!("printf: write error");
        return 1;
    }
    0
}

/// Decode one backslash escape, advancing past it.
fn escape_char(chars: &[char], i: &mut usize) -> String {
    let next = chars[*i + 1];
    *i += 2;
    match next {
        'n' => "\n".to_string(),
        't' => "\t".to_string(),
        'r' => "\r".to_string(),
        'a' => "\u{07}".to_string(),
        'b' => "\u{08}".to_string(),
        'f' => "\u{0c}".to_string(),
        'v' => "\u{0b}".to_string(),
        '\\' => "\\".to_string(),
        '0' | '1' | '2' | '3' | '4' | '5' | '6' | '7' => {
            let mut value = next.to_digit(8).unwrap();
            let mut taken = 1;
            while taken < 3 && *i < chars.len() && chars[*i].is_digit(8) {
                value = value * 8 + chars[*i].to_digit(8).unwrap();
                *i += 1;
                taken += 1;
            }
            char::from_u32(value).map(String::from).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

/// `expr` for integer arithmetic and string comparison.
pub fn expr(args: &[String]) -> i32 {
    let operands: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();

    match operands.as_slice() {
        ["length", text] => {
            println!("{}", text.chars().count());
            return if text.is_empty() { 1 } else { 0 };
        }
        ["substr", text, start, length] => {
            let start: usize = start.parse().unwrap_or(1);
            let length: usize = length.parse().unwrap_or(0);
            let piece: String =
                text.chars().skip(start.saturating_sub(1)).take(length).collect();
            println!("{}", piece);
            return if piece.is_empty() { 1 } else { 0 };
        }
        ["index", text, set] => {
            let position = text
                .char_indices()
                .find(|(_, c)| set.contains(*c))
                .map(|(index, _)| index + 1)
                .unwrap_or(0);
            println!("{}", position);
            return if position == 0 { 1 } else { 0 };
        }
        _ => {}
    }

    if operands.len() != 3 {
        if operands.len() == 1 {
            println!("{}", operands[0]);
            return 0;
        }
        eprintln!("usage: expr ARG OPERATOR ARG");
        return 2;
    }
    let left = operands[0].parse::<i64>();
    let right = operands[2].parse::<i64>();
    let numeric = |f: fn(i64, i64) -> i64| -> Option<i64> {
        match (&left, &right) {
            (Ok(a), Ok(b)) => Some(f(*a, *b)),
            _ => None,
        }
    };

    let value = match operands[1] {
        "+" => numeric(|a, b| a + b),
        "-" => numeric(|a, b| a - b),
        "*" => numeric(|a, b| a * b),
        "/" | "%" => {
            if right.as_ref().map(|v| *v == 0).unwrap_or(false) {
                eprintln!("expr: division by zero");
                return 2;
            }
            if operands[1] == "/" {
                numeric(|a, b| a / b)
            } else {
                numeric(|a, b| a % b)
            }
        }
        "=" => Some((operands[0] == operands[2]) as i64),
        "!=" => Some((operands[0] != operands[2]) as i64),
        "<" => numeric(|a, b| (a < b) as i64),
        "<=" => numeric(|a, b| (a <= b) as i64),
        ">" => numeric(|a, b| (a > b) as i64),
        ">=" => numeric(|a, b| (a >= b) as i64),
        _ => None,
    };

    match value {
        Some(value) => {
            println!("{}", value);
            if value == 0 {
                1
            } else {
                0
            }
        }
        None => {
            eprintln!("expr: non-numeric argument");
            2
        }
    }
}

/// `chmod MODE FILE...` with octal modes and the common symbolic forms.
pub fn chmod(args: &[String]) -> i32 {
    use std::os::unix::fs::PermissionsExt;

    // The mode is the first argument even when it starts with '-', so the
    // usual flag splitting cannot be used here.
    let operands: Vec<&String> = args[1..].iter().filter(|a| *a != "--").collect();
    if operands.len() < 2 {
        eprintln!("usage: chmod MODE file...");
        return 2;
    }
    let spec = operands[0];
    let mut status = 0;

    for path in &operands[1..] {
        let current = match std::fs::metadata(path.as_str()) {
            Ok(metadata) => metadata.permissions().mode() & 0o7777,
            Err(err) => {
                eprintln!("chmod: {}: {}", path, err);
                status = 1;
                continue;
            }
        };
        let mode = match parse_mode(spec, current) {
            Some(mode) => mode,
            None => {
                eprintln!("chmod: invalid mode: {}", spec);
                return 2;
            }
        };
        if let Err(err) = std::fs::set_permissions(path.as_str(), PermissionsExt::from_mode(mode)) {
            eprintln!("chmod: {}: {}", path, err);
            status = 1;
        }
    }
    status
}

fn parse_mode(spec: &str, current: u32) -> Option<u32> {
    if spec.chars().all(|c| ('0'..='7').contains(&c)) && !spec.is_empty() {
        return u32::from_str_radix(spec, 8).ok();
    }

    // [ugoa...][+-=][rwx...]
    let chars: Vec<char> = spec.chars().collect();
    let mut index = 0;
    let mut who = 0u32;
    while index < chars.len() && matches!(chars[index], 'u' | 'g' | 'o' | 'a') {
        who |= match chars[index] {
            'u' => 0o700,
            'g' => 0o070,
            'o' => 0o007,
            _ => 0o777,
        };
        index += 1;
    }
    if who == 0 {
        who = 0o777;
    }
    let operator = *chars.get(index)?;
    if !matches!(operator, '+' | '-' | '=') {
        return None;
    }
    index += 1;

    let mut bits = 0u32;
    while index < chars.len() {
        bits |= match chars[index] {
            'r' => 0o444,
            'w' => 0o222,
            'x' => 0o111,
            _ => return None,
        };
        index += 1;
    }
    let bits = bits & who;

    Some(match operator {
        '+' => current | bits,
        '-' => current & !bits,
        _ => (current & !who) | bits,
    })
}
