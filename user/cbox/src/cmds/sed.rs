//! A stream editor.
//!
//! Addresses select lines, commands act on them, and the pattern space is
//! printed at the end of the cycle unless -n said not to.

use crate::regex::Regex;
use std::io::{BufRead, BufReader, Write};

enum Address {
    /// Every line.
    Always,
    Line(usize),
    Last,
    Match(Regex),
}

impl Address {
    fn matches(&self, number: usize, last: bool, line: &str) -> bool {
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
    Transliterate { from: Vec<char>, to: Vec<char> },
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
    fn selects(&mut self, number: usize, last: bool, line: &str) -> bool {
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
                    self.active = !end.matches(number + 1, false, "");
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
                if from.chars().count() != to.chars().count() {
                    return Err("y needs two sets of the same length".into());
                }
                Action::Transliterate { from: from.chars().collect(), to: to.chars().collect() }
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
    matched: &str,
    text: &[char],
    caps: &crate::regex::Captures,
) -> String {
    let mut out = String::new();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str(matched),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some(digit) if digit.is_ascii_digit() => {
                    let index = digit.to_digit(10).unwrap_or(0) as usize;
                    if let Some(Some((start, end))) = caps.get(index) {
                        out.extend(text[*start..*end].iter());
                    }
                }
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}

fn substitute(
    line: &str,
    regex: &Regex,
    replacement: &str,
    global: bool,
    which: usize,
) -> (String, bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut cursor = 0usize;
    let mut seen = 0usize;
    let mut changed = false;
    let mut caps = crate::regex::Captures::new();

    while cursor <= chars.len() {
        let (start, end) = match regex.find_captures(&chars, cursor, &mut caps) {
            Some(span) => span,
            None => break,
        };
        seen += 1;
        let take = seen >= which && (global || seen == which);
        out.extend(chars[cursor..start].iter());
        let matched: String = chars[start..end].iter().collect();
        if take {
            out.push_str(&expand(replacement, &matched, &chars, &caps));
            changed = true;
        } else {
            out.push_str(&matched);
        }
        if end == start {
            // An empty match would not advance on its own.
            if start < chars.len() {
                out.push(chars[start]);
            }
            cursor = start + 1;
        } else {
            cursor = end;
        }
        if take && !global {
            break;
        }
    }
    if cursor < chars.len() {
        out.extend(chars[cursor..].iter());
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
        let stdin = std::io::stdin();
        let lines: Vec<String> = BufReader::new(stdin.lock())
            .lines()
            .map_while(Result::ok)
            .collect();
        let mut out = String::new();
        run(&mut commands, &lines, quiet, &mut out);
        print!("{}", out);
        let _ = std::io::stdout().flush();
        return 0;
    }

    let mut status = 0;
    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("sed: {}: {}", path, err);
                status = 1;
                continue;
            }
        };
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        let mut out = String::new();
        for command in commands.iter_mut() {
            command.active = false;
        }
        run(&mut commands, &lines, quiet, &mut out);
        if in_place {
            if let Err(err) = std::fs::write(path, out.as_bytes()) {
                eprintln!("sed: {}: {}", path, err);
                status = 1;
            }
        } else {
            print!("{}", out);
        }
    }
    let _ = std::io::stdout().flush();
    status
}

fn run(commands: &mut [Command], lines: &[String], quiet: bool, out: &mut String) {
    let total = lines.len();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let last = number == total;
        let mut space = line.clone();
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
                        out.push_str(&space);
                        out.push('\n');
                    }
                }
                Action::Print => {
                    out.push_str(&space);
                    out.push('\n');
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
                    out.push_str(&number.to_string());
                    out.push('\n');
                }
                Action::Transliterate { from, to } => {
                    space = space
                        .chars()
                        .map(|c| match from.iter().position(|f| *f == c) {
                            Some(at) => to[at],
                            None => c,
                        })
                        .collect();
                }
            }
        }

        if !deleted && !quiet {
            out.push_str(&space);
            out.push('\n');
        }
        if quit {
            break;
        }
    }
}
