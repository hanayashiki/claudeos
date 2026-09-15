//! The card slot's SD host controller: EMMC2 on the BCM2711, at bus address
//! 0x7e340000, which the processor reaches at 0xfe340000.
//!
//! Upstream Linux binds `brcm,bcm2711-emmc2` with `drivers/mmc/host/sdhci-iproc.c`,
//! as `bcm2711_data`, and Raspberry Pi's tree does the same. That entry is the
//! source for every difference from the plain controller in `EMMC2` below:
//!
//! - `sdhci_iproc_writel` waits after a register write only while the bus
//!   clock is at or below 400 kHz: four clock periods rounded up to a
//!   microsecond, or ten microseconds while the clock is stopped. Above that
//!   it does not wait.
//! - `sdhci_iproc_writew` and `sdhci_iproc_writeb` give 32-bit-only access
//!   with the block and command registers held back until the command, which
//!   the shared code in `crate::mmc::sdhci` already does.
//! - `.set_clock = sdhci_set_clock`, whose `sdhci_enable_clk` gives the
//!   internal clock 150 ms to settle.
//! - `.set_power = sdhci_set_power_and_bus_voltage`: the card's supply
//!   regulator, then `sdhci_set_power_noreg`, which clears the power register
//!   before writing the voltage and the power bit (no
//!   `SDHCI_QUIRK_SINGLE_POWER_WRITE`).
//! - No `SDHCI_QUIRK_NO_HISPD_BIT`, so `sdhci_set_ios` sets the high-speed bit
//!   for SD high-speed timing.
//! - `SDHCI_QUIRK_MULTIBLOCK_READ_ACMD12`, which `sdhci_setup_host` turns into
//!   `SDHCI_AUTO_CMD12`: the controller sends CMD12 after a multi-block
//!   transfer itself.
//! - `sdhci_iproc_bcm2711_get_min_clock` returns 200 kHz, because this
//!   integration hangs when the bus clock is too far below the core clock;
//!   the initial rates tried stop there.
//!
//! **Power and signalling.** The board's device tree gives the node
//! `vmmc-supply`, a `regulator-fixed` switched by expander line 6
//! (`SD_PWR_ON`, active high), and `vqmmc-supply`, a `regulator-gpio` on
//! expander line 4 (`VDD_SD_IO_SEL`) whose `states` give 3.3 V for 0. The
//! expander belongs to the VideoCore firmware and is driven through its
//! mailbox, as the WiFi driver drives `WL_ON`. Signalling stays at 3.3 V.
//!
//! **Card detection.** The node says `broken-cd`: the controller's card
//! present bit is not wired, and Linux polls by trying to initialise a card.
//! This driver tries once, at boot.
//!
//! **Without a device tree.** QEMU's `raspi4b` hands the kernel a tag list
//! rather than a device tree. Its EMMC2 is at the same address,
//! `EMMC2_OFFSET` in `hw/arm/bcm2838_peripherals.c`, with no mailbox GPIOs
//! behind it, so that address is used and the supplies are left alone.

use crate::arch;
use crate::arch::mailbox::{Mailbox, EXPANDER_BASE};
use crate::mm::phys_to_virt;
use crate::mmc::delay::sleep_ms;
use crate::mmc::sdhci::{Sdhci, Variant};
use alloc::format;
use alloc::string::String;

pub const COMPATIBLE: &[u8] = b"brcm,bcm2711-emmc2";
const EXPANDER_COMPATIBLE: &[u8] = b"raspberrypi,firmware-gpio";
/// Where QEMU's `raspi4b` puts EMMC2.
const QEMU_REGS: u64 = 0xFE34_0000;
/// `RPI_FIRMWARE_EMMC2_CLK_ID` in `include/soc/bcm2835/raspberrypi-firmware.h`.
const CLOCK_EMMC2: u32 = 12;
/// `sdhci_iproc_bcm2711_get_min_clock`.
pub const MIN_CLOCK: u32 = 200_000;
/// 3.3 V in the `states` of a `regulator-gpio`, in microvolts.
const MICROVOLTS_3V3: u32 = 3_300_000;
/// `regulator-settling-time-us` on the board's `sd_io_1v8_reg`, rounded up.
const IO_VOLTAGE_SETTLE_MS: u64 = 5;
/// The capabilities register's base clock field in a version 3.00 host,
/// `SDHCI_CLOCK_V3_BASE_MASK`, in MHz.
const CAPS_BASE_CLOCK_SHIFT: u32 = 8;
const CAPS_BASE_CLOCK_MASK: u32 = 0xFF;

/// `sdhci_iproc_writel`.
fn write_delay_us(clock: u32) -> u64 {
    if clock > 400_000 {
        0
    } else if clock != 0 {
        ((4 * 1_000_000 + clock - 1) / clock) as u64
    } else {
        10
    }
}

pub static EMMC2: Variant = Variant {
    name: "EMMC2 (sdhci-iproc)",
    write_delay_us,
    clock_stable_ms: 150,
    clear_power_first: true,
    high_speed_bit: true,
    auto_cmd12: true,
};

/// An expander line and the value that turns it on.
#[derive(Clone, Copy)]
struct Line {
    gpio: u32,
    on: u32,
}

pub struct Controller {
    /// Where the processor reaches the registers.
    pub phys: u64,
    regs: u64,
    mailbox: Option<Mailbox>,
    /// `vmmc-supply`: the card's power.
    power: Option<Line>,
    /// `vqmmc-supply`: the signalling voltage, with `on` the value for 3.3 V.
    signalling: Option<Line>,
    /// How the controller was found, for the log.
    pub source: &'static str,
}

