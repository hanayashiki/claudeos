//! Line editing: history, cursor movement and completion.
//!
//! The terminal is put in raw mode for the duration of a prompt so the shell
//! sees each keystroke, and restored before a command runs so that the job,
//! not the editor, owns the terminal.

use crate::sys;
use std::io::Write;

const HISTORY_LIMIT: usize = 500;

pub struct Editor {
    history: Vec<String>,
    /// The terminal settings to put back before running a command.
    cooked: Option<sys::Termios>,
}

impl Editor {
    pub fn new() -> Editor {
        Editor { history: Vec::new(), cooked: sys::tcgets(sys::STDIN) }
    }

    fn enter_raw(&self) -> bool {
        let Some(cooked) = self.cooked else { return false };
        let mut raw = cooked;
        // Signals are handled by the editor itself while a line is being typed;
        // the terminal goes back to cooked mode before any command runs.
        raw.c_lflag &= !(sys::ICANON | sys::ECHO | sys::ISIG);
        raw.c_cc[6] = 1; // VMIN: one byte is enough to return
        raw.c_cc[5] = 0; // VTIME: no timeout
        sys::tcsets(sys::STDIN, &raw)
    }

    fn leave_raw(&self) {
        if let Some(cooked) = self.cooked {
            sys::tcsets(sys::STDIN, &cooked);
        }
    }

    pub fn add_history(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().map(|l| l.as_str()) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Read one line. Returns None at end of file.
    pub fn read_line(&mut self, prompt: &str, completer: &dyn Completer) -> Option<String> {
        if !self.enter_raw() {
            // Not a terminal: fall back to whole lines from the kernel.
            return read_cooked_line(prompt);
        }

        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0usize;
        let mut browsing: Option<usize> = None;
        let mut stash = String::new();

        emit(prompt);
        let result = loop {
            let Some(byte) = read_byte() else {
                break if buffer.is_empty() { None } else { Some(collect(&buffer)) };
            };

            match byte {
                b'\r' | b'\n' => {
                    emit("\r\n");
                    break Some(collect(&buffer));
                }
                0x03 => {
                    // Ctrl-C abandons the line and starts a fresh one.
                    emit("^C\r\n");
                    buffer.clear();
                    cursor = 0;
                    browsing = None;
                    emit(prompt);
                    continue;
                }
                0x04 => {
                    if buffer.is_empty() {
                        emit("\r\n");
                        break None;
                    }
                    if cursor < buffer.len() {
                        buffer.remove(cursor);
                    }
                }
                0x7F | 0x08 => {
                    if cursor > 0 {
                        cursor -= 1;
                        buffer.remove(cursor);
                    }
                }
                0x01 => cursor = 0,             // Ctrl-A
                0x05 => cursor = buffer.len(),  // Ctrl-E
                0x02 => cursor = cursor.saturating_sub(1), // Ctrl-B
                0x06 => cursor = (cursor + 1).min(buffer.len()), // Ctrl-F
                0x0B => buffer.truncate(cursor), // Ctrl-K
                0x15 => {
                    // Ctrl-U: discard everything before the cursor.
                    buffer.drain(..cursor);
                    cursor = 0;
                }
                0x17 => {
                    // Ctrl-W: discard the word before the cursor.
                    let mut start = cursor;
                    while start > 0 && buffer[start - 1].is_whitespace() {
                        start -= 1;
                    }
                    while start > 0 && !buffer[start - 1].is_whitespace() {
                        start -= 1;
                    }
                    buffer.drain(start..cursor);
                    cursor = start;
                }
                0x0C => {
                    // Ctrl-L: clear the screen, keep the line.
                    emit("\x1b[2J\x1b[H");
                }
                b'\t' => {
                    complete(completer, &mut buffer, &mut cursor, prompt);
                }
                0x1B => {
                    match read_escape() {
                        Some(Key::Up) => {
                            if self.history.is_empty() {
                                continue;
                            }
                            let index = match browsing {
                                None => {
                                    stash = collect(&buffer);
                                    self.history.len() - 1
                                }
                                Some(0) => 0,
                                Some(i) => i - 1,
                            };
                            browsing = Some(index);
                            buffer = self.history[index].chars().collect();
                            cursor = buffer.len();
                        }
                        Some(Key::Down) => match browsing {
                            None => continue,
                            Some(i) if i + 1 < self.history.len() => {
                                browsing = Some(i + 1);
                                buffer = self.history[i + 1].chars().collect();
                                cursor = buffer.len();
                            }
                            Some(_) => {
                                browsing = None;
                                buffer = stash.chars().collect();
                                cursor = buffer.len();
                            }
                        },
                        Some(Key::Left) => cursor = cursor.saturating_sub(1),
                        Some(Key::Right) => cursor = (cursor + 1).min(buffer.len()),
                        Some(Key::Home) => cursor = 0,
                        Some(Key::End) => cursor = buffer.len(),
                        Some(Key::Delete) => {
                            if cursor < buffer.len() {
                                buffer.remove(cursor);
                            }
                        }
                        None => continue,
                    }
                }
                byte if byte >= 0x20 => {
                    buffer.insert(cursor, byte as char);
                    cursor += 1;
                }
                _ => continue,
            }
            redraw(prompt, &buffer, cursor);
        };

        self.leave_raw();
        result
    }
}

enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
}

