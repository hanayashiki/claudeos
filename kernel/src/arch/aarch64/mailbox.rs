//! The VideoCore firmware's property interface, reached through its mailbox.
//!
//! Part of this board is not wired to the application processor at all. The
//! WiFi chip's power line, WL_ON, is an output of a GPIO expander that only
//! the VideoCore can drive, and the clock the SDIO controller divides its bus
//! clock from is set, and known, only by the VideoCore's firmware. Both are
//! asked for the way Linux asks: a buffer of tags in memory the VideoCore can
//! reach, its bus address written to mailbox 1 on the property channel, and
//! the answer read back from the same buffer once mailbox 0 says the firmware
//! has written it.
//!
//! The registers are `drivers/mailbox/bcm2835-mailbox.c`. The buffer layout,
//! the channel and the transaction are `drivers/firmware/raspberrypi.c`, and
//! the tag numbers and their value layouts are
//! `include/soc/bcm2835/raspberrypi-firmware.h`, all in Raspberry Pi's
//! `rpi-6.6.y` tree.

use crate::arch;
use crate::mm::frame::alloc_contiguous;
use crate::mm::{phys_to_virt, PAGE_SIZE};

/// What the mailbox node calls itself.
const COMPATIBLE: &[u8] = b"brcm,bcm2835-mbox";

// Mailbox 0 carries messages to the processor and mailbox 1 carries them to
// the VideoCore. `bcm2835-mailbox.c`: ARM_0_MAIL0 is 0x00 and ARM_0_MAIL1 is
// 0x20, and each has its read/write register at +0x00 and status at +0x18.
const MAIL0_RD: u64 = 0x00;
const MAIL0_STA: u64 = 0x18;
const MAIL1_WRT: u64 = 0x20;
const MAIL1_STA: u64 = 0x38;
const ARM_MS_FULL: u32 = 1 << 31;
const ARM_MS_EMPTY: u32 = 1 << 30;

/// `raspberrypi.c`, MBOX_CHAN_PROPERTY. A message is a 28-bit address with
/// the channel in its low four bits, MBOX_MSG.
const CHANNEL_PROPERTY: u32 = 8;

/// `raspberrypi-firmware.h`, enum rpi_firmware_property_status.
const STATUS_REQUEST: u32 = 0;
const STATUS_SUCCESS: u32 = 0x8000_0000;
/// The end tag.
const PROPERTY_END: u32 = 0;

/// Tags, from `enum rpi_firmware_property_tag`.
const GET_CLOCK_RATE: u32 = 0x0003_0002;
const GET_GPIO_STATE: u32 = 0x0003_0041;
const SET_GPIO_STATE: u32 = 0x0003_8041;
const GET_GPIO_CONFIG: u32 = 0x0003_0043;
const SET_GPIO_CONFIG: u32 = 0x0003_8043;

/// `enum rpi_firmware_clk_id`: the clock the `mmcnr` controller runs from.
pub const CLOCK_EMMC: u32 = 1;

/// The expander's lines are numbered from here in the firmware's GPIO tags.
/// `gpio-raspberrypi-exp.c`, RPI_EXP_GPIO_BASE.
pub const EXPANDER_BASE: u32 = 128;

/// How long the firmware has to answer. `rpi_firmware_transaction` waits one
/// second, HZ jiffies, for the reply.
const REPLY_TIMEOUT_US: u64 = 1_000_000;

#[derive(Clone, Copy, Debug)]
pub enum Error {
    /// Mailbox 1 stayed full, so the request could not be posted.
    Busy,
    /// No reply in the time `rpi_firmware_transaction` allows.
    NoReply,
    /// A reply arrived on a channel other than the one asked on.
    WrongChannel(u32),
    /// The firmware answered the buffer as a whole with this instead of
    /// success, which is what it does for a tag it does not know.
    Status(u32),
    /// The request does not fit in the buffer.
    TooLong,
}