fn be32_cells(value: &[u8]) -> alloc::vec::Vec<u32> {
    value.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The expander line a regulator node switches: its `gpio` or `gpios`
/// property, three cells of controller, line and flags, where the controller
/// has to be the firmware's expander.
fn regulator_line(regulator: &arch::fdt::Node) -> Option<u32> {
    let cells = be32_cells(regulator.property(b"gpios").or_else(|| regulator.property(b"gpio"))?);
    if cells.len() < 2 {
        return None;
    }
    let controller = arch::fdt::find_phandle(cells[0])?;
    let compatible = controller.property(b"compatible")?;
    if !compatible.split(|&b| b == 0).any(|entry| entry == EXPANDER_COMPATIBLE) {
        return None;
    }
    Some(EXPANDER_BASE + cells[1])
}

/// Find the controller and what powers the slot. Nothing is touched but the
/// device tree and the mailbox page.
pub fn find() -> Result<Controller, String> {
    if arch::fdt::blob().is_none() {
        return Ok(Controller {
            phys: QEMU_REGS,
            regs: phys_to_virt(QEMU_REGS),
            mailbox: None,
            power: None,
            signalling: None,
            source: "no device tree, so QEMU's raspi4b address",
        });
    }
    let node = arch::fdt::find_enabled_compatible(COMPATIBLE)
        .ok_or_else(|| String::from("the device tree has no enabled \"brcm,bcm2711-emmc2\" node"))?;
    let (base, size) = node.reg(0).ok_or_else(|| String::from("the EMMC2 node has no address the processor can reach"))?;
    if base < arch::DEVICE_PHYS_BASE || base + size > arch::HHDM_LIMIT {
        return Err(format!("the EMMC2 registers at {:#x} are outside the device window", base));
    }
    let power = node.cell(b"vmmc-supply").and_then(arch::fdt::find_phandle).and_then(|regulator| {
        let gpio = regulator_line(&regulator)?;
        // `enable-active-high`; without it a fixed regulator's line is active low.
        let on = if regulator.property(b"enable-active-high").is_some() { 1 } else { 0 };
        Some(Line { gpio, on })
    });
    let signalling = node.cell(b"vqmmc-supply").and_then(arch::fdt::find_phandle).and_then(|regulator| {
        let gpio = regulator_line(&regulator)?;
        // `states`: pairs of microvolts and the line's value.
        let states = be32_cells(regulator.property(b"states")?);
        let on = states.chunks_exact(2).find(|pair| pair[0] == MICROVOLTS_3V3)?[1];
        Some(Line { gpio, on })
    });
    let mailbox = if power.is_some() || signalling.is_some() {
        Some(Mailbox::find().map_err(|why| format!("{}; the slot's supplies cannot be switched without it", why))?)
    } else {
        None
    };
    Ok(Controller {
        phys: base,
        regs: phys_to_virt(base),
        mailbox,
        power,
        signalling,
        source: "the device tree",
    })
}

impl Controller {
    /// The controller, with the clock it divides the bus clock from. As
    /// `sdhci_setup_host` decides it: the capabilities register's base clock
    /// when it gives one, and otherwise the clock the device tree names, whose
    /// rate the firmware knows. Also returns both, for the log.
    pub fn host(&mut self) -> Result<(Sdhci, u32, Option<u32>), String> {
        // Reading the capabilities needs no clock; the divider is set later.
        let probe = unsafe { Sdhci::new(self.regs, 0, &EMMC2) };
        let (caps, _) = probe.capabilities();
        let from_caps = ((caps >> CAPS_BASE_CLOCK_SHIFT) & CAPS_BASE_CLOCK_MASK) * 1_000_000;
        let from_firmware = match self.mailbox.as_mut() {
            Some(mailbox) => mailbox.clock_rate(CLOCK_EMMC2).ok().filter(|&rate| rate != 0),
            None => None,
        };
        let base = if from_caps != 0 { from_caps } else { from_firmware.unwrap_or(0) };
        if base == 0 {
            return Err(String::from("neither the capabilities register nor the firmware gives EMMC2's base clock"));
        }
        // The controller is this value's alone: nothing else in the kernel
        // knows the address.
        Ok((unsafe { Sdhci::new(self.regs, base, &EMMC2) }, from_caps, from_firmware))
    }

    /// Switch the card's supply, and on the way up select 3.3 V signalling:
    /// what `sdhci_set_power_and_bus_voltage` and
    /// `mmc_set_initial_signal_voltage` do through the two regulators.
    pub fn supply(&mut self, on: bool) -> Result<(), String> {
        let Some(mailbox) = self.mailbox.as_mut() else { return Ok(()) };
        if let Some(power) = self.power {
            let value = if on { power.on } else { 1 - power.on.min(1) };
            mailbox
                .gpio_output(power.gpio, value)
                .map_err(|e| format!("setting SD_PWR_ON through the firmware: {:?}", e))?;
        }
        if on {
            if let Some(signalling) = self.signalling {
                mailbox
                    .gpio_output(signalling.gpio, signalling.on)
                    .map_err(|e| format!("selecting 3.3 V signalling through the firmware: {:?}", e))?;
                sleep_ms(IO_VOLTAGE_SETTLE_MS);
            }
        }
        Ok(())
    }

    /// What the supplies are, for the log.
    pub fn supplies(&self) -> String {
        let line = |line: Option<Line>| match line {
            Some(line) => format!("firmware gpio {} = {}", line.gpio, line.on),
            None => String::from("not described"),
        };
        format!("power {}, 3.3 V signalling {}", line(self.power), line(self.signalling))
    }
}
