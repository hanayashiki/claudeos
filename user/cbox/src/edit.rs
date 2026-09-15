//! Line editing: history, cursor movement and completion.
//!
//! The terminal is put in raw mode for the duration of a prompt so the shell
//! sees each keystroke, and restored before a command runs so that the job,
//! not the editor, owns the terminal.

use crate::sys;
use std::io::Write;

const HISTORY_LIMIT: usize = 500;

/// How a line ended.
pub enum Line {
    Text(String),
    /// Ctrl-C: the line is thrown away, and so is whatever it was continuing.
    Interrupted,
    /// Ctrl-D on an empty line, or the input running out.
    EndOfInput,
}

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
    pub fn read_line(&mut self, prompt: &str, completer: &dyn Completer) -> Line {
        if !self.enter_raw() {
            // Not a terminal: fall back to whole lines from the kernel.
            return match read_cooked_line(prompt) {
                Some(line) => Line::Text(line),
                None => Line::EndOfInput,
            };
        }

        // The line as the bytes typed, never decoded or changed: cbox passes
        // bytes through, as Unix does. See `step_left` for the one use made of
        // UTF-8 here, which is where the cursor stops.
        let mut buffer: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        let mut browsing: Option<usize> = None;
        let mut stash: Vec<u8> = Vec::new();

        emit(prompt);
        let result = loop {
            let Some(byte) = read_byte() else {
                break if buffer.is_empty() { Line::EndOfInput } else { Line::Text(collect(&buffer)) };
            };

            match byte {
                b'\r' | b'\n' => {
                    emit("\r\n");
                    break Line::Text(collect(&buffer));
                }
                0x03 => {
                    // Ctrl-C abandons the line, and with it anything the line
                    // was continuing.
                    emit("^C\r\n");
                    break Line::Interrupted;
                }
                0x04 => {
                    if buffer.is_empty() {
                        emit("\r\n");
                        break Line::EndOfInput;
                    }
                    if cursor < buffer.len() {
                        let end = step_right(&buffer, cursor);
                        buffer.drain(cursor..end);
                    }
                }
                0x7F | 0x08 => {
                    if cursor > 0 {
                        let start = step_left(&buffer, cursor);
                        buffer.drain(start..cursor);
                        cursor = start;
                    }
                }
                0x01 => cursor = 0,                                // Ctrl-A
                0x05 => cursor = buffer.len(),                     // Ctrl-E
                0x02 => cursor = step_left(&buffer, cursor),       // Ctrl-B
                0x06 => cursor = step_right(&buffer, cursor),      // Ctrl-F
                0x0B => buffer.truncate(cursor),                   // Ctrl-K
                0x15 => {
                    // Ctrl-U: discard everything before the cursor.
                    buffer.drain(..cursor);
                    cursor = 0;
                }
                0x17 => {
                    // Ctrl-W: discard the word before the cursor. Words are
                    // divided by ASCII whitespace only, which no byte of a
                    // UTF-8 character can be, so this never stops inside one.
                    let mut start = cursor;
                    while start > 0 && buffer[start - 1].is_ascii_whitespace() {
                        start -= 1;
                    }
                    while start > 0 && !buffer[start - 1].is_ascii_whitespace() {
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
                                    stash = buffer.clone();
                                    self.history.len() - 1
                                }
                                Some(0) => 0,
                                Some(i) => i - 1,
                            };
                            browsing = Some(index);
                            buffer = self.history[index].as_bytes().to_vec();
                            cursor = buffer.len();
                        }
                        Some(Key::Down) => match browsing {
                            None => continue,
                            Some(i) if i + 1 < self.history.len() => {
                                browsing = Some(i + 1);
                                buffer = self.history[i + 1].as_bytes().to_vec();
                                cursor = buffer.len();
                            }
                            Some(_) => {
                                browsing = None;
                                buffer = stash.clone();
                                cursor = buffer.len();
                            }
                        },
                        Some(Key::Left) => cursor = step_left(&buffer, cursor),
                        Some(Key::Right) => cursor = step_right(&buffer, cursor),
                        Some(Key::Home) => cursor = 0,
                        Some(Key::End) => cursor = buffer.len(),
                        Some(Key::Delete) => {
                            if cursor < buffer.len() {
                                let end = step_right(&buffer, cursor);
                                buffer.drain(cursor..end);
                            }
                        }
                        None => continue,
                    }
                }
                byte if byte >= 0x20 => {
                    // Every byte of 0x20 and up goes in as it came, those of a
                    // UTF-8 character and those that are not UTF-8 alike.
                    buffer.insert(cursor, byte);
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

/// Whether a byte continues a UTF-8 character: 0x80 to 0xBF.
fn continues(byte: u8) -> bool {
    (0x80..=0xBF).contains(&byte)
}

/// The bytes a UTF-8 character starting with `lead` has, by its high bits:
/// two for 110xxxxx, three for 1110xxxx, four for 11110xxx, and one for any
/// other byte.
fn declared_length(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Where the cursor stops one character to the right of `at`.
///
/// This and `step_left` are the one place the editor knows about UTF-8, and
/// that is not at odds with keeping bytes as they are: they choose where the
/// cursor may stop, so that one key moves over or deletes one character a
/// UTF-8 terminal drew, and they never change, add or drop a byte. A lead
/// byte is one character with as many continuation bytes after it as it
/// declares, fewer when a byte that cannot continue it comes first. Any other
/// byte is a character of its own, and so is a continuation byte past what
/// its lead declared or with no lead before it, which the terminal draws as a
/// replacement glyph.
fn step_right(buffer: &[u8], at: usize) -> usize {
    let Some(&lead) = buffer.get(at) else {
        return buffer.len();
    };
    let mut end = at + 1;
    while end < buffer.len() && end - at < declared_length(lead) && continues(buffer[end]) {
        end += 1;
    }
    end
}

/// Where the cursor stops one character to the left of `at`: the start of
/// the last character that begins before it. The characters are found from
/// the start of the line, since bytes read backwards cannot say which lead a
/// continuation byte belongs to; a typed line is short.
fn step_left(buffer: &[u8], at: usize) -> usize {
    let mut start = 0;
    let mut next = 0;
    while next < at.min(buffer.len()) {
        start = next;
        next = step_right(buffer, next);
    }
    start
}

/// The characters, counted as the cursor steps over them, in `buffer`.
fn steps(buffer: &[u8]) -> usize {
    let mut count = 0;
    let mut at = 0;
    while at < buffer.len() {
        at = step_right(buffer, at);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::{step_left, step_right, steps};

    /// Backspace from the end of `line`, as the editor does it.
    fn backspace(line: &[u8]) -> Vec<u8> {
        let start = step_left(line, line.len());
        line[..start].to_vec()
    }

    #[test]
    fn a_step_is_one_character_a_terminal_draws() {
        let line = "タ日本🎉x".as_bytes();
        assert_eq!(backspace(line), "タ日本🎉".as_bytes());
        assert_eq!(backspace(&backspace(line)), "タ日本".as_bytes());
        assert_eq!(steps(line), 5);
        // From the start, right steps land after each whole character.
        let mut at = 0;
        let mut stops = Vec::new();
        while at < line.len() {
            at = step_right(line, at);
            stops.push(at);
        }
        assert_eq!(stops, vec![3, 6, 9, 13, 14]);
    }

    #[test]
    fn bytes_that_are_not_utf8_are_steps_of_their_own_and_kept() {
        // A stray continuation byte, a lead byte with nothing after it, and 0xff.
        let line = [b'a', 0x80, b'b', 0xE3, 0xFF, b'c'];
        assert_eq!(steps(&line), 6);
        assert_eq!(step_left(&line, 2), 1);
        assert_eq!(step_left(&line, 5), 4);
        assert_eq!(step_right(&line, 3), 4);
        assert_eq!(backspace(&line[..5]), vec![b'a', 0x80, b'b', 0xE3]);
        // A lead byte followed by more continuation bytes than it declares:
        // タ, then each extra byte is a character of its own.
        let long = [0xE3, 0x82, 0xBF, 0x80, 0x80];
        assert_eq!(step_right(&long, 0), 3);
        assert_eq!(step_right(&long, 3), 4);
        assert_eq!(steps(&long), 3);
        assert_eq!(step_left(&long, 5), 4);
        assert_eq!(step_left(&long, 3), 0);
        // One stray byte after a whole character: backspace takes only it.
        assert_eq!(backspace(&[0xE3, 0x82, 0xBF, 0x80]), vec![0xE3, 0x82, 0xBF]);
        // A cursor left inside a character, as while one is being typed.
        assert_eq!(step_left(&[b'a', 0xE3, 0x82, 0xBF], 3), 1);
    }
}

/// The typed line handed to the shell. The shell still holds lines as
/// `String`, so bytes that are not UTF-8 are replaced by U+FFFD here, the same
/// way the shell's own `read_line` treats input that is not a terminal. Valid
/// UTF-8 arrives exactly as typed. This boundary goes when the shell takes
/// lines as bytes.
fn collect(buffer: &[u8]) -> String {
    String::from_utf8_lossy(buffer).into_owned()
}

fn emit(text: &str) {
    emit_bytes(text.as_bytes());
}

fn emit_bytes(bytes: &[u8]) {
    let _ = sys::write(sys::STDOUT, bytes);
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

/// Draw the prompt and the line's bytes as they are, and put the cursor after
/// the characters before it, one column each.
fn redraw(prompt: &str, buffer: &[u8], cursor: usize) {
    let mut out: Vec<u8> = Vec::with_capacity(buffer.len() + prompt.len() + 16);
    out.push(b'\r');
    out.extend_from_slice(prompt.as_bytes());
    out.extend_from_slice(buffer);
    out.extend_from_slice(b"\x1b[K"); // erase whatever the line used to be longer by
    let column = prompt.chars().count() + steps(&buffer[..cursor]);
    out.push(b'\r');
    if column > 0 {
        out.extend_from_slice(format!("\x1b[{}C", column).as_bytes());
    }
    emit_bytes(&out);
}

/// Supplies candidate completions for the word under the cursor.
pub trait Completer {
    /// Candidates for `word`, which is the first word of the line when
    /// `is_command` is set.
    fn candidates(&self, word: &str, is_command: bool) -> Vec<String>;
}

fn complete(completer: &dyn Completer, buffer: &mut Vec<u8>, cursor: &mut usize, prompt: &str) {
    // The word ends at the cursor and starts after the last separator before
    // it. Separators are ASCII, so the word's bytes are whole characters.
    let before = &buffer[..*cursor];
    let start = before
        .iter()
        .rposition(|&b| b.is_ascii_whitespace() || b == b'|' || b == b';' || b == b'>' || b == b'<')
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = String::from_utf8_lossy(&before[start..]).into_owned();
    let word = word.as_str();
    let is_command = before[..start].iter().all(|b| b.is_ascii_whitespace());

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
    buffer.splice(start..*cursor, insert.bytes());
    *cursor = start + insert.len();
    if matches.len() == 1 && !insert.ends_with('/') {
        buffer.insert(*cursor, b' ');
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
