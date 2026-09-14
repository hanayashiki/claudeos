//! The network to join, from `/etc/wifi.conf`.
//!
//! The file is three lines, `ssid=`, `psk=` and `country=`, and the first two
//! are secrets. What holds them here is built so that leaking them takes
//! writing new code rather than a slip: neither `Ssid` nor `Passphrase` has a
//! `Debug` or `Display`, so no `println!` or panic message can include one;
//! the only ways to get at the bytes are the two methods that put them into a
//! join request and the comparison against a scan result; and both zero their
//! storage when dropped. Parse errors name the line and the problem and never
//! the value.
//!
//! Once read, the file is removed from the filesystem, so that nothing running
//! on the board can print it back.

use alloc::vec::Vec;

pub const PATH: &str = "/etc/wifi.conf";

/// `IEEE80211_MAX_SSID_LEN`.
pub const MAX_SSID_LEN: usize = 32;
/// A WPA passphrase is 8 to 63 printable characters (IEEE 802.11i, Annex
/// H.4). The 64-character hexadecimal form is not accepted, since it is not
/// what this file holds.
const MIN_PASSPHRASE_LEN: usize = 8;
const MAX_PASSPHRASE_LEN: usize = 63;

/// The country when the file names none. The user is in Japan.
const DEFAULT_COUNTRY: [u8; 2] = *b"JP";

pub struct Ssid {
    bytes: [u8; MAX_SSID_LEN],
    len: usize,
}

impl Ssid {
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether a network the chip reports is this one.
    pub fn matches(&self, other: &[u8]) -> bool {
        other.len() == self.len && other == &self.bytes[..self.len]
    }

    /// Copy the name into a join request's SSID field.
    pub fn copy_into(&self, field: &mut [u8]) {
        field[..self.len].copy_from_slice(&self.bytes[..self.len]);
    }
}

impl Drop for Ssid {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

pub struct Passphrase {
    bytes: [u8; MAX_PASSPHRASE_LEN],
    len: usize,
}

impl Passphrase {
    pub fn len(&self) -> usize {
        self.len
    }

    /// Copy the passphrase into a PMK request's key field.
    pub fn copy_into(&self, field: &mut [u8]) {
        field[..self.len].copy_from_slice(&self.bytes[..self.len]);
    }
}

impl Drop for Passphrase {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Two letters, which are not a secret and may be printed.
#[derive(Clone, Copy)]
pub struct Country(pub [u8; 2]);

impl core::fmt::Display for Country {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.0[0] as char, self.0[1] as char)
    }
}

pub struct Config {
    pub ssid: Ssid,
    pub passphrase: Passphrase,
    pub country: Country,
}

/// Why the file could not be used. None of these carries any of the file's
/// contents.
#[derive(Clone, Copy)]
pub enum Error {
    NotFound,
    Unreadable,
    /// A line with no `=`, by line number.
    NoEquals(usize),
    /// A key this file does not have, by line number.
    UnknownKey(usize),
    NoSsid,
    NoPassphrase,
    SsidLength,
    PassphraseLength,
    PassphraseCharacters,
    Country,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotFound => write!(f, "{} does not exist", PATH),
            Error::Unreadable => write!(f, "{} could not be read", PATH),
            Error::NoEquals(line) => write!(f, "line {} of {} has no '='", line, PATH),
            Error::UnknownKey(line) => write!(f, "line {} of {} has a key other than ssid, psk or country", line, PATH),
            Error::NoSsid => write!(f, "{} has no ssid line", PATH),
            Error::NoPassphrase => write!(f, "{} has no psk line", PATH),
            Error::SsidLength => write!(f, "the ssid in {} is not 1 to 32 bytes", PATH),
            Error::PassphraseLength => write!(f, "the psk in {} is not 8 to 63 characters", PATH),
            Error::PassphraseCharacters => write!(f, "the psk in {} has a character outside printable ASCII", PATH),
            Error::Country => write!(f, "the country in {} is not two capital letters", PATH),
        }
    }
}

impl Config {
    /// The network's PMK, derived from the passphrase with the name as the
    /// salt. This is the third way the bytes are used, and like the other two
    /// it hands none of them out.
    pub fn pmk(&self) -> super::wpa::Pmk {
        super::wpa::Pmk::from_passphrase(
            &self.passphrase.bytes[..self.passphrase.len],
            &self.ssid.bytes[..self.ssid.len],
        )
    }
}

/// Parse the file's text.
pub fn parse(text: &[u8]) -> Result<Config, Error> {
    let mut ssid: Option<Ssid> = None;
    let mut passphrase: Option<Passphrase> = None;
    let mut country = Country(DEFAULT_COUNTRY);
    for (index, line) in text.split(|&b| b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(equals) = line.iter().position(|&b| b == b'=') else {
            return Err(Error::NoEquals(index + 1));
        };
        let (key, value) = (&line[..equals], &line[equals + 1..]);
        match key {
            b"ssid" => {
                if value.is_empty() || value.len() > MAX_SSID_LEN {
                    return Err(Error::SsidLength);
                }
                let mut bytes = [0u8; MAX_SSID_LEN];
                bytes[..value.len()].copy_from_slice(value);
                ssid = Some(Ssid { bytes, len: value.len() });
            }
            b"psk" => {
                if value.len() < MIN_PASSPHRASE_LEN || value.len() > MAX_PASSPHRASE_LEN {
                    return Err(Error::PassphraseLength);
                }
                if !value.iter().all(|&b| (0x20..0x7f).contains(&b)) {
                    return Err(Error::PassphraseCharacters);
                }
                let mut bytes = [0u8; MAX_PASSPHRASE_LEN];
                bytes[..value.len()].copy_from_slice(value);
                passphrase = Some(Passphrase { bytes, len: value.len() });
            }
            b"country" => {
                if value.len() != 2 || !value.iter().all(|b| b.is_ascii_uppercase()) {
                    return Err(Error::Country);
                }
                country = Country([value[0], value[1]]);
            }
            _ => return Err(Error::UnknownKey(index + 1)),
        }
    }
    Ok(Config {
        ssid: ssid.ok_or(Error::NoSsid)?,
        passphrase: passphrase.ok_or(Error::NoPassphrase)?,
        country,
    })
}

/// Read the file, parse it, and take it off the filesystem.
///
/// The copy read into memory is zeroed before it is freed, whatever the
/// outcome. The file is removed only when it parsed, so a broken one can still
/// be looked at on the board.
pub fn load() -> Result<Config, Error> {
    let node = crate::fs::lookup(PATH).map_err(|_| Error::NotFound)?;
    let size = node.size() as usize;
    let mut text: Vec<u8> = alloc::vec![0u8; size];
    let read = node.read_at(crate::fs::Offset::START, &mut text).map_err(|_| Error::Unreadable);
    let result = match read {
        Ok(n) => parse(&text[..n]),
        Err(error) => Err(error),
    };
    text.fill(0);
    drop(text);
    drop(node);
    if result.is_ok() {
        let _ = crate::fs::unlink(PATH, false);
    }
    result
}
