//! File names: which names FAT can hold, how two names are compared, and how
//! a name is stored as a short name and long-name entries.
//!
//! **Which names.** Linux's vfat rules (fs/fat/namei_vfat.c): trailing periods
//! are dropped (`vfat_striptail_len`), so `page.` is `page`; a name left empty
//! by that, a name ending in a space, or one holding a character below 0x20 or
//! one of `* ? < > | " : / \` (`vfat_bad_char`) is EINVAL; a name of more than
//! 255 UTF-16 units is ENAMETOOLONG. Everything else a program can pass is
//! valid UTF-8 and is stored as its UTF-16 units, characters outside the Basic
//! Multilingual Plane as surrogate pairs.
//!
//! **Comparing.** ASCII letters match in either case and every other character
//! only itself, code point by code point, with no Unicode normalisation: this
//! is Linux vfat with `utf8`, whose case table covers ASCII only. So `Index.HTML`
//! finds `index.html`, `É.txt` does not find `é.txt`, and a name the Mac wrote
//! decomposed is found only by its decomposed spelling. A name keeps the case
//! it was created with. Trailing periods are dropped from both names first.
//!
//! **Storing.** A name that is already a valid upper-case 8.3 name gets a
//! short entry alone. Any other name gets long-name entries holding it and a
//! short name made from it with a numeric tail, `~1` upwards, that no other
//! entry in the directory uses. This is Linux's default `shortname=mixed`. The
//! short name is made as fatgen103's "Basis-Name Generation Algorithm" makes
//! it, except that periods before the last one are skipped rather than ending
//! the name, as Windows and Linux do. Generated short names are ASCII only: a
//! character with no ASCII upper-case form becomes `_`.
//!
//! **Reading short names.** Bytes 0x80 and above are code page 437, Linux's
//! default `codepage=437`. The lower-case flags Windows NT keeps in DIR_NTRes
//! (0x08 for the name, 0x10 for the extension) are honoured, as Linux does.

use super::{le16, put16, put8, FsError};
use alloc::string::String;
use alloc::vec::Vec;

pub type Short = [u8; 11];

/// The most UTF-16 units a long name holds, and the units in one entry.
pub const MAX_UNITS: usize = 255;
pub const UNITS_PER_ENTRY: usize = 13;
pub const MAX_LONG_ENTRIES: usize = 20;
/// Where a long-name entry's 13 units are: LDIR_Name1, LDIR_Name2 and
/// LDIR_Name3 (fatgen103, "FAT Long Directory Entries").
pub const UNIT_AT: [usize; UNITS_PER_ENTRY] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
/// LDIR_Ord's flag on the entry holding the end of the name.
pub const LAST_LONG_ENTRY: u8 = 0x40;
/// DIR_NTRes flags for a short name shown in lower case.
pub const LOWER_BASE: u8 = 0x08;
pub const LOWER_EXT: u8 = 0x10;
/// The largest numeric tail tried (fatgen103: "~1" to "~999999").
const MAX_TAIL: u32 = 999_999;

/// `name` with trailing periods dropped, which is what both a lookup and a
/// comparison use.
pub fn key(name: &str) -> &str {
    name.trim_end_matches('.')
}

/// Whether two names name the same entry.
pub fn same(a: &str, b: &str) -> bool {
    key(a).eq_ignore_ascii_case(key(b))
}

/// The name an entry created as `name` is given, or why FAT cannot hold it.
pub fn check(name: &str) -> Result<&str, FsError> {
    let kept = key(name);
    if kept.chars().map(char::len_utf16).sum::<usize>() > MAX_UNITS {
        return Err(FsError::NameTooLong);
    }
    if kept.is_empty() || kept.ends_with(' ') {
        return Err(FsError::BadName);
    }
    if kept.chars().any(|c| (c as u32) < 0x20 || matches!(c, '*' | '?' | '<' | '>' | '|' | '"' | ':' | '/' | '\\')) {
        return Err(FsError::BadName);
    }
    Ok(kept)
}

