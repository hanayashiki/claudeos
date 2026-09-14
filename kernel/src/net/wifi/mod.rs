//! The Raspberry Pi 4's WiFi: a Cypress CYW43455 on the SDIO bus of the Arasan
//! host controller at 0x7e300000.
//!
//! The layers, from the bottom:
//!
//! - `arch::mailbox`: the VideoCore firmware, which alone can drive the
//!   chip's power line WL_ON and knows the controller's clock rate.
//! - `sdhci`: the host controller.
//! - `sdio`: the card: enumeration and the CMD52 and CMD53 transfers.
//! - `chip`: function 1's window onto the chip's backplane, its cores, and
//!   putting firmware on its ARM and starting it.
//! - `protocol`: SDPCM frames on function 2 and the BCDC control messages,
//!   events and Ethernet frames inside them.
//! - this file: finding the hardware in the device tree, the order of
//!   bring-up, the request and response loop with the firmware, and the card
//!   the network stack sends through.
//!
//! **Why a task.** Bring-up uploads 600 KiB through a PIO data port, waits
//! for clocks and for the firmware to boot, and scans for seconds. None of
//! that can happen at probe time, which runs with interrupts masked before
//! the first process exists: the board's watchdog is fed by the timer
//! interrupt and resets the board after 15 seconds without one. So `probe`
//! only reads the device tree and claims the card for the stack, and
//! everything else runs in the driver's own kernel task, where interrupts are
//! on and a long wait is an ordinary sleep. That task owns the controller,
//! the card and the chip outright; nothing else in the kernel can reach them.
//! Other tasks hand it frames through a queue.
//!
//! **Nothing is signalled by interrupt yet.** The chip is polled once a
//! timer tick and whenever a frame is queued, the way brcmfmac runs a bus in
//! its poll mode.

pub mod chip;
pub mod config;
pub mod delay;
pub mod nvram;
pub mod protocol;
pub mod sdhci;
pub mod sdio;
pub mod test;

use crate::abi::Errno;
use crate::arch;
use crate::arch::mailbox::{self, Mailbox};
use crate::arch::paging::AddressSpace;
use crate::mm::phys_to_virt;
use crate::net::Interface;
use crate::sched::{self, WaitQueue};
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use chip::{Backplane, Chip};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use delay::{now_us, sleep_ms, spin_us, Deadline};
use protocol::{DcmdResponse, Event, HeaderError};
use sdio::SdioError;

/// What the controller node says it is. Raspberry Pi's tree gives `mmcnr`
/// this and "brcm,bcm2835-mmc"; upstream's gives its `sdhci` node this alone.
const CONTROLLER_COMPATIBLE: &[u8] = b"brcm,bcm2835-sdhci";
/// The chip, a child of the controller node in both trees.
const CHIP_COMPATIBLE: &[u8] = b"brcm,bcm4329-fmac";
const EXPANDER_COMPATIBLE: &[u8] = b"raspberrypi,firmware-gpio";
const GPIO_COMPATIBLE: &[u8] = b"brcm,bcm2711-gpio";

/// Where the three files are, named as brcmfmac asks for them and put there
/// by `scripts/build-user-aarch64.sh` from `scripts/fetch-wifi-firmware.sh`.
const FIRMWARE_PATH: &str = "/lib/firmware/brcm/brcmfmac43455-sdio.bin";
const NVRAM_PATH: &str = "/lib/firmware/brcm/brcmfmac43455-sdio.txt";
const CLM_PATH: &str = "/lib/firmware/brcm/brcmfmac43455-sdio.clm_blob";

/// `DCMD_RESP_TIMEOUT` and `CTL_DONE_TIMEOUT` in `sdio.c`: how long a control
/// request may take.
const CONTROL_TIMEOUT_MS: u64 = 2500;
/// `BRCMF_RXBOUND`: frames read in one go before looking at anything else.
const RXBOUND: usize = 50;
/// `TXRETRIES`: a control frame is sent this many more times if the bus fails.
const TXRETRIES: usize = 2;
/// `BRCMF_DCMD_SMLEN`: the buffer `brcmf_c_preinit_dcmds` gives "ver".
const DCMD_SMLEN: usize = 256;
/// `BRCMF_ESCAN_TIMER_INTERVAL_MS` is ten seconds; a scan of both bands with
/// the firmware's own dwell times takes several, so this allows fifteen.
const SCAN_TIMEOUT_MS: u64 = 15_000;
/// `sizeof(struct brcmf_rev_info_le)`: seventeen words.
const REVINFO_LEN: usize = 68;
/// Frames waiting for the task. The same depth as the wired driver's queue.
const TX_QUEUE_DEPTH: usize = 64;
/// WL_ON is read back after each change during the first power cycle, which
/// is two changes; a later cycle only happens when the first found no card.
const WL_ON_LOGGED_CHANGES: u32 = 2;

// ---------------------------------------------------------------------------
// What the stack sees
// ---------------------------------------------------------------------------

pub struct WifiCard {
    mac: Spinlock<[u8; 6]>,
    link: AtomicBool,
}

static CARD: WifiCard = WifiCard { mac: Spinlock::new([0; 6]), link: AtomicBool::new(false) };
static TX: Spinlock<VecDeque<Vec<u8>>> = Spinlock::new(VecDeque::new());
static WAKE: WaitQueue = WaitQueue::new();
static ATTACHED: AtomicBool = AtomicBool::new(false);
static RECEIVED: AtomicU64 = AtomicU64::new(0);
static RX_ERRORS: AtomicU64 = AtomicU64::new(0);
static TX_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Glommed frames received, which this driver does not take apart.
static GLOMS: AtomicU64 = AtomicU64::new(0);

impl Interface for WifiCard {
    /// The firmware's own address, which is zero until bring-up has asked
    /// for it; nothing is sent before then, because the link is down.
    fn mac(&self) -> [u8; 6] {
        *self.mac.lock()
    }

