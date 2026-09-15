//! The SD memory card in the board's microSD slot.
//!
//! Initialisation follows Linux's MMC core in Raspberry Pi's `rpi-6.6.y`:
//! `drivers/mmc/core/core.c` (`mmc_rescan_try_freq`, `mmc_power_up`,
//! `mmc_power_off`), `sd.c` (`mmc_attach_sd`, `mmc_sd_get_cid`,
//! `mmc_sd_init_card`, `mmc_decode_csd`, `mmc_decode_scr`, `mmc_read_switch`,
//! `mmc_sd_switch_hs`), `sd_ops.c` and `mmc_ops.c` for the commands and their
//! arguments, and `block.c` for reads, writes, and the wait for a write to be
//! programmed. Command numbers and status bits are
//! `include/linux/mmc/{mmc,sd}.h`.
//!
//! Left out: 1.8 V signalling and UHS-I, so the bus stays at 3.3 V and at most
//! 50 MHz; the SD status register (ACMD13), which only gives erase sizes;
//! CMD23, erase and discard; the card's own write cache, which SD 6.0 cards
//! have and which stays off unless a host turns it on, as `sd_enable_cache`
//! does and nothing here does; SDIO and MMC cards; and a card taken out or
//! put in after boot.
//!
//! The functions that read and write blocks are private to this module and to
//! `partition`, its child. Nothing else in the kernel can name a block of the
//! card: what the filesystem gets is a `Partition`.

mod partition;

pub use partition::{choose, Partition};

use crate::mmc::delay::{sleep_ms, spin_us, Deadline};
use crate::mmc::sdhci::{Command, Data, Error, Response, Sdhci, Transfer};
use alloc::format;
use alloc::string::String;

const MMC_GO_IDLE_STATE: u8 = 0;
const MMC_ALL_SEND_CID: u8 = 2;
const SD_SEND_RELATIVE_ADDR: u8 = 3;
const SD_SWITCH: u8 = 6;
const SD_APP_SET_BUS_WIDTH: u8 = 6;
const MMC_SELECT_CARD: u8 = 7;
const SD_SEND_IF_COND: u8 = 8;
const MMC_SEND_CSD: u8 = 9;
const MMC_STOP_TRANSMISSION: u8 = 12;
const MMC_SEND_STATUS: u8 = 13;
const MMC_SET_BLOCKLEN: u8 = 16;
const MMC_READ_SINGLE_BLOCK: u8 = 17;
const MMC_READ_MULTIPLE_BLOCK: u8 = 18;
const MMC_WRITE_BLOCK: u8 = 24;
const MMC_WRITE_MULTIPLE_BLOCK: u8 = 25;
const SD_APP_OP_COND: u8 = 41;
const SD_APP_SEND_SCR: u8 = 51;
const MMC_APP_CMD: u8 = 55;

