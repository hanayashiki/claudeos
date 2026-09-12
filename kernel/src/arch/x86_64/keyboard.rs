//! PS/2 keyboard. Scan code set 1, as the 8042 controller delivers it on a PC.

use super::io::inb;
use crate::sync::Spinlock;

const SCANCODE_LOWER: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'1'; t[0x03] = b'2'; t[0x04] = b'3'; t[0x05] = b'4'; t[0x06] = b'5';
    t[0x07] = b'6'; t[0x08] = b'7'; t[0x09] = b'8'; t[0x0A] = b'9'; t[0x0B] = b'0';
    t[0x0C] = b'-'; t[0x0D] = b'='; t[0x0E] = 0x7F; t[0x0F] = b'\t';
    t[0x10] = b'q'; t[0x11] = b'w'; t[0x12] = b'e'; t[0x13] = b'r'; t[0x14] = b't';
    t[0x15] = b'y'; t[0x16] = b'u'; t[0x17] = b'i'; t[0x18] = b'o'; t[0x19] = b'p';
    t[0x1A] = b'['; t[0x1B] = b']'; t[0x1C] = b'\n';
    t[0x1E] = b'a'; t[0x1F] = b's'; t[0x20] = b'd'; t[0x21] = b'f'; t[0x22] = b'g';
    t[0x23] = b'h'; t[0x24] = b'j'; t[0x25] = b'k'; t[0x26] = b'l'; t[0x27] = b';';
    t[0x28] = b'\''; t[0x29] = b'`'; t[0x2B] = b'\\';
    t[0x2C] = b'z'; t[0x2D] = b'x'; t[0x2E] = b'c'; t[0x2F] = b'v'; t[0x30] = b'b';
    t[0x31] = b'n'; t[0x32] = b'm'; t[0x33] = b','; t[0x34] = b'.'; t[0x35] = b'/';
    t[0x39] = b' ';
    t
};

const SCANCODE_UPPER: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'!'; t[0x03] = b'@'; t[0x04] = b'#'; t[0x05] = b'$'; t[0x06] = b'%';
    t[0x07] = b'^'; t[0x08] = b'&'; t[0x09] = b'*'; t[0x0A] = b'('; t[0x0B] = b')';
    t[0x0C] = b'_'; t[0x0D] = b'+'; t[0x0E] = 0x7F; t[0x0F] = b'\t';
    t[0x10] = b'Q'; t[0x11] = b'W'; t[0x12] = b'E'; t[0x13] = b'R'; t[0x14] = b'T';
    t[0x15] = b'Y'; t[0x16] = b'U'; t[0x17] = b'I'; t[0x18] = b'O'; t[0x19] = b'P';
    t[0x1A] = b'{'; t[0x1B] = b'}'; t[0x1C] = b'\n';
    t[0x1E] = b'A'; t[0x1F] = b'S'; t[0x20] = b'D'; t[0x21] = b'F'; t[0x22] = b'G';
    t[0x23] = b'H'; t[0x24] = b'J'; t[0x25] = b'K'; t[0x26] = b'L'; t[0x27] = b':';
    t[0x28] = b'"'; t[0x29] = b'~'; t[0x2B] = b'|';
    t[0x2C] = b'Z'; t[0x2D] = b'X'; t[0x2E] = b'C'; t[0x2F] = b'V'; t[0x30] = b'B';
    t[0x31] = b'N'; t[0x32] = b'M'; t[0x33] = b'<'; t[0x34] = b'>'; t[0x35] = b'?';
    t[0x39] = b' ';
    t
};

static SHIFT_HELD: Spinlock<bool> = Spinlock::new(false);
static CTRL_HELD: Spinlock<bool> = Spinlock::new(false);

/// Take the byte the keyboard controller is holding and turn it into the
/// character it stands for, or nothing when the key was a modifier, a release,
/// or a key with no character.
pub fn read_byte() -> Option<u8> {
    let scancode = unsafe { inb(0x60) };
    let released = scancode & 0x80 != 0;
    let code = (scancode & 0x7F) as usize;

    match code {
        0x2A | 0x36 => {
            *SHIFT_HELD.lock() = !released;
            return None;
        }
        0x1D => {
            *CTRL_HELD.lock() = !released;
            return None;
        }
        _ => {}
    }
    if released || code >= 128 {
        return None;
    }

    let shift = *SHIFT_HELD.lock();
    let ctrl = *CTRL_HELD.lock();
    let byte = if shift { SCANCODE_UPPER[code] } else { SCANCODE_LOWER[code] };
    if byte == 0 {
        return None;
    }
    let byte = if ctrl && byte.is_ascii_alphabetic() {
        byte.to_ascii_lowercase() - b'a' + 1
    } else {
        byte
    };
    Some(byte)
}

