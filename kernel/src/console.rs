//! Console: serial and PS/2 keyboard input with a line discipline, and
//! serial output.

use crate::abi::{Errno, Termios, ECHO, ICANON};
use crate::serial::SERIAL;
use crate::sync::Spinlock;
use core::fmt::Write;

const RING_SIZE: usize = 1024;

struct Ring {
    buf: [u8; RING_SIZE],
    head: usize,
    tail: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring { buf: [0; RING_SIZE], head: 0, tail: 0 }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % RING_SIZE;
        if next == self.tail {
            return; // full; drop the newest byte
        }
        self.buf[self.head] = byte;
        self.head = next;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % RING_SIZE;
        Some(byte)
    }

    fn len(&self) -> usize {
        (self.head + RING_SIZE - self.tail) % RING_SIZE
    }
}

static INPUT: Spinlock<Ring> = Spinlock::new(Ring::new());

/// Readers blocked waiting for the terminal.
static WAITING: crate::sched::WaitQueue = crate::sched::WaitQueue::new();

/// The line being edited, and then handed to readers once it is complete.
struct LineBuffer {
    data: [u8; RING_SIZE],
    len: usize,
    pos: usize,
    /// Set once the line has been terminated. Until then the contents are
    /// still being edited and must not be handed to a reader, or backspace
    /// would have nothing left to erase.
    ready: bool,
    /// Set when the line was terminated by Ctrl-D on an empty line.
    eof: bool,
}

static LINE: Spinlock<LineBuffer> = Spinlock::new(LineBuffer {
    data: [0; RING_SIZE],
    len: 0,
    pos: 0,
    ready: false,
    eof: false,
});

pub static TERMIOS: Spinlock<Termios> = Spinlock::new(Termios {
    c_iflag: crate::abi::ICRNL | crate::abi::IXON,
    c_oflag: crate::abi::OPOST | crate::abi::ONLCR,
    c_cflag: 0o2277,
    c_lflag: crate::abi::ISIG | ICANON | ECHO | crate::abi::ECHOE,
    c_line: 0,
    // VINTR, VQUIT, VERASE, VKILL, VEOF, VTIME, VMIN, VSWTC, VSTART, VSTOP,
    // VSUSP, then the rest unset.
    c_cc: [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
           0, 0, 0, 0, 0, 0],
    c_ispeed: 38400,
    c_ospeed: 38400,
});

/// Feed one received byte to the terminal.
///
/// Signal-generating characters are acted on here, in the interrupt that
/// delivered them, because the process that will eventually read the terminal
/// is usually blocked waiting for the job that needs the signal.
pub fn push_byte(byte: u8) {
    let (isig, intr, quit, susp) = {
        let termios = TERMIOS.lock();
        (
            termios.c_lflag & crate::abi::ISIG != 0,
            termios.c_cc[0],
            termios.c_cc[1],
            termios.c_cc[10],
        )
    };

    if isig && byte != 0 {
        let signal = if byte == intr {
            Some((crate::abi::SIGINT, "^C\n"))
        } else if byte == quit {
            Some((crate::abi::SIGQUIT, "^\\\n"))
        } else if byte == susp {
            Some((crate::abi::SIGTSTP, "^Z\n"))
        } else {
            None
        };
        if let Some((signal, mark)) = signal {
            {
                let mut line = LINE.lock();
                line.len = 0;
                line.pos = 0;
                line.ready = false;
            }
            echo(mark.as_bytes());
            crate::sched::signal_foreground(signal);
            return;
        }
    }

    INPUT.lock().push(byte);
    WAITING.wake_all();
    crate::sched::io_ready();
}

pub fn available() -> usize {
    let line = LINE.lock();
    if line.ready && line.pos < line.len {
        return line.len - line.pos;
    }
    drop(line);
    INPUT.lock().len()
}

pub fn write(buf: &[u8]) {
    let mut serial = SERIAL.lock();
    for &byte in buf {
        serial.write_byte(byte);
    }
}

fn echo(bytes: &[u8]) {
    let mut serial = SERIAL.lock();
    for &byte in bytes {
        serial.write_byte(byte);
    }
}

/// Read from the console, honouring the current terminal settings.
/// A process that is not in the terminal's foreground group must not take the
/// input the foreground job is waiting for. It is stopped with SIGTTIN and
/// picks the read up again once it is continued in the foreground.
fn claim_terminal() -> Result<(), Errno> {
    loop {
        let foreground = crate::sched::foreground();
        if foreground == 0 {
            return Ok(());
        }
        let task = crate::sched::current();
        if task.pid == 1 || task.pgid == 0 || task.pgid == foreground {
            return Ok(());
        }
        // A process that has said it does not want SIGTTIN cannot be stopped
        // by it, so the read fails outright rather than looping.
        let bit = 1u64 << (crate::abi::SIGTTIN as u64 & 63);
        let action = task.signal_actions[crate::abi::SIGTTIN as usize];
        if task.signal_mask & bit != 0 || action.handler == crate::signal::SIG_IGN {
            return Err(Errno::EIO);
        }
        let pgid = task.pgid;
        crate::sched::signal_group(pgid, crate::abi::SIGTTIN);
        crate::sched::stop_for_signal(crate::abi::SIGTTIN);
        // Continued. Anything else waiting unwinds the read so it can be
        // taken on the way out.
        if crate::sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
    }
}

