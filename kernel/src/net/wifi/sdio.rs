//! The SDIO card on the other end of the host controller.
//!
//! Enumeration, the direct and extended I/O commands, and the per-function
//! registers, ported from Linux's MMC core in Raspberry Pi's `rpi-6.6.y`:
//! `drivers/mmc/core/core.c` (`mmc_rescan_try_freq`, `mmc_power_up`,
//! `mmc_select_voltage`), `sdio.c` (`mmc_attach_sdio`, `mmc_sdio_init_card`,
//! `sdio_read_cccr`, the speed and bus width switches), `sdio_ops.c` (the
//! command arguments), `sdio_io.c` (function enable, block size, and how a
//! transfer is split between block mode and byte mode) and `sdio_cis.c`
//! (the tuples read). The command numbers and register offsets are
//! `include/linux/mmc/{mmc,sd,sdio}.h`.
//!
//! What is left out is what a WiFi chip on a fixed board does not need: SD
//! memory and combo cards, 1.8 V signalling, and card removal.

use super::delay::{sleep_ms, spin_us, Deadline};
use super::sdhci::{Command, Data, Error, Response, Sdhci, Transfer};

const MMC_GO_IDLE_STATE: u8 = 0;
const SD_SEND_RELATIVE_ADDR: u8 = 3;
const SD_IO_SEND_OP_COND: u8 = 5;
const MMC_SELECT_CARD: u8 = 7;
const SD_IO_RW_DIRECT: u8 = 52;
const SD_IO_RW_EXTENDED: u8 = 53;

/// The ready bit of an operation condition response, `MMC_CARD_BUSY`.
const MMC_CARD_BUSY: u32 = 0x8000_0000;

const R5_ERROR: u32 = 1 << 11;
const R5_FUNCTION_NUMBER: u32 = 1 << 9;
const R5_OUT_OF_RANGE: u32 = 1 << 8;

// Card common control registers.
pub const CCCR_CCCR: u32 = 0x00;
pub const CCCR_IOEX: u32 = 0x02;
pub const CCCR_IORX: u32 = 0x03;
pub const CCCR_IENX: u32 = 0x04;
pub const CCCR_INTX: u32 = 0x05;
pub const CCCR_ABORT: u32 = 0x06;
pub const CCCR_IF: u32 = 0x07;
pub const CCCR_CAPS: u32 = 0x08;
pub const CCCR_CIS: u32 = 0x09;
pub const CCCR_POWER: u32 = 0x12;
pub const CCCR_SPEED: u32 = 0x13;

const CCCR_REV_1_10: u8 = 1;
const CCCR_REV_1_20: u8 = 2;
const CCCR_REV_3_00: u8 = 3;
const CAP_SMB: u8 = 0x02;
const CAP_LSC: u8 = 0x40;
const CAP_4BLS: u8 = 0x80;
const POWER_SMPC: u8 = 0x01;
const SPEED_SHS: u8 = 0x01;
/// `SDIO_SPEED_EHS`, which is `SDIO_SPEED_SDR25`.
const SPEED_EHS: u8 = 0x02;
const BUS_WIDTH_MASK: u8 = 0x03;
const BUS_WIDTH_4BIT: u8 = 0x02;
/// Bit 3 of the abort register, RES: reset the card's I/O.
const ABORT_RES: u8 = 0x08;

const fn fbr_base(function: u8) -> u32 {
    function as u32 * 0x100
}
const FBR_CIS: u32 = 0x09;
const FBR_BLKSIZE: u32 = 0x10;

// CIS tuples, `sdio_cis.c`.
const CISTPL_MANFID: u8 = 0x20;
const CISTPL_FUNCE: u8 = 0x22;
const CISTPL_END: u8 = 0xFF;
const SPEED_VAL: [u32; 16] = [0, 10, 12, 13, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 70, 80];
const SPEED_UNIT: [u32; 8] = [10_000, 100_000, 1_000_000, 10_000_000, 0, 0, 0, 0];

