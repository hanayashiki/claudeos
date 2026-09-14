//! The SD host controller the WiFi chip is wired to.
//!
//! This is the Arasan SDHCI at 0x7e300000, the block Raspberry Pi's tree calls
//! `mmcnr` and upstream's calls `sdhci`, with GPIO 34 to 39 routed to it. The
//! register map is the standard one (`drivers/mmc/host/sdhci.h`). Two things
//! about this integration are not standard, and both reference drivers work
//! around both the same way -- `bcm2835-mmc.c` in Raspberry Pi's tree, which
//! is what `mmcnr` binds to on a Pi, and `sdhci-iproc.c` upstream:
//!
//! **Only 32-bit register access works.** An 8- or 16-bit register is read
//! as the 32-bit word it lives in, changed, and written back whole.
//!
//! **A write can be lost if another reaches the same register within two SD
//! clock cycles.** So every write is followed by a pause of two clock periods
//! plus a microsecond (BCM2835_SDHCI_WRITE_DELAY), and the transfer mode
//! register, which shares a word with the command register, is not written on
//! its own: it is held and goes out in the same write as the command, the way
//! `bcm2835_mmc_writew` keeps it in `host->shadow`. The data port is exempt,
//! as the reference says, and is written without the pause.
//!
//! Nothing here takes the controller's interrupt. Every command is polled to
//! completion, which is what makes this usable from one kernel task that
//! nobody else touches the controller from.
//!
//! Data moves by PIO through the buffer register, the path `bcm2835-mmc.c`
//! uses for small transfers. The DMA path it uses for large ones goes through
//! the VideoCore's DMA controller and is not ported.

use super::delay::{spin_us, Deadline};

// ---------------------------------------------------------------------------
// Registers, `sdhci.h`
// ---------------------------------------------------------------------------

const BLOCK_SIZE: u64 = 0x04; // 16 bits, and BLOCK_COUNT in the top half
const ARGUMENT: u64 = 0x08;
const TRANSFER_MODE: u64 = 0x0C; // 16 bits, and COMMAND in the top half
const RESPONSE: u64 = 0x10;
const BUFFER: u64 = 0x20;
const PRESENT_STATE: u64 = 0x24;
const HOST_CONTROL: u64 = 0x28; // byte 0; POWER_CONTROL is byte 1
const CLOCK_CONTROL: u64 = 0x2C; // 16 bits; TIMEOUT_CONTROL byte 2, SOFTWARE_RESET byte 3
const INT_STATUS: u64 = 0x30;
const INT_ENABLE: u64 = 0x34;
const SIGNAL_ENABLE: u64 = 0x38;
const CAPABILITIES: u64 = 0x40;
const CAPABILITIES_1: u64 = 0x44;
const SLOT_INT_STATUS: u64 = 0xFC; // HOST_VERSION is the top half

const CMD_INHIBIT: u32 = 0x0000_0001;
const DATA_INHIBIT: u32 = 0x0000_0002;
const SPACE_AVAILABLE: u32 = 0x0000_0400;
const DATA_AVAILABLE: u32 = 0x0000_0800;

const CTRL_4BITBUS: u32 = 0x02;
const POWER_ON: u32 = 0x01;
const POWER_330: u32 = 0x0E;

const CLOCK_INT_EN: u32 = 0x0001;
const CLOCK_INT_STABLE: u32 = 0x0002;
const CLOCK_CARD_EN: u32 = 0x0004;
const DIVIDER_SHIFT: u32 = 8;
const DIVIDER_HI_SHIFT: u32 = 6;
const DIV_MASK: u32 = 0xFF;
const DIV_HI_MASK: u32 = 0x300;
const DIV_MASK_LEN: u32 = 8;
/// The largest divisor a version 3.00 controller has, `SDHCI_MAX_DIV_SPEC_300`.
const MAX_DIV_SPEC_300: u32 = 2046;

pub const RESET_ALL: u32 = 0x01;
pub const RESET_CMD: u32 = 0x02;
pub const RESET_DATA: u32 = 0x04;