/// fatgen103, "FAT Long Directory Entries", ChkSum: the checksum every
/// long-name entry carries of the short name it belongs to.
pub fn checksum(short: &Short) -> u8 {
    short.iter().fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

/// Whether an ASCII byte may be in a generated short name: upper-case letters,
/// digits, and the punctuation fatgen103 allows ("FAT Directory Structure",
/// DIR_Name).
fn short_char(byte: u8) -> bool {
    byte.is_ascii_uppercase()
        || byte.is_ascii_digit()
        || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'(' | b')' | b'-' | b'@' | b'^' | b'_' | b'`' | b'{' | b'}' | b'~')
}

/// The short name `name` is, if it is exactly a valid upper-case 8.3 name:
/// one to eight characters, then optionally a period and one to three more,
/// all of them allowed in short names.
pub fn exact_short(name: &str) -> Option<Short> {
    let (base, ext) = match name.rsplit_once('.') {
        Some((base, ext)) if !ext.is_empty() => (base, ext),
        Some(_) => return None,
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
        return None;
    }
    if !base.bytes().chain(ext.bytes()).all(short_char) {
        return None;
    }
    let mut short = [b' '; 11];
    for (slot, byte) in short.iter_mut().zip(base.bytes()) {
        *slot = byte;
    }
    for (slot, byte) in short.iter_mut().skip(8).zip(ext.bytes()) {
        *slot = byte;
    }
    Some(short)
}

/// The short name as it is shown and looked up: the name, and a period and
/// the extension when there is one, without padding.
pub fn shown(short: &Short, lower: u8) -> Option<String> {
    let [b0, b1, b2, b3, b4, b5, b6, b7, e0, e1, e2] = *short;
    // fatgen103: 0x05 in the first byte stands for 0xE5, which is a
    // character in some code pages and also the mark of a deleted entry.
    let base = [if b0 == 0x05 { 0xE5 } else { b0 }, b1, b2, b3, b4, b5, b6, b7];
    let ext = [e0, e1, e2];
    let base_len = base.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let ext_len = ext.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    if base_len == 0 {
        return None;
    }
    let mut out = String::new();
    for &byte in base.iter().take(base_len) {
        out.push(oem_char(byte, lower & LOWER_BASE != 0)?);
    }
    if ext_len > 0 {
        out.push('.');
        for &byte in ext.iter().take(ext_len) {
            out.push(oem_char(byte, lower & LOWER_EXT != 0)?);
        }
    }
    if out == "." || out == ".." {
        return None;
    }
    Some(out)
}

/// A short-name byte as a character, or nothing for one no path can spell.
fn oem_char(byte: u8, lower: bool) -> Option<char> {
    match byte {
        0x00..=0x1F | b'/' => None,
        0x20..=0x7F => Some(if lower { byte.to_ascii_lowercase() } else { byte } as char),
        _ => CP437_HIGH.get((byte - 0x80) as usize).copied(),
    }
}

/// What a generated short name is made from.
struct Basis {
    base: [u8; 8],
    base_len: usize,
    ext: [u8; 3],
    ext_len: usize,
}

/// A long-name character as a short-name byte: nothing for a space or a
/// period, which are left out, the ASCII upper-case form where it is allowed,
/// and `_` for everything else.
fn basis_byte(c: char) -> Option<u8> {
    if c == ' ' || c == '.' {
        return None;
    }
    let upper = c.to_ascii_uppercase();
    if upper.is_ascii() && short_char(upper as u8) {
        Some(upper as u8)
    } else {
        Some(b'_')
    }
}

fn basis(name: &str) -> Basis {
    // The extension follows the last period, unless only periods and spaces
    // come before it, in which case the whole name is the base, as in
    // `vfat_create_shortname`.
    let (base_part, ext_part) = match name.rsplit_once('.') {
        Some((before, after)) if before.chars().any(|c| c != '.' && c != ' ') => (before, after),
        _ => (name, ""),
    };
    let mut basis = Basis { base: [b' '; 8], base_len: 0, ext: [b' '; 3], ext_len: 0 };
    for byte in base_part.chars().filter_map(basis_byte) {
        let Some(slot) = basis.base.get_mut(basis.base_len) else { break };
        *slot = byte;
        basis.base_len += 1;
    }
    for byte in ext_part.chars().filter_map(basis_byte) {
        let Some(slot) = basis.ext.get_mut(basis.ext_len) else { break };
        *slot = byte;
        basis.ext_len += 1;
    }
    if basis.base_len == 0 {
        basis.base = [b'_', b' ', b' ', b' ', b' ', b' ', b' ', b' '];
        basis.base_len = 1;
    }
    basis
}

