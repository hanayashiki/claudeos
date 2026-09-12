//! Small applets.

use super::split_flags;

pub fn yes(args: &[String]) -> i32 {
    let text = if args.len() > 1 { args[1..].join(" ") } else { "y".to_string() };
    loop {
        println!("{}", text);
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

/// `test` / `[`: evaluate a single condition.
pub fn test(args: &[String]) -> i32 {
    let mut operands: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
    if args[0] == "[" {
        if operands.last() != Some(&"]") {
            eprintln!("[: missing ]");
            return 2;
        }
        operands.pop();
    }

    let result = match operands.len() {
        0 => false,
        1 => !operands[0].is_empty(),
        2 => {
            let value = operands[1];
            match operands[0] {
                "-n" => !value.is_empty(),
                "-z" => value.is_empty(),
                "-e" => std::fs::symlink_metadata(value).is_ok(),
                "-f" => std::fs::metadata(value).map(|m| m.is_file()).unwrap_or(false),
                "-d" => std::fs::metadata(value).map(|m| m.is_dir()).unwrap_or(false),
                "-s" => std::fs::metadata(value).map(|m| m.len() > 0).unwrap_or(false),
                "-r" | "-w" => std::fs::metadata(value).is_ok(),
                "-x" => {
                    use std::os::unix::fs::MetadataExt;
                    std::fs::metadata(value).map(|m| m.mode() & 0o111 != 0).unwrap_or(false)
                }
                "!" => value.is_empty(),
                _ => false,
            }
        }
        3 => {
            let (left, operator, right) = (operands[0], operands[1], operands[2]);
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
    };
    if result {
        0
    } else {
        1
    }
}

/// `printf FORMAT [ARG...]` with the handful of conversions scripts use.
pub fn printf(args: &[String]) -> i32 {
    use std::io::Write;
    let Some(format) = args.get(1) else {
        eprintln!("usage: printf FORMAT [ARGUMENT...]");
        return 2;
    };
    let operands = &args[2..];
    let mut out = String::new();
    let mut next = 0usize;
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                out.push(match chars[i + 1] {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    other => other,
                });
                i += 2;
            }
            '%' if i + 1 < chars.len() => {
                let conversion = chars[i + 1];
                let argument = operands.get(next).cloned().unwrap_or_default();
                match conversion {
                    '%' => out.push('%'),
                    's' => {
                        out.push_str(&argument);
                        next += 1;
                    }
                    'd' | 'i' => {
                        out.push_str(&argument.trim().parse::<i64>().unwrap_or(0).to_string());
                        next += 1;
                    }
                    'c' => {
                        out.push(argument.chars().next().unwrap_or(' '));
                        next += 1;
                    }
                    other => {
                        out.push('%');
                        out.push(other);
                    }
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(out.as_bytes());
    let _ = handle.flush();
    0
}

/// `expr` for integer arithmetic and string comparison.
pub fn expr(args: &[String]) -> i32 {
    let operands: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
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
        "/" => numeric(|a, b| if b == 0 { 0 } else { a / b }),
        "%" => numeric(|a, b| if b == 0 { 0 } else { a % b }),
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
