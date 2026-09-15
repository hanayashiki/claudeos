//! The SD host controller the WiFi chip is wired to.
//!
//! This is the Arasan SDHCI at 0x7e300000, the block Raspberry Pi's tree calls
//! `mmcnr` and upstream's calls `sdhci`, with GPIO 34 to 39 routed to it. The
//! command code is `crate::mmc::sdhci`, shared with the card slot's controller;
//! what is particular to this block is the `ARASAN` variant below, from
//! `bcm2835-mmc.c` in Raspberry Pi's tree, which is what `mmcnr` binds to on a
//! Pi.
//!
//! **A write can be lost if another reaches the same register within two SD
//! clock cycles.** So every register write is followed by a pause of two clock
//! periods plus a microsecond (BCM2835_SDHCI_WRITE_DELAY), computed from a
//! clock of at least 400 kHz (`MIN_FREQ`). The data port is exempt, as the
//! reference says, and is written without the pause.
//!
//! The internal clock gets 20 ms to settle (`bcm2835_mmc_set_clock`), the
//! power register is written once with the voltage and the power bit
//! together (`bcm2835_mmc_set_ios`), the high-speed bit is never set, and
//! nothing asks the controller to send CMD12 by itself: CMD53, the only data
//! command this controller carries, ends without one.
//!
//! Data moves by PIO through the buffer register, the path `bcm2835-mmc.c`
//! uses for small transfers. The DMA path it uses for large ones goes through
//! the VideoCore's DMA controller and is not ported.

pub use crate::mmc::sdhci::*;

/// `MIN_FREQ` in `bcm2835-mmc.c`: the write pause is never computed from a
/// clock slower than this.
const MIN_FREQ: u32 = 400_000;

/// `bcm2835_mmc_writel`: two clock periods and a microsecond.
fn write_delay_us(clock: u32) -> u64 {
    let clock = clock.max(MIN_FREQ);
    (2 * 1_000_000 / clock) as u64 + 1
}

pub static ARASAN: Variant = Variant {
    name: "Arasan (bcm2835-mmc)",
    write_delay_us,
    clock_stable_ms: 20,
    clear_power_first: false,
    high_speed_bit: false,
    auto_cmd12: false,
};