    /// Hand the frame to the driver's task. The contract says this must not
    /// sleep, and a transfer on the SDIO bus is milliseconds of PIO that
    /// belongs in the task that owns the bus.
    fn transmit(&self, frame: &[u8]) -> Result<(), Errno> {
        if !self.link.load(Ordering::Relaxed) {
            return Err(Errno::ENETDOWN);
        }
        {
            let mut queue = TX.lock();
            if queue.len() >= TX_QUEUE_DEPTH {
                TX_DROPPED.fetch_add(1, Ordering::Relaxed);
                return Err(Errno::EAGAIN);
            }
            queue.push_back(frame.to_vec());
        }
        WAKE.wake_all();
        Ok(())
    }

    fn link_up(&self) -> bool {
        self.link.load(Ordering::Relaxed)
    }
}

pub fn attached() -> bool {
    ATTACHED.load(Ordering::Relaxed)
}

pub fn counters() -> (u64, u64, u64) {
    (RECEIVED.load(Ordering::Relaxed), RX_ERRORS.load(Ordering::Relaxed), TX_DROPPED.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Finding it
// ---------------------------------------------------------------------------

/// The pin group the controller node points at, `sdio_pins` on a Pi 4: GPIO
/// 34 to 39 on alternate function 3.
struct Pins {
    /// Kernel address of the GPIO block.
    gpio: u64,
    pins: Vec<u32>,
    function: Vec<u32>,
    pull: Vec<u32>,
}

/// Everything `probe` takes out of the tree, for the task.
struct Found {
    regs: u64,
    four_bit: bool,
    wl_on: Option<u32>,
    pins: Option<Pins>,
    mailbox: Mailbox,
}

static FOUND: Spinlock<Option<Found>> = Spinlock::new(None);

fn be32_cells(value: &[u8]) -> Vec<u32> {
    value.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The firmware GPIO number of an expander line, found by its name in
/// `gpio-line-names`. `gpio-raspberrypi-exp.c` numbers lines from 128.
fn expander_line(name: &[u8]) -> Option<u32> {
    let node = arch::fdt::find_enabled_compatible(EXPANDER_COMPATIBLE)?;
    let names = node.property(b"gpio-line-names")?;
    let index = names.split(|&b| b == 0).position(|entry| entry == name)?;
    Some(mailbox::EXPANDER_BASE + index as u32)
}

/// The pins the controller's `pinctrl-0` names, and the GPIO block they are
/// set through.
fn sdio_pins(controller: &arch::fdt::Node) -> Option<Pins> {
    let group = arch::fdt::find_phandle(controller.cell(b"pinctrl-0")?)?;
    let pins = be32_cells(group.property(b"brcm,pins")?);
    let function = be32_cells(group.property(b"brcm,function")?);
    let pull = group.property(b"brcm,pull").map(be32_cells).unwrap_or_default();
    let (base, size) = arch::fdt::find_enabled_compatible(GPIO_COMPATIBLE)?.reg(0)?;
    if base < arch::DEVICE_PHYS_BASE || base + size > arch::HHDM_LIMIT {
        return None;
    }
    Some(Pins { gpio: phys_to_virt(base), pins, function, pull })
}

/// Find the controller and what drives the chip's power, and claim the card
/// for the stack. Nothing is touched here but the device tree and the mailbox
/// page; the hardware is the task's.
pub fn probe() -> bool {
    let Some(node) = arch::fdt::find_enabled_compatible(CONTROLLER_COMPATIBLE) else {
        if arch::fdt::blob().is_none() {
            crate::println!("wifi: no device tree, so nothing says where the SDIO controller is");
        } else {
            crate::println!("wifi: no enabled \"brcm,bcm2835-sdhci\" node; this board has no WiFi controller");
        }
        return false;
    };
    if arch::fdt::find_compatible(CHIP_COMPATIBLE).is_none() {
        crate::println!("wifi: the device tree names no \"brcm,bcm4329-fmac\" chip; leaving the controller alone");
        return false;
    }
    let Some((base, size)) = node.reg(0) else {
        crate::println!("wifi: the controller node has no address the processor can reach; giving up");
        return false;
    };
    if base < arch::DEVICE_PHYS_BASE || base + size > arch::HHDM_LIMIT {
        crate::println!("wifi: controller registers at {:#x} are outside the device window; giving up", base);
        return false;
    }
    let mailbox = match Mailbox::find() {
        Ok(mailbox) => mailbox,
        Err(why) => {
            crate::println!("wifi: {}; without the firmware there is no clock rate and no power line", why);
            return false;
        }
    };
    let four_bit = node.cell(b"bus-width") == Some(4);
    let wl_on = expander_line(b"WL_ON");
    let pins = sdio_pins(&node);
    crate::println!(
        "wifi: sdio controller at {:#x}, irq {}, {} bus, WL_ON {}, pins {}",
        base,
        node.interrupt(0).map(|i| i as i32).unwrap_or(-1),
        if four_bit { "4-bit" } else { "1-bit" },
        match wl_on {
            Some(line) => format!("is firmware gpio {}", line),
            None => String::from("not named in the tree"),
        },
        match &pins {
            Some(p) => format!("{:?}", p.pins),
            None => String::from("not named in the tree"),
        }
    );
    *FOUND.lock() = Some(Found { regs: phys_to_virt(base), four_bit, wl_on, pins, mailbox });
    ATTACHED.store(true, Ordering::Relaxed);
    crate::net::attach(&CARD);
    true
}

/// Start the driver's task, which does the rest.
pub fn start_task() {
    let space = AddressSpace::current();
    let Some(mut task) = Task::new("wifid", space) else {
        crate::println!("wifi: cannot create the driver task");
        return;
    };
    task.prepare_kernel_frame(task_main as extern "C" fn() -> ! as usize as u64);
    let pid = sched::register(task);
    crate::println!("wifi: driver task is pid {}", pid);
}

extern "C" fn task_main() -> ! {
    let found = FOUND.lock().take();
    if let Some(found) = found {
        let start = now_us();
        match bring_up(found) {
            Ok(dongle) => run(dongle),
            Err(why) => {
                crate::println!("wifi: stopped {} ms into bring-up: {}", (now_us() - start) / 1000, why)
            }
        }
    }
    loop {
        sleep_ms(60_000);
    }
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// Put the SDIO pins on the controller with the pulls the tree gives.
/// `pinctrl-bcm2835.c`: three bits of function select per pin, ten pins to a
/// register; and on this chip two bits of pull per pin, sixteen to a register
/// from 0xe4, where the tree's `BCM2835_PUD_*` values are translated to the
/// BCM2711's encoding.
fn route_pins(pins: &Pins) {
    const GPIO_PUP_PDN_CNTRL_REG0: u64 = 0xE4;
    for (i, &pin) in pins.pins.iter().enumerate() {
        if pin > 57 {
            continue;
        }
        let function = if pins.function.len() == 1 { pins.function[0] } else { pins.function.get(i).copied().unwrap_or(0) };
        let select = pins.gpio + (pin as u64 / 10) * 4;
        let shift = (pin % 10) * 3;
        unsafe {
            let before = core::ptr::read_volatile(select as *const u32);
            let after = (before & !(7 << shift)) | ((function & 7) << shift);
            core::ptr::write_volatile(select as *mut u32, after);
        }
        let pull = if pins.pull.len() == 1 { pins.pull.first().copied() } else { pins.pull.get(i).copied() };
        if let Some(pull) = pull {
            // BCM2835_PUD_OFF, _DOWN, _UP are 0, 1, 2; BCM2711 wants none,
            // up, down as 0, 1, 2.
            let value = match pull {
                0 => 0,
                1 => 2,
                2 => 1,
                _ => continue,
            };
            let register = pins.gpio + GPIO_PUP_PDN_CNTRL_REG0 + (pin as u64 / 16) * 4;
            let shift = (pin % 16) * 2;
            unsafe {
                let before = core::ptr::read_volatile(register as *const u32);
                core::ptr::write_volatile(register as *mut u32, (before & !(3 << shift)) | (value << shift));
            }
        }
    }
}

fn read_file(path: &str) -> Result<Vec<u8>, Errno> {
    let node = crate::fs::lookup(path)?;
    let mut data = vec![0u8; node.size() as usize];
    let n = node.read_at(crate::fs::Offset::START, &mut data)?;
    data.truncate(n);
    Ok(data)
}

fn format_mac(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// A cipher or AKM suite as the standard writes one: OUI, then type.
fn format_suite(suite: u32) -> String {
    format!("{:02X}-{:02X}-{:02X}:{}", suite >> 24, (suite >> 16) & 0xFF, (suite >> 8) & 0xFF, suite & 0xFF)
}

fn format_suites(suites: &[u32]) -> String {
    let mut out = String::new();
    for (i, suite) in suites.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format_suite(*suite));
    }
    if out.is_empty() {
        out.push_str("none");
    }
    out
}

impl From<SdioError> for String {
    fn from(error: SdioError) -> String {
        format!("{}", error)
    }
}

fn bring_up(mut found: Found) -> Result<Dongle, String> {
    let mailbox = &mut found.mailbox;
    let base_clock = mailbox
        .clock_rate(mailbox::CLOCK_EMMC)
        .map_err(|e| format!("asking the firmware for the EMMC clock's rate: {:?}", e))?;
    crate::println!("wifi: the firmware says the EMMC clock runs at {} Hz", base_clock);
    if base_clock == 0 {
        return Err(String::from("the EMMC clock is stopped"));
    }

    if let Some(line) = found.wl_on {
        match (mailbox.gpio_config(line), mailbox.gpio_state(line)) {
            (Ok(config), Ok(state)) => crate::println!(
                "wifi: WL_ON before: direction {}, polarity {}, state {}",
                config.direction,
                config.polarity,
                state
            ),
            (config, state) => crate::println!("wifi: WL_ON cannot be read: {:?} {:?}", config.err(), state.err()),
        }
    } else {
        crate::println!("wifi: no line is named WL_ON, so the chip's power is left as the firmware set it");
    }

    if let Some(pins) = &found.pins {
        route_pins(pins);
    }

    let mut host = unsafe { sdhci::Sdhci::new(found.regs, base_clock) };
    let (caps, caps1) = host.capabilities();
    crate::println!(
        "wifi: sdhci version {:#06x}, capabilities {:#010x} {:#010x}, present state {:#010x}",
        host.version(),
        caps,
        caps1,
        host.present_state()
    );
    host.init().map_err(|e| format!("resetting the controller: {}", e))?;

    let wl_on = found.wl_on;
    let card = {
        // `mmc-pwrseq-simple` with `reset-gpios = <&expgpio 1 GPIO_ACTIVE_LOW>`,
        // upstream's description of this board: the reset asserted is the line
        // low, and released is the line high. What the firmware's reply says
        // is not taken as proof the line moved; the line is read back.
        let mut changes = 0u32;
        let mut power = |run: bool| {
            let Some(line) = wl_on else { return };
            let wanted = run as u32;
            let set = mailbox.gpio_output(line, wanted);
            if changes < WL_ON_LOGGED_CHANGES {
                match (set, mailbox.gpio_state(line)) {
                    (Ok(()), Ok(state)) => crate::println!("wifi: WL_ON set to {}, reads back {}", wanted, state),
                    (set, state) => crate::println!(
                        "wifi: WL_ON set to {}: {:?}, read back: {:?}",
                        wanted,
                        set.err(),
                        state
                    ),
                }
            } else if let Err(e) = set {
                crate::println!("wifi: setting WL_ON {}: {:?}", wanted, e);
            }
            changes += 1;
        };
        match sdio::attach(host, &mut power) {
            Ok(card) => card,
            Err((_, error)) => return Err(format!("no SDIO card answered: {}", error)),
        }
    };

    let id = card.identity;
    crate::println!(
        "wifi: sdio card {:04x}:{:04x}, {} functions, rca {:#06x}, ocr {:#010x}, CCCR {} SDIO {}, {}{}{}{}speed byte {:#04x} ({} Hz)",
        id.vendor,
        id.device,
        id.functions,
        id.rca,
        id.ocr,
        id.cccr_version,
        id.sdio_version,
        if id.multi_block { "multi-block " } else { "" },
        if id.high_speed { "high-speed " } else { "" },
        if id.wide_bus { "4-bit-low-speed " } else { "" },
        if id.low_speed { "low-speed " } else { "" },
        id.max_tran_speed,
        id.max_dtr
    );
    for function in 1..=id.functions.min(7) {
        let f = card.function(function);
        crate::println!(
            "wifi: function {}: {:04x}:{:04x}, largest block {}, enable timeout {} ms",
            function,
            f.vendor,
            f.device,
            f.max_block_size,
            f.enable_timeout_ms
        );
    }
    let device = card.function(chip::FUNC_BACKPLANE).device;

    let mut card = card;
    // `brcmf_sdiod_probe`.
    card.set_block_size(chip::FUNC_BACKPLANE, chip::F1_BLOCK_SIZE)?;
    card.set_block_size(chip::FUNC_WLAN, chip::F2_BLOCK_SIZE)?;
    let waited = card.enable_function(chip::FUNC_BACKPLANE, None)?;
    crate::println!("wifi: function 1 ready after {} us", waited);

    let mut bp = Backplane::new(card);
    let slow = bp.read32(chip::SI_ENUM_BASE)?;
    crate::println!(
        "wifi: chip id {:#06x} read over the backplane at {} Hz",
        slow & 0xFFFF,
        bp.card().host().clock()
    );
    let clock = bp.card().enable_full_speed(found.four_bit)?;
    let fast = bp.read32(chip::SI_ENUM_BASE)?;
    crate::println!(
        "wifi: now at {} Hz on {} data lines; chip id register reads {:#010x}{}",
        clock,
        if found.four_bit { 4 } else { 1 },
        fast,
        if fast == slow { ", the same" } else { ", which is DIFFERENT" }
    );
    if fast != slow {
        return Err(String::from("the chip id reads differently at full speed"));
    }

    let chip = chip::attach(&mut bp)?;

    let firmware = read_file(FIRMWARE_PATH)
        .map_err(|_| format!("{} is not in the image; run scripts/fetch-wifi-firmware.sh", FIRMWARE_PATH))?;
    let text = read_file(NVRAM_PATH).map_err(|_| format!("{} is not in the image", NVRAM_PATH))?;
    let nvram = nvram::strip(&text).map_err(|e| format!("the NVRAM file cannot be used: {:?}", e))?;
    let clm = read_file(CLM_PATH).ok();
    crate::println!(
        "wifi: firmware {} bytes, NVRAM {} bytes of text making {}, CLM {}",
        firmware.len(),
        text.len(),
        nvram.len(),
        match &clm {
            Some(c) => format!("{} bytes", c.len()),
            None => String::from("absent"),
        }
    );

    chip::download(&mut bp, &chip, &firmware, &nvram)?;
    drop(firmware);
    let save_restore = chip::start(&mut bp, &chip, device)?;
    crate::println!("wifi: firmware started, save/restore {}", if save_restore { "on" } else { "off" });

    let mut dongle = Dongle::new(bp, chip);
    dongle.preinit(clm.as_deref())?;

    let config = match config::load() {
        Ok(config) => {
            // Nothing about the name or the passphrase is printed, not even
            // their lengths.
            crate::println!("wifi: {} read, country {}", config::PATH, config.country);
            Some(config)
        }
        Err(why) => {
            crate::println!("wifi: {}; scanning only", why);
            None
        }
    };
    let country = config.as_ref().map(|c| c.country).unwrap_or(config::Country(*b"JP"));
    dongle.configure(country)?;
    dongle.scan(config.as_ref())?;
    dongle.config = config;
    Ok(dongle)
}

// ---------------------------------------------------------------------------
// The firmware
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum IoctlError {
    Bus(SdioError),
    NoCredit,
    Timeout,
    Firmware(i32),
}

impl From<SdioError> for IoctlError {
    fn from(error: SdioError) -> IoctlError {
        IoctlError::Bus(error)
    }
}

impl core::fmt::Display for IoctlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IoctlError::Bus(error) => write!(f, "{}", error),
            IoctlError::NoCredit => write!(f, "the firmware gave no credit to send"),
            IoctlError::Timeout => write!(f, "no response within {} ms", CONTROL_TIMEOUT_MS),
            IoctlError::Firmware(code) => write!(f, "the firmware answered with error {}", code),
        }
    }
}

