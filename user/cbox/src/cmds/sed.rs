//! A stream editor.
//!
//! Addresses select lines, commands act on them, and the pattern space is
//! printed at the end of the cycle unless -n said not to.
//!
//! The pattern space holds bytes. A file handed to sed is not always text,
//! and one that is not still has lines in it that a script can select and
//! substitute in; refusing it, or replacing the bytes that do not decode,
//! would lose what the file said.

use crate::regex::Regex;
use std::io::{Read, Write};

enum Address {
    /// Every line.
    Always,
    Line(usize),
    Last,
    Match(Regex),
}

impl Address {
    fn matches(&self, number: usize, last: bool, line: &[u8]) -> bool {
        match self {
            Address::Always => true,
            Address::Line(want) => number == *want,
            Address::Last => last,
            Address::Match(regex) => regex.is_match(line),
        }
    }
}

enum Action {
    /// s/regex/replacement/
    Substitute { regex: Regex, replacement: String, global: bool, which: usize, print: bool },
    Print,
    Delete,
    Quit,
    LineNumber,
    /// y/abc/xyz/
    Transliterate { from: Vec<u8>, to: Vec<u8> },
}

struct Command {
    start: Address,
    /// Set for a two-address range, with `active` tracking whether the range
    /// is currently open.
    end: Option<Address>,
    active: bool,
    negated: bool,
    action: Action,
}

impl Command {
    fn selects(&mut self, number: usize, last: bool, line: &[u8]) -> bool {
        let chosen = match &self.end {
            None => self.start.matches(number, last, line),
            Some(end) => {
                if self.active {
                    // A range stays open until its end matches, and the line
                    // that closes it is still part of the range.
                    if end.matches(number, last, line) {
                        self.active = false;
                    }
                    true
                } else if self.start.matches(number, last, line) {
                    self.active = !end.matches(number + 1, false, b"");
                    true
                } else {
                    false
                }
            }
        };
        chosen != self.negated
    }
}

/// Read one address, or nothing when the script does not start with one.
fn parse_address(chars: &[char], position: &mut usize, extended: bool) -> Option<Address> {
    match chars.get(*position) {
        Some('$') => {
            *position += 1;
            Some(Address::Last)
        }
        Some('/') => {
            *position += 1;
            let mut pattern = String::new();
            while let Some(&c) = chars.get(*position) {
                *position += 1;
                if c == '/' {
                    break;
                }
                if c == '\\' {
                    if let Some(&next) = chars.get(*position) {
                        *position += 1;
                        if next != '/' {
                            pattern.push('\\');
                        }
                        pattern.push(next);
                        continue;
                    }
                }
                pattern.push(c);
            }
            Some(Address::Match(Regex::new(&pattern, extended)))
        }
        Some(c) if c.is_ascii_digit() => {
            let mut number = 0usize;
            while let Some(c) = chars.get(*position).filter(|c| c.is_ascii_digit()) {
                number = number * 10 + c.to_digit(10).unwrap_or(0) as usize;
                *position += 1;
            }
            Some(Address::Line(number))
        }
        _ => None,
    }
}

/// Read text up to the next unescaped `delimiter`.
fn parse_until(chars: &[char], position: &mut usize, delimiter: char) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.get(*position) {
        *position += 1;
        if c == delimiter {
            return Some(out);
        }
        if c == '\\' {
            match chars.get(*position) {
                Some(&next) => {
                    *position += 1;
                    if next == delimiter {
                        out.push(delimiter);
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                }
                None => out.push('\\'),
            }
            continue;
        }
        out.push(c);
    }
    None
}