/// The initial clock rates tried, in order. `freqs` in `core.c`.
pub const INIT_FREQUENCIES: [u32; 4] = [400_000, 300_000, 200_000, 100_000];

/// The voltages the host offers: `bcm2835_mmc_add_host` sets
/// `MMC_VDD_32_33 | MMC_VDD_33_34`, bits 20 and 21 of an OCR.
const HOST_OCR: u32 = (1 << 20) | (1 << 21);
/// `bcm2835_mmc_add_host`: `max_blk_size` and `max_blk_count`.
const HOST_MAX_BLOCK_SIZE: usize = 512;
const HOST_MAX_BLOCK_COUNT: usize = 65535;
/// `sdio_io_rw_ext_helper`: the most blocks one CMD53 can name.
const MAX_CMD53_BLOCKS: usize = 511;
/// `host->ios.power_delay_ms`, which `mmc_alloc_host` sets to ten.
const POWER_DELAY_MS: u64 = 10;
/// `mmc_send_io_op_cond`: a hundred tries ten milliseconds apart.
const OP_COND_TRIES: u32 = 100;
const OP_COND_DELAY_MS: u64 = 10;
/// A function's enable timeout when its CIS gives none, which is what
/// `cistpl_funce_func` falls back to: one second.
const DEFAULT_ENABLE_TIMEOUT_MS: u32 = 1000;

/// What a failed operation was doing, and the controller's error.
#[derive(Clone, Copy)]
pub struct SdioError {
    pub what: &'static str,
    pub host: Option<Error>,
}

impl core::fmt::Display for SdioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.host {
            Some(error) => write!(f, "{}: {}", self.what, error),
            None => write!(f, "{}", self.what),
        }
    }
}

fn fail(what: &'static str) -> impl FnOnce(Error) -> SdioError {
    move |error| SdioError { what, host: Some(error) }
}

/// What the card said about itself during enumeration.
#[derive(Clone, Copy, Default)]
pub struct Identity {
    pub ocr: u32,
    pub functions: u8,
    pub rca: u16,
    pub cccr_version: u8,
    pub sdio_version: u8,
    pub multi_block: bool,
    pub low_speed: bool,
    pub wide_bus: bool,
    pub high_power: bool,
    pub high_speed: bool,
    pub vendor: u16,
    pub device: u16,
    pub common_block_size: u16,
    /// TPLFE_MAX_TRAN_SPEED as the card gives it, and what it decodes to.
    pub max_tran_speed: u8,
    pub max_dtr: u32,
}

/// One function, from its own CIS.
#[derive(Clone, Copy, Default)]
pub struct Function {
    /// The ids `sdio_read_func_cis` gives the function: its own MANFID tuple
    /// if it has one, the card's otherwise. brcmfmac's per-chip switches look
    /// at function 1's device id.
    pub vendor: u16,
    pub device: u16,
    pub max_block_size: u16,
    pub enable_timeout_ms: u32,
}

/// The card, enumerated and selected, and the controller it is on.
pub struct Card {
    host: Sdhci,
    pub identity: Identity,
    functions: [Function; 8],
    /// Each function's block size as last set.
    block_size: [u16; 8],
}

/// Enumerate the card the way `mmc_rescan` does: at each initial rate in
/// turn, power up, reset, and ask for an SDIO card, powering down again if
/// none answers.
///
/// `power` drives the card's reset line, the job of `mmc-pwrseq-simple`
/// upstream: false holds the card in reset and true lets it run. The line is
/// released after the bus has power and before the first command, which is
/// the order `mmc_power_up` calls the power sequence in.
pub fn attach(mut host: Sdhci, power: &mut dyn FnMut(bool)) -> Result<Card, (Sdhci, SdioError)> {
    let mut last = SdioError { what: "no initial clock rate was tried", host: None };
    for &frequency in INIT_FREQUENCIES.iter() {
        match try_frequency(&mut host, power, frequency) {
            Ok((identity, functions)) => {
                crate::println!("wifi: sdio card answered at {} Hz", host.clock());
                return Ok(Card { host, identity, functions, block_size: [0; 8] });
            }
            Err(error) => {
                crate::println!("wifi: no sdio card at {} Hz: {}", frequency, error);
                last = error;
                // `mmc_power_off`: the power sequence first, then the bus.
                power(false);
                let _ = host.set_clock(0);
                host.power_off();
            }
        }
    }
    Err((host, last))
}