impl From<IoctlError> for String {
    fn from(error: IoctlError) -> String {
        format!("{}", error)
    }
}

/// The chip with firmware running, and the state of the SDPCM link to it.
/// brcmfmac keeps the same in `struct brcmf_sdio`.
pub struct Dongle {
    bp: Backplane,
    chip: Chip,
    sdio_core: u32,
    /// The next sequence number to send and the highest the firmware allows.
    /// `brcmf_sdio_probe` starts `tx_seq` at `SDPCM_SEQ_WRAP - 1`, and
    /// `tx_max` is zero until a header says otherwise.
    tx_seq: u8,
    tx_max: u8,
    rx_seq: u8,
    reqid: u16,
    flow_control: u8,
    /// A frame was NAKed and the firmware has not yet said it will resend.
    rxskip: bool,
    /// The last read stopped at `RXBOUND` with frames still waiting.
    rxpending: bool,
    frame: Vec<u8>,
    rx: Vec<u8>,
    response: Option<(DcmdResponse, Vec<u8>)>,
    events: VecDeque<Event>,
    scan: Vec<protocol::Bss>,
    scan_done: Option<u32>,
    mac: [u8; 6],
    config: Option<config::Config>,
    announced_ready: bool,
}

impl Dongle {
    fn new(bp: Backplane, chip: Chip) -> Dongle {
        let sdio_core = chip.sdio_core().map(|c| c.base).unwrap_or(0);
        Dongle {
            bp,
            chip,
            sdio_core,
            tx_seq: 255,
            tx_max: 0,
            rx_seq: 0,
            reqid: 0,
            flow_control: 0,
            rxskip: false,
            rxpending: false,
            frame: Vec::with_capacity(protocol::MAX_CONTROL),
            rx: Vec::with_capacity(protocol::MAX_CONTROL),
            response: None,
            events: VecDeque::new(),
            scan: Vec::new(),
            scan_done: None,
            mac: [0; 6],
            config: None,
            announced_ready: false,
        }
    }