fn parse_script(script: &str, extended: bool) -> Result<Vec<Command>, String> {
    let chars: Vec<char> = script.chars().collect();
    let mut position = 0;
    let mut commands = Vec::new();

    loop {
        while matches!(chars.get(position), Some(';') | Some('\n') | Some(' ') | Some('\t')) {
            position += 1;
        }
        if position >= chars.len() {
            return Ok(commands);
        }
        if chars[position] == '#' {
            while position < chars.len() && chars[position] != '\n' {
                position += 1;
            }
            continue;
        }

        let start = parse_address(&chars, &mut position, extended);
        let mut end = None;
        if start.is_some() && chars.get(position) == Some(&',') {
            position += 1;
            end = parse_address(&chars, &mut position, extended);
        }
        let mut negated = false;
        while chars.get(position) == Some(&'!') {
            negated = !negated;
            position += 1;
        }
        while chars.get(position) == Some(&' ') {
            position += 1;
        }

        let letter = match chars.get(position) {
            Some(letter) => *letter,
            None => return Err("missing a command after an address".into()),
        };
        position += 1;

        let action = match letter {
            's' => {
                let delimiter = match chars.get(position) {
                    Some(d) => *d,
                    None => return Err("s needs a delimiter".into()),
                };
                position += 1;
                let pattern = parse_until(&chars, &mut position, delimiter)
                    .ok_or("unterminated s command")?;
                let replacement = parse_until(&chars, &mut position, delimiter)
                    .ok_or("unterminated s command")?;
                let mut global = false;
                let mut print = false;
                let mut which = 1usize;
                while let Some(&flag) = chars.get(position) {
                    match flag {
                        'g' => global = true,
                        'p' => print = true,
                        c if c.is_ascii_digit() => {
                            which = c.to_digit(10).unwrap_or(1) as usize;
                        }
                        _ => break,
                    }
                    position += 1;
                }
                Action::Substitute {
                    regex: Regex::new(&pattern, extended),
                    replacement,
                    global,
                    which: which.max(1),
                    print,
                }
            }
            'y' => {
                let delimiter = match chars.get(position) {
                    Some(d) => *d,
                    None => return Err("y needs a delimiter".into()),
                };
                position += 1;
                let from = parse_until(&chars, &mut position, delimiter)
                    .ok_or("unterminated y command")?;
                let to = parse_until(&chars, &mut position, delimiter)
                    .ok_or("unterminated y command")?;
                if from.len() != to.len() {
                    return Err("y needs two sets of the same length".into());
                }
                Action::Transliterate { from: from.into_bytes(), to: to.into_bytes() }
            }
            'p' => Action::Print,
            'd' => Action::Delete,
            'q' => Action::Quit,
            '=' => Action::LineNumber,
            other => return Err(format!("unknown command `{}`", other)),
        };

        commands.push(Command {
            start: start.unwrap_or(Address::Always),
            end,
            active: false,
            negated,
            action,
        });
    }
}

/// Expand a replacement: `&` is the whole match and `\1` to `\9` are the
/// groups the pattern captured.
fn expand(
    replacement: &str,
    matched: &[u8],
    text: &[u8],
    caps: &crate::regex::Captures,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bytes = replacement.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'&' => out.extend_from_slice(matched),
            b'\\' => match bytes.next() {
                Some(b'n') => out.push(b'\n'),
                Some(b't') => out.push(b'\t'),
                Some(b'&') => out.push(b'&'),
                Some(b'\\') => out.push(b'\\'),
                Some(digit) if digit.is_ascii_digit() => {
                    let index = (digit - b'0') as usize;
                    if let Some(Some((start, end))) = caps.get(index) {
                        out.extend_from_slice(&text[*start..*end]);
                    }
                }
                Some(other) => out.push(other),
                None => out.push(b'\\'),
            },
            other => out.push(other),
        }
    }
    out
}

fn substitute(
    line: &[u8],
    regex: &Regex,
    replacement: &str,
    global: bool,
    which: usize,
) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut seen = 0usize;
    let mut changed = false;
    let mut caps = crate::regex::Captures::new();

    while cursor <= line.len() {
        let (start, end) = match regex.find_captures(line, cursor, &mut caps) {
            Some(span) => span,
            None => break,
        };
        seen += 1;
        let take = seen >= which && (global || seen == which);
        out.extend_from_slice(&line[cursor..start]);
        if take {
            out.extend_from_slice(&expand(replacement, &line[start..end], line, &caps));
            changed = true;
        } else {
            out.extend_from_slice(&line[start..end]);
        }
        if end == start {
            // An empty match would not advance on its own.
            if start < line.len() {
                out.push(line[start]);
            }
            cursor = start + 1;
        } else {
            cursor = end;
        }
        if take && !global {
            break;
        }
    }
    if cursor < line.len() {
        out.extend_from_slice(&line[cursor..]);
    }
    (out, changed)
}

