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
//!   bring-up, the request and response loop with the firmware, joining, and
//!   the card the network stack sends through.
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
//! **Joining.** The chip's standard firmware has no supplicant, so the WPA2
//! handshake runs in the kernel, in `wpa`, the way Raspberry Pi OS runs
//! wpa_supplicant beside brcmfmac. The driver sets the security and the
//! station's RSN element and asks to join, as `brcmf_cfg80211_connect` does
//! without a firmware supplicant. Once the firmware reports the association,
//! EAPOL frames from the access point go to the supplicant, its replies go out
//! as ordinary data frames, and the keys it produces are installed with the
//! "wsec_key" iovar. The link the stack sees is up only once the pairwise and
//! group keys are in.
//!
//! **Getting the link back.** Nothing in user space notices a lost link and
//! tries again, so the driver does, from bring-up on and for as long as there
//! is no link: a fresh scan, the strongest usable access point with the
//! configured name from that scan, a join that names it, and the handshake,
//! repeated with growing waits until the link is up. `reconnect` decides the
//! order and the timing, and this file carries it out. A scan does not stop
//! the task: the request goes out, the results arrive as events the loop reads
//! once a tick, and frames are sent and received in between.
//!
//! **Nothing is signalled by interrupt yet.** The chip is polled once a
//! timer tick and whenever a frame is queued, the way brcmfmac runs a bus in
//! its poll mode.

pub mod chip;
pub mod config;
pub mod delay;
pub mod nvram;
pub mod protocol;
pub mod reconnect;
pub mod sdhci;
pub mod sdio;
pub mod test;
pub mod wpa;

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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use delay::{now_us, sleep_ms, spin_us, Deadline};
use protocol::{DcmdResponse, Event, HeaderError};
use reconnect::{Effect, Failure, Input, Reconnect};
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
/// `sizeof(struct brcmf_rev_info_le)`: seventeen words.
const REVINFO_LEN: usize = 68;
/// `MAX_CAPS_BUFFER_SIZE` in `feature.c`: room for the "cap" string.
const CAPS_LEN: usize = 768;
/// Frames waiting for the task. The same depth as the wired driver's queue.
const TX_QUEUE_DEPTH: usize = 64;
/// WL_ON is read back after each change during the first power cycle, which
/// is two changes; a later cycle only happens when the first found no card.
const WL_ON_LOGGED_CHANGES: u32 = 2;
/// How long a data frame waits for the firmware's window before it is
/// dropped, the way a full ring drops one.
const DATA_CREDIT_WAIT_MS: u64 = 100;
/// `WLAN_REASON_DEAUTH_LEAVING` in hostap's `ieee802_11_defs.h`: the reason
/// wpa_supplicant gives when it leaves an association itself, as when an
/// authentication times out in `wpa_supplicant_timeout`.
const REASON_DEAUTH_LEAVING: u16 = 3;
/// EAPOL frames waiting for the task's loop. A handshake is four frames and a
/// rekey two, so a few is plenty; more than that is dropped.
const EAPOL_QUEUE_DEPTH: usize = 8;
/// How old a frame held from before the association may be and still be
/// handled: `wpa_supplicant_event_assoc` in hostap's `events.c` takes the
/// pending frame when it is under 200 ms old.
const PENDING_EAPOL_MAX_AGE_US: u64 = 200_000;

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
static SENT: AtomicU64 = AtomicU64::new(0);
static RX_ERRORS: AtomicU64 = AtomicU64::new(0);
static TX_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Glommed frames received, which this driver does not take apart.
static GLOMS: AtomicU64 = AtomicU64::new(0);
/// Data frames that arrived while the link was down, which are dropped.
static EARLY_FRAMES: AtomicU64 = AtomicU64::new(0);
/// How many of those are described in the log.
const EARLY_FRAMES_LOGGED: u64 = 8;
/// `wifi.hide=N` on the command line: the configured network is taken as
/// absent from the first N scans, bring-up's included, so that a network
/// missing at boot can be shown without switching an access point off. Zero,
/// the default, is off.
static HIDE_SCANS: AtomicU32 = AtomicU32::new(0);
/// `wifi.drop=S`: S seconds after the link first comes up, the driver
/// disassociates once and tells the reconnect machine the link was lost, so
/// that losing the link can be shown without touching the access point. Zero,
/// the default, is off.
static DROP_AFTER_S: AtomicU64 = AtomicU64::new(0);
/// `ETH_P_PAE` in Linux's `if_ether.h`: IEEE 802.1X, which carries the WPA2
/// handshake.
const ETHERTYPE_EAPOL: u16 = 0x888E;