fn try_frequency(host: &mut Sdhci, power: &mut dyn FnMut(bool), frequency: u32) -> Result<(Identity, [Function; 8]), SdioError> {
    // `mmc_power_up`.
    power(false);
    host.power_on();
    host.set_clock(0).map_err(fail("stopping the clock"))?;
    host.set_bus_width(false);
    sleep_ms(POWER_DELAY_MS);
    power(true);
    host.set_clock(frequency).map_err(fail("setting the initial clock"))?;
    sleep_ms(POWER_DELAY_MS);

    // `sdio_reset`: whatever the abort register holds, with RES added. A card
    // in the middle of something else ignores it or fails it, and either is
    // fine, which is why the reference does not look at the result.
    let abort = match direct(host, false, 0, CCCR_ABORT, 0) {
        Ok(value) => value | ABORT_RES,
        Err(_) => ABORT_RES,
    };
    let _ = direct(host, true, 0, CCCR_ABORT, abort);

    // `mmc_go_idle`.
    spin_us(1000);
    let _ = host.command(Command { opcode: MMC_GO_IDLE_STATE, argument: 0, response: Response::NONE }, None);
    spin_us(1000);

    // `mmc_attach_sdio`. The reference sends CMD8 before this to find SD
    // memory; an SDIO-only card does not answer it, so it is not sent.
    let probe = host
        .command(Command { opcode: SD_IO_SEND_OP_COND, argument: 0, response: Response::R4 }, None)
        .map_err(fail("CMD5 probe"))?[0];
    let ocr = select_voltage(probe);
    if ocr == 0 {
        return Err(SdioError { what: "the card offers no voltage the host has", host: None });
    }

    // `mmc_sdio_init_card`.
    let mut ready = 0;
    for _ in 0..OP_COND_TRIES {
        let response = host
            .command(Command { opcode: SD_IO_SEND_OP_COND, argument: ocr, response: Response::R4 }, None)
            .map_err(fail("CMD5 with a voltage"))?[0];
        if response & MMC_CARD_BUSY != 0 {
            ready = response;
            break;
        }
        sleep_ms(OP_COND_DELAY_MS);
    }
    if ready == 0 {
        return Err(SdioError { what: "the card never became ready after CMD5", host: None });
    }

    let mut identity = Identity { ocr: ready, functions: ((ready >> 28) & 7) as u8, ..Identity::default() };

    let rca = host
        .command(Command { opcode: SD_SEND_RELATIVE_ADDR, argument: 0, response: Response::R6 }, None)
        .map_err(fail("CMD3"))?[0];
    identity.rca = (rca >> 16) as u16;

    host.command(
        Command { opcode: MMC_SELECT_CARD, argument: (identity.rca as u32) << 16, response: Response::R1 },
        None,
    )
    .map_err(fail("CMD7"))?;

    read_cccr(host, &mut identity)?;
    read_common_cis(host, &mut identity)?;

    let mut functions = [Function::default(); 8];
    for function in 1..=identity.functions.min(7) {
        functions[function as usize] = read_function_cis(host, &identity, function)?;
    }
    Ok((identity, functions))
}