pub fn main(args: &[String]) -> i32 {
    let mut quiet = false;
    let mut in_place = false;
    let mut extended = false;
    let mut scripts: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    // args[0] is the applet name, as it is for every applet here.
    let mut index = 1;

    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "-n" => quiet = true,
            "-i" => in_place = true,
            "-r" | "-E" => extended = true,
            "-e" => {
                index += 1;
                match args.get(index) {
                    Some(script) => scripts.push(script.clone()),
                    None => {
                        eprintln!("sed: -e needs a script");
                        return 2;
                    }
                }
            }
            _ if argument.starts_with('-') && argument.len() > 1 => {
                // Bundled single-letter flags, as in -ne.
                for flag in argument.chars().skip(1) {
                    match flag {
                        'n' => quiet = true,
                        'i' => in_place = true,
                        'r' | 'E' => extended = true,
                        other => {
                            eprintln!("sed: unknown option -{}", other);
                            return 2;
                        }
                    }
                }
            }
            _ if scripts.is_empty() => scripts.push(argument.clone()),
            _ => files.push(argument.clone()),
        }
        index += 1;
    }

    if scripts.is_empty() {
        eprintln!("usage: sed [-n] [-i] [-e script] script [file...]");
        return 2;
    }
    let mut commands = match parse_script(&scripts.join("\n"), extended) {
        Ok(commands) => commands,
        Err(message) => {
            eprintln!("sed: {}", message);
            return 2;
        }
    };

    if files.is_empty() {
        if in_place {
            eprintln!("sed: -i needs a file");
            return 2;
        }
        let mut data = Vec::new();
        if std::io::stdin().read_to_end(&mut data).is_err() {
            eprintln!("sed: -: cannot read input");
            return 1;
        }
        let out = edit(&mut commands, &data, quiet);
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(&out);
        let _ = handle.flush();
        return 0;
    }

    let mut status = 0;
    for path in &files {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("sed: {}: {}", path, err);
                status = 1;
                continue;
            }
        };
        for command in commands.iter_mut() {
            command.active = false;
        }
        let out = edit(&mut commands, &data, quiet);
        if in_place {
            if let Err(err) = std::fs::write(path, &out) {
                eprintln!("sed: {}: {}", path, err);
                status = 1;
            }
        } else {
            let stdout = std::io::stdout();
            let _ = stdout.lock().write_all(&out);
        }
    }
    let _ = std::io::stdout().flush();
    status
}

/// Run the script over one input. The last line of a file that does not end
/// in a newline has not got one to print either, so a run over such a file
/// gives back a file of the same shape rather than one with a line ending
/// sed invented.
fn edit(commands: &mut [Command], data: &[u8], quiet: bool) -> Vec<u8> {
    let lines: Vec<&[u8]> =
        super::lines(data).iter().map(|line| super::without_newline(line)).collect();
    let terminated = data.last() == Some(&b'\n');
    let mut out = Vec::new();
    run(commands, &lines, quiet, terminated, &mut out);
    out
}

/// Write one piece of output, supplying the newline the piece before it was
/// owed.
///
/// A pattern space that came from a last line with no newline after it is
/// written without one, but anything printed afterwards has to be separated
/// from it, so the newline is owed rather than dropped. That leaves the
/// missing newline at the end of the output and nowhere else, whether the
/// line that was missing one was printed once, printed twice, or deleted.
fn emit(out: &mut Vec<u8>, owed: &mut bool, body: &[u8], ends: bool) {
    if *owed {
        out.push(b'\n');
        *owed = false;
    }
    out.extend_from_slice(body);
    if ends {
        out.push(b'\n');
    } else {
        *owed = true;
    }
}

fn run(
    commands: &mut [Command],
    lines: &[&[u8]],
    quiet: bool,
    terminated: bool,
    out: &mut Vec<u8>,
) {
    let total = lines.len();
    let mut owed = false;
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let last = number == total;
        // Whether a pattern space printed in this cycle ends in a newline.
        let ends = !last || terminated;
        let mut space = line.to_vec();
        let mut deleted = false;
        let mut quit = false;

        for command in commands.iter_mut() {
            if !command.selects(number, last, &space) {
                continue;
            }
            match &command.action {
                Action::Substitute { regex, replacement, global, which, print } => {
                    let (new, changed) = substitute(&space, regex, replacement, *global, *which);
                    space = new;
                    if changed && *print {
                        emit(out, &mut owed, &space, ends);
                    }
                }
                Action::Print => {
                    emit(out, &mut owed, &space, ends);
                }
                Action::Delete => {
                    deleted = true;
                    break;
                }
                Action::Quit => {
                    quit = true;
                    break;
                }
                Action::LineNumber => {
                    emit(out, &mut owed, number.to_string().as_bytes(), true);
                }
                Action::Transliterate { from, to } => {
                    for byte in space.iter_mut() {
                        if let Some(at) = from.iter().position(|f| f == byte) {
                            *byte = to[at];
                        }
                    }
                }
            }
        }

        if !deleted && !quiet {
            emit(out, &mut owed, &space, ends);
        }
        if quit {
            break;
        }
    }
}