pub fn read(buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.is_empty() {
        return Ok(0);
    }
    claim_terminal()?;
    let (canonical, echo_on) = {
        let termios = TERMIOS.lock();
        (termios.c_lflag & ICANON != 0, termios.c_lflag & ECHO != 0)
    };

    if !canonical {
        // Raw mode: block until at least one byte is available.
        loop {
            let mut n = 0;
            while n < buf.len() {
                match INPUT.lock().pop() {
                    Some(byte) => {
                        buf[n] = byte;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n > 0 {
                if echo_on {
                    echo(&buf[..n]);
                }
                return Ok(n);
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            WAITING.wait_until(|| {
                INPUT.lock().len() > 0 || crate::sched::has_pending_signal()
            });
        }
    }

    loop {
        // Hand out whatever is left of the current line, once it is finished.
        {
            let mut line = LINE.lock();
            if line.ready && line.pos < line.len {
                let n = buf.len().min(line.len - line.pos);
                let pos = line.pos;
                buf[..n].copy_from_slice(&line.data[pos..pos + n]);
                line.pos += n;
                if line.pos == line.len {
                    line.len = 0;
                    line.pos = 0;
                    line.ready = false;
                }
                return Ok(n);
            }
            if line.eof {
                line.eof = false;
                line.ready = false;
                line.len = 0;
                line.pos = 0;
                return Ok(0);
            }
            // A finished but empty line reads as end of file.
            if line.ready && line.len == 0 {
                line.ready = false;
                return Ok(0);
            }
        }

        if !gather_line(echo_on) {
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            WAITING.wait_until(|| {
                INPUT.lock().len() > 0 || crate::sched::has_pending_signal()
            });
        }
    }
}

/// Pull bytes from the input ring into the line buffer. Returns true once a
/// complete line (or an end-of-file) is ready.
fn gather_line(echo_on: bool) -> bool {
    loop {
        let byte = match INPUT.lock().pop() {
            Some(byte) => byte,
            None => return false,
        };

        let mut line = LINE.lock();
        match byte {
            b'\n' | b'\r' => {
                let at = line.len;
                if at < RING_SIZE {
                    line.data[at] = b'\n';
                    line.len = at + 1;
                }
                line.pos = 0;
                line.ready = true;
                drop(line);
                if echo_on {
                    echo(b"\n");
                }
                return true;
            }
            0x7F | 0x08 => {
                if line.len > 0 {
                    line.len -= 1;
                    drop(line);
                    if echo_on {
                        echo(b"\x08 \x08");
                    }
                }
            }
            0x04 => {
                // Ctrl-D: end the line as it stands, or report end of file if
                // nothing has been typed yet.
                if line.len == 0 {
                    line.eof = true;
                } else {
                    line.pos = 0;
                }
                line.ready = true;
                return true;
            }
            0x15 => {
                // Ctrl-U: kill the line.
                let count = line.len;
                line.len = 0;
                drop(line);
                if echo_on {
                    for _ in 0..count {
                        echo(b"\x08 \x08");
                    }
                }
            }
            byte => {
                let at = line.len;
                if at < RING_SIZE - 1 {
                    line.data[at] = byte;
                    line.len = at + 1;
                    drop(line);
                    if echo_on {
                        echo(&[byte]);
                    }
                }
            }
        }
    }
}

/// Drain the UART receive FIFO into the input ring.
pub fn serial_irq() {
    loop {
        let byte = { SERIAL.lock().try_read() };
        match byte {
            Some(b) => push_byte(b),
            None => break,
        }
    }
}

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

pub fn keyboard_irq() {
    let scancode = unsafe { crate::io::inb(0x60) };
    let released = scancode & 0x80 != 0;
    let code = (scancode & 0x7F) as usize;

    match code {
        0x2A | 0x36 => {
            *SHIFT_HELD.lock() = !released;
            return;
        }
        0x1D => {
            *CTRL_HELD.lock() = !released;
            return;
        }
        _ => {}
    }
    if released || code >= 128 {
        return;
    }

    let shift = *SHIFT_HELD.lock();
    let ctrl = *CTRL_HELD.lock();
    let byte = if shift { SCANCODE_UPPER[code] } else { SCANCODE_LOWER[code] };
    if byte == 0 {
        return;
    }
    let byte = if ctrl && byte.is_ascii_alphabetic() {
        byte.to_ascii_lowercase() - b'a' + 1
    } else {
        byte
    };
    push_byte(byte);
}

pub fn init() {
    // Anything that arrived while the kernel was still coming up is sitting
    // in the UART; move it into the ring before interrupts take over.
    serial_irq();
    SERIAL.lock().enable_rx_interrupt();
    crate::cpu::pic::unmask(1); // keyboard
    crate::cpu::pic::unmask(4); // COM1
}

pub fn print_fmt(args: core::fmt::Arguments) {
    let _ = SERIAL.lock().write_fmt(args);
}