/// `mmc_select_voltage`, without the power cycle this host does not ask for:
/// the voltages below the defined range dropped, what the host cannot supply
/// dropped, and then the highest voltage left and the one below it.
pub fn select_voltage(ocr: u32) -> u32 {
    let ocr = ocr & !0x7F & 0x00FF_FFFF & HOST_OCR;
    if ocr == 0 {
        return 0;
    }
    let bit = 31 - ocr.leading_zeros();
    ocr & (3 << (bit - 1))
}

/// CMD52, `mmc_io_rw_direct_host`: one byte read or written, and the
/// response's error bits checked.
fn direct(host: &mut Sdhci, write: bool, function: u8, address: u32, value: u8) -> Result<u8, Error> {
    let argument = direct_argument(write, function, address, value);
    let response = host.command(Command { opcode: SD_IO_RW_DIRECT, argument, response: Response::R5 }, None)?[0];
    check_r5(response)?;
    Ok(response as u8)
}

/// The CMD52 argument: read/write, function, the read-after-write flag, the
/// register and the byte. `mmc_io_rw_direct_host` never sets
/// read-after-write for a write it does not read back, and nothing here does.
pub const fn direct_argument(write: bool, function: u8, address: u32, value: u8) -> u32 {
    (if write { 0x8000_0000 } else { 0 })
        | ((function as u32) << 28)
        | ((address & 0x1FFFF) << 9)
        | value as u32
}

/// The CMD53 argument, `mmc_io_rw_extended`. `blocks` of zero is byte mode,
/// where a count of 512 is written as zero.
pub const fn extended_argument(write: bool, function: u8, address: u32, increment: bool, blocks: usize, size: usize) -> u32 {
    let mut argument = (if write { 0x8000_0000 } else { 0 })
        | ((function as u32) << 28)
        | (if increment { 0x0400_0000 } else { 0 })
        | ((address & 0x1FFFF) << 9);
    if blocks == 0 {
        argument |= if size == 512 { 0 } else { size as u32 };
    } else {
        argument |= 0x0800_0000 | blocks as u32;
    }
    argument
}

fn check_r5(response: u32) -> Result<(), Error> {
    if response & R5_ERROR != 0 {
        return Err(Error { what: "R5 reports a general error", status: response });
    }
    if response & R5_FUNCTION_NUMBER != 0 {
        return Err(Error { what: "R5 reports an invalid function number", status: response });
    }
    if response & R5_OUT_OF_RANGE != 0 {
        return Err(Error { what: "R5 reports an argument out of range", status: response });
    }
    Ok(())
}

/// `sdio_read_cccr`, without the UHS half.
fn read_cccr(host: &mut Sdhci, identity: &mut Identity) -> Result<(), SdioError> {
    let data = direct(host, false, 0, CCCR_CCCR, 0).map_err(fail("reading CCCR revision"))?;
    identity.cccr_version = data & 0x0F;
    identity.sdio_version = data >> 4;
    if identity.cccr_version > CCCR_REV_3_00 {
        return Err(SdioError { what: "unrecognised CCCR structure version", host: None });
    }
    let caps = direct(host, false, 0, CCCR_CAPS, 0).map_err(fail("reading CCCR capabilities"))?;
    identity.multi_block = caps & CAP_SMB != 0;
    identity.low_speed = caps & CAP_LSC != 0;
    identity.wide_bus = caps & CAP_4BLS != 0;
    if identity.cccr_version >= CCCR_REV_1_10 {
        let power = direct(host, false, 0, CCCR_POWER, 0).map_err(fail("reading CCCR power"))?;
        identity.high_power = power & POWER_SMPC != 0;
    }
    if identity.cccr_version >= CCCR_REV_1_20 {
        let speed = direct(host, false, 0, CCCR_SPEED, 0).map_err(fail("reading CCCR speed"))?;
        identity.high_speed = speed & SPEED_SHS != 0;
    }
    Ok(())
}