/// The initial clock rates tried, in order: `freqs` in `core.c`, without the
/// 100 kHz below `sdhci_iproc_bcm2711_get_min_clock`.
const INIT_FREQUENCIES: [u32; 3] = [400_000, 300_000, 200_000];
/// 3.2 to 3.4 V: what the capabilities' 3.3 V bit and the fixed 3.3 V
/// `vmmc-supply` both give `ocr_avail`.
const HOST_OCR: u32 = (1 << 20) | (1 << 21);
/// `host->ios.power_delay_ms`, ten unless the tree says otherwise.
const POWER_DELAY_MS: u64 = 10;
/// `mmc_send_app_op_cond`: a hundred tries ten milliseconds apart.
const OP_COND_TRIES: u32 = 100;
const OP_COND_DELAY_MS: u64 = 10;
const SD_OCR_CCS: u32 = 1 << 30;
const MMC_CARD_BUSY: u32 = 1 << 31;
/// `mmc_app_cmd` refuses a CMD55 whose response lacks this.
const R1_APP_CMD: u32 = 1 << 5;
const R1_READY_FOR_DATA: u32 = 1 << 8;
const R1_STATE_TRAN: u32 = 4;
const R1_OUT_OF_RANGE: u32 = 1 << 31;
/// `CMD_ERRORS` in `block.c`: out of range, address, block length, write
/// protect, ECC, internal controller and general errors.
const CMD_ERRORS: u32 = R1_OUT_OF_RANGE | (1 << 30) | (1 << 29) | (1 << 26) | (1 << 21) | (1 << 20) | (1 << 19);
/// `CCC_SWITCH` in a CSD's command classes.
const CCC_SWITCH: u32 = 1 << 10;
const SD_SCR_BUS_WIDTH_1: u32 = 1 << 0;
const SD_SCR_BUS_WIDTH_4: u32 = 1 << 2;
const SD_BUS_WIDTH_4: u32 = 2;
const SCR_SPEC_VER_1: u32 = 1;
/// `HIGH_SPEED_MAX_DTR`.
const HIGH_SPEED_MAX_DTR: u32 = 50_000_000;
/// `SD_MODE_HIGH_SPEED`, bit `HIGH_SPEED_BUS_SPEED` of byte 13 of a switch
/// status.
const SD_MODE_HIGH_SPEED: u8 = 1 << 1;
/// `SDHCI_CAN_DO_HISPD`, which is what gives a host `MMC_CAP_SD_HIGHSPEED`.
const CAN_DO_HISPD: u32 = 1 << 21;
/// `MMC_BLK_TIMEOUT_MS`: how long a written card may stay busy.
const BUSY_TIMEOUT_MS: u64 = 10_000;
/// Blocks moved by one command.
const CHUNK_BLOCKS: usize = 256;
const BLOCK: usize = 512;
/// A 25 MHz default-speed bus, for a CSD whose rate field decodes to nothing.
const DEFAULT_SPEED_DTR: u32 = 25_000_000;

/// `tran_exp` and `tran_mant` in `sd.c`.
const TRAN_EXP: [u32; 8] = [10_000, 100_000, 1_000_000, 10_000_000, 0, 0, 0, 0];
const TRAN_MANT: [u32; 16] = [0, 10, 12, 13, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 70, 80];

/// What the card said about itself.
#[derive(Clone, Copy, Default)]
pub struct Identity {
    /// SDHC and SDXC cards are addressed by block, SDSC cards by byte.
    pub block_addressed: bool,
    /// The card's size in 512-byte blocks.
    pub blocks: u64,
    pub high_speed: bool,
    pub four_bit: bool,
    pub clock: u32,
    pub rca: u16,
    /// The CID's manufacturer id and product name.
    pub manufacturer: u8,
    pub product: [u8; 5],
    /// The CSD's permanent or temporary write protection.
    pub write_protected: bool,
}

pub struct Card {
    host: Sdhci,
    pub identity: Identity,
}

/// Why a try at one clock rate failed, and whether it was the silence of an
/// empty slot.
struct Failure {
    what: String,
    no_answer: bool,
}

fn failed(what: &str) -> impl FnOnce(Error) -> Failure + '_ {
    move |error| Failure { what: format!("{}: {}", what, error), no_answer: error.is_timeout() }
}

fn refused(what: String) -> Failure {
    Failure { what, no_answer: false }
}

/// `UNSTUFF_BITS`: `size` bits from bit `start` of a 128-bit register given
/// most significant word first, as a long response arrives.
fn bits(register: &[u32; 4], start: u32, size: u32) -> u32 {
    let mask = if size < 32 { (1u32 << size) - 1 } else { u32::MAX };
    let at = 3 - (start / 32) as usize;
    let shift = start % 32;
    let mut value = register[at] >> shift;
    if size + shift > 32 {
        value |= register[at - 1] << ((32 - shift) % 32);
    }
    value & mask
}