const INT_RESPONSE: u32 = 0x0000_0001;
const INT_DATA_END: u32 = 0x0000_0002;
const INT_SPACE_AVAIL: u32 = 0x0000_0010;
const INT_DATA_AVAIL: u32 = 0x0000_0020;
pub const INT_CARD_INT: u32 = 0x0000_0100;
const INT_ERROR: u32 = 0x0000_8000;
const INT_TIMEOUT: u32 = 0x0001_0000;
const INT_CRC: u32 = 0x0002_0000;
const INT_END_BIT: u32 = 0x0004_0000;
const INT_INDEX: u32 = 0x0008_0000;
const INT_DATA_TIMEOUT: u32 = 0x0010_0000;
const INT_DATA_CRC: u32 = 0x0020_0000;
const INT_DATA_END_BIT: u32 = 0x0040_0000;
const INT_BUS_POWER: u32 = 0x0080_0000;
const INT_CMD_ERRORS: u32 = INT_TIMEOUT | INT_CRC | INT_END_BIT | INT_INDEX;
const INT_DATA_ERRORS: u32 = INT_DATA_TIMEOUT | INT_DATA_CRC | INT_DATA_END_BIT;

const TRNS_BLK_CNT_EN: u32 = 0x02;
const TRNS_AUTO_CMD12: u32 = 0x04;
const TRNS_AUTO_CMD23: u32 = 0x08;
const TRNS_READ: u32 = 0x10;
const TRNS_MULTI: u32 = 0x20;

const CMD_RESP_NONE: u32 = 0x00;
const CMD_RESP_LONG: u32 = 0x01;
const CMD_RESP_SHORT: u32 = 0x02;
const CMD_RESP_SHORT_BUSY: u32 = 0x03;
const CMD_CRC: u32 = 0x08;
const CMD_INDEX: u32 = 0x10;
const CMD_DATA: u32 = 0x20;

/// The data timeout counter's value, `TIMEOUT_VAL` in `bcm2835-mmc.c`.
const TIMEOUT_VAL: u32 = 0xE;
/// `SDHCI_DEFAULT_BOUNDARY_ARG`, the log2 of 512 KiB less twelve, which
/// `bcm2835_mmc_prepare_data` puts in the top bits of the block size.
const DEFAULT_BOUNDARY_ARG: u32 = 7;
/// `MIN_FREQ` in `bcm2835-mmc.c`: the write pause is never computed from a
/// clock slower than this.
const MIN_FREQ: u32 = 400_000;

/// The interrupt sources polled for, which are the ones `bcm2835_mmc_init`
/// and `bcm2835_mmc_set_transfer_irqs` enable for a PIO transfer.
const INT_POLLED: u32 = INT_BUS_POWER
    | INT_DATA_END_BIT
    | INT_DATA_CRC
    | INT_DATA_TIMEOUT
    | INT_INDEX
    | INT_END_BIT
    | INT_CRC
    | INT_TIMEOUT
    | INT_DATA_END
    | INT_RESPONSE
    | INT_DATA_AVAIL
    | INT_SPACE_AVAIL;

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// What a command's response looks like, in the flags of
/// `include/linux/mmc/core.h`, which is what decides the controller's
/// response type and checks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Response(u8);

const RSP_PRESENT: u8 = 1 << 0;
const RSP_136: u8 = 1 << 1;
const RSP_CRC: u8 = 1 << 2;
const RSP_BUSY: u8 = 1 << 3;
const RSP_OPCODE: u8 = 1 << 4;

impl Response {
    pub const NONE: Response = Response(0);
    pub const R1: Response = Response(RSP_PRESENT | RSP_CRC | RSP_OPCODE);
    pub const R1B: Response = Response(RSP_PRESENT | RSP_CRC | RSP_OPCODE | RSP_BUSY);
    pub const R2: Response = Response(RSP_PRESENT | RSP_136 | RSP_CRC);
    pub const R3: Response = Response(RSP_PRESENT);
    pub const R4: Response = Response(RSP_PRESENT);
    pub const R5: Response = Response(RSP_PRESENT | RSP_CRC | RSP_OPCODE);
    pub const R6: Response = Response(RSP_PRESENT | RSP_CRC | RSP_OPCODE);
    pub const R7: Response = Response(RSP_PRESENT | RSP_CRC | RSP_OPCODE);