fn collect(buffer: &[char]) -> String {
    buffer.iter().collect()
}

fn emit(text: &str) {
    let _ = sys::write(sys::STDOUT, text.as_bytes());
}

fn read_byte() -> Option<u8> {
    let mut byte = [0u8; 1];
    match sys::read(sys::STDIN, &mut byte) {
        n if n > 0 => Some(byte[0]),
        _ => None,
    }
}

/// Decode the tail of an escape sequence that has already consumed ESC.
fn read_escape() -> Option<Key> {
    let second = read_byte()?;
    if second != b'[' && second != b'O' {
        return None;
    }
    let third = read_byte()?;
    match third {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'0'..=b'9' => {
            // A numbered sequence such as ESC [ 3 ~ for Delete.
            let mut number = (third - b'0') as u32;
            loop {
                let next = read_byte()?;
                match next {
                    b'0'..=b'9' => number = number * 10 + (next - b'0') as u32,
                    b'~' => break,
                    _ => return None,
                }
            }
            match number {
                1 | 7 => Some(Key::Home),
                3 => Some(Key::Delete),
                4 | 8 => Some(Key::End),
                _ => None,
            }
        }
        _ => None,
    }
}

fn redraw(prompt: &str, buffer: &[char], cursor: usize) {
    let mut out = String::with_capacity(buffer.len() + prompt.len() + 16);
    out.push('\r');
    out.push_str(prompt);
    out.extend(buffer.iter());
    out.push_str("\x1b[K"); // erase whatever the line used to be longer by
    let column = prompt.chars().count() + cursor;
    out.push('\r');
    if column > 0 {
        out.push_str("\x1b[");
        out.push_str(&column.to_string());
        out.push('C');
    }
    emit(&out);
}

/// Supplies candidate completions for the word under the cursor.
pub trait Completer {
    /// Candidates for `word`, which is the first word of the line when
    /// `is_command` is set.
    fn candidates(&self, word: &str, is_command: bool) -> Vec<String>;
}

fn complete(completer: &dyn Completer, buffer: &mut Vec<char>, cursor: &mut usize, prompt: &str) {
    let text: String = buffer[..*cursor].iter().collect();
    let start = text
        .rfind(|c: char| c.is_whitespace() || c == '|' || c == ';' || c == '>' || c == '<')
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &text[start..];
    let is_command = text[..start].trim().is_empty();

    let matches = completer.candidates(word, is_command);
    if matches.is_empty() {
        return;
    }

    let insert = if matches.len() == 1 {
        matches[0].clone()
    } else {
        let shared = common_prefix(&matches);
        if shared.len() > word.len() {
            shared
        } else {
            // Ambiguous: show what is available and leave the line alone.
            emit("\r\n");
            let mut line = String::new();
            for (index, candidate) in matches.iter().enumerate() {
                line.push_str(candidate);
                if index % 4 == 3 {
                    line.push_str("\r\n");
                } else {
                    for _ in candidate.len()..18 {
                        line.push(' ');
                    }
                }
            }
            if !line.ends_with('\n') {
                line.push_str("\r\n");
            }
            emit(&line);
            redraw(prompt, buffer, *cursor);
            return;
        }
    };

    // Replace the word with the completion.
    let word_chars = word.chars().count();
    for _ in 0..word_chars {
        *cursor -= 1;
        buffer.remove(*cursor);
    }
    for c in insert.chars() {
        buffer.insert(*cursor, c);
        *cursor += 1;
    }
    if matches.len() == 1 && !insert.ends_with('/') {
        buffer.insert(*cursor, ' ');
        *cursor += 1;
    }
    redraw(prompt, buffer, *cursor);
}

fn common_prefix(items: &[String]) -> String {
    let first = &items[0];
    let mut length = first.len();
    for item in &items[1..] {
        length = length.min(
            first
                .chars()
                .zip(item.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a.len_utf8())
                .sum(),
        );
    }
    first[..length].to_string()
}

/// Read a whole line from a kernel that is doing the editing for us.
fn read_cooked_line(prompt: &str) -> Option<String> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = sys::read(sys::STDIN, &mut byte);
        if n <= 0 {
            if out.is_empty() {
                return None;
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&out).to_string())
}