fn command(host: &mut Sdhci, opcode: u8, argument: u32, response: Response) -> Result<[u32; 4], Error> {
    host.command(Command { opcode, argument, response }, None)
}

/// Enumerate the card the way `mmc_rescan` does: at each initial rate in turn,
/// power up, reset, and ask for an SD card, powering down again if none
/// answers. `supply` switches the slot's regulators.
pub fn attach(mut host: Sdhci, supply: &mut dyn FnMut(bool) -> Result<(), String>) -> Result<Card, String> {
    let mut last = String::new();
    let mut answered = false;
    for &frequency in INIT_FREQUENCIES.iter() {
        match try_frequency(&mut host, supply, frequency) {
            Ok(identity) => return Ok(Card { host, identity }),
            Err(failure) => {
                answered |= !failure.no_answer;
                last = failure.what;
                // `mmc_power_off`: the clock and the bus off, then the
                // regulator, and a millisecond before the next power-up.
                let _ = host.set_clock(0);
                host.set_high_speed(false);
                host.set_bus_width(false);
                host.power_off();
                let _ = supply(false);
                sleep_ms(1);
            }
        }
    }
    if answered {
        Err(format!("the card in the slot did not initialise: {}", last))
    } else {
        Err(String::from("nothing answered in the card slot, which is empty or holds no SD memory card"))
    }
}

fn try_frequency(host: &mut Sdhci, supply: &mut dyn FnMut(bool) -> Result<(), String>, frequency: u32) -> Result<Identity, Failure> {
    // `mmc_power_up`.
    host.set_clock(0).map_err(failed("stopping the clock"))?;
    host.set_bus_width(false);
    host.set_high_speed(false);
    supply(true).map_err(refused)?;
    host.power_on();
    sleep_ms(POWER_DELAY_MS);
    host.set_clock(frequency).map_err(failed("setting the initial clock"))?;
    sleep_ms(POWER_DELAY_MS);

    // `mmc_rescan_try_freq`. `sdio_reset` is not sent: it is CMD52, which an
    // SD memory card ignores, and this slot takes nothing else.
    go_idle(host);
    let _ = send_if_cond(host);

    // `mmc_attach_sd`: ask for the card's voltages without waiting for it.
    let offered = app_op_cond(host, 0)?;
    let ocr = crate::mmc::select_voltage(offered & !0x7FFF, HOST_OCR);
    if ocr == 0 {
        return Err(refused(format!("the card offers no voltage from 3.2 to 3.4 V (OCR {:#010x})", offered)));
    }

    // `mmc_sd_get_cid`.
    go_idle(host);
    let mut request = ocr;
    if send_if_cond(host).is_ok() {
        request |= SD_OCR_CCS;
    }
    app_op_cond(host, request)?;
    let cid = command(host, MMC_ALL_SEND_CID, 0, Response::R2).map_err(failed("CMD2"))?;

    // `mmc_sd_init_card`.
    let rca = (command(host, SD_SEND_RELATIVE_ADDR, 0, Response::R6).map_err(failed("CMD3"))?[0] >> 16) as u16;
    let csd = command(host, MMC_SEND_CSD, (rca as u32) << 16, Response::R2).map_err(failed("CMD9"))?;
    let mut identity = Identity { rca, ..Identity::default() };
    let (command_classes, max_dtr) = decode_csd(&csd, &mut identity).map_err(refused)?;
    // `mmc_decode_csd` takes a version 2 CSD to mean a block-addressed card.
    identity.block_addressed = bits(&csd, 126, 2) == 1;
    identity.manufacturer = bits(&cid, 120, 8) as u8;
    for (i, byte) in identity.product.iter_mut().enumerate() {
        *byte = bits(&cid, 96 - 8 * i as u32, 8) as u8;
    }
    command(host, MMC_SELECT_CARD, (rca as u32) << 16, Response::R1).map_err(failed("CMD7"))?;

    // `mmc_sd_setup_card`: the SCR, then `mmc_read_switch`.
    let scr = send_scr(host, rca)?;
    let register = [0, 0, u32::from_be_bytes([scr[0], scr[1], scr[2], scr[3]]), u32::from_be_bytes([scr[4], scr[5], scr[6], scr[7]])];
    if bits(&register, 60, 4) != 0 {
        return Err(refused(format!("unrecognised SCR structure version {}", bits(&register, 60, 4))));
    }
    let spec = bits(&register, 56, 4);
    let bus_widths = bits(&register, 48, 4);
    if bus_widths & SD_SCR_BUS_WIDTH_1 == 0 || bus_widths & SD_SCR_BUS_WIDTH_4 == 0 {
        return Err(refused(format!("the SCR gives invalid bus widths {:#x}", bus_widths)));
    }
    let mut high_speed_capable = false;
    if spec >= SCR_SPEC_VER_1 && command_classes & CCC_SWITCH != 0 {
        // A failed check is a card without switch functions, as the
        // reference treats it, not a failed card.
        if let Ok(status) = switch(host, 0x00FF_FFF0) {
            high_speed_capable = status[13] & SD_MODE_HIGH_SPEED != 0;
        }
    }

    // `mmc_sd_switch_hs`, and the clock `mmc_sd_get_max_clock` gives.
    let (caps, _) = host.capabilities();
    if high_speed_capable && caps & CAN_DO_HISPD != 0 {
        let status = switch(host, 0x80FF_FFF1).map_err(failed("switching to high speed"))?;
        if status[16] & 0xF == 1 {
            identity.high_speed = true;
            host.set_high_speed(true);
        }
    }
    let target = if identity.high_speed { HIGH_SPEED_MAX_DTR } else { max_dtr };
    identity.clock = host.set_clock(target).map_err(failed("raising the clock"))?;

    // The wider bus, which every SD card has and this host has.
    app_cmd(host, rca)?;
    command(host, SD_APP_SET_BUS_WIDTH, SD_BUS_WIDTH_4, Response::R1).map_err(failed("ACMD6"))?;
    host.set_bus_width(true);
    identity.four_bit = true;

    // A byte-addressed card is told the block length `block.c` reads in.
    if !identity.block_addressed {
        command(host, MMC_SET_BLOCKLEN, BLOCK as u32, Response::R1).map_err(failed("CMD16"))?;
    }
    Ok(identity)
}