    /// The command register's low byte for this response, as
    /// `bcm2835_mmc_send_command` builds it.
    pub const fn flags(self, data: bool) -> u32 {
        let mut flags = if self.0 & RSP_PRESENT == 0 {
            CMD_RESP_NONE
        } else if self.0 & RSP_136 != 0 {
            CMD_RESP_LONG
        } else if self.0 & RSP_BUSY != 0 {
            CMD_RESP_SHORT_BUSY
        } else {
            CMD_RESP_SHORT
        };
        if self.0 & RSP_CRC != 0 {
            flags |= CMD_CRC;
        }
        if self.0 & RSP_OPCODE != 0 {
            flags |= CMD_INDEX;
        }
        if data {
            flags |= CMD_DATA;
        }
        flags
    }

    fn busy(self) -> bool {
        self.0 & RSP_BUSY != 0
    }
}

#[derive(Clone, Copy)]
pub struct Command {
    pub opcode: u8,
    pub argument: u32,
    pub response: Response,
}

/// Bytes to move with a command, in whole blocks.
pub enum Data<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

pub struct Transfer<'a> {
    pub block_size: usize,
    pub blocks: usize,
    pub data: Data<'a>,
}

impl Transfer<'_> {
    fn len(&self) -> usize {
        match &self.data {
            Data::Read(buf) => buf.len(),
            Data::Write(buf) => buf.len(),
        }
    }
}

/// What went wrong, with the interrupt status at the time so the log says
/// which bits the controller raised.
#[derive(Clone, Copy)]
pub struct Error {
    pub what: &'static str,
    pub status: u32,
}