/// The basis with `~n` in its name part, cutting the name part short so the
/// two fit in eight bytes (fatgen103, "The Numeric-Tail Generation
/// Algorithm").
fn with_tail(basis: &Basis, n: u32) -> Short {
    let mut digits = [b'0'; 7];
    let mut count = 0usize;
    let mut value = n;
    loop {
        if let Some(slot) = digits.get_mut(count) {
            *slot = b'0' + (value % 10) as u8;
        }
        count += 1;
        value /= 10;
        if value == 0 || count >= digits.len() {
            break;
        }
    }
    let keep = basis.base_len.min(8usize.saturating_sub(count + 1));
    let mut short = [b' '; 11];
    let mut at = 0usize;
    for &byte in basis.base.iter().take(keep) {
        put8(&mut short, at, byte);
        at += 1;
    }
    put8(&mut short, at, b'~');
    at += 1;
    for &digit in digits.iter().take(count).rev() {
        put8(&mut short, at, digit);
        at += 1;
    }
    for (i, &byte) in basis.ext.iter().take(basis.ext_len).enumerate() {
        put8(&mut short, 8 + i, byte);
    }
    short
}

/// The short name for a new entry `name`, which `check` has passed, and
/// whether long-name entries go with it; nothing if every numeric tail is
/// taken. `taken` says whether a short name is already used in the directory.
pub fn short_name(name: &str, taken: impl Fn(&Short) -> bool) -> Option<(Short, bool)> {
    if let Some(short) = exact_short(name) {
        if !taken(&short) {
            return Some((short, false));
        }
    }
    // A name that would be a valid 8.3 name in upper case keeps that short
    // name, without a tail, when it is free (fatgen103: no lossy conversion,
    // and the name fits).
    if name.is_ascii() {
        if let Some(short) = exact_short(&name.to_ascii_uppercase()) {
            if !taken(&short) {
                return Some((short, true));
            }
        }
    }
    let basis = basis(name);
    (1..=MAX_TAIL).map(|n| with_tail(&basis, n)).find(|short| !taken(short)).map(|short| (short, true))
}

/// The long-name entries for `units`, a name of 1 to 255 units, in the order
/// they are written, which is the end of the name first (fatgen103, "FAT Long
/// Directory Entries"). A name that does not fill its last entry is ended by
/// one 0x0000 unit and padded with 0xFFFF.
pub fn long_entries(units: &[u16], checksum: u8) -> Vec<[u8; 32]> {
    let count = units.len().div_ceil(UNITS_PER_ENTRY).min(MAX_LONG_ENTRIES);
    let mut out = Vec::with_capacity(count);
    for ord in (1..=count).rev() {
        let mut entry = [0u8; 32];
        let flag = if ord == count { LAST_LONG_ENTRY } else { 0 };
        put8(&mut entry, 0, ord as u8 | flag);
        put8(&mut entry, 11, super::dir::ATTR_LONG_NAME);
        put8(&mut entry, 13, checksum);
        let start = (ord - 1) * UNITS_PER_ENTRY;
        for (k, &at) in UNIT_AT.iter().enumerate() {
            let i = start + k;
            let unit = match units.get(i) {
                Some(&unit) => unit,
                None if i == units.len() => 0x0000,
                None => 0xFFFF,
            };
            put16(&mut entry, at, unit);
        }
        out.push(entry);
    }
    out
}

/// The 13 units of one long-name entry.
pub fn entry_units(entry: &[u8; 32]) -> [u16; UNITS_PER_ENTRY] {
    UNIT_AT.map(|at| le16(entry, at))
}

/// A long name read off the card as a string, or nothing if its UTF-16 is not
/// valid, as with an unpaired surrogate, or it holds a character no path can
/// spell. The entry is then shown by its short name, as fatgen103 says of a
/// long name that does not belong to its short entry.
pub fn from_units(units: &[u16]) -> Option<String> {
    let mut out = String::new();
    for c in char::decode_utf16(units.iter().copied()) {
        let c = c.ok()?;
        if c == '/' || c == '\0' {
            return None;
        }
        out.push(c);
    }
    if out.is_empty() || out == "." || out == ".." {
        return None;
    }
    Some(out)
}

/// Code page 437 from 0x80 to 0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{A0}',
];