/// The mailbox and the page shared with the VideoCore.
///
/// Owned by one caller at a time, which is what `&mut self` on every
/// transaction means: a request and its reply use the same buffer, so two
/// interleaved would read each other's answers.
pub struct Mailbox {
    /// Kernel address of the register block.
    regs: u64,
    /// Physical address of the buffer page.
    buffer: u64,
    /// The same page as the VideoCore addresses it.
    bus: u32,
}

/// A line of the firmware GPIO expander, as `GET_GPIO_CONFIG` describes it.
/// `gpio-raspberrypi-exp.c`, struct gpio_get_config.
#[derive(Clone, Copy, Debug)]
pub struct GpioConfig {
    pub direction: u32,
    pub polarity: u32,
    pub term_en: u32,
    pub term_pull_up: u32,
}

#[inline]
fn counter_us() -> u64 {
    let frequency = arch::counter_frequency();
    if frequency == 0 {
        return 0;
    }
    (arch::cycle_counter() as u128 * 1_000_000 / frequency as u128) as u64
}

impl Mailbox {
    /// Find the mailbox in the device tree and set aside a page for the
    /// buffer, at an address the VideoCore can reach.
    pub fn find() -> Result<Mailbox, &'static str> {
        let node = arch::fdt::find_enabled_compatible(COMPATIBLE)
            .ok_or("no enabled \"brcm,bcm2835-mbox\" node in the device tree")?;
        let (base, size) = node.reg(0).ok_or("the mailbox node has no reachable address")?;
        if base < arch::DEVICE_PHYS_BASE || base + size > arch::HHDM_LIMIT {
            return Err("the mailbox registers are outside the device window");
        }
        // `rpi_firmware_property_list` takes its buffer from
        // `dma_alloc_coherent`, which is memory the device can address. Here
        // that is decided by the `soc` bus's `dma-ranges`: the page has to be
        // inside the window, and the address handed over is the translated
        // one.
        let buffer = alloc_contiguous(1).ok_or("no page for the mailbox buffer")?;
        let bus = node
            .dma_address(buffer)
            .and_then(|bus| u32::try_from(bus).ok())
            .ok_or("the mailbox buffer is not in memory the VideoCore can address")?;
        if bus & 0xF != 0 {
            return Err("the mailbox buffer's bus address has bits in the channel field");
        }
        Ok(Mailbox { regs: phys_to_virt(base), buffer, bus })
    }

    fn read(&self, offset: u64) -> u32 {
        unsafe { core::ptr::read_volatile((self.regs + offset) as *const u32) }
    }

    fn write(&self, offset: u64, value: u32) {
        unsafe { core::ptr::write_volatile((self.regs + offset) as *mut u32, value) }
    }

    fn word(&self, index: usize) -> *mut u32 {
        (phys_to_virt(self.buffer) + (index * 4) as u64) as *mut u32
    }

    /// One tag, with `values` as its value buffer both ways.
    ///
    /// `rpi_firmware_property`: the buffer is its total size, a request code
    /// of zero, then the tag, the value buffer's size in bytes, zero, the
    /// values, and an end tag. The firmware writes success or an error code
    /// over the request code and the answer over the values.
    pub fn property(&mut self, tag: u32, values: &mut [u32]) -> Result<(), Error> {
        let words = 2 + 3 + values.len() + 1;
        if words * 4 > PAGE_SIZE {
            return Err(Error::TooLong);
        }
        unsafe {
            core::ptr::write_volatile(self.word(0), (words * 4) as u32);
            core::ptr::write_volatile(self.word(1), STATUS_REQUEST);
            core::ptr::write_volatile(self.word(2), tag);
            core::ptr::write_volatile(self.word(3), (values.len() * 4) as u32);
            core::ptr::write_volatile(self.word(4), 0);
            for (i, value) in values.iter().enumerate() {
                core::ptr::write_volatile(self.word(5 + i), *value);
            }
            core::ptr::write_volatile(self.word(5 + values.len()), PROPERTY_END);
        }
        // The VideoCore reads the buffer from memory, not from this
        // processor's cache. `wmb()` in the reference stands for the same
        // ordering.
        let virt = phys_to_virt(self.buffer);
        arch::clean_data_cache(virt, words * 4);

        // Nothing else in the kernel reads mailbox 0, so anything already
        // waiting there is not the answer to this request.
        while self.read(MAIL0_STA) & ARM_MS_EMPTY == 0 {
            let _ = self.read(MAIL0_RD);
        }

        let start = counter_us();
        while self.read(MAIL1_STA) & ARM_MS_FULL != 0 {
            if counter_us().wrapping_sub(start) > REPLY_TIMEOUT_US {
                return Err(Error::Busy);
            }
            core::hint::spin_loop();
        }
        self.write(MAIL1_WRT, (self.bus & !0xF) | CHANNEL_PROPERTY);

        let reply = loop {
            if self.read(MAIL0_STA) & ARM_MS_EMPTY == 0 {
                break self.read(MAIL0_RD);
            }
            if counter_us().wrapping_sub(start) > REPLY_TIMEOUT_US {
                return Err(Error::NoReply);
            }
            core::hint::spin_loop();
        };
        if reply & 0xF != CHANNEL_PROPERTY {
            return Err(Error::WrongChannel(reply & 0xF));
        }

        // What the firmware wrote is in memory and not in the cache.
        arch::invalidate_data_cache(virt, words * 4);
        let status = unsafe { core::ptr::read_volatile(self.word(1)) };
        if status != STATUS_SUCCESS {
            return Err(Error::Status(status));
        }
        // The tag's own request/response word is not looked at, as
        // `rpi_firmware_property_list` does not look at it: on this board the
        // firmware leaves its response bit clear for the GPIO tags while
        // carrying them out. What a caller trusts is a value read back.
        for (i, value) in values.iter_mut().enumerate() {
            *value = unsafe { core::ptr::read_volatile(self.word(5 + i)) };
        }
        Ok(())
    }

    /// The rate of one of the firmware's clocks, in hertz.
    /// `struct rpi_firmware_clk_rate_request`: an id and a rate.
    pub fn clock_rate(&mut self, clock: u32) -> Result<u32, Error> {
        let mut values = [clock, 0];
        self.property(GET_CLOCK_RATE, &mut values)?;
        Ok(values[1])
    }

    /// `rpi_exp_gpio_get_polarity` and `rpi_exp_gpio_get_direction`: the
    /// firmware answers a good request by writing zero over the line number.
    pub fn gpio_config(&mut self, line: u32) -> Result<GpioConfig, Error> {
        let mut values = [line, 0, 0, 0, 0];
        self.property(GET_GPIO_CONFIG, &mut values)?;
        if values[0] != 0 {
            return Err(Error::Status(values[0]));
        }
        Ok(GpioConfig {
            direction: values[1],
            polarity: values[2],
            term_en: values[3],
            term_pull_up: values[4],
        })
    }

    /// `rpi_exp_gpio_get`.
    pub fn gpio_state(&mut self, line: u32) -> Result<u32, Error> {
        let mut values = [line, 0];
        self.property(GET_GPIO_STATE, &mut values)?;
        if values[0] != 0 {
            return Err(Error::Status(values[0]));
        }
        Ok(values[1])
    }

    /// Make a line an output at `state`, keeping its polarity.
    /// `rpi_exp_gpio_dir_out`: direction 1, no termination, and the polarity
    /// read back first so that it is retained.
    pub fn gpio_output(&mut self, line: u32, state: u32) -> Result<(), Error> {
        let config = self.gpio_config(line)?;
        let mut values = [line, 1, config.polarity, 0, 0, state];
        self.property(SET_GPIO_CONFIG, &mut values)?;
        if values[0] != 0 {
            return Err(Error::Status(values[0]));
        }
        Ok(())
    }

    /// `rpi_exp_gpio_set`.
    pub fn gpio_set(&mut self, line: u32, state: u32) -> Result<(), Error> {
        let mut values = [line, state];
        self.property(SET_GPIO_STATE, &mut values)?;
        if values[0] != 0 {
            return Err(Error::Status(values[0]));
        }
        Ok(())
    }
}