/// A CIS pointer: three bytes, low first. `sdio_read_cis`.
fn cis_pointer(host: &mut Sdhci, at: u32) -> Result<u32, SdioError> {
    let mut pointer = 0u32;
    for i in 0..3 {
        let byte = direct(host, false, 0, at + i, 0).map_err(fail("reading a CIS pointer"))?;
        pointer |= (byte as u32) << (i * 8);
    }
    Ok(pointer)
}

/// Walk one CIS and hand each tuple to `each`. `sdio_read_cis`: a code, a
/// link that is the body's length, and the body; 0xFF ends the chain and a
/// code of zero is padding with no link.
fn walk_cis(host: &mut Sdhci, mut at: u32, each: &mut dyn FnMut(u8, &[u8])) -> Result<(), SdioError> {
    let mut body = [0u8; 255];
    for _ in 0..256 {
        let code = direct(host, false, 0, at, 0).map_err(fail("reading a CIS tuple"))?;
        if code == CISTPL_END {
            return Ok(());
        }
        if code == 0 {
            at += 1;
            continue;
        }
        let link = direct(host, false, 0, at + 1, 0).map_err(fail("reading a CIS link"))?;
        if link == 0xFF {
            return Ok(());
        }
        for i in 0..link as u32 {
            body[i as usize] = direct(host, false, 0, at + 2 + i, 0).map_err(fail("reading a CIS body"))?;
        }
        each(code, &body[..link as usize]);
        at += 2 + link as u32;
    }
    Err(SdioError { what: "the CIS did not end", host: None })
}

/// The common CIS: the manufacturer and card ids, and the common function
/// extension, whose speed byte is the card's maximum rate.
/// `cistpl_manfid` and `cistpl_funce_common`.
fn read_common_cis(host: &mut Sdhci, identity: &mut Identity) -> Result<(), SdioError> {
    let pointer = cis_pointer(host, CCCR_CIS)?;
    let mut found = *identity;
    walk_cis(host, pointer, &mut |code, body| match code {
        CISTPL_MANFID if body.len() >= 4 => {
            found.vendor = u16::from_le_bytes([body[0], body[1]]);
            found.device = u16::from_le_bytes([body[2], body[3]]);
        }
        CISTPL_FUNCE if body.len() >= 4 && body[0] == 0 => {
            found.common_block_size = u16::from_le_bytes([body[1], body[2]]);
            found.max_tran_speed = body[3];
            found.max_dtr = SPEED_VAL[((body[3] >> 3) & 15) as usize] * SPEED_UNIT[(body[3] & 7) as usize];
        }
        _ => {}
    })?;
    *identity = found;
    Ok(())
}

/// A function's own CIS: its ids if it gives them, its largest block size,
/// and how long it may take to come ready. `sdio_read_func_cis`, and
/// `cistpl_funce_func`: bytes 12 and 13 of the extension, and 28 and 29 in
/// tens of milliseconds from SDIO 1.1 on.
fn read_function_cis(host: &mut Sdhci, identity: &Identity, function: u8) -> Result<Function, SdioError> {
    let pointer = cis_pointer(host, fbr_base(function) + FBR_CIS)?;
    let mut found = Function { enable_timeout_ms: DEFAULT_ENABLE_TIMEOUT_MS, ..Function::default() };
    let version = identity.sdio_version;
    walk_cis(host, pointer, &mut |code, body| match code {
        CISTPL_MANFID if body.len() >= 4 => {
            found.vendor = u16::from_le_bytes([body[0], body[1]]);
            found.device = u16::from_le_bytes([body[2], body[3]]);
        }
        CISTPL_FUNCE if !body.is_empty() && body[0] == 1 => {
            if body.len() >= 14 {
                found.max_block_size = u16::from_le_bytes([body[12], body[13]]);
            }
            if version > 0 && body.len() >= 30 {
                found.enable_timeout_ms = u16::from_le_bytes([body[28], body[29]]) as u32 * 10;
            }
        }
        _ => {}
    })?;
    if found.vendor == 0 {
        found.vendor = identity.vendor;
        found.device = identity.device;
    }
    Ok(found)
}