    /// `txctl_ok` and `data_ok`: the firmware's window is open.
    fn has_credit(&self) -> bool {
        let room = self.tx_max.wrapping_sub(self.tx_seq);
        room != 0 && room & 0x80 == 0
    }

    /// Look at the SDIO core's interrupt status and do what it asks.
    /// `brcmf_sdio_dpc`, less the clock and flow-control-queue handling this
    /// driver has no use for.
    fn poll(&mut self) -> Result<(), SdioError> {
        let address = self.sdio_core + chip::SD_INTSTATUS;
        let mut status = self.bp.read32(address)? & chip::HOSTINTMASK;
        if status != 0 {
            self.bp.write32(address, status)?;
        }
        if status & chip::I_HMB_FC_CHANGE != 0 {
            status &= !chip::I_HMB_FC_CHANGE;
            self.bp.write32(address, chip::I_HMB_FC_CHANGE)?;
            let now = self.bp.read32(address)?;
            status |= now & chip::HOSTINTMASK;
        }
        if status & chip::I_HMB_HOST_INT != 0 {
            status |= self.host_mail()?;
        }
        if self.rxskip {
            status &= !chip::I_HMB_FRAME_IND;
        }
        if status & chip::I_HMB_FRAME_IND != 0 || self.rxpending {
            self.read_frames()?;
        }
        Ok(())
    }