impl Error {
    /// The card did not answer at all, which is what an empty slot looks like
    /// and not a fault in the controller.
    pub fn is_timeout(&self) -> bool {
        self.status & INT_TIMEOUT != 0
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} (status {:#010x})", self.what, self.status)
    }
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

/// How long a line may stay inhibited before a command. `bcm2835_mmc_send_command`
/// waits 1000 times 10 microseconds.
const INHIBIT_TRIES: u32 = 1000;
const INHIBIT_STEP_US: u64 = 10;
/// How long a command or its data may take before the controller is taken to
/// have hung. The reference arms a ten second timer for every request.
const REQUEST_TIMEOUT_MS: u64 = 10_000;

/// The divisor field for a target clock: the smallest even divisor that does
/// not overshoot, or one when the base clock is already slow enough.
/// `bcm2835_mmc_set_clock`. Returns the register bits and the clock that
/// results.
pub const fn clock_divider(base: u32, target: u32) -> (u32, u32) {
    let div = if base <= target {
        1
    } else {
        let mut div = 2;
        while div < MAX_DIV_SPEC_300 {
            if base / div <= target {
                break;
            }
            div += 2;
        }
        div
    };
    let actual = base / div;
    let field = div >> 1;
    let bits = ((field & DIV_MASK) << DIVIDER_SHIFT)
        | (((field & DIV_HI_MASK) >> DIV_MASK_LEN) << DIVIDER_HI_SHIFT);
    (bits, actual)
}

pub struct Sdhci {
    /// Kernel address of the register block.
    regs: u64,
    /// The clock the bus clock is divided from, in hertz.
    base_clock: u32,
    /// The bus clock as last set, zero while it is stopped.
    clock: u32,
}

impl Sdhci {
    /// # Safety
    ///
    /// `regs` must be the kernel address of this controller's register block,
    /// and nothing else may touch the controller while this value exists.
    pub unsafe fn new(regs: u64, base_clock: u32) -> Sdhci {
        Sdhci { regs, base_clock, clock: 0 }
    }

    pub fn clock(&self) -> u32 {
        self.clock
    }

    #[inline]
    fn read(&self, offset: u64) -> u32 {
        unsafe { core::ptr::read_volatile((self.regs + offset) as *const u32) }
    }

    /// A register write followed by the pause `bcm2835_mmc_writel` takes.
    fn write(&self, offset: u64, value: u32) {
        unsafe { core::ptr::write_volatile((self.regs + offset) as *mut u32, value) };
        let clock = self.clock.max(MIN_FREQ);
        spin_us((2 * 1_000_000 / clock) as u64 + 1);
    }

    /// The data port, which `mmc_raw_writel` writes without the pause.
    fn write_raw(&self, offset: u64, value: u32) {
        unsafe { core::ptr::write_volatile((self.regs + offset) as *mut u32, value) };
    }

    /// One byte of a 32-bit register: read the word, replace the byte, write
    /// the word. `bcm2835_mmc_writeb`.
    fn write_byte(&self, offset: u64, byte: u32, value: u32) {
        let shift = byte * 8;
        let word = (self.read(offset) & !(0xFF << shift)) | ((value & 0xFF) << shift);
        self.write(offset, word);
    }

    /// The low sixteen bits of a 32-bit register, the same way.
    fn write_low_word(&self, offset: u64, value: u32) {
        let word = (self.read(offset) & !0xFFFF) | (value & 0xFFFF);
        self.write(offset, word);
    }

    pub fn version(&self) -> u32 {
        self.read(SLOT_INT_STATUS) >> 16
    }

    pub fn capabilities(&self) -> (u32, u32) {
        (self.read(CAPABILITIES), self.read(CAPABILITIES_1))
    }

    pub fn present_state(&self) -> u32 {
        self.read(PRESENT_STATE)
    }

    pub fn interrupt_status(&self) -> u32 {
        self.read(INT_STATUS)
    }

    /// Reset some of the controller and wait for the bits to clear, for up to
    /// 100 ms. `bcm2835_mmc_reset`.
    pub fn reset(&mut self, mask: u32) -> Result<(), Error> {
        self.write_byte(CLOCK_CONTROL, 3, mask);
        if mask & RESET_ALL != 0 {
            self.clock = 0;
        }
        for _ in 0..100 {
            if (self.read(CLOCK_CONTROL) >> 24) & mask == 0 {
                return Ok(());
            }
            spin_us(1000);
        }
        Err(Error { what: "reset never completed", status: self.read(INT_STATUS) })
    }

    /// Reset everything and choose which interrupt sources are recorded.
    /// `bcm2835_mmc_init` with `soft` false. None of them is signalled,
    /// because nothing takes the interrupt.
    pub fn init(&mut self) -> Result<(), Error> {
        self.reset(RESET_ALL)?;
        self.write(INT_ENABLE, INT_POLLED);
        self.write(SIGNAL_ENABLE, 0);
        Ok(())
    }

    /// Set the bus clock to at most `hz`, or stop it for zero.
    /// `bcm2835_mmc_set_clock`: clock off, divisor in with the internal
    /// clock on, up to 20 ms for it to be stable, then out to the card.
    pub fn set_clock(&mut self, hz: u32) -> Result<u32, Error> {
        self.write_low_word(CLOCK_CONTROL, 0);
        if hz == 0 {
            self.clock = 0;
            return Ok(0);
        }
        let (bits, actual) = clock_divider(self.base_clock, hz);
        let clk = bits | CLOCK_INT_EN;
        self.write_low_word(CLOCK_CONTROL, clk);
        let mut stable = false;
        for _ in 0..20 {
            if self.read(CLOCK_CONTROL) & CLOCK_INT_STABLE != 0 {
                stable = true;
                break;
            }
            spin_us(1000);
        }
        if !stable {
            return Err(Error { what: "internal clock never stabilised", status: self.read(INT_STATUS) });
        }
        // The pause after each write is two periods of the clock the card
        // sees, so it follows the new rate from here on.
        self.clock = actual;
        self.write_low_word(CLOCK_CONTROL, clk | CLOCK_CARD_EN);
        Ok(actual)
    }

    /// Power the bus at 3.3 V, the one voltage `bcm2835_mmc_set_ios` ever
    /// selects.
    pub fn power_on(&mut self) {
        self.write_byte(HOST_CONTROL, 1, POWER_330 | POWER_ON);
    }

    pub fn power_off(&mut self) {
        self.write_byte(HOST_CONTROL, 1, 0);
    }

    /// One data line or four. The high-speed bit is never set, which is the
    /// reference's `SDHCI_QUIRK_NO_HISPD_BIT`.
    pub fn set_bus_width(&mut self, four: bool) {
        let control = self.read(HOST_CONTROL) & 0xFF;
        let control = if four { control | CTRL_4BITBUS } else { control & !CTRL_4BITBUS };
        self.write_byte(HOST_CONTROL, 0, control);
    }

    /// Send a command, move its data if it has any, and return the first word
    /// of the response.
    ///
    /// On any error the command and data lines are reset before returning,
    /// which is what `bcm2835_mmc_tasklet_finish` does for a request that
    /// failed; without it the next command finds the lines inhibited.
    pub fn command(&mut self, cmd: Command, transfer: Option<Transfer>) -> Result<[u32; 4], Error> {
        let result = self.run(cmd, transfer);
        if result.is_err() {
            let _ = self.reset(RESET_CMD);
            let _ = self.reset(RESET_DATA);
            self.write(INT_STATUS, 0xFFFF_FFFF);
        }
        result
    }

    fn run(&mut self, cmd: Command, mut transfer: Option<Transfer>) -> Result<[u32; 4], Error> {
        let has_data = transfer.is_some();
        let mut inhibit = CMD_INHIBIT;
        if has_data || cmd.response.busy() {
            inhibit |= DATA_INHIBIT;
        }
        let mut free = false;
        for _ in 0..INHIBIT_TRIES {
            if self.read(PRESENT_STATE) & inhibit == 0 {
                free = true;
                break;
            }
            spin_us(INHIBIT_STEP_US);
        }
        if !free {
            return Err(Error { what: "controller never released the inhibit bits", status: self.read(PRESENT_STATE) });
        }

        // Anything left over from before belongs to no command.
        self.write(INT_STATUS, 0xFFFF_FFFF);

        // `bcm2835_mmc_prepare_data`.
        if has_data || cmd.response.busy() {
            self.write_byte(CLOCK_CONTROL, 2, TIMEOUT_VAL);
        }
        let mut mode = (self.read(TRANSFER_MODE) & 0xFFFF) & !(TRNS_AUTO_CMD12 | TRNS_AUTO_CMD23);
        if let Some(t) = &transfer {
            if t.block_size == 0 || t.blocks == 0 || t.block_size > 0xFFF || t.blocks > 0xFFFF || t.len() != t.block_size * t.blocks {
                return Err(Error { what: "transfer is not a whole number of blocks", status: 0 });
            }
            // Block size and count are one 32-bit write, so they arrive
            // together rather than within two clock cycles of each other.
            let size = (DEFAULT_BOUNDARY_ARG << 12) | t.block_size as u32;
            self.write(BLOCK_SIZE, ((t.blocks as u32) << 16) | size);
            // `bcm2835_mmc_set_transfer_mode`: no automatic CMD12 or CMD23,
            // because nothing here sets up either for a host that has
            // `SDHCI_AUTO_CMD23` and a request without `sbc`.
            mode = TRNS_BLK_CNT_EN;
            if t.blocks > 1 {
                mode |= TRNS_MULTI;
            }
            if matches!(t.data, Data::Read(_)) {
                mode |= TRNS_READ;
            }
        }

        self.write(ARGUMENT, cmd.argument);
        // The transfer mode held back until now, in the same write as the
        // command, which is what starts the command.
        let command = ((cmd.opcode as u32) << 8) | cmd.response.flags(has_data);
        self.write(TRANSFER_MODE, (command << 16) | mode);

        let deadline = Deadline::after_ms(REQUEST_TIMEOUT_MS);
        let status = loop {
            let status = self.read(INT_STATUS);
            if status & (INT_RESPONSE | INT_ERROR) != 0 {
                break status;
            }
            if deadline.expired() {
                return Err(Error { what: "no response and no error from the controller", status });
            }
            core::hint::spin_loop();
        };
        if status & INT_CMD_ERRORS != 0 || (status & INT_ERROR != 0 && status & INT_RESPONSE == 0) {
            let what = if status & INT_TIMEOUT != 0 {
                "command timed out"
            } else if status & INT_CRC != 0 {
                "command response CRC error"
            } else if status & INT_END_BIT != 0 {
                "command response end bit error"
            } else if status & INT_INDEX != 0 {
                "command response index error"
            } else {
                "command error"
            };
            return Err(Error { what, status });
        }
        self.write(INT_STATUS, INT_RESPONSE);

        // `bcm2835_mmc_finish_command`. A long response loses its CRC byte in
        // the controller, so the four words are shifted up a byte.
        let mut response = [0u32; 4];
        if cmd.response.0 & RSP_PRESENT != 0 {
            if cmd.response.0 & RSP_136 != 0 {
                for (i, word) in response.iter_mut().enumerate() {
                    let at = RESPONSE + (3 - i as u64) * 4;
                    *word = self.read(at) << 8;
                    if i != 3 {
                        *word |= self.read(at - 4) >> 24;
                    }
                }
            } else {
                response[0] = self.read(RESPONSE);
            }
        }

        match transfer.as_mut() {
            None => {
                if cmd.response.busy() {
                    self.wait_data_end(deadline)?;
                }
            }
            Some(t) => {
                let block_size = t.block_size;
                match &mut t.data {
                    Data::Read(buf) => {
                        for block in buf.chunks_mut(block_size) {
                            self.wait_buffer(DATA_AVAILABLE, INT_DATA_AVAIL, deadline)?;
                            // `bcm2835_bcm2835_mmc_read_block_pio`: a word at a
                            // time, low byte first.
                            for bytes in block.chunks_mut(4) {
                                let word = self.read(BUFFER).to_le_bytes();
                                bytes.copy_from_slice(&word[..bytes.len()]);
                            }
                        }
                    }
                    Data::Write(buf) => {
                        for block in buf.chunks(block_size) {
                            self.wait_buffer(SPACE_AVAILABLE, INT_SPACE_AVAIL, deadline)?;
                            for bytes in block.chunks(4) {
                                let mut word = [0u8; 4];
                                word[..bytes.len()].copy_from_slice(bytes);
                                self.write_raw(BUFFER, u32::from_le_bytes(word));
                            }
                        }
                    }
                }
                self.wait_data_end(deadline)?;
            }
        }
        Ok(response)
    }

    /// Wait for the buffer to be ready for a block, as the present state
    /// says, which is the condition `bcm2835_mmc_transfer_pio` loops on.
    fn wait_buffer(&mut self, ready: u32, interrupt: u32, deadline: Deadline) -> Result<(), Error> {
        loop {
            if self.read(PRESENT_STATE) & ready != 0 {
                self.write(INT_STATUS, interrupt);
                return Ok(());
            }
            let status = self.read(INT_STATUS);
            if status & INT_DATA_ERRORS != 0 {
                return Err(data_error(status));
            }
            if deadline.expired() {
                return Err(Error { what: "the buffer never became ready", status });
            }
            core::hint::spin_loop();
        }
    }

    fn wait_data_end(&mut self, deadline: Deadline) -> Result<(), Error> {
        loop {
            let status = self.read(INT_STATUS);
            if status & INT_DATA_ERRORS != 0 {
                return Err(data_error(status));
            }
            if status & INT_DATA_END != 0 {
                self.write(INT_STATUS, INT_DATA_END | INT_DATA_AVAIL | INT_SPACE_AVAIL);
                return Ok(());
            }
            if deadline.expired() {
                return Err(Error { what: "the transfer never completed", status });
            }
            core::hint::spin_loop();
        }
    }
}

fn data_error(status: u32) -> Error {
    let what = if status & INT_DATA_TIMEOUT != 0 {
        "data timed out"
    } else if status & INT_DATA_CRC != 0 {
        "data CRC error"
    } else {
        "data end bit error"
    };
    Error { what, status }
}