/// `mmc_decode_csd`: the size, the write protection, the command classes and
/// the default-speed rate.
fn decode_csd(csd: &[u32; 4], identity: &mut Identity) -> Result<(u32, u32), String> {
    let structure = bits(csd, 126, 2);
    let rate = TRAN_EXP[bits(csd, 96, 3) as usize] * TRAN_MANT[bits(csd, 99, 4) as usize];
    let max_dtr = if rate == 0 { DEFAULT_SPEED_DTR } else { rate.min(DEFAULT_SPEED_DTR) };
    let classes = bits(csd, 84, 12);
    identity.blocks = match structure {
        0 => {
            let c_size = bits(csd, 62, 12) as u64;
            let multiplier = bits(csd, 47, 3);
            let read_bl_len = bits(csd, 80, 4);
            if !(9..=11).contains(&read_bl_len) {
                return Err(format!("the CSD gives a block length of 2^{}", read_bl_len));
            }
            ((c_size + 1) << (multiplier + 2)) << (read_bl_len - 9)
        }
        1 => (bits(csd, 48, 22) as u64 + 1) << 10,
        other => return Err(format!("unrecognised CSD structure version {}", other)),
    };
    identity.write_protected = bits(csd, 12, 2) != 0;
    Ok((classes, max_dtr))
}

/// `mmc_go_idle`: a millisecond either side of CMD0.
fn go_idle(host: &mut Sdhci) {
    sleep_ms(1);
    let _ = command(host, MMC_GO_IDLE_STATE, 0, Response::NONE);
    sleep_ms(1);
    sleep_ms(1);
}