/// Describe one of the first frames to arrive before the link is up. With the
/// firmware doing the handshake, an EAPOL-Key frame should never reach the
/// host, so one here says the firmware passed the access point's message on
/// instead of answering it. Only the type, the length and the EAPOL-Key
/// information field are printed, and none of them is key material.
fn note_early_frame(frame: &[u8]) {
    let n = EARLY_FRAMES.fetch_add(1, Ordering::Relaxed);
    if n >= EARLY_FRAMES_LOGGED || frame.len() < 14 {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    let body = &frame[14..];
    if ethertype == ETHERTYPE_EAPOL && body.len() >= 7 {
        // IEEE 802.1X-2004 section 7.5: protocol version, packet type and body
        // length; then IEEE 802.11-2016 section 12.7.2: descriptor type and the
        // key information field, big-endian.
        crate::println!(
            "wifi: before the link, an EAPOL frame of {} bytes: version {}, packet type {}, descriptor {}, key information {:#06x}",
            frame.len(),
            body[0],
            body[1],
            body[4],
            u16::from_be_bytes([body[5], body[6]])
        );
    } else {
        crate::println!("wifi: before the link, a frame of {} bytes with ethertype {:#06x}", frame.len(), ethertype);
    }
}

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

/// Frames received, receive errors, and frames dropped on the way out.
pub fn counters() -> (u64, u64, u64) {
    (RECEIVED.load(Ordering::Relaxed), RX_ERRORS.load(Ordering::Relaxed), TX_DROPPED.load(Ordering::Relaxed))
}

/// `wifi.hide=` and `wifi.drop=` from the command line, before the driver's
/// task starts. Each is printed when it is on, so a log shows why the network
/// went missing or the link dropped.
pub fn set_debug(hide_scans: u32, drop_after_s: u64) {
    HIDE_SCANS.store(hide_scans, Ordering::Relaxed);
    DROP_AFTER_S.store(drop_after_s, Ordering::Relaxed);
    if hide_scans != 0 {
        crate::println!("wifi: wifi.hide={}: the configured network is taken as absent from the first {} scans", hide_scans, hide_scans);
    }
    if drop_after_s != 0 {
        crate::println!("wifi: wifi.drop={}: the link is dropped once, {} s after it first comes up", drop_after_s, drop_after_s);
    }
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

/// The reconnect machine's clock: milliseconds on the architected counter.
fn now_ms() -> u64 {
    now_us() / 1000
}

/// Whether the supplicant can join this access point, or why not: WPA2-PSK
/// with CCMP for pairwise and group traffic, and management frame protection
/// not required.
fn usable(bss: &protocol::Bss) -> Result<(), &'static str> {
    let s = bss.security;
    let offers = s.rsn
        && s.group == wpa::SUITE_CCMP
        && s.pairwise[..s.pairwise_count].contains(&wpa::SUITE_CCMP)
        && s.akm[..s.akm_count].contains(&protocol::AKM_PSK);
    if !offers {
        return Err("the network does not offer WPA2-PSK with CCMP for pairwise and group traffic");
    }
    if s.capabilities.unwrap_or(0) & protocol::RSN_CAP_MFPR != 0 {
        return Err("the network requires management frame protection, which this supplicant does not do");
    }
    Ok(())
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

    if let Some(tag) = build_line(&firmware) {
        crate::println!("wifi: firmware build: {}", tag);
    }
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
    /// `BRCMF_VIF_STATUS_ASSOC_SUCCESS`: the firmware has associated.
    associated: bool,
    /// The access point associated with, from the SET_SSID event.
    /// `profile->bssid`.
    aa: Option<[u8; 6]>,
    /// The RSN element the last join put in the association request.
    own_rsn: Vec<u8>,
    /// The handshake with the access point, from the association on.
    supplicant: Option<wpa::Supplicant>,
    /// EAPOL frames read from the chip and not yet given to the supplicant.
    eapol: VecDeque<Vec<u8>>,
    /// The last EAPOL frame that arrived before the association event, and
    /// when. `pending_eapol_rx` and `pending_eapol_rx_time`.
    pending_eapol: Option<(Vec<u8>, u64)>,
    /// What to scan for and join while there is no link. `None` with no
    /// configured network, which is scanned for once and never joined.
    reconnect: Option<Reconnect<protocol::Bss>>,
    /// What the machine asked for and the loop has not done yet.
    effects: VecDeque<Effect<protocol::Bss>>,
    /// A scan the machine asked for is running.
    scanning: bool,
    /// Scans finished since bring-up, bring-up's included, for `wifi.hide=`.
    scans: u32,
    /// The access point the join in progress named, from the scan that chose
    /// it. Its RSN element is the one message 3 has to carry.
    target: Option<protocol::Bss>,
    /// When `wifi.drop=` leaves the association, on the microsecond clock.
    drop_at: Option<u64>,
    dropped: bool,
    /// Every BSSID the scan saw carrying the configured network's name, so an
    /// event's address can be checked against them without printing either.
    configured_bssids: Vec<[u8; 6]>,
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
            associated: false,
            aa: None,
            own_rsn: Vec::new(),
            supplicant: None,
            eapol: VecDeque::new(),
            pending_eapol: None,
            reconnect: None,
            effects: VecDeque::new(),
            scanning: false,
            scans: 0,
            target: None,
            drop_at: None,
            dropped: false,
            configured_bssids: Vec::new(),
        }
    }

    /// Whether an event came from one of the configured network's access
    /// points, said without naming the address.
    fn event_source(&self, event: &Event) -> &'static str {
        if event.addr == [0; 6] {
            ""
        } else if self.configured_bssids.contains(&event.addr) {
            ", from a BSSID the scan saw for the configured network"
        } else if event.addr[0] & 0x02 != 0 {
            ", from a locally administered address the scan did not see"
        } else {
            ", from an address the scan did not see for the configured network"
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
                        let ethertype = frame.get(12..14).map(|t| u16::from_be_bytes([t[0], t[1]]));
                        if ethertype == Some(ETHERTYPE_EAPOL) {
                            // The handshake is the loop's to run, after the
                            // events read with this frame, since the
                            // association event can arrive in the same read.
                            if self.eapol.len() < EAPOL_QUEUE_DEPTH {
                                self.eapol.push_back(frame.to_vec());
                            }
                        } else if CARD.link.load(Ordering::Relaxed) {
                            crate::net::receive(frame);
                        } else {
                            note_early_frame(frame);
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

    /// The receive buffer still holds the last frame read, which for a set
    /// request is the firmware's echo of what was sent. Every byte the buffer
    /// has room for is overwritten, not only the ones in use.
    fn forget_last_frame(&mut self) {
        let capacity = self.rx.capacity();
        self.rx.clear();
        self.rx.resize(capacity, 0);
        self.rx.clear();
        if let Some((_, data)) = self.response.as_mut() {
            data.fill(0);
        }
        self.response = None;
    }

    /// A control request whose payload is a secret: its echo is zeroed along
    /// with every copy of the request.
    fn ioctl_secret(&mut self, cmd: u32, payload: &[u8]) -> Result<(), IoctlError> {
        let result = self.ioctl(cmd, true, payload.len(), payload);
        self.forget_last_frame();
        result.map(|mut data| data.fill(0))
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

    /// `set_iovar` for a value that is a secret.
    fn set_iovar_secret(&mut self, name: &str, value: &[u8]) -> Result<(), IoctlError> {
        let mut request = protocol::iovar(name, value);
        let result = self.ioctl_secret(protocol::C_SET_VAR, &request);
        request.fill(0);
        result
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
                None => {
                    // `brcmf_c_process_clm_blob` reads the status only after a
                    // failure. It is read here as well, because the blob and the
                    // firmware come from different releases; zero means the
                    // firmware took the blob. Without its regulatory data the
                    // radio must not join anything, so any other value stops
                    // bring-up.
                    match self.get_iovar_u32("clmload_status") {
                        Ok(0) => crate::println!(
                            "wifi: CLM blob loaded, {} bytes in {} pieces; clmload_status 0",
                            clm.len(),
                            pieces
                        ),
                        Ok(status) => {
                            return Err(format!("the firmware took the CLM blob but reports clmload_status {}", status))
                        }
                        Err(error) => return Err(format!("the CLM blob was sent, but clmload_status: {}", error)),
                    }
                }
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
        // `brcmf_feat_firmware_capabilities`: the features the firmware says
        // it has, space-separated.
        match self.get_iovar("cap", CAPS_LEN) {
            Ok(caps) => crate::println!("wifi: firmware capabilities: {}", first_line(&caps).trim_end()),
            Err(error) => crate::println!("wifi: cap: {}", error),
        }
        if let Err(error) = self.set_iovar_u32("mpc", 1) {
            crate::println!("wifi: mpc: {}", error);
        }
        Ok(())
    }

    /// The country, the events to be told about, and the interface up.
    fn configure(&mut self, country: config::Country) -> Result<(), String> {
        // The country decides which channels and powers the CLM data allows.
        // If the firmware does not take it, nothing further is done.
        self.set_iovar("country", &protocol::country(country.0))
            .map_err(|e| format!("the firmware refused country {}: {}", country, e))?;
        let answer = self.get_iovar("country", 12)?;
        if answer.len() < 12 || answer[8..10] != country.0 {
            return Err(format!("country {} was set, but the firmware does not report it back", country));
        }
        crate::println!(
            "wifi: country set to {}; the firmware reports {}{} revision {}",
            country,
            answer[8] as char,
            answer[9] as char,
            i32::from_le_bytes([answer[4], answer[5], answer[6], answer[7]])
        );

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

        // `brcmf_config_dongle` then sets the interface's type through
        // `brcmf_cfg80211_change_iface`, which for a station is infrastructure
        // mode, 1; 0 would make a join create or join an ad hoc network, where
        // no access point starts a handshake. WHD's `whd_wifi_prepare_join` and
        // cyw43's `Control::join` set it too. What the firmware had before is
        // printed.
        let before = self.ioctl(protocol::C_GET_INFRA, false, 4, &[0; 4]);
        self.set_u32(protocol::C_SET_INFRA, 1)?;
        match before {
            Ok(data) if data.len() >= 4 => crate::println!(
                "wifi: infrastructure mode set; it was {}",
                u32::from_le_bytes([data[0], data[1], data[2], data[3]])
            ),
            Ok(_) => crate::println!("wifi: infrastructure mode set; reading it before gave a short answer"),
            Err(error) => crate::println!("wifi: infrastructure mode set; reading it before failed: {}", error),
        }
        Ok(())
    }

    /// Ask the firmware to scan every channel. The results arrive as events,
    /// which `on_event` collects into `scan` until the one that says the scan
    /// is over sets `scan_done`. Bring-up waits for that; the loop reads it
    /// once a tick. `brcmf_run_escan`.
    fn request_scan(&mut self) -> Result<(), IoctlError> {
        self.scan.clear();
        self.scan_done = None;
        self.set_iovar("escan", &protocol::escan_request(0x1234))
    }

    /// Scan every channel and say what was seen, without naming any network:
    /// how many, on which channels, and whether the configured one was among
    /// them, with the security it advertises.
    fn scan(&mut self, config: Option<&config::Config>) -> Result<(), String> {
        let start = now_us();
        self.request_scan()?;
        let deadline = Deadline::after_ms(reconnect::SCAN_TIMEOUT_MS);
        while self.scan_done.is_none() {
            if deadline.expired() {
                crate::println!("wifi: the scan did not finish within {} ms", reconnect::SCAN_TIMEOUT_MS);
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
            let bssids: Vec<[u8; 6]> =
                self.scan.iter().filter(|bss| config.ssid.matches(bss.ssid())).map(|bss| bss.bssid).collect();
            self.configured_bssids = bssids;
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

    /// Tell the reconnect machine what happened, and keep what it asks for.
    fn feed(&mut self, input: Input<protocol::Bss>) {
        if let Some(machine) = self.reconnect.as_mut() {
            let effects = machine.step(now_ms(), input);
            self.effects.extend(effects);
        }
    }

    /// Do what the machine asked for, in order. Doing it can tell the machine
    /// more, a scan or a join refused, and what that asks for is done in the
    /// same call.
    fn carry_out(&mut self) {
        while let Some(effect) = self.effects.pop_front() {
            match effect {
                Effect::Scan => match self.request_scan() {
                    Ok(()) => self.scanning = true,
                    Err(error) => {
                        self.scanning = false;
                        self.feed(Input::ScanRefused(format!("{}", error)));
                    }
                },
                Effect::Join(bss) => self.join(bss),
                Effect::Disassociate => self.disassociate(REASON_DEAUTH_LEAVING),
                Effect::Log(line) => crate::println!("{}", line),
            }
        }
    }

    /// Whether the attempt in progress is bring-up's, which the log describes
    /// step by step: requests, events, handshake messages. Later attempts are
    /// described by the reconnect machine's lines alone, so that an attempt
    /// every half minute does not fill the log.
    fn detailed(&self) -> bool {
        self.reconnect.as_ref().map_or(true, |machine| machine.first_attempt())
    }

    /// The access point to join from the scan that just finished, or why
    /// there is none. With `wifi.hide=` or `wifi.drop=` on, every scan also
    /// gets a line, so that a log shows the scan each attempt made; without
    /// them, only bring-up's scan has one.
    fn choose(&mut self) -> Result<protocol::Bss, Failure> {
        let result = self.choose_from_scan();
        let hide = HIDE_SCANS.load(Ordering::Relaxed);
        if hide != 0 || DROP_AFTER_S.load(Ordering::Relaxed) != 0 {
            crate::println!(
                "wifi: debug: scan {} finished with {} results: {}{}",
                self.scans,
                self.scan.len(),
                match &result {
                    Ok(bss) => format!("the configured network on channel {}", bss.channel),
                    Err(failure) => format!("{}", failure),
                },
                if self.scans <= hide { ", hidden by wifi.hide" } else { "" }
            );
        }
        result
    }

    /// The strongest access point with the configured name, in the scan that
    /// just finished, among those the supplicant can join.
    fn choose_from_scan(&mut self) -> Result<protocol::Bss, Failure> {
        self.scans += 1;
        let Some(config) = self.config.as_ref() else { return Err(Failure::NotFound) };
        if self.scans <= HIDE_SCANS.load(Ordering::Relaxed) {
            return Err(Failure::NotFound);
        }
        let matching: Vec<&protocol::Bss> = self.scan.iter().filter(|bss| config.ssid.matches(bss.ssid())).collect();
        self.configured_bssids = matching.iter().map(|bss| bss.bssid).collect();
        if let Some(bss) = matching.iter().filter(|bss| usable(bss).is_ok()).max_by_key(|bss| bss.rssi) {
            return Ok((*bss).clone());
        }
        match matching.iter().max_by_key(|bss| bss.rssi) {
            Some(bss) => Err(Failure::Unsuitable(usable(bss).err().unwrap_or("the network cannot be joined"))),
            None => Err(Failure::NotFound),
        }
    }

    /// Join the access point the reconnect machine chose.
    fn join(&mut self, bss: protocol::Bss) {
        let Some(config) = self.config.take() else { return };
        let result = self.join_with(&config, &bss);
        self.config = Some(config);
        match result {
            Ok(()) => self.target = Some(bss),
            Err(why) => {
                self.target = None;
                self.feed(Input::JoinRefused(why));
            }
        }
    }

    /// The security and the join, in the order `brcmf_cfg80211_connect` sets
    /// them up for WPA2-PSK with CCMP when the supplicant is on the host.
    fn join_with(&mut self, config: &config::Config, bss: &protocol::Bss) -> Result<(), String> {
        let own_rsn = wpa::station_rsn_element(bss.wmm);

        // `brcmf_cfg80211_connect` hands the RSN element wpa_supplicant built
        // to the firmware as "wpaie", which puts it in the association request.
        self.set_iovar("wpaie", &own_rsn).map_err(|e| format!("wpaie: {}", e))?;
        // `brcmf_set_wpa_version`, WPA2.
        self.set_iovar_u32("wpa_auth", protocol::WPA2_AUTH_PSK | protocol::WPA2_AUTH_UNSPECIFIED)
            .map_err(|e| format!("wpa_auth: {}", e))?;
        // `brcmf_set_auth_type`, open system.
        self.set_iovar_u32("auth", 0).map_err(|e| format!("auth: {}", e))?;
        // `brcmf_set_wsec_mode`: CCMP for pairwise and group.
        self.set_iovar_u32("wsec", protocol::AES_ENABLED).map_err(|e| format!("wsec: {}", e))?;
        // `brcmf_set_key_mgmt` for AKM 00-0F-AC:2. The firmware lists "mfp"
        // in "cap", so brcmfmac has `BRCMF_FEAT_MFP` and sets "mfp" from the
        // RSN element's capabilities, which have neither bit: `BRCMF_MFP_NONE`.
        // brcmfmac does not look at the answer.
        let _ = self.set_iovar_u32("mfp", 0);
        self.set_iovar_u32("wpa_auth", protocol::WPA2_AUTH_PSK).map_err(|e| format!("wpa_auth: {}", e))?;
        // Without `BRCMF_FEAT_FWSUP` brcmfmac leaves "sup_wpa" alone.

        self.associated = false;
        self.aa = None;
        self.supplicant = None;
        self.eapol.clear();
        self.pending_eapol = None;
        self.own_rsn = own_rsn;
        let mut join = protocol::ext_join_params(config.ssid.len(), bss.bssid, |field| config.ssid.copy_into(field));
        let joined = self.set_iovar_secret("join", &join);
        join.fill(0);
        if let Err(error) = joined {
            if self.detailed() {
                crate::println!("wifi: the join iovar was refused ({}); asking with WLC_SET_SSID", error);
            }
            let mut params = protocol::ssid_le(config.ssid.len(), |field| config.ssid.copy_into(field));
            let set = self.ioctl_secret(protocol::C_SET_SSID, &params);
            params.fill(0);
            set.map_err(|e| format!("WLC_SET_SSID: {}", e))?;
        }
        if self.detailed() {
            crate::println!("wifi: join requested, the station's RSN element {} bytes, WMM {}", self.own_rsn.len(), bss.wmm);
        }
        Ok(())
    }

    /// `brcmf_is_linkup` and `brcmf_is_linkdown` without a firmware
    /// supplicant: a successful SET_SSID is the association, and starts the
    /// handshake; a deauthentication, a disassociation, or a link event
    /// without the link flag ends it. The stack's link comes up in
    /// `handshake`, once the keys are installed.
    fn link_event(&mut self, event: &Event) {
        if event.event_type == protocol::E_SET_SSID {
            if event.status == protocol::E_STATUS_SUCCESS {
                self.associated = true;
                self.aa = Some(event.addr);
                // The machine hears of the association before the supplicant
                // starts, because starting it can run a held frame through the
                // handshake, and a frame refused there ends the attempt.
                self.feed(Input::Associated);
                self.start_supplicant(event.addr);
            } else {
                self.feed(Input::AssociationFailed { status: event.status, reason: event.reason });
            }
            return;
        }
        let down = matches!(event.event_type, protocol::E_DEAUTH | protocol::E_DEAUTH_IND | protocol::E_DISASSOC_IND)
            || (event.event_type == protocol::E_LINK && event.flags & protocol::EVENT_MSG_LINK == 0);
        if down {
            self.lose_association();
            self.feed(Input::LinkLost { cause: protocol::event_name(event.event_type), reason: event.reason });
        }
    }

    /// The supplicant for a new association, with what `wpa_sm_set_assoc_wpa_ie`
    /// and `wpa_sm_set_ap_rsn_ie` give hostap's: this station's element from
    /// the join, and the access point's from the scan.
    fn start_supplicant(&mut self, aa: [u8; 6]) {
        // The held frame's age is taken at the association event, before the
        // PMK is derived, as hostap's is.
        let associated_at = now_us();
        let Some(config) = self.config.as_ref() else {
            crate::println!("wifi: associated, but there is no configuration to derive a key from");
            return;
        };
        let pmk = config.pmk();
        // The access point the join named, as the scan that chose it saw it.
        // The scan is searched only for an association with another, which
        // a join that fell back to WLC_SET_SSID, naming no BSSID, can make.
        let bss = match self.target.as_ref() {
            Some(target) if target.bssid == aa => Some(target),
            _ => self.scan.iter().find(|bss| bss.bssid == aa),
        };
        let association = wpa::Association {
            own: self.mac,
            aa,
            own_rsn: self.own_rsn.clone(),
            ap_rsn: bss.and_then(|bss| bss.rsn_element.clone()),
            ap_rsnx: bss.and_then(|bss| bss.rsnx_element.clone()),
            group_cipher: bss.map(|bss| bss.security.group).unwrap_or(wpa::SUITE_CCMP),
            eapol_version: wpa::EAPOL_VERSION,
        };
        let seen = bss.is_some();
        self.supplicant = Some(wpa::Supplicant::new(association, pmk, crate::rng::fill));
        if self.detailed() || !seen {
            crate::println!(
                "wifi: associated{}; the handshake can start",
                if seen { "" } else { " with a BSS the scan did not see, so message 3 cannot be checked" }
            );
        }

        // `wpa_supplicant_event_assoc`: the frame held from just before the
        // association is handled if it is young enough and came from the
        // access point, and forgotten either way.
        if let Some((frame, at)) = self.pending_eapol.take() {
            let age = associated_at.saturating_sub(at);
            let from_aa = frame.get(6..12) == Some(&aa[..]);
            if age < PENDING_EAPOL_MAX_AGE_US && from_aa {
                if self.detailed() {
                    crate::println!("wifi: handling the EAPOL frame that arrived {} ms before the association", age / 1000);
                }
                self.handshake(&frame);
            } else if self.detailed() {
                crate::println!(
                    "wifi: an EAPOL frame from before the association was forgotten: {} ms old, {}",
                    age / 1000,
                    if from_aa { "from the access point" } else { "from another address" }
                );
            }
        }
    }

    /// Forget the association: the supplicant and its keys, and the stack's
    /// link. Whoever calls this tells the reconnect machine why, unless the
    /// machine asked for it.
    fn lose_association(&mut self) {
        self.associated = false;
        self.aa = None;
        self.supplicant = None;
        self.eapol.clear();
        self.pending_eapol = None;
        CARD.link.store(false, Ordering::Relaxed);
    }

    /// `brcmf_cfg80211_disconnect`, which hostap's `wpa_sm_deauthenticate`
    /// reaches through nl80211: WLC_DISASSOC with the reason and the access
    /// point's address. Before any association the address is the one the
    /// join named, which is how an attempt given up stops the firmware's join.
    fn disassociate(&mut self, reason: u16) {
        let peer = self.aa.or(self.target.as_ref().map(|target| target.bssid));
        if let Some(peer) = peer {
            let request = protocol::scb_val(reason as u32, peer);
            let result = self.ioctl(protocol::C_DISASSOC, true, request.len(), &request);
            if self.detailed() {
                match result {
                    Ok(_) => crate::println!("wifi: disassociated with reason {}", reason),
                    Err(error) => crate::println!("wifi: WLC_DISASSOC with reason {}: {}", reason, error),
                }
            }
        }
        self.target = None;
        self.lose_association();
    }

    /// `wifi.drop=`: once the time has come and the link is up, leave the
    /// association the way the reconnect machine's own disassociation does,
    /// and tell the machine the link was lost, as an access point ending it
    /// would.
    fn drop_if_due(&mut self) {
        let Some(at) = self.drop_at else { return };
        if now_us() < at || !CARD.link.load(Ordering::Relaxed) {
            return;
        }
        self.drop_at = None;
        self.dropped = true;
        crate::println!("wifi: wifi.drop: leaving the association once, as the command line asks");
        self.disassociate(REASON_DEAUTH_LEAVING);
        self.feed(Input::LinkLost { cause: "wifi.drop", reason: REASON_DEAUTH_LEAVING as u32 });
    }

    /// One EAPOL frame from the chip: to the supplicant, its reply to the
    /// access point, then the keys to the chip, in that order.
    fn handshake(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let mut source = [0u8; 6];
        source.copy_from_slice(&frame[6..12]);
        if self.aa.is_none() || self.supplicant.is_none() {
            // `wpa_supplicant_rx_eapol`: the association event and the frame
            // come by different paths, so a frame ahead of the event is kept,
            // the latest one only, until the event arrives.
            if self.detailed() {
                crate::println!("wifi: an EAPOL frame before the association; held until it");
            }
            self.pending_eapol = Some((frame.to_vec(), now_us()));
            return;
        }
        // Bring-up's attempt has every step printed. After that a group key
        // handshake's steps still are, since one comes an hour or a day
        // apart, and an attempt's are left to the reconnect machine's lines.
        let detailed = self.detailed();
        let (Some(aa), Some(supplicant)) = (self.aa, self.supplicant.as_mut()) else {
            return;
        };
        if source != aa {
            if detailed {
                crate::println!("wifi: an EAPOL frame from an address other than the access point's; dropped");
            }
            return;
        }
        let outcome = match supplicant.receive(&frame[14..]) {
            Ok(outcome) => outcome,
            Err(error) => {
                if detailed {
                    crate::println!("wifi: handshake frame refused: {}", error);
                }
                if let Some(reason) = error.deauthenticate() {
                    self.disassociate(reason);
                    self.feed(Input::HandshakeFailed { reason });
                }
                return;
            }
        };
        let say = detailed || outcome.step == wpa::Step::GroupMessage1;
        let (received, sent) = match outcome.step {
            wpa::Step::Message1 => ("message 1 received", "message 2 sent"),
            wpa::Step::Message3 => ("message 3 verified", "message 4 sent"),
            wpa::Step::GroupMessage1 => ("group key message 1 verified", "group key message 2 sent"),
        };
        if say {
            crate::println!("wifi: handshake: {}", received);
        }

        let mut ethernet = Vec::with_capacity(14 + outcome.reply.len());
        ethernet.extend_from_slice(&aa);
        ethernet.extend_from_slice(&self.mac);
        ethernet.extend_from_slice(&ETHERTYPE_EAPOL.to_be_bytes());
        ethernet.extend_from_slice(&outcome.reply);
        if let Err(error) = self.send_data(&ethernet) {
            if say {
                crate::println!("wifi: handshake: the reply was not sent: {}", error);
            }
            return;
        }
        if say {
            crate::println!("wifi: handshake: {}", sent);
        }

        if let Some(pairwise) = &outcome.pairwise {
            // wpa_supplicant's nl80211 driver passes the access point's
            // address and a sequence counter of zero for the PTK.
            match self.install_key(0, pairwise.key(), Some(aa), [0; wpa::RSC_LEN]) {
                Ok(()) => {
                    if say {
                        crate::println!("wifi: handshake: pairwise key installed");
                    }
                }
                Err(error) => {
                    if say {
                        crate::println!("wifi: handshake: installing the pairwise key failed: {}", error);
                    }
                    self.disassociate(wpa::REASON_UNSPECIFIED);
                    self.feed(Input::HandshakeFailed { reason: wpa::REASON_UNSPECIFIED });
                    return;
                }
            }
        }
        if let Some(group) = &outcome.group {
            match self.install_key(group.index() as u32, group.key(), None, group.rsc()) {
                Ok(()) => {
                    if say {
                        crate::println!("wifi: handshake: group key installed, index {}", group.index());
                    }
                }
                Err(error) => {
                    if say {
                        crate::println!("wifi: handshake: installing the group key failed: {}", error);
                    }
                    self.disassociate(wpa::REASON_UNSPECIFIED);
                    self.feed(Input::HandshakeFailed { reason: wpa::REASON_UNSPECIFIED });
                    return;
                }
            }
        }
        let completed = self.supplicant.as_ref().map(|s| s.completed()).unwrap_or(false);
        if completed && !CARD.link.load(Ordering::Relaxed) {
            CARD.link.store(true, Ordering::Relaxed);
            self.feed(Input::LinkUp);
            let drop_after = DROP_AFTER_S.load(Ordering::Relaxed);
            if drop_after != 0 && !self.dropped && self.drop_at.is_none() {
                self.drop_at = Some(now_us() + drop_after * 1_000_000);
            }
        }
    }

    /// `send_key_to_dongle` with the key `brcmf_cfg80211_add_key` builds. For a
    /// key without a peer address it then adds `AES_ENABLED` to "wsec", as
    /// add_key does for a key that is not an "ext_key".
    fn install_key(&mut self, index: u32, key: &[u8], peer: Option<[u8; 6]>, rsc: [u8; wpa::RSC_LEN]) -> Result<(), IoctlError> {
        let mut request = protocol::wsec_key(index, key.len(), peer, rsc, |field| field.copy_from_slice(key));
        let result = self.set_iovar_secret("wsec_key", &request);
        request.fill(0);
        result?;
        if peer.is_none() {
            let wsec = self.get_iovar_u32("wsec")?;
            self.set_iovar_u32("wsec", wsec | protocol::AES_ENABLED)?;
        }
        Ok(())
    }

    /// One Ethernet frame to the firmware, once its window allows.
    /// `brcmf_sdio_txpkt` without glomming.
    fn send_data(&mut self, frame: &[u8]) -> Result<(), IoctlError> {
        let deadline = Deadline::after_ms(DATA_CREDIT_WAIT_MS);
        while !self.has_credit() {
            if deadline.expired() {
                return Err(IoctlError::NoCredit);
            }
            self.poll()?;
            spin_us(500);
        }
        protocol::data_frame(&mut self.frame, self.tx_seq, frame);
        match self.bp.f2_write(&self.frame) {
            Ok(()) => {
                self.tx_seq = self.tx_seq.wrapping_add(1);
                Ok(())
            }
            Err(error) => {
                self.tx_fail();
                Err(IoctlError::Bus(error))
            }
        }
    }
}

/// The build line Broadcom's firmware images carry, such as
/// `43455c0-roml/43455_sdio-pno-...-idsup-idauth Version: 7.45.241 ...`: the
/// chip, the features compiled in, the version and the date. brcmfmac does
/// not read it. It is printed so the log shows which features the loaded
/// build has, the supplicant among them.
fn build_line(image: &[u8]) -> Option<String> {
    const MARK: &[u8] = b"-roml/";
    let at = image.windows(MARK.len()).position(|w| w == MARK)?;
    let start = image[..at].iter().rposition(|&b| b == 0).map(|i| i + 1).unwrap_or(0);
    let end = at + image[at..].iter().position(|&b| b == 0)?;
    if end - start > 512 {
        return None;
    }
    Some(String::from_utf8_lossy(&image[start..end]).into_owned())
}

fn first_line(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn run(mut dongle: Dongle) -> ! {
    // With no configured network, bring-up's scan was all there is to do.
    if dongle.config.is_some() {
        dongle.reconnect = Some(Reconnect::new(now_ms()));
        // Bring-up's scan is the first attempt's.
        let chosen = dongle.choose();
        dongle.feed(Input::Scanned(chosen));
        dongle.carry_out();
    }
    let mut last_tick = crate::trap::ticks();
    let mut errors = 0u32;
    let mut send_errors = 0u32;
    loop {
        WAKE.wait_until_or_at(last_tick + 1, || !TX.lock().is_empty());
        if let Err(error) = dongle.poll() {
            errors += 1;
            if errors <= 5 {
                crate::println!("wifi: polling the chip failed: {}", error);
            }
        }
        while let Some(event) = dongle.events.pop_front() {
            if dongle.detailed() {
                crate::println!(
                    "wifi: event {} {} status {} reason {} flags {:#x} auth type {}{}",
                    protocol::event_name(event.event_type),
                    event.event_type,
                    event.status,
                    event.reason,
                    event.flags,
                    event.auth_type,
                    dongle.event_source(&event)
                );
            }
            dongle.link_event(&event);
        }
        while let Some(frame) = dongle.eapol.pop_front() {
            dongle.handshake(&frame);
        }
        loop {
            let next = TX.lock().pop_front();
            let Some(frame) = next else { break };
            match dongle.send_data(&frame) {
                Ok(()) => {
                    SENT.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    TX_DROPPED.fetch_add(1, Ordering::Relaxed);
                    send_errors += 1;
                    if send_errors <= 5 {
                        crate::println!("wifi: a frame of {} bytes was not sent: {}", frame.len(), error);
                    }
                }
            }
        }
        if dongle.scanning && dongle.scan_done.is_some() {
            dongle.scanning = false;
            let chosen = dongle.choose();
            dongle.feed(Input::Scanned(chosen));
        }
        dongle.drop_if_due();
        dongle.feed(Input::Tick);
        dongle.carry_out();
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