impl Card {
    pub fn host(&mut self) -> &mut Sdhci {
        &mut self.host
    }

    pub fn function(&self, function: u8) -> Function {
        self.functions[function as usize & 7]
    }

    pub fn block_size(&self, function: u8) -> u16 {
        self.block_size[function as usize & 7]
    }

    /// Switch to the card's full speed and four data lines, in the order
    /// `mmc_sdio_init_card` does it: high speed if the card has it
    /// (`sdio_enable_hs`), the clock raised to what that allows
    /// (`mmc_sdio_get_max_clock`), and then the bus widened
    /// (`sdio_enable_4bit_bus`). The host sets its own high-speed bit never,
    /// which is how the reference drives this controller.
    pub fn enable_full_speed(&mut self, four_bit: bool) -> Result<u32, SdioError> {
        let high_speed = if self.identity.high_speed {
            let speed = self.read_byte(0, CCCR_SPEED).map_err(|e| SdioError { what: "reading CCCR speed", host: e.host })?;
            self.write_byte(0, CCCR_SPEED, speed | SPEED_EHS)
                .map_err(|e| SdioError { what: "enabling high speed", host: e.host })?;
            true
        } else {
            false
        };
        let target = if high_speed { 50_000_000 } else { self.identity.max_dtr };
        let clock = self.host.set_clock(target).map_err(fail("raising the clock"))?;

        if four_bit && !(self.identity.low_speed && !self.identity.wide_bus) {
            let control = self.read_byte(0, CCCR_IF).map_err(|e| SdioError { what: "reading CCCR bus interface", host: e.host })?;
            let control = (control & !BUS_WIDTH_MASK) | BUS_WIDTH_4BIT;
            self.write_byte(0, CCCR_IF, control)
                .map_err(|e| SdioError { what: "setting four data lines", host: e.host })?;
            self.host.set_bus_width(true);
        }
        Ok(clock)
    }

    /// CMD52 read.
    pub fn read_byte(&mut self, function: u8, address: u32) -> Result<u8, SdioError> {
        direct(&mut self.host, false, function, address, 0).map_err(fail("CMD52 read"))
    }

    /// CMD52 write.
    pub fn write_byte(&mut self, function: u8, address: u32, value: u8) -> Result<(), SdioError> {
        direct(&mut self.host, true, function, address, value).map(|_| ()).map_err(fail("CMD52 write"))
    }

    /// `sdio_set_block_size`: the two bytes of the function's block size
    /// register.
    pub fn set_block_size(&mut self, function: u8, size: u16) -> Result<(), SdioError> {
        if size as usize > HOST_MAX_BLOCK_SIZE {
            return Err(SdioError { what: "block size larger than the host allows", host: None });
        }
        let base = fbr_base(function) + FBR_BLKSIZE;
        self.write_byte(0, base, size as u8)?;
        self.write_byte(0, base + 1, (size >> 8) as u8)?;
        self.block_size[function as usize & 7] = size;
        Ok(())
    }

    /// `sdio_enable_func`: set the function's enable bit, then wait for its
    /// ready bit for as long as the function's timeout allows.
    pub fn enable_function(&mut self, function: u8, timeout_ms: Option<u32>) -> Result<u64, SdioError> {
        let enables = self.read_byte(0, CCCR_IOEX)?;
        self.write_byte(0, CCCR_IOEX, enables | (1 << function))?;
        let timeout = timeout_ms.unwrap_or(self.functions[function as usize & 7].enable_timeout_ms);
        let start = super::delay::now_us();
        let deadline = Deadline::after_ms(timeout as u64);
        loop {
            let ready = self.read_byte(0, CCCR_IORX)?;
            if ready & (1 << function) != 0 {
                return Ok(super::delay::now_us() - start);
            }
            if deadline.expired() {
                return Err(SdioError { what: "function did not come ready", host: None });
            }
            sleep_ms(1);
        }
    }

