//! The WiFi chip's NVRAM, turned from the text file it ships as into the form
//! the firmware reads out of the top of its RAM.
//!
//! A port of `brcmf_fw_nvram_strip` in Linux's
//! `drivers/net/wireless/broadcom/brcm80211/brcmfmac/firmware.c`, down to its
//! state machine, because the firmware reads the result byte by byte and a
//! different reading of an odd line would give it different settings: every
//! `key=value` line becomes the same bytes followed by a NUL, comments and
//! blank lines disappear, the whole is padded with NULs to a four-byte
//! boundary with at least one, and a word goes on the end saying how many
//! words there are.
//!
//! Two things the reference does are left out and refused instead. A file
//! holding settings for several PCIe devices (a `devpath` or `pcie/` key) is
//! stripped down to one device there; this chip is on SDIO and its file has
//! neither, so such a file is an error here. And the reference replaces
//! `macaddr` when the platform supplies an address; the Pi's device tree gives
//! the WiFi node none, so Linux on a Pi leaves it, and so does this.

use alloc::vec::Vec;

/// `BRCMF_FW_MAX_NVRAM_SIZE`.
pub const MAX_NVRAM_SIZE: usize = 64000;
/// Added when the file does not set `boardrev`. `BRCMF_FW_DEFAULT_BOARDREV`.
const DEFAULT_BOARDREV: &[u8] = b"boardrev=0xff";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Nothing survived stripping.
    Empty,
    /// The file describes several PCIe devices.
    MultipleDevices,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Key,
    Value,
    Comment,
    End,
}

/// `is_nvram_char`: printable ASCII, less the comment marker.
fn is_nvram_char(c: u8) -> bool {
    c != b'#' && (0x20..0x7f).contains(&c)
}

/// `is_whitespace`.
fn is_whitespace(c: u8) -> bool {
    c == b' ' || c == b'\r' || c == b'\n' || c == b'\t'
}

struct Parser<'a> {
    data: &'a [u8],
    out: Vec<u8>,
    pos: usize,
    entry: usize,
    multi_device: bool,
    boardrev_found: bool,
}

impl Parser<'_> {
    /// The byte at `at`, or the NUL the reference's C string has after its
    /// last byte.
    fn byte(&self, at: usize) -> u8 {
        self.data.get(at).copied().unwrap_or(0)
    }

    fn starts(&self, prefix: &[u8]) -> bool {
        self.data[self.entry..].starts_with(prefix)
    }

    /// `brcmf_nvram_handle_idle`.
    fn idle(&mut self) -> State {
        let c = self.byte(self.pos);
        if c == b'\n' {
            return State::Comment;
        }
        if is_whitespace(c) || c == 0 {
            self.pos += 1;
            return State::Idle;
        }
        if c == b'#' {
            return State::Comment;
        }
        if is_nvram_char(c) {
            self.entry = self.pos;
            return State::Key;
        }
        // An invalid character, which the reference skips over with a warning.
        self.pos += 1;
        State::Idle
    }

    /// `brcmf_nvram_handle_key`.
    fn key(&mut self) -> State {
        let c = self.byte(self.pos);
        let mut state = State::Key;
        if c == b'=' {
            // RAW1 lines are treated as comments.
            state = if self.starts(b"RAW1") { State::Comment } else { State::Value };
            if self.starts(b"devpath") || self.starts(b"pcie/") {
                self.multi_device = true;
            }
            if self.starts(b"boardrev") {
                self.boardrev_found = true;
            }
        } else if !is_nvram_char(c) || c == b' ' {
            // '=' expected: the entry is skipped.
            return State::Comment;
        }
        self.pos += 1;
        state
    }

    /// `brcmf_nvram_handle_value`.
    fn value(&mut self) -> State {
        let c = self.byte(self.pos);
        if !is_nvram_char(c) {
            let entry = &self.data[self.entry..self.pos];
            if self.out.len() + entry.len() + 1 >= MAX_NVRAM_SIZE {
                return State::End;
            }
            self.out.extend_from_slice(entry);
            self.out.push(0);
            return State::Idle;
        }
        self.pos += 1;
        State::Value
    }

    /// `brcmf_nvram_handle_comment`: everything up to and including the next
    /// newline, or to the end.
    fn comment(&mut self) -> State {
        let rest = &self.data[self.pos.min(self.data.len())..];
        let skip = match rest.iter().position(|&b| b == b'\n') {
            Some(at) => at,
            // `strchr(sol, '\0')`: the terminator just past the data, or a NUL
            // inside it.
            None => rest.iter().position(|&b| b == 0).unwrap_or(rest.len()),
        };
        self.pos += skip + 1;
        State::Idle
    }
}

/// The NVRAM as the firmware reads it, length word included.
pub fn strip(data: &[u8]) -> Result<Vec<u8>, Error> {
    // The reference limits what it parses to the maximum size, since some
    // files are mostly comments.
    let data = &data[..data.len().min(MAX_NVRAM_SIZE)];
    let mut parser = Parser {
        data,
        out: Vec::with_capacity(data.len() + DEFAULT_BOARDREV.len() + 8),
        pos: 0,
        entry: 0,
        multi_device: false,
        boardrev_found: false,
    };
    let mut state = State::Idle;
    while parser.pos < data.len() {
        state = match state {
            State::Idle => parser.idle(),
            State::Key => parser.key(),
            State::Value => parser.value(),
            State::Comment => parser.comment(),
            State::End => State::End,
        };
        if state == State::End {
            break;
        }
    }
    if parser.multi_device {
        return Err(Error::MultipleDevices);
    }
    if parser.out.is_empty() {
        return Err(Error::Empty);
    }

    // `brcmf_fw_add_defaults`.
    if !parser.boardrev_found {
        parser.out.extend_from_slice(DEFAULT_BOARDREV);
        parser.out.push(0);
    }

    // Round up past at least one NUL, then the token: the length in words in
    // the low half and its complement in the high half.
    let mut out = parser.out;
    let length = (out.len() + 1).div_ceil(4) * 4;
    out.resize(length, 0);
    let words = (length / 4) as u32;
    let token = (!words << 16) | (words & 0xFFFF);
    out.extend_from_slice(&token.to_le_bytes());
    Ok(out)
}