    /// `brcmf_sdio_hostmail`.
    fn host_mail(&mut self) -> Result<u32, SdioError> {
        let data = self.bp.read32(self.sdio_core + chip::SD_TOHOSTMAILBOXDATA)?;
        self.bp.write32(self.sdio_core + chip::SD_TOSBMAILBOX, chip::SMB_INT_ACK)?;
        let mut status = 0;
        if data & chip::HMB_DATA_FWHALT != 0 {
            crate::println!("wifi: the firmware reports that it has halted");
        }
        if data & chip::HMB_DATA_NAKHANDLED != 0 {
            self.rxskip = false;
            status |= chip::I_HMB_FRAME_IND;
        }
        if data & (chip::HMB_DATA_DEVREADY | chip::HMB_DATA_FWREADY) != 0 && !self.announced_ready {
            let version = (data & chip::HMB_DATA_VERSION_MASK) >> chip::HMB_DATA_VERSION_SHIFT;
            crate::println!(
                "wifi: the firmware says it is ready, SDPCM protocol version {}{}",
                version,
                if version == chip::SDPCM_PROT_VERSION { "" } else { ", which is not the version this driver speaks" }
            );
            self.announced_ready = true;
        }
        if data & chip::HMB_DATA_FC != 0 {
            self.flow_control = (data >> 24) as u8;
        }
        Ok(status)
    }

    /// `brcmf_sdio_rxfail`.
    fn rx_fail(&mut self, abort: bool, rtx: bool) -> Result<(), SdioError> {
        RX_ERRORS.fetch_add(1, Ordering::Relaxed);
        if abort {
            self.bp.card().write_byte(0, sdio::CCCR_ABORT, chip::FUNC_WLAN)?;
        }
        self.bp.write8(chip::SBSDIO_FUNC1_FRAMECTRL, 1)?;
        for _ in 0..0xFFFF {
            let hi = self.bp.read8(chip::SBSDIO_FUNC1_RFRAMEBCHI)?;
            let lo = self.bp.read8(chip::SBSDIO_FUNC1_RFRAMEBCLO)?;
            if hi == 0 && lo == 0 {
                break;
            }
        }
        if rtx {
            self.bp.write32(self.sdio_core + chip::SD_TOSBMAILBOX, chip::SMB_NAK)?;
            self.rxskip = true;
        }
        Ok(())
    }

    /// `brcmf_sdio_txfail`.
    fn tx_fail(&mut self) {
        let _ = self.bp.card().write_byte(0, sdio::CCCR_ABORT, chip::FUNC_WLAN);
        let _ = self.bp.write8(chip::SBSDIO_FUNC1_FRAMECTRL, 2);
        for _ in 0..3 {
            let hi = self.bp.read8(chip::SBSDIO_FUNC1_WFRAMEBCHI).unwrap_or(0);
            let lo = self.bp.read8(chip::SBSDIO_FUNC1_WFRAMEBCLO).unwrap_or(0);
            if hi == 0 && lo == 0 {
                break;
            }
        }
    }