/// `mmc_send_if_cond` for 2.7 to 3.6 V and the 0xAA check pattern.
fn send_if_cond(host: &mut Sdhci) -> Result<(), ()> {
    let response = command(host, SD_SEND_IF_COND, 0x1AA, Response::R7).map_err(|_| ())?[0];
    if response & 0xFF == 0xAA {
        Ok(())
    } else {
        Err(())
    }
}

/// `mmc_app_cmd`.
fn app_cmd(host: &mut Sdhci, rca: u16) -> Result<(), Failure> {
    let response = command(host, MMC_APP_CMD, (rca as u32) << 16, Response::R1).map_err(failed("CMD55"))?[0];
    if response & R1_APP_CMD == 0 {
        return Err(refused(String::from("the card did not take CMD55 as an application command")));
    }
    Ok(())
}

/// `mmc_send_app_op_cond`: one try for an OCR of zero, and otherwise until the
/// card reports it has finished powering up.
fn app_op_cond(host: &mut Sdhci, ocr: u32) -> Result<u32, Failure> {
    for _ in 0..OP_COND_TRIES {
        app_cmd(host, 0)?;
        let response = command(host, SD_APP_OP_COND, ocr, Response::R3).map_err(failed("ACMD41"))?[0];
        if ocr == 0 || response & MMC_CARD_BUSY != 0 {
            return Ok(response);
        }
        sleep_ms(OP_COND_DELAY_MS);
    }
    Err(refused(String::from("the card never left the busy state after ACMD41")))
}

/// `mmc_app_send_scr`: eight bytes, most significant first.
fn send_scr(host: &mut Sdhci, rca: u16) -> Result<[u8; 8], Failure> {
    app_cmd(host, rca)?;
    let mut scr = [0u8; 8];
    let transfer = Transfer { block_size: 8, blocks: 1, data: Data::Read(&mut scr), stop: false };
    host.command(Command { opcode: SD_APP_SEND_SCR, argument: 0, response: Response::R1 }, Some(transfer))
        .map_err(failed("ACMD51"))?;
    Ok(scr)
}

/// `mmc_sd_switch`: CMD6 with its 64-byte status.
fn switch(host: &mut Sdhci, argument: u32) -> Result<[u8; 64], Error> {
    let mut status = [0u8; 64];
    let transfer = Transfer { block_size: 64, blocks: 1, data: Data::Read(&mut status), stop: false };
    host.command(Command { opcode: SD_SWITCH, argument, response: Response::R1 }, Some(transfer))?;
    Ok(status)
}

impl Card {
    /// The argument that names block `lba`: its number on a block-addressed
    /// card, its byte offset on a byte-addressed one.
    fn address(&self, lba: u64) -> Option<u32> {
        if self.identity.block_addressed {
            u32::try_from(lba).ok()
        } else {
            lba.checked_mul(BLOCK as u64).and_then(|at| u32::try_from(at).ok())
        }
    }

    /// Read whole blocks from `lba`.
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String> {
        self.check_range(lba, buf.len())?;
        for (i, chunk) in buf.chunks_mut(CHUNK_BLOCKS * BLOCK).enumerate() {
            self.transfer(lba + (i * CHUNK_BLOCKS) as u64, Data::Read(chunk))?;
        }
        Ok(())
    }