    /// `sdio_disable_func`.
    pub fn disable_function(&mut self, function: u8) -> Result<(), SdioError> {
        let enables = self.read_byte(0, CCCR_IOEX)?;
        self.write_byte(0, CCCR_IOEX, enables & !(1 << function))
    }

    /// The most bytes one byte-mode CMD53 may carry for a function.
    /// `sdio_max_byte_size`: the host's limit, the function's own largest
    /// block from its CIS, and 512. No quirk in `quirks.h` applies to this
    /// chip. A function whose CIS gave no size is taken at its current block
    /// size, so the split below always makes progress.
    fn max_byte_size(&self, function: u8) -> usize {
        let from_cis = self.functions[function as usize & 7].max_block_size as usize;
        let current = self.block_size[function as usize & 7] as usize;
        let limit = if from_cis != 0 { from_cis } else { current.max(1) };
        HOST_MAX_BLOCK_SIZE.min(limit).min(512)
    }

    /// One CMD53, `mmc_io_rw_extended`.
    fn extended(&mut self, write: bool, function: u8, address: u32, increment: bool, data: Data, blocks: usize, size: usize) -> Result<(), SdioError> {
        let argument = extended_argument(write, function, address, increment, blocks, size);
        let transfer = Transfer { block_size: size, blocks: blocks.max(1), data };
        let response = self
            .host
            .command(Command { opcode: SD_IO_RW_EXTENDED, argument, response: Response::R5 }, Some(transfer))
            .map_err(fail("CMD53"))?[0];
        check_r5(response).map_err(fail("CMD53 response"))
    }

    /// Read `buf.len()` bytes, split the way `sdio_io_rw_ext_helper` splits
    /// them: whole blocks in block mode, at most 511 to a command, and the
    /// remainder in byte mode.
    pub fn read(&mut self, function: u8, mut address: u32, increment: bool, buf: &mut [u8]) -> Result<(), SdioError> {
        let block = self.block_size[function as usize & 7] as usize;
        let max_bytes = self.max_byte_size(function);
        let mut done = 0;
        if self.identity.multi_block && block != 0 && buf.len() > max_bytes {
            let max_blocks = HOST_MAX_BLOCK_COUNT.min(MAX_CMD53_BLOCKS);
            while buf.len() - done >= block {
                let blocks = ((buf.len() - done) / block).min(max_blocks);
                let size = blocks * block;
                self.extended(false, function, address, increment, Data::Read(&mut buf[done..done + size]), blocks, block)?;
                done += size;
                if increment {
                    address += size as u32;
                }
            }
        }
        while done < buf.len() {
            let size = (buf.len() - done).min(max_bytes);
            self.extended(false, function, address, increment, Data::Read(&mut buf[done..done + size]), 0, size)?;
            done += size;
            if increment {
                address += size as u32;
            }
        }
        Ok(())
    }

    /// Write `data`, split the same way.
    pub fn write(&mut self, function: u8, mut address: u32, increment: bool, data: &[u8]) -> Result<(), SdioError> {
        let block = self.block_size[function as usize & 7] as usize;
        let max_bytes = self.max_byte_size(function);
        let mut done = 0;
        if self.identity.multi_block && block != 0 && data.len() > max_bytes {
            let max_blocks = HOST_MAX_BLOCK_COUNT.min(MAX_CMD53_BLOCKS);
            while data.len() - done >= block {
                let blocks = ((data.len() - done) / block).min(max_blocks);
                let size = blocks * block;
                self.extended(true, function, address, increment, Data::Write(&data[done..done + size]), blocks, block)?;
                done += size;
                if increment {
                    address += size as u32;
                }
            }
        }
        while done < data.len() {
            let size = (data.len() - done).min(max_bytes);
            self.extended(true, function, address, increment, Data::Write(&data[done..done + size]), 0, size)?;
            done += size;
            if increment {
                address += size as u32;
            }
        }
        Ok(())
    }
}