    /// `brcmf_sdio_readframes` without glomming, which this driver turns off
    /// in `preinit`: each frame's first 64 bytes, then the rest padded the
    /// way the reference pads it. The length hint in each header, which lets
    /// the reference skip the first read of the next frame, is not used.
    fn read_frames(&mut self) -> Result<(), SdioError> {
        self.rxpending = false;
        for _ in 0..RXBOUND {
            let mut head = [0u8; protocol::FIRST_READ];
            if let Err(error) = self.bp.f2_read(&mut head) {
                crate::println!("wifi: reading a frame header failed: {}", error);
                self.rx_fail(true, true)?;
                return Ok(());
            }
            let header = match protocol::parse_header(&head) {
                Ok(header) => header,
                Err(HeaderError::NoData) => return Ok(()),
                Err(HeaderError::TooShort) => {
                    RX_ERRORS.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(error) => {
                    crate::println!("wifi: bad frame header: {:?}", error);
                    self.rx_fail(false, false)?;
                    continue;
                }
            };
            // `brcmf_sdio_hdparse`: the window, with its sanity check.
            let mut window = header.window;
            if window.wrapping_sub(self.tx_seq) > 0x40 {
                window = self.tx_seq.wrapping_add(2);
            }
            self.tx_max = window;
            self.flow_control = header.flow_control;
            self.rx_seq = header.seq.wrapping_add(1);

            let len = header.len as usize;
            let rest = if header.channel == protocol::CHANNEL_CONTROL {
                protocol::control_remaining(len)
            } else {
                protocol::rx_remaining(len)
            };
            let mut rx = core::mem::take(&mut self.rx);
            rx.clear();
            rx.extend_from_slice(&head);
            rx.resize(protocol::FIRST_READ + rest, 0);
            if rest > 0 {
                if let Err(error) = self.bp.f2_read(&mut rx[protocol::FIRST_READ..]) {
                    crate::println!("wifi: reading {} bytes of a frame failed: {}", rest, error);
                    self.rx = rx;
                    self.rx_fail(true, header.channel == protocol::CHANNEL_EVENT)?;
                    continue;
                }
            }
            let payload = &rx[header.data_offset as usize..len.min(rx.len())];
            match header.channel {
                protocol::CHANNEL_CONTROL => {
                    if let Some((response, data)) = protocol::parse_dcmd(payload) {
                        self.response = Some((response, data.to_vec()));
                    }
                }
                protocol::CHANNEL_EVENT => {
                    if let Some((_, packet)) = protocol::strip_bcdc(payload) {
                        if let Some((event, data)) = protocol::parse_event(packet) {
                            self.on_event(event, data);
                        }
                    }
                }
                protocol::CHANNEL_DATA => {
                    if let Some((_, frame)) = protocol::strip_bcdc(payload) {
                        RECEIVED.fetch_add(1, Ordering::Relaxed);
                        if CARD.link.load(Ordering::Relaxed) {
                            crate::net::receive(frame);
                        }
                    }
                }
                protocol::CHANNEL_GLOM => {
                    GLOMS.fetch_add(1, Ordering::Relaxed);
                }
                other => {
                    crate::println!("wifi: a frame on channel {}, which this driver does not take", other);
                }
            }
            self.rx = rx;
        }
        self.rxpending = true;
        Ok(())
    }

    fn on_event(&mut self, event: Event, data: &[u8]) {
        match event.event_type {
            protocol::E_ESCAN_RESULT => {
                if event.status == protocol::E_STATUS_PARTIAL {
                    if let Some(bss) = protocol::parse_escan_result(data) {
                        if self.scan.len() < 256 {
                            self.scan.push(bss);
                        }
                    }
                } else {
                    self.scan_done = Some(event.status);
                }
            }
            _ => {
                if self.events.len() >= 64 {
                    self.events.pop_front();
                }
                self.events.push_back(event);
            }
        }
    }

    /// One control request and its response. `brcmf_proto_bcdc_msg` through
    /// `brcmf_sdio_bus_txctl` and back through `brcmf_proto_bcdc_cmplt`.
    fn ioctl(&mut self, cmd: u32, set: bool, buflen: usize, payload: &[u8]) -> Result<Vec<u8>, IoctlError> {
        let deadline = Deadline::after_ms(CONTROL_TIMEOUT_MS);
        // `brcmf_sdio_dpc` sends a control frame only with credit and with
        // function 2 ready (`brcmf_sdio_f2_ready`, CCCR IORx).
        loop {
            let ready = self.bp.card().read_byte(0, sdio::CCCR_IORX)? & (1 << chip::FUNC_WLAN) != 0;
            if ready && self.has_credit() {
                break;
            }
            if deadline.expired() {
                return Err(IoctlError::NoCredit);
            }
            self.poll()?;
            sleep_ms(1);
        }
        self.reqid = self.reqid.wrapping_add(1);
        protocol::control_frame(&mut self.frame, self.tx_seq, self.reqid, cmd, set, 0, buflen, payload);
        let mut sent = Err(SdioError { what: "not sent", host: None });
        for _ in 0..=TXRETRIES {
            sent = self.bp.f2_write(&self.frame);
            match sent {
                Ok(()) => {
                    self.tx_seq = self.tx_seq.wrapping_add(1);
                    break;
                }
                Err(_) => self.tx_fail(),
            }
        }
        // A request may carry a secret; the scratch buffer does not keep it.
        self.frame.fill(0);
        sent?;

        self.response = None;
        let deadline = Deadline::after_ms(CONTROL_TIMEOUT_MS);
        let mut spins = 0;
        loop {
            self.poll()?;
            if let Some((response, data)) = self.response.take() {
                if response.id == self.reqid {
                    if let Some(code) = response.error {
                        return Err(IoctlError::Firmware(code));
                    }
                    return Ok(data);
                }
            }
            if deadline.expired() {
                return Err(IoctlError::Timeout);
            }
            // Most answers come back within a millisecond or two, so the
            // first few looks are close together.
            if spins < 20 {
                spin_us(250);
                spins += 1;
            } else {
                sleep_ms(1);
            }
        }
    }

    /// `brcmf_fil_iovar_data_get`: the name, then `len` bytes of room.
    fn get_iovar(&mut self, name: &str, len: usize) -> Result<Vec<u8>, IoctlError> {
        let request = protocol::iovar(name, &vec![0u8; len]);
        let mut data = self.ioctl(protocol::C_GET_VAR, false, request.len(), &request)?;
        data.truncate(len);
        Ok(data)
    }

    fn set_iovar(&mut self, name: &str, value: &[u8]) -> Result<(), IoctlError> {
        let request = protocol::iovar(name, value);
        self.ioctl(protocol::C_SET_VAR, true, request.len(), &request).map(|_| ())
    }

    fn set_iovar_u32(&mut self, name: &str, value: u32) -> Result<(), IoctlError> {
        self.set_iovar(name, &value.to_le_bytes())
    }

    fn get_iovar_u32(&mut self, name: &str) -> Result<u32, IoctlError> {
        let data = self.get_iovar(name, 4)?;
        if data.len() < 4 {
            return Err(IoctlError::Firmware(0));
        }
        Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
    }

    fn set_u32(&mut self, cmd: u32, value: u32) -> Result<(), IoctlError> {
        self.ioctl(cmd, true, 4, &value.to_le_bytes()).map(|_| ())
    }

    /// `brcmf_sdio_bus_preinit` and the parts of `brcmf_c_preinit_dcmds` that
    /// matter here: the address, the regulatory data, and the version.
    fn preinit(&mut self, clm: Option<&[u8]>) -> Result<(), String> {
        // `brcmf_sdio_bus_preinit`, which names these from the device's side:
        // "bus:txglom" is the firmware putting several frames to the host in
        // one transfer. Below SDIO core revision 12 the reference turns that
        // off; from 12 on it sets the alignment instead and takes glommed
        // frames apart in `brcmf_sdio_rxglom`. This driver does not port that,
        // and nothing here needs the throughput it buys, so it sets the
        // alignment as the reference does and then turns glomming off on every
        // revision, which is what embassy's cyw43 does in `Control::init`.
        let sdio_rev = self.chip.sdio_core().map(|c| c.rev).unwrap_or(0);
        if sdio_rev >= 12 {
            if let Err(error) = self.set_iovar_u32("bus:txglomalign", protocol::HEAD_ALIGN as u32) {
                crate::println!("wifi: bus:txglomalign refused: {}", error);
            }
        }
        match self.set_iovar_u32("bus:txglom", 0) {
            Ok(()) => crate::println!("wifi: frame glomming towards the host turned off (SDIO core rev {})", sdio_rev),
            Err(error) => crate::println!("wifi: the firmware refused to turn glomming off: {}", error),
        }

        let mac = self.get_iovar("cur_etheraddr", 6)?;
        if mac.len() == 6 {
            self.mac.copy_from_slice(&mac);
            *CARD.mac.lock() = self.mac;
        }
        crate::println!("wifi: the firmware's address is {}", format_mac(&self.mac));

        match self.ioctl(protocol::C_GET_REVINFO, false, REVINFO_LEN, &[0u8; REVINFO_LEN]) {
            Ok(info) if info.len() >= REVINFO_LEN => {
                let word = |i: usize| u32::from_le_bytes([info[i * 4], info[i * 4 + 1], info[i * 4 + 2], info[i * 4 + 3]]);
                crate::println!(
                    "wifi: revinfo: chip {:#x} rev {}, board {:#x} rev {:#x}, ucode {:#x}, phy {} rev {}",
                    word(11),
                    word(3),
                    word(5),
                    word(7),
                    word(9),
                    word(12),
                    word(13)
                );
            }
            Ok(_) => crate::println!("wifi: revinfo answer too short"),
            Err(error) => crate::println!("wifi: revinfo: {}", error),
        }

        // `brcmf_c_download_blob`.
        if let Some(clm) = clm {
            let mut flag = protocol::DL_BEGIN;
            let mut pieces = 0;
            let mut sent = 0;
            let mut failed = None;
            while sent < clm.len() {
                let size = (clm.len() - sent).min(protocol::CLM_CHUNK);
                if sent + size == clm.len() {
                    flag |= protocol::DL_END;
                }
                let chunk = protocol::clm_chunk(flag, &clm[sent..sent + size]);
                if let Err(error) = self.set_iovar("clmload", &chunk) {
                    failed = Some(error);
                    break;
                }
                flag &= !protocol::DL_BEGIN;
                sent += size;
                pieces += 1;
            }
            match failed {
                None => crate::println!("wifi: CLM blob loaded, {} bytes in {} pieces", clm.len(), pieces),
                Some(error) => {
                    let status = self.get_iovar_u32("clmload_status");
                    return Err(format!(
                        "loading the CLM blob: {}, clmload_status {}",
                        error,
                        match status {
                            Ok(s) => format!("{}", s),
                            Err(e) => format!("unreadable ({})", e),
                        }
                    ));
                }
            }
        } else {
            crate::println!("wifi: no CLM blob, so the firmware has only its built-in channels");
        }

        let version = self.get_iovar("ver", DCMD_SMLEN)?;
        crate::println!("wifi: firmware: {}", first_line(&version));
        match self.get_iovar("clmver", DCMD_SMLEN) {
            Ok(clmver) => crate::println!("wifi: CLM: {}", first_line(&clmver)),
            Err(error) => crate::println!("wifi: clmver: {}", error),
        }
        if let Err(error) = self.set_iovar_u32("mpc", 1) {
            crate::println!("wifi: mpc: {}", error);
        }
        Ok(())
    }

    /// The country, the events to be told about, and the interface up.
    fn configure(&mut self, country: config::Country) -> Result<(), String> {
        match self.set_iovar("country", &protocol::country(country.0)) {
            Ok(()) => match self.get_iovar("country", 12) {
                Ok(answer) if answer.len() >= 12 => crate::println!(
                    "wifi: country set to {}; the firmware reports {}{} revision {}",
                    country,
                    answer[8] as char,
                    answer[9] as char,
                    i32::from_le_bytes([answer[4], answer[5], answer[6], answer[7]])
                ),
                _ => crate::println!("wifi: country set to {}", country),
            },
            Err(error) => crate::println!("wifi: the firmware refused country {}: {}", country, error),
        }

        // `brcmf_fweh_activate_events`: the extended form first, then the
        // plain mask.
        let mask = protocol::event_mask(&[
            protocol::E_SET_SSID,
            protocol::E_AUTH,
            protocol::E_DEAUTH,
            protocol::E_DEAUTH_IND,
            protocol::E_ASSOC,
            protocol::E_REASSOC,
            protocol::E_DISASSOC,
            protocol::E_DISASSOC_IND,
            protocol::E_LINK,
            protocol::E_PRUNE,
            protocol::E_PSK_SUP,
            protocol::E_IF,
            protocol::E_ESCAN_RESULT,
        ]);
        let mut ext = vec![1u8, 3, protocol::EVENTING_MASK_LEN as u8, 0];
        ext.extend_from_slice(&mask);
        if self.set_iovar("event_msgs_ext", &ext).is_err() {
            self.set_iovar("event_msgs", &mask)?;
        }

        // `brcmf_config_dongle` sends it with zero.
        self.set_u32(protocol::C_UP, 0)?;
        crate::println!("wifi: interface up");
        Ok(())
    }

    /// Scan every channel and say what was seen, without naming any network:
    /// how many, on which channels, and whether the configured one was among
    /// them, with the security it advertises.
    fn scan(&mut self, config: Option<&config::Config>) -> Result<(), String> {
        self.scan.clear();
        self.scan_done = None;
        let start = now_us();
        self.set_iovar("escan", &protocol::escan_request(0x1234))?;
        let deadline = Deadline::after_ms(SCAN_TIMEOUT_MS);
        while self.scan_done.is_none() {
            if deadline.expired() {
                crate::println!("wifi: the scan did not finish within {} ms", SCAN_TIMEOUT_MS);
                break;
            }
            self.poll()?;
            sleep_ms(10);
        }
        let mut channels: Vec<(u8, usize)> = Vec::new();
        let mut distinct = 0;
        for (i, bss) in self.scan.iter().enumerate() {
            // The same network is reported once per beacon or probe response
            // heard, so count each BSSID once.
            if self.scan[..i].iter().any(|seen| seen.bssid == bss.bssid) {
                continue;
            }
            distinct += 1;
            match channels.iter_mut().find(|(channel, _)| *channel == bss.channel) {
                Some((_, count)) => *count += 1,
                None => channels.push((bss.channel, 1)),
            }
        }
        channels.sort();
        let mut summary = String::new();
        for (channel, count) in &channels {
            summary.push_str(&format!(" {}x{}", channel, count));
        }
        crate::println!(
            "wifi: scan finished in {} ms with status {:?}: {} results, {} distinct networks; by channel:{}; {} glommed frames received so far",
            (now_us() - start) / 1000,
            self.scan_done,
            self.scan.len(),
            distinct,
            summary,
            GLOMS.load(Ordering::Relaxed)
        );
        if let Some(config) = config {
            match self.scan.iter().filter(|bss| config.ssid.matches(bss.ssid())).max_by_key(|bss| bss.rssi) {
                Some(bss) => {
                    crate::println!("wifi: found the configured network on channel {}", bss.channel);
                    let s = bss.security;
                    let caps = s.capabilities.unwrap_or(0);
                    crate::println!(
                        "wifi: its security: privacy {}, RSN element {}, WPA element {}; group {}, pairwise {}, AKM {}; capabilities {} (MFP capable {}, MFP required {})",
                        bss.capability & 0x10 != 0,
                        s.rsn,
                        s.wpa,
                        if s.rsn { format_suite(s.group) } else { String::from("none") },
                        format_suites(&s.pairwise[..s.pairwise_count]),
                        format_suites(&s.akm[..s.akm_count]),
                        match s.capabilities {
                            Some(c) => format!("{:#06x}", c),
                            None => String::from("absent"),
                        },
                        caps & protocol::RSN_CAP_MFPC != 0,
                        caps & protocol::RSN_CAP_MFPR != 0
                    );
                }
                None => crate::println!("wifi: the configured network was not among them"),
            }
        }
        Ok(())
    }
}

fn first_line(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn run(mut dongle: Dongle) -> ! {
    let mut last_tick = crate::trap::ticks();
    let mut errors = 0u32;
    loop {
        WAKE.wait_until_or_at(last_tick + 1, || !TX.lock().is_empty());
        if let Err(error) = dongle.poll() {
            errors += 1;
            if errors <= 5 {
                crate::println!("wifi: polling the chip failed: {}", error);
            }
        }
        while let Some(event) = dongle.events.pop_front() {
            crate::println!(
                "wifi: event {} status {} reason {} flags {:#x}",
                event.event_type,
                event.status,
                event.reason,
                event.flags
            );
        }
        loop {
            let next = TX.lock().pop_front();
            let Some(frame) = next else { break };
            // Data frames are not sent yet; nothing queues one while the
            // link is down.
            drop(frame);
            TX_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        let now = crate::trap::ticks();
        if now != last_tick {
            let missed = (now - last_tick).min(8);
            for _ in 0..missed {
                crate::net::tick();
            }
            last_tick = now;
        }
    }
}