    /// Write whole blocks at `lba`, and wait until the card has programmed
    /// them.
    fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
        self.check_range(lba, data.len())?;
        for (i, chunk) in data.chunks(CHUNK_BLOCKS * BLOCK).enumerate() {
            self.transfer(lba + (i * CHUNK_BLOCKS) as u64, Data::Write(chunk))?;
        }
        Ok(())
    }

    fn check_range(&self, lba: u64, bytes: usize) -> Result<(), String> {
        let count = (bytes / BLOCK) as u64;
        if bytes % BLOCK != 0 || lba.checked_add(count).map_or(true, |end| end > self.identity.blocks) {
            return Err(format!("{} bytes at block {} are not whole blocks inside the card", bytes, lba));
        }
        Ok(())
    }

    /// One read or write command, tried a second time after a failure.
    fn transfer(&mut self, lba: u64, mut data: Data) -> Result<(), String> {
        let blocks = match &data {
            Data::Read(buf) => buf.len(),
            Data::Write(buf) => buf.len(),
        } / BLOCK;
        let address = self.address(lba).ok_or_else(|| format!("block {} cannot be addressed on this card", lba))?;
        let writing = matches!(data, Data::Write(_));
        let opcode = match (writing, blocks > 1) {
            (false, false) => MMC_READ_SINGLE_BLOCK,
            (false, true) => MMC_READ_MULTIPLE_BLOCK,
            (true, false) => MMC_WRITE_BLOCK,
            (true, true) => MMC_WRITE_MULTIPLE_BLOCK,
        };
        let mut last = String::new();
        for _ in 0..2 {
            let again = match &mut data {
                Data::Read(buf) => Data::Read(&mut buf[..]),
                Data::Write(buf) => Data::Write(&buf[..]),
            };
            let transfer = Transfer { block_size: BLOCK, blocks, data: again, stop: blocks > 1 };
            let result = self.host.command(Command { opcode, argument: address, response: Response::R1 }, Some(transfer));
            let outcome = match result {
                Ok(response) if response[0] & CMD_ERRORS != 0 => {
                    Err(format!("CMD{} at block {} reports status {:#010x}", opcode, lba, response[0]))
                }
                Ok(_) => {
                    if blocks > 1 && !self.host.variant().auto_cmd12 {
                        command(&mut self.host, MMC_STOP_TRANSMISSION, 0, Response::R1B)
                            .map_err(|e| format!("CMD12 after CMD{}: {}", opcode, e))
                            .map(|_| ())
                    } else {
                        Ok(())
                    }
                }
                Err(error) => Err(format!("CMD{} of {} blocks at block {}: {}", opcode, blocks, lba, error)),
            };
            let outcome = outcome.and_then(|()| if writing { self.wait_programmed() } else { Ok(()) });
            match outcome {
                Ok(()) => return Ok(()),
                Err(why) => {
                    last = why;
                    // Back to the transfer state before trying again: a card
                    // left sending or receiving data takes CMD12, and one
                    // already in the transfer state ignores it.
                    let _ = command(&mut self.host, MMC_STOP_TRANSMISSION, 0, Response::R1B);
                    let _ = self.wait_programmed();
                }
            }
        }
        Err(last)
    }

    /// `mmc_blk_card_busy`: CMD13 until the card is ready for data and in the
    /// transfer state, for up to `MMC_BLK_TIMEOUT_MS`.
    fn wait_programmed(&mut self) -> Result<(), String> {
        let deadline = Deadline::after_ms(BUSY_TIMEOUT_MS);
        let mut polls = 0u32;
        loop {
            let status = command(&mut self.host, MMC_SEND_STATUS, (self.identity.rca as u32) << 16, Response::R1)
                .map_err(|e| format!("CMD13: {}", e))?[0];
            // Out of range is left out: `block.c` does the same, because a
            // transfer that ends on the card's last block may report it.
            if status & CMD_ERRORS & !R1_OUT_OF_RANGE != 0 {
                return Err(format!("the card reports status {:#010x} after a write", status));
            }
            if status & R1_READY_FOR_DATA != 0 && (status >> 9) & 0xF == R1_STATE_TRAN {
                return Ok(());
            }
            if deadline.expired() {
                return Err(format!("the card stayed busy for {} ms after a write, status {:#010x}", BUSY_TIMEOUT_MS, status));
            }
            if polls < 32 {
                spin_us(100);
            } else {
                sleep_ms(1);
            }
            polls += 1;
        }
    }
}
