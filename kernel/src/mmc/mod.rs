//! What the Pi 4's two SD host controllers have in common.
//!
//! The board has two SDHCI blocks. The Arasan at 0x7e300000 carries the WiFi
//! chip over SDIO (`net::wifi`), and the EMMC2 controller at 0x7e340000 is
//! wired to the microSD slot (`storage`). Both follow the standard register
//! map, `drivers/mmc/host/sdhci.h`, and both are driven the same way: a
//! command, its response, and data moved by PIO through the buffer register.
//! That part lives in `sdhci`, once. What each integration does differently --
//! how long to wait after a register write, whether the controller sends CMD12
//! itself, whether the high-speed bit may be set -- is a `sdhci::Variant`
//! defined beside the driver that uses it, so a quirk of one controller never
//! reaches the other.

pub mod delay;
pub mod sdhci;

/// `mmc_select_voltage` in `drivers/mmc/core/core.c`, for a host without
/// `MMC_CAP2_FULL_PWR_CYCLE`: the voltages below the defined range dropped,
/// what the host cannot supply dropped, and then the highest voltage left and
/// the one below it.
pub fn select_voltage(ocr: u32, host_ocr: u32) -> u32 {
    let ocr = ocr & !0x7F & 0x00FF_FFFF & host_ocr;
    if ocr == 0 {
        return 0;
    }
    // Bits 0 to 6 are masked off above, so the highest bit left is at least 7.
    let bit = 31 - ocr.leading_zeros();
    ocr & (3 << (bit - 1))
}
