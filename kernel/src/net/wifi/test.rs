//! What can be checked about the WiFi driver with no WiFi chip present.
//!
//! QEMU's Pi 4 has no CYW43455, so everything that is arithmetic on bytes is
//! checked here against values worked out by hand from the reference driver's
//! layouts: how the NVRAM text becomes the blob the firmware reads, how SDPCM
//! frames and BCDC messages are framed and padded, how events and scan results
//! are taken apart, the SDIO command arguments, the clock divisor, the voltage
//! choice, the credentials file, and the two device tree lookups the driver
//! added.
//!
//! Run with `net=test` on the kernel command line, alongside the protocol
//! checks.

use super::config;
use super::nvram;
use super::protocol::{self, HeaderError};
use super::reconnect::{self, Effect, Failure, Input, Reconnect, Target};
use super::sdhci;
use super::sdio;
use super::wpa;
use crate::arch;
use crate::net::genettest::{place, Builder};
use alloc::string::String;
use alloc::vec::Vec;

pub struct Report {
    pub passed: usize,
    pub failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, holds: bool) {
        if holds {
            self.passed += 1;
            crate::println!("  ok    {}", name);
        } else {
            self.failed += 1;
            crate::println!("  FAIL  {}", name);
        }
    }

    fn value(&mut self, name: &str, actual: u64, expected: u64) {
        if actual == expected {
            self.passed += 1;
            crate::println!("  ok    {} = {:#x}", name, actual);
        } else {
            self.failed += 1;
            crate::println!("  FAIL  {}: got {:#x}, wanted {:#x}", name, actual, expected);
        }
    }
}

pub fn run(report: &mut Report) {
    nvram_text(report);
    control_frames(report);
    headers(report);
    padding(report);
    control_responses(report);
    events(report);
    scan_results(report);
    requests(report);
    sdio_arguments(report);
    clock(report);
    voltage(report);
    credentials(report);
    device_tree(report);
    supplicant_vectors(report);
    induction_handshake(report);
    group_rekey(report);
    key_layout(report);
    reconnect_absent_at_boot(report);
    reconnect_other_channel(report);
    reconnect_join_refused(report);
    reconnect_handshake_failure(report);
    reconnect_flapping(report);
    reconnect_a_day_away(report);
}

// ---------------------------------------------------------------------------
// Getting the link back
// ---------------------------------------------------------------------------

/// An access point as the reconnect machine's checks see one: its channel.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Ap(u8);

impl Target for Ap {
    fn channel(&self) -> u8 {
        self.0
    }
}

/// How long a scan takes in these scripts, which is what one took on the
/// board.
const SCAN_MS: u64 = 2_400;

/// What one step of the machine asked for.
#[derive(Default)]
struct Asked {
    scan: bool,
    join: Option<Ap>,
    disassociate: bool,
}

/// The machine, a clock moved by hand, and every line the machine printed.
struct Script {
    machine: Reconnect<Ap>,
    now: u64,
    lines: Vec<String>,
    scans: u64,
    joins: u64,
    /// Whether lines are printed as they come as well as kept. A script that
    /// runs for a simulated day keeps them to itself.
    echo: bool,
}

impl Script {
    /// A machine made at time zero, whose first scan is bring-up's.
    fn new(echo: bool) -> Script {
        Script { machine: Reconnect::new(0), now: 0, lines: Vec::new(), scans: 0, joins: 0, echo }
    }

    /// A machine whose first attempt joined `ap`: associated 2 s after the
    /// request and the link up 0.6 s after that.
    fn up_on(ap: Ap) -> Script {
        let mut script = Script::new(true);
        script.feed(Input::Scanned(Ok(ap)));
        script.now += 2_000;
        script.feed(Input::Associated);
        script.now += 600;
        script.feed(Input::LinkUp);
        script
    }

    fn feed(&mut self, input: Input<Ap>) -> Asked {
        let mut asked = Asked::default();
        for effect in self.machine.step(self.now, input) {
            match effect {
                Effect::Scan => {
                    asked.scan = true;
                    self.scans += 1;
                }
                Effect::Join(ap) => {
                    asked.join = Some(ap);
                    self.joins += 1;
                }
                Effect::Disassociate => asked.disassociate = true,
                Effect::Log(line) => {
                    if self.echo {
                        crate::println!("        {}", line);
                    }
                    self.lines.push(line);
                }
            }
        }
        asked
    }

    /// Move the clock to the machine's next deadline, and tick there.
    fn wait(&mut self) -> Asked {
        if let Some(at) = self.machine.deadline() {
            self.now = self.now.max(at);
        }
        self.feed(Input::Tick)
    }

    /// Wait for the next attempt, which has to begin with a scan, and finish
    /// that scan with `result` 2.4 s later. `None` if the attempt did not
    /// begin with a scan.
    fn attempt(&mut self, result: Result<Ap, Failure>) -> Option<Asked> {
        if !self.wait().scan {
            return None;
        }
        self.now += SCAN_MS;
        Some(self.feed(Input::Scanned(result)))
    }

    /// Join `ap` after a scan found it: associated 2 s after the request, and
    /// the link up 0.6 s after that. Whether the join was asked for.
    fn rejoin(&mut self, ap: Ap) -> bool {
        let joined = self.attempt(Ok(ap)).and_then(|asked| asked.join) == Some(ap);
        self.now += 2_000;
        self.feed(Input::Associated);
        self.now += 600;
        self.feed(Input::LinkUp);
        joined && self.machine.link_up()
    }

    /// How far off the machine's next deadline is.
    fn scheduled(&self) -> Option<u64> {
        self.machine.deadline().map(|at| at - self.now)
    }
}

/// The network missing from bring-up's scan and from the next six, then there.
fn reconnect_absent_at_boot(report: &mut Report) {
    let mut s = Script::new(true);
    let boot = s.feed(Input::Scanned(Err(Failure::NotFound)));
    report.check(
        "reconnect, absent at boot: no join, and the next attempt 4 s on",
        boot.join.is_none() && s.scheduled() == Some(4_000),
    );
    let mut waits = Vec::new();
    let mut every_one_scanned = true;
    for _ in 0..6 {
        waits.push(s.scheduled().unwrap_or(0));
        match s.attempt(Err(Failure::NotFound)) {
            Some(asked) => every_one_scanned &= asked.join.is_none(),
            None => every_one_scanned = false,
        }
    }
    report.check(
        "reconnect, absent at boot: attempts 4, 8, 16, 30, 30 and 30 s after each failure",
        waits == [4_000, 8_000, 16_000, 30_000, 30_000, 30_000],
    );
    report.check("reconnect, absent at boot: each attempt a scan of its own, and no join", every_one_scanned && s.scans == 6);
    report.check("reconnect, absent at boot: the network in the next scan is joined", s.rejoin(Ap(6)));
    report.check(
        "reconnect, absent at boot: two lines, the first failure and the join on channel 6",
        s.lines.len() == 2 && s.lines[0].contains("not in the scan") && s.lines[1].contains("joined on channel 6"),
    );
    s.now += reconnect::LINK_STABLE_MS;
    s.feed(Input::Tick);
    report.check(
        "reconnect, absent at boot: a minute up ends the run, in a line counting the 7 failed attempts",
        s.lines.len() == 3 && s.lines[2].contains("7 failed attempts") && s.machine.deadline().is_none(),
    );
}

/// A link up for an hour, lost, and the network found again on another
/// channel.
fn reconnect_other_channel(report: &mut Report) {
    let mut s = Script::up_on(Ap(6));
    s.now += 3_600_000;
    s.feed(Input::Tick);
    let before = s.lines.len();
    let lost = s.feed(Input::LinkLost { cause: "DEAUTH_IND", reason: 7 });
    report.check(
        "reconnect, lost: nothing asked at once, and the first attempt 2 s after the loss",
        !lost.scan && lost.join.is_none() && s.scheduled() == Some(reconnect::FIRST_RETRY_MS),
    );
    report.check(
        "reconnect, lost: one line, naming the event and the reason",
        s.lines.len() == before + 1 && s.lines[before].contains("link lost: DEAUTH_IND reason 7"),
    );
    let late = s.feed(Input::LinkLost { cause: "LINK", reason: 0 });
    report.check(
        "reconnect, lost: the end of the association arriving again changes nothing",
        !late.scan && s.scheduled() == Some(reconnect::FIRST_RETRY_MS) && s.lines.len() == before + 1,
    );
    let asked = s.attempt(Ok(Ap(11)));
    report.check(
        "reconnect, lost: a new scan, and the join goes to what it found, on channel 11",
        s.scans == 1 && asked.and_then(|asked| asked.join) == Some(Ap(11)),
    );
    s.now += 2_000;
    s.feed(Input::Associated);
    s.now += 600;
    s.feed(Input::LinkUp);
    report.check(
        "reconnect, lost: the link up on channel 11, in one more line",
        s.machine.link_up() && s.lines.len() == before + 2 && s.lines[before + 1].contains("joined on channel 11"),
    );
}

/// The firmware refusing every join for forty attempts.
fn reconnect_join_refused(report: &mut Report) {
    let refusal = || String::from("wpaie: the firmware answered with error -23");
    let mut s = Script::new(false);
    s.feed(Input::Scanned(Ok(Ap(1))));
    s.feed(Input::JoinRefused(refusal()));
    let mut waits = Vec::new();
    let mut joined_each_time = true;
    for _ in 0..40 {
        waits.push(s.scheduled().unwrap_or(0));
        match s.attempt(Ok(Ap(1))) {
            Some(asked) if asked.join == Some(Ap(1)) => {
                s.feed(Input::JoinRefused(refusal()));
            }
            _ => joined_each_time = false,
        }
    }
    report.check(
        "reconnect, join refused: forty attempts after the first, each one a scan and a join",
        joined_each_time && s.scans == 40 && s.joins == 41,
    );
    report.check(
        "reconnect, join refused: waits of 4, 8 and 16 s, then 30 s every time",
        waits[..3] == [4_000, 8_000, 16_000] && waits[3..].iter().all(|&wait| wait == reconnect::MAX_RETRY_MS),
    );
    let most = 1 + s.now / reconnect::REPORT_INTERVAL_MS;
    report.check(
        "reconnect, join refused: the refusal once, with what the firmware said, then a count every 5 minutes",
        s.lines.len() >= 2
            && s.lines.len() as u64 <= most
            && s.lines[0].contains("error -23")
            && s.lines[1..].iter().all(|line| line.contains("more times since the last report")),
    );
}

/// Attempts that end after the join request: the handshake refused, no
/// handshake, no association, and the access point ending one, then a join.
fn reconnect_handshake_failure(report: &mut Report) {
    let mut s = Script::new(true);
    s.feed(Input::Scanned(Ok(Ap(6))));
    s.now += 2_000;
    s.feed(Input::Associated);
    s.now += 300;
    let refused = s.feed(Input::HandshakeFailed { reason: 17 });
    report.check(
        "reconnect, handshake: refused with reason 17, the next attempt 4 s on, and nothing more asked",
        !refused.disassociate
            && refused.join.is_none()
            && s.scheduled() == Some(4_000)
            && s.lines.last().is_some_and(|line| line.contains("reason 17")),
    );

    let asked = s.attempt(Ok(Ap(6)));
    s.now += 2_000;
    s.feed(Input::Associated);
    s.now += reconnect::HANDSHAKE_TIMEOUT_MS - 1;
    let early = s.feed(Input::Tick);
    s.now += 1;
    let late = s.feed(Input::Tick);
    report.check(
        "reconnect, handshake: an association is left for 10 s, then left for good and retried 8 s on",
        asked.is_some() && !early.disassociate && late.disassociate && s.scheduled() == Some(8_000),
    );

    s.attempt(Ok(Ap(6)));
    s.now += reconnect::ASSOCIATE_TIMEOUT_MS - 1;
    let early = s.feed(Input::Tick);
    s.now += 1;
    let late = s.feed(Input::Tick);
    report.check(
        "reconnect, handshake: a join with no association is left for 10 s, then abandoned and retried 16 s on",
        !early.disassociate && late.disassociate && s.scheduled() == Some(16_000),
    );

    s.attempt(Ok(Ap(6)));
    s.now += 2_000;
    s.feed(Input::Associated);
    s.now += 4_000;
    s.feed(Input::LinkLost { cause: "DEAUTH_IND", reason: 15 });
    report.check(
        "reconnect, handshake: an attempt the access point ends is retried at the cap",
        s.scheduled() == Some(reconnect::MAX_RETRY_MS) && s.lines.last().is_some_and(|line| line.contains("DEAUTH_IND reason 15")),
    );
    let stray = s.feed(Input::Associated);
    report.check("reconnect, handshake: an association no attempt is waiting for is left", stray.disassociate);

    report.check("reconnect, handshake: then joined", s.rejoin(Ap(6)));
    report.check("reconnect, handshake: five lines, one per kind of failure and the join", s.lines.len() == 5);
}

/// A link that comes up and is lost 5 s later, thirty times.
fn reconnect_flapping(report: &mut Report) {
    let mut s = Script::up_on(Ap(6));
    let began = s.now;
    let mut waits = Vec::new();
    let mut rejoined = true;
    for _ in 0..30 {
        s.now += 5_000;
        s.feed(Input::LinkLost { cause: "DEAUTH_IND", reason: 2 });
        waits.push(s.scheduled().unwrap_or(0));
        rejoined &= s.rejoin(Ap(6));
    }
    report.check("reconnect, flapping: thirty losses, each joined again", rejoined);
    report.check(
        "reconnect, flapping: the waits 4, 8 and 16 s, then 30 s, since no link stayed up a minute",
        waits[..3] == [4_000, 8_000, 16_000] && waits[3..].iter().all(|&wait| wait == reconnect::MAX_RETRY_MS),
    );
    let most = 3 + 2 * (s.now - began) / reconnect::REPORT_INTERVAL_MS;
    report.check(
        "reconnect, flapping: the first loss and its join in a line each, then two lines every 5 minutes at most",
        s.lines.len() >= 3 && s.lines.len() as u64 <= most && s.lines[1].contains("link lost") && s.lines[2].contains("joined"),
    );
    let before = s.lines.len();
    s.now += reconnect::LINK_STABLE_MS;
    s.feed(Input::Tick);
    report.check(
        "reconnect, flapping: a minute up ends the run, in a line counting the 30 losses",
        s.lines.len() == before + 1 && s.lines[before].contains("30 losses"),
    );
    s.now += 1_000;
    s.feed(Input::LinkLost { cause: "DEAUTH_IND", reason: 2 });
    report.check(
        "reconnect, flapping: a loss after that waits 2 s again, and has its line",
        s.scheduled() == Some(reconnect::FIRST_RETRY_MS) && s.lines.len() == before + 2,
    );
}

/// The network gone for a day from bring-up on, and a minute of ticks with a
/// scan that never finishes.
fn reconnect_a_day_away(report: &mut Report) {
    const DAY_MS: u64 = 24 * 3600 * 1000;
    let mut s = Script::new(false);
    s.feed(Input::Scanned(Err(Failure::NotFound)));
    let mut attempts = 0u64;
    let mut capped = true;
    while s.now < DAY_MS {
        let wait = s.scheduled().unwrap_or(0);
        if attempts >= 3 {
            capped &= wait == reconnect::MAX_RETRY_MS;
        }
        if s.attempt(Err(Failure::NotFound)).is_none() {
            break;
        }
        attempts += 1;
    }
    report.check(
        "reconnect, a day away: a scan every 32.4 s, all day",
        s.scans == attempts && attempts >= DAY_MS / (reconnect::MAX_RETRY_MS + SCAN_MS),
    );
    report.check("reconnect, a day away: the wait at the cap from the fourth attempt on", capped);
    let most = 1 + s.now / reconnect::REPORT_INTERVAL_MS;
    report.check(
        "reconnect, a day away: no more than one line per 5 minutes",
        s.lines.len() >= 2 && s.lines.len() as u64 <= most,
    );
    crate::println!("        {} attempts and {} lines in the day; the last: {}", attempts, s.lines.len(), s.lines.last().map_or("", |line| line.as_str()));

    let (lines, scans) = (s.lines.len(), s.scans);
    for _ in 0..6_000 {
        s.now += 10;
        s.feed(Input::Tick);
    }
    report.check(
        "reconnect, a day away: a minute of ticks 10 ms apart with no scan finishing: at most two scans and one line",
        s.scans > scans && s.scans - scans <= 2 && s.lines.len() - lines <= 1,
    );
}

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    digits
        .chunks(2)
        .map(|pair| {
            let value = |d: u8| if d.is_ascii_digit() { d - b'0' } else { (d | 0x20) - b'a' + 10 };
            (value(pair[0]) << 4) | value(pair[1])
        })
        .collect()
}

/// Published vectors for the primitives: the SHA-1 PRF and the passphrase to
/// PSK mapping as hostap's `src/crypto/crypto_module_tests.c` checks them
/// (IEEE 802.11 Annex J), and RFC 3394 test vector 4.1 for key wrap.
fn supplicant_vectors(report: &mut Report) {
    let mut out = [0u8; 64];
    wpa::prf_sha1(&[0x0b; 20], b"prefix", b"Hi There", &mut out);
    report.check(
        "prf-sha1: key 0x0b x 20, \"Hi There\"",
        out[..] == hex("bcd4c650b30b96849518 29e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")[..],
    );
    wpa::prf_sha1(b"Jefe", b"prefix", b"what do ya want for nothing?", &mut out);
    report.check(
        "prf-sha1: key \"Jefe\"",
        out[..] == hex("51f4de5b33f249adf81aeb713a3c20f4fe631446fabdfa58244759ae58ef9009a99abf4eac2ca5fa87e692c440eb40023e7babb206d61de7b92f41529092b8fc")[..],
    );
    wpa::prf_sha1(&[0xaa; 20], b"prefix", &[0xdd; 50], &mut out);
    report.check(
        "prf-sha1: key 0xaa x 20, 0xdd x 50",
        out[..] == hex("e1ac546ec4cb636f9976487be5c86be17a0252ca5d8d8df12cfb0473525249ce9dd8d177ead710bc9b590547239107aef7b4abd43d87f0a68f1cbd9e2b6f7607")[..],
    );

    let psk = [
        ("password", "IEEE", "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"),
        ("ThisIsAPassword", "ThisIsASSID", "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af"),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ",
            "becb93866bb8c3832cb777c2f559807c8c59afcb6eae734885001300a981cc62",
        ),
    ];
    for (passphrase, ssid, expected) in psk {
        let pmk = wpa::Pmk::from_passphrase(passphrase.as_bytes(), ssid.as_bytes());
        report.check("pbkdf2: an 802.11 Annex J passphrase", pmk.equals(&hex(expected)));
    }

    let kek: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let plain = hex("00112233445566778899AABBCCDDEEFF");
    let wrapped = hex("1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5");
    let mut out = [0u8; 24];
    report.check("aes key wrap: RFC 3394 4.1", wpa::aes_wrap(&kek, &plain, &mut out) && out[..] == wrapped[..]);
    let mut back = [0u8; 16];
    report.check("aes key unwrap: RFC 3394 4.1", wpa::aes_unwrap(&kek, &wrapped, &mut back) && back[..] == plain[..]);
    let mut bad = wrapped.clone();
    bad[23] ^= 1;
    report.check("aes key unwrap: a changed byte fails the integrity check", !wpa::aes_unwrap(&kek, &bad, &mut back));
}

// Wireshark's published wpa-Induction.pcap: SSID "Coherer", passphrase
// "Induction", access point 00:0c:41:82:b2:55, station 00:0d:93:82:36:3a.
// The four EAPOL frames, frames 87, 89, 92 and 94, from the 802.1X header on.
const INDUCTION_M1: &str = "0203007502008a001000000000000000003e8e967dacd960324cac5b6aa721235bf57b949771c867989f49d04ed47c69330000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000016dd14000fac04592da88096c461da246c69001e877f3d";
const INDUCTION_M2: &str = "0203007502010a00100000000000000000cdf405ceb9d889ef3dec42609828fae546b7add7baecbb1a394eac5214b1d3860000000000000000000000000000000000000000000000000000000000000000a462a7029ad5ba30b6af0df391988e45001630140100000fac020100000fac040100000fac020000";
const INDUCTION_M3: &str = "020300af0213ca001000000000000000013e8e967dacd960324cac5b6aa721235bf57b949771c867989f49d04ed47c6933f57b949771c867989f49d04ed47c6934cf0200000000000000000000000000007d0af6df51e99cde7a187453f0f935370050cfa72cde35b2c1e2319255806ab364179fd9673041b9a5939fa1a2010d2ac794e25168055f794ddc1fdfae3521f4446bfd11da98345f543df6ce199df8fe48f8cdd17adca87bf45711183c496d41aa0c";
const INDUCTION_M4: &str = "0203005f02030a001000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010bba3bdfbcfde2bc537509d71f2ecd10000";
/// The beacon's RSN element (frame 1) and the station's, from its association
/// request (frame 82).
const INDUCTION_AP_RSN: &str = "30180100000fac020200000fac04000fac020100000fac020000";
const INDUCTION_STA_RSN: &str = "30140100000fac020100000fac040100000fac020000";
/// The PMK for "Induction" and "Coherer", worked out separately with Python's
/// `hashlib.pbkdf2_hmac`; with it the capture's own MICs on messages 2, 3 and
/// 4 check, which is what shows it is right.
const INDUCTION_PMK: &str = "a288fcf0caaacda9a9f58633ff35e8992a01d9c10ba5e02efdf8cb5d730ce7bc";

/// The station's SNonce from message 2, in place of the generator.
fn induction_snonce(buf: &mut [u8]) {
    buf.copy_from_slice(&hex(INDUCTION_M2)[17..49]);
}

fn induction_supplicant(passphrase: &str, ap_rsn: &str) -> wpa::Supplicant {
    let association = wpa::Association {
        own: [0x00, 0x0d, 0x93, 0x82, 0x36, 0x3a],
        aa: [0x00, 0x0c, 0x41, 0x82, 0xb2, 0x55],
        own_rsn: hex(INDUCTION_STA_RSN),
        ap_rsn: Some(hex(ap_rsn)),
        ap_rsnx: None,
        // The beacon's group suite, 00-0F-AC:2: this access point sends group
        // traffic with TKIP, so its GTK is 32 bytes.
        group_cipher: wpa::SUITE_TKIP,
        // The captured station sent version 2; the driver sends hostap's 1.
        eapol_version: 2,
    };
    wpa::Supplicant::new(association, wpa::Pmk::from_passphrase(passphrase.as_bytes(), b"Coherer"), induction_snonce)
}

/// Our frame with the Key Length field the captured station wrote, signed
/// again, so it can be compared with the capture whole. The station in the
/// capture (2006) wrote 16 in messages 2 and 4; hostap and IEEE 802.11-2020
/// write 0, which is what this supplicant sends.
fn with_key_length_16(s: &wpa::Supplicant, frame: &[u8]) -> Option<Vec<u8>> {
    let mut copy = frame.to_vec();
    copy[7..9].copy_from_slice(&16u16.to_be_bytes());
    let mic = s.mic_under_current_key(&copy)?;
    copy[81..97].copy_from_slice(&mic);
    Some(copy)
}

fn induction_handshake(report: &mut Report) {
    report.check("induction: the PMK", wpa::Pmk::from_passphrase(b"Induction", b"Coherer").equals(&hex(INDUCTION_PMK)));
    let (m1, m2, m3, m4) = (hex(INDUCTION_M1), hex(INDUCTION_M2), hex(INDUCTION_M3), hex(INDUCTION_M4));

    let mut s = induction_supplicant("Induction", INDUCTION_AP_RSN);
    match s.receive(&m1) {
        Ok(out) => {
            report.check("induction: message 1 gives message 2", out.step == wpa::Step::Message1 && out.pairwise.is_none() && out.group.is_none());
            report.check("induction: message 2 has key length 0", out.reply.len() == m2.len() && out.reply[7..9] == [0, 0]);
            report.check(
                "induction: the MIC of the captured message 2 under our PTK is the captured MIC",
                s.mic_under_current_key(&m2).map(|m| m[..] == m2[81..97]).unwrap_or(false),
            );
            report.check(
                "induction: message 2 is the captured one, byte for byte, given the capture's key length",
                with_key_length_16(&s, &out.reply).map(|f| f == m2).unwrap_or(false),
            );
        }
        Err(e) => report.check(&alloc::format!("induction: message 1 refused: {}", e), false),
    }
    match s.receive(&m3) {
        Ok(out) => {
            report.check("induction: message 3 verified, gives message 4", out.step == wpa::Step::Message3 && s.completed());
            report.check("induction: message 4 has key length 0", out.reply.len() == m4.len() && out.reply[7..9] == [0, 0]);
            report.check(
                "induction: message 4 is the captured one, byte for byte, given the capture's key length",
                with_key_length_16(&s, &out.reply).map(|f| f == m4).unwrap_or(false),
            );
            report.check("induction: a pairwise key of 16 bytes", out.pairwise.as_ref().map(|k| k.key().len()) == Some(16));
            report.check(
                "induction: a TKIP group key of 32 bytes, index 1 to 3, RSC cf 02",
                out.group
                    .as_ref()
                    .map(|g| g.key().len() == 32 && (1..=3).contains(&g.index()) && g.rsc() == [0xcf, 0x02, 0, 0, 0, 0])
                    .unwrap_or(false),
            );
        }
        Err(e) => report.check(&alloc::format!("induction: message 3 refused: {}", e), false),
    }
    report.check("induction: message 3 again is a replay", matches!(s.receive(&m3), Err(wpa::Error::ReplayCounterNotIncreased)));
    report.check("induction: message 1 again is a replay", matches!(s.receive(&m1), Err(wpa::Error::ReplayCounterNotIncreased)));

    let mut s = induction_supplicant("Induction", INDUCTION_AP_RSN);
    let _ = s.receive(&m1);
    let mut changed = m3.clone();
    changed[120] ^= 0x01;
    report.check("induction: a changed byte in message 3 fails the MIC", matches!(s.receive(&changed), Err(wpa::Error::MicMismatch) | Err(wpa::Error::NoKeyForMic)));
    report.check("induction: the unchanged message 3 is still taken after that", s.receive(&m3).is_ok());

    let mut s = induction_supplicant("InductioN", INDUCTION_AP_RSN);
    let _ = s.receive(&m1);
    report.check("induction: the wrong passphrase fails message 3's MIC", matches!(s.receive(&m3), Err(wpa::Error::NoKeyForMic) | Err(wpa::Error::MicMismatch)));

    let mut s = induction_supplicant("Induction", INDUCTION_STA_RSN);
    let _ = s.receive(&m1);
    let result = s.receive(&m3);
    report.check(
        "induction: an RSN element other than the beacon's fails, with reason 17",
        matches!(result, Err(wpa::Error::RsnElementDiffers)) && result.err().and_then(|e| e.deauthenticate()) == Some(17),
    );

    let mut s = induction_supplicant("Induction", INDUCTION_AP_RSN);
    report.check("induction: message 3 before message 1 has no key to check", matches!(s.receive(&m3), Err(wpa::Error::NoKeyForMic)));

    // The same capture with the group cipher taken as CCMP: the 32-byte GTK
    // does not fit, and hostap deauthenticates.
    let association = wpa::Association {
        own: [0x00, 0x0d, 0x93, 0x82, 0x36, 0x3a],
        aa: [0x00, 0x0c, 0x41, 0x82, 0xb2, 0x55],
        own_rsn: hex(INDUCTION_STA_RSN),
        ap_rsn: Some(hex(INDUCTION_AP_RSN)),
        ap_rsnx: None,
        group_cipher: wpa::SUITE_CCMP,
        eapol_version: 2,
    };
    let mut s = wpa::Supplicant::new(association, wpa::Pmk::from_passphrase(b"Induction", b"Coherer"), induction_snonce);
    let _ = s.receive(&m1);
    let result = s.receive(&m3);
    report.check(
        "induction: a group key of the wrong length for the cipher fails, with reason 1",
        matches!(result, Err(wpa::Error::GtkLength)) && result.err().and_then(|e| e.deauthenticate()) == Some(1) && !s.completed(),
    );
    report.check("induction: a frame too short is refused", matches!(s.receive(&m1[..98]), Err(wpa::Error::TooShort)));
}

/// A rekey after the captured handshake. No published capture has one, so
/// the group message is built with the supplicant's own PTK; this checks the
/// parsing, the reply's layout and the replay and reinstall rules.
fn group_rekey(report: &mut Report) {
    let mut s = induction_supplicant("Induction", INDUCTION_AP_RSN);
    let _ = s.receive(&hex(INDUCTION_M1));
    let _ = s.receive(&hex(INDUCTION_M3));
    let gtk = [0x11u8; 32];
    let rsc = [5, 0, 0, 0, 0, 0, 0, 0];
    let Some(g1) = s.group_message_for_test([0, 0, 0, 0, 0, 0, 0, 2], 2, &gtk, rsc) else {
        report.check("group: message 1 built", false);
        return;
    };
    match s.receive(&g1) {
        Ok(out) => {
            report.check(
                "group: message 1 gives message 2 and key index 2 with RSC 5",
                out.step == wpa::Step::GroupMessage1
                    && out.group.as_ref().map(|g| g.index() == 2 && g.key() == &gtk[..] && g.rsc() == [5, 0, 0, 0, 0, 0]).unwrap_or(false),
            );
            report.check(
                "group: message 2 is key information 0x0322, replay 2, no key data",
                out.reply.len() == 99 && out.reply[5..7] == [0x03, 0x22] && out.reply[16] == 2 && out.reply[97..99] == [0, 0],
            );
            report.check(
                "group: message 2's MIC is under the PTK",
                s.mic_under_current_key(&out.reply).map(|m| m[..] == out.reply[81..97]).unwrap_or(false),
            );
        }
        Err(e) => report.check(&alloc::format!("group: message 1 refused: {}", e), false),
    }
    report.check("group: the same message again is a replay", matches!(s.receive(&g1), Err(wpa::Error::ReplayCounterNotIncreased)));
    if let Some(again) = s.group_message_for_test([0, 0, 0, 0, 0, 0, 0, 3], 2, &gtk, rsc) {
        report.check("group: the same key under a new counter is not installed twice", s.receive(&again).map(|o| o.group.is_none()).unwrap_or(false));
    }
    let fresh = induction_supplicant("Induction", INDUCTION_AP_RSN);
    report.check("group: no group message is built before a PTK", fresh.group_message_for_test([0; 8], 1, &gtk, rsc).is_none());
}

/// `struct brcmf_wsec_key_le` for the two keys the supplicant installs, and
/// the station's RSN element as `wpa_gen_wpa_ie_rsn` writes it.
fn key_layout(report: &mut Report) {
    report.check(
        "station rsn: CCMP group, one CCMP pairwise, one PSK AKM, 16 replay counters with WMM",
        wpa::station_rsn_element(true) == hex("30140100000fac040100000fac040100000fac020c00"),
    );
    report.check("station rsn: no capabilities without WMM", wpa::station_rsn_element(false) == hex("30140100000fac040100000fac040100000fac020000"));
    report.check("disassoc: the reason and the address in 12 bytes", protocol::scb_val(1, [1, 2, 3, 4, 5, 6]) == [1, 0, 0, 0, 1, 2, 3, 4, 5, 6, 0, 0]);
    let peer = [0x00, 0x0c, 0x41, 0x82, 0xb2, 0x55];
    let pairwise = protocol::wsec_key(0, 16, Some(peer), [0; 6], |field| field.copy_from_slice(&[0xAB; 16]));
    report.check(
        "wsec_key pairwise: index 0, length 16, the key, CCMP, no flags, IV initialized, the peer at 156",
        pairwise.len() == 164
            && pairwise[0..8] == [0, 0, 0, 0, 16, 0, 0, 0]
            && pairwise[8..24] == [0xAB; 16]
            && pairwise[24..112].iter().all(|&b| b == 0)
            && pairwise[112..120] == [4, 0, 0, 0, 0, 0, 0, 0]
            && pairwise[132..136] == [1, 0, 0, 0]
            && pairwise[156..162] == peer,
    );
    let group = protocol::wsec_key(2, 16, None, [0xcf, 0x02, 0x03, 0x04, 0x05, 0x06], |field| field.copy_from_slice(&[0xCD; 16]));
    report.check(
        "wsec_key group: index 2, the primary flag, the RSC as IV high 0x06050403 and low 0x02cf, no address",
        group[0..4] == [2, 0, 0, 0]
            && group[116..120] == [2, 0, 0, 0]
            && group[140..144] == [0x03, 0x04, 0x05, 0x06]
            && group[144..146] == [0xcf, 0x02]
            && group[156..162] == [0; 6],
    );
}

fn nvram_text(report: &mut Report) {
    // A comment, a CRLF line, a blank line, leading spaces, a key with a
    // space in it (skipped), a RAW1 line (skipped), and a value containing a
    // space (kept).
    let text = b"# comment\nkey1=value1\r\n\n  key2=value 2\nbad key=x\nRAW1=zzz\nboardrev=0x1304\nlast=1\n";
    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"key1=value1\0key2=value 2\0boardrev=0x1304\0last=1\0");
    // 48 bytes, rounded up past at least one NUL to 52: thirteen words.
    expected.extend_from_slice(&[0, 0, 0, 0]);
    expected.extend_from_slice(&0xFFF2_000Du32.to_le_bytes());
    match nvram::strip(text) {
        Ok(blob) => report.check("nvram: lines kept, comments and bad keys dropped, length word", blob == expected),
        Err(_) => report.check("nvram: lines kept, comments and bad keys dropped, length word", false),
    }

    // No boardrev: the default is added. A last line with no newline is not
    // taken, which is what the reference's loop bound does.
    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(b"a=1\0boardrev=0xff\0");
    expected.extend_from_slice(&[0, 0]);
    expected.extend_from_slice(&((!5u32 << 16) | 5).to_le_bytes());
    match nvram::strip(b"a=1\nb=2") {
        Ok(blob) => report.check("nvram: boardrev added, an unterminated last line left out", blob == expected),
        Err(_) => report.check("nvram: boardrev added, an unterminated last line left out", false),
    }
    report.check(
        "nvram: a file for several PCIe devices is refused",
        nvram::strip(b"devpath0=pcie/1/4/\n0:x=1\n") == Err(nvram::Error::MultipleDevices),
    );
    report.check("nvram: nothing but comments is refused", nvram::strip(b"# only\n\n") == Err(nvram::Error::Empty));
}

fn control_frames(report: &mut Report) {
    // "ver" as `brcmf_c_preinit_dcmds` asks for it: GET_VAR, the name and 256
    // bytes of room, the first request on a fresh bus.
    let request = protocol::iovar("ver", &[0u8; 256]);
    let mut frame = Vec::new();
    protocol::control_frame(&mut frame, 255, 1, protocol::C_GET_VAR, false, 0, request.len(), &request);
    report.value("control: 12 + 16 + 260 bytes, no padding", frame.len() as u64, 288);
    report.check("control: length and its complement", frame[0..4] == [0x20, 0x01, 0xDF, 0xFE]);
    report.check("control: sequence 255, channel 0, data offset 12", frame[4..8] == [0xFF, 0x00, 0x00, 0x0C]);
    report.check("control: a zero word", frame[8..12] == [0, 0, 0, 0]);
    report.check("control: GET_VAR", frame[12..16] == [0x06, 0x01, 0, 0]);
    report.check("control: buffer length 260", frame[16..20] == [0x04, 0x01, 0, 0]);
    report.check("control: request id 1 in the top half, a get", frame[20..24] == [0, 0, 1, 0]);
    report.check("control: the name follows", &frame[28..32] == b"ver\0");

    // A CLM chunk as a set: 1448 bytes, padded to the next 512-byte block.
    let chunk = protocol::clm_chunk(protocol::DL_BEGIN, &[0xAA; 1400]);
    let request = protocol::iovar("clmload", &chunk);
    protocol::control_frame(&mut frame, 7, 0x1234, protocol::C_SET_VAR, true, 0, request.len(), &request);
    report.value("control: a 1448-byte request padded to 1536", frame.len() as u64, 1536);
    report.value("control: its header length excludes the padding", u16::from_le_bytes([frame[0], frame[1]]) as u64, 1448);
    report.check("control: a set, request id 0x1234", frame[20..24] == [0x02, 0, 0x34, 0x12]);
}

fn headers(report: &mut Report) {
    report.check("header: all zero is no data", protocol::parse_header(&[0u8; 12]) == Err(HeaderError::NoData));
    let mut bytes = [0u8; 12];
    protocol::pack_header(&mut bytes, 100, 9, protocol::CHANNEL_EVENT, 12);
    bytes[8] = 0x5A; // flow control
    bytes[9] = 0x21; // window
    match protocol::parse_header(&bytes) {
        Ok(h) => report.check(
            "header: what was packed comes back",
            h.len == 100 && h.seq == 9 && h.channel == protocol::CHANNEL_EVENT && h.data_offset == 12 && h.flow_control == 0x5A && h.window == 0x21,
        ),
        Err(_) => report.check("header: what was packed comes back", false),
    }
    let mut bad = bytes;
    bad[2] ^= 1;
    report.check("header: a wrong complement is refused", protocol::parse_header(&bad) == Err(HeaderError::Checksum));
    let mut bad = [0u8; 12];
    protocol::pack_header(&mut bad, 20, 0, protocol::CHANNEL_DATA, 24);
    report.check("header: a data offset past the frame is refused", protocol::parse_header(&bad) == Err(HeaderError::BadDataOffset));
    let mut long = [0u8; 12];
    protocol::pack_header(&mut long, 3000, 0, protocol::CHANNEL_DATA, 12);
    report.check("header: a data frame over 2048 bytes is refused", protocol::parse_header(&long) == Err(HeaderError::TooLong));
    let mut control = [0u8; 12];
    protocol::pack_header(&mut control, 3000, 0, protocol::CHANNEL_CONTROL, 12);
    report.check("header: a control frame over 2048 bytes is not", protocol::parse_header(&control).is_ok());
}

fn padding(report: &mut Report) {
    report.value("tx pad: 288 is aligned", protocol::tx_pad(288) as u64, 0);
    report.value("tx pad: 289 to eight", protocol::tx_pad(289) as u64, 7);
    report.value("tx pad: 512 is aligned", protocol::tx_pad(512) as u64, 0);
    report.value("tx pad: 513 to a block", protocol::tx_pad(513) as u64, 511);
    report.value("tx pad: 1024 is a block", protocol::tx_pad(1024) as u64, 0);
    report.value("rx rest: a 64-byte frame", protocol::rx_remaining(64) as u64, 0);
    report.value("rx rest: 100 bytes, 36 left, to eight", protocol::rx_remaining(100) as u64, 40);
    report.value("rx rest: 600 bytes, 536 left, to a block", protocol::rx_remaining(600) as u64, 1024);
    report.value("rx rest: 2000 bytes, a block would pass 2048", protocol::rx_remaining(2000) as u64, 1936);
    report.value("control rest: 600 bytes, to a block", protocol::control_remaining(600) as u64, 1024);
}

fn control_responses(report: &mut Report) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&262u32.to_le_bytes());
    payload.extend_from_slice(&8u32.to_le_bytes());
    payload.extend_from_slice(&((5u32 << 16) | 0x01).to_le_bytes());
    payload.extend_from_slice(&(-23i32).to_le_bytes());
    payload.extend_from_slice(b"answer!!");
    match protocol::parse_dcmd(&payload) {
        Some((response, data)) => report.check(
            "dcmd: id 5, an error of -23, the data after the header",
            response.id == 5 && response.error == Some(-23) && response.cmd == 262 && data == b"answer!!",
        ),
        None => report.check("dcmd: id 5, an error of -23, the data after the header", false),
    }

    let frame = [0x20, 0, 0x01, 1, 0xEE, 0xEE, 0xEE, 0xEE, 1, 2, 3];
    match protocol::strip_bcdc(&frame) {
        Some((ifidx, data)) => report.check("bcdc: interface 1, one word of signals skipped", ifidx == 1 && data == [1, 2, 3]),
        None => report.check("bcdc: interface 1, one word of signals skipped", false),
    }
    report.check("bcdc: version 1 is refused", protocol::strip_bcdc(&[0x10, 0, 0, 0, 1]).is_none());
}

/// An event packet as the firmware sends it.
fn event_packet(event_type: u32, status: u32, flags: u16, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0xFF; 6]);
    p.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
    p.extend_from_slice(&0x886Cu16.to_be_bytes());
    p.extend_from_slice(&32769u16.to_be_bytes());
    p.extend_from_slice(&((10 + 48 + data.len()) as u16).to_be_bytes());
    p.push(0);
    p.extend_from_slice(&[0x00, 0x10, 0x18]);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&2u16.to_be_bytes());
    p.extend_from_slice(&flags.to_be_bytes());
    p.extend_from_slice(&event_type.to_be_bytes());
    p.extend_from_slice(&status.to_be_bytes());
    p.extend_from_slice(&7u32.to_be_bytes()); // reason
    p.extend_from_slice(&0u32.to_be_bytes()); // auth type
    p.extend_from_slice(&(data.len() as u32).to_be_bytes());
    p.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    p.extend_from_slice(&[0u8; 16]);
    p.push(0);
    p.push(0);
    p.extend_from_slice(data);
    p
}

fn events(report: &mut Report) {
    let packet = event_packet(protocol::E_LINK, 0, protocol::EVENT_MSG_LINK, &[9, 9]);
    report.value("event: a packet with two bytes of data", packet.len() as u64, 74);
    match protocol::parse_event(&packet) {
        Some((event, data)) => report.check(
            "event: big-endian fields, the address, the data",
            event.event_type == protocol::E_LINK
                && event.flags == protocol::EVENT_MSG_LINK
                && event.reason == 7
                && event.addr == [0x10, 0x20, 0x30, 0x40, 0x50, 0x60]
                && data == [9, 9],
        ),
        None => report.check("event: big-endian fields, the address, the data", false),
    }
    let mut wrong = packet.clone();
    wrong[12] = 0x08;
    report.check("event: another ethertype is not an event", protocol::parse_event(&wrong).is_none());
    let mut short = packet.clone();
    short.truncate(73);
    report.check("event: data shorter than it says is refused", protocol::parse_event(&short).is_none());
    let mask = protocol::event_mask(&[protocol::E_ESCAN_RESULT, protocol::E_SET_SSID]);
    report.check("event mask: 69 is byte 8 bit 5, 0 is byte 0 bit 0", mask[8] == 0x20 && mask[0] == 0x01);
    report.value("event mask: 24 bytes for 191 events", protocol::EVENTING_MASK_LEN as u64, 24);
}

/// An escan result holding one BSS, laid out as `struct brcmf_bss_info_le`.
fn escan_result(ssid: &[u8], chanspec: u16, ctl_ch: u8, rssi: i16) -> Vec<u8> {
    let bss_len = 128usize;
    let mut r = Vec::new();
    r.extend_from_slice(&((12 + bss_len) as u32).to_le_bytes());
    r.extend_from_slice(&109u32.to_le_bytes());
    r.extend_from_slice(&0x1234u16.to_le_bytes());
    r.extend_from_slice(&1u16.to_le_bytes());
    let mut bss = alloc::vec![0u8; bss_len];
    bss[0..4].copy_from_slice(&109u32.to_le_bytes());
    bss[4..8].copy_from_slice(&(bss_len as u32).to_le_bytes());
    bss[8..14].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03]);
    bss[16..18].copy_from_slice(&0x0411u16.to_le_bytes());
    bss[18] = ssid.len() as u8;
    bss[19..19 + ssid.len()].copy_from_slice(ssid);
    bss[72..74].copy_from_slice(&chanspec.to_le_bytes());
    bss[78..80].copy_from_slice(&rssi.to_le_bytes());
    bss[88] = ctl_ch;
    r.extend_from_slice(&bss);
    r
}

fn scan_results(report: &mut Report) {
    let data = escan_result(b"network", 0xD02A, 36, -61);
    match protocol::parse_escan_result(&data) {
        Some(bss) => report.check(
            "escan: the name, the control channel, the signal",
            bss.ssid() == b"network" && bss.channel == 36 && bss.rssi == -61 && bss.bssid[5] == 3,
        ),
        None => report.check("escan: the name, the control channel, the signal", false),
    }
    let data = escan_result(b"x", 0x1006, 0, -40);
    match protocol::parse_escan_result(&data) {
        Some(bss) => report.value("escan: no ctl_ch, the chanspec's channel", bss.channel as u64, 6),
        None => report.check("escan: no ctl_ch, the chanspec's channel", false),
    }
    let mut two = escan_result(b"x", 0x1006, 6, -40);
    two[10] = 2;
    report.check("escan: more than one BSS in an event is refused", protocol::parse_escan_result(&two).is_none());
    let mut lying = escan_result(b"x", 0x1006, 6, -40);
    lying[12 + 4] = 100;
    report.check("escan: a BSS length that does not fit is refused", protocol::parse_escan_result(&lying).is_none());
    security(report);
}

/// An RSN element after the fixed part: version 1, CCMP group, one CCMP
/// pairwise suite, PSK and SAE, and capabilities with MFP capable set.
fn security(report: &mut Report) {
    // Version 2 bytes, group 4, pairwise count 2 and one suite 4, AKM count
    // 2 and two suites 8, capabilities 2: a body of 24.
    let rsn: [u8; 26] = [
        48, 24, 1, 0, 0x00, 0x0F, 0xAC, 0x04, 1, 0, 0x00, 0x0F, 0xAC, 0x04, 2, 0, 0x00, 0x0F, 0xAC,
        0x02, 0x00, 0x0F, 0xAC, 0x08, 0x80, 0x00,
    ];
    let mut ies = alloc::vec![0x00, 3, b'a', b'b', b'c'];
    ies.extend_from_slice(&rsn);
    ies.extend_from_slice(&[0xDD, 4, 0x00, 0x50, 0xF2, 0x01]);
    let s = protocol::parse_security(&ies);
    report.check(
        "rsn: CCMP group and pairwise, PSK and SAE, MFP capable not required, and a WPA element",
        s.rsn
            && s.wpa
            && s.group == protocol::SUITE_CCMP
            && s.pairwise_count == 1
            && s.pairwise[0] == protocol::SUITE_CCMP
            && s.akm_count == 2
            && s.akm[0] == protocol::AKM_PSK
            && s.akm[1] == protocol::AKM_SAE
            && s.capabilities == Some(protocol::RSN_CAP_MFPC),
    );
    let cut = protocol::parse_security(&[48, 6, 1, 0, 0x00, 0x0F, 0xAC, 0x04]);
    report.check("rsn: an element that stops after the group suite", cut.rsn && cut.group == protocol::SUITE_CCMP && cut.akm_count == 0);
    report.check("rsn: an element longer than what is left is not read", !protocol::parse_security(&[48, 40, 1, 0]).rsn);

    let mut data = escan_result(b"x", 0x1006, 6, -40);
    let bss_len = 128 + ies.len();
    data[0..4].copy_from_slice(&((12 + bss_len) as u32).to_le_bytes());
    data[12 + 4..12 + 8].copy_from_slice(&(bss_len as u32).to_le_bytes());
    data[12 + 116..12 + 118].copy_from_slice(&128u16.to_le_bytes());
    data[12 + 120..12 + 124].copy_from_slice(&(ies.len() as u32).to_le_bytes());
    data.extend_from_slice(&ies);
    match protocol::parse_escan_result(&data) {
        Some(bss) => report.check("escan: the elements after the fixed part are read", bss.security.rsn && bss.security.akm_count == 2),
        None => report.check("escan: the elements after the fixed part are read", false),
    }
}

fn requests(report: &mut Report) {
    let escan = protocol::escan_request(0x1234);
    report.value("escan request: 8 + 64 bytes", escan.len() as u64, 72);
    report.check(
        "escan request: version 1, start, sync id, broadcast, any type, active",
        escan[0..8] == [1, 0, 0, 0, 1, 0, 0x34, 0x12] && escan[44..50] == [0xFF; 6] && escan[50] == 2 && escan[51] == 0,
    );
    report.check("escan request: -1 for the four timings", escan[52..68].iter().all(|&b| b == 0xFF));
    report.check("country: JP as abbreviation and code, revision 0", protocol::country(*b"JP") == *b"JP\0\0\0\0\0\0JP\0\0");
    let begin = protocol::clm_chunk(protocol::DL_BEGIN, &[1, 2, 3]);
    report.check("clm: begin with handler version 1, type 2, length 3", begin[0..12] == [0x02, 0x10, 0x02, 0, 3, 0, 0, 0, 0, 0, 0, 0]);
    let end = protocol::clm_chunk(protocol::DL_END, &[1]);
    report.check("clm: end", end[0..2] == [0x04, 0x10]);
    join_payloads(report);
}

fn join_payloads(report: &mut Report) {
    let pmk = protocol::wsec_pmk(9, protocol::WSEC_PASSPHRASE, |key| key.copy_from_slice(b"abcdefghi"));
    report.value("pmk: 132 bytes", pmk.len() as u64, 132);
    report.check("pmk: length 9, the passphrase flag, the key, zeroes after", pmk[0..4] == [9, 0, 1, 0] && &pmk[4..13] == b"abcdefghi" && pmk[13..].iter().all(|&b| b == 0));

    let join = protocol::ext_join_params(4, [0xAA, 0xBB, 0xCC, 1, 2, 3], |field| field.copy_from_slice(b"home"));
    report.value("join: 68 bytes", join.len() as u64, 68);
    report.check("join: the SSID length and name", join[0..4] == [4, 0, 0, 0] && &join[4..8] == b"home" && join[8..36].iter().all(|&b| b == 0));
    report.check("join: scan type -1, three bytes of padding", join[36] == 0xFF && join[37..40] == [0, 0, 0]);
    report.check("join: nprobes, active, passive and home time -1", join[40..56].iter().all(|&b| b == 0xFF));
    report.check("join: the chosen BSSID at 56, padding, no chanspecs", join[56..62] == [0xAA, 0xBB, 0xCC, 1, 2, 3] && join[62..68] == [0; 6]);
    let ssid = protocol::ssid_le(4, |field| field.copy_from_slice(b"home"));
    report.check("set ssid: the SSID alone in 36 bytes", ssid.len() == 36 && ssid[0] == 4 && &ssid[4..8] == b"home");

    let mut frame = Vec::new();
    protocol::data_frame(&mut frame, 3, &[0x5A; 60]);
    report.value("data: 12 + 2 + 4 + 60 bytes, rounded up to 80", frame.len() as u64, 80);
    report.check("data: header length 78, channel 2, data offset 14", frame[0..2] == [78, 0] && frame[4..8] == [3, 2, 0, 14]);
    report.check("data: a BCDC header of version 2 before the frame", frame[14..18] == [0x20, 0, 0, 0] && frame[18..78].iter().all(|&b| b == 0x5A));
}

fn sdio_arguments(report: &mut Report) {
    report.value("CMD52 read of CCCR abort", sdio::direct_argument(false, 0, 0x06, 0) as u64, 0x0000_0C00);
    report.value("CMD52 write of ChipClkCSR", sdio::direct_argument(true, 1, 0x1000E, 0x28) as u64, 0x9200_1C28);
    report.value("CMD53 four bytes from the backplane", sdio::extended_argument(false, 1, 0x8000, true, 0, 4) as u64, 0x1500_0004);
    report.value("CMD53 three blocks to function 2", sdio::extended_argument(true, 2, 0x8000, true, 3, 512) as u64, 0xAD00_0003);
    report.value("CMD53 512 bytes in byte mode is a count of 0", sdio::extended_argument(false, 2, 0x8000, false, 0, 512) as u64, 0x2100_0000);
}

fn clock(report: &mut Report) {
    let (bits, actual) = sdhci::clock_divider(250_000_000, 400_000);
    report.value("divider: 250 MHz to 400 kHz is 626", bits as u64, 0x3940);
    report.value("divider: which gives", actual as u64, 399_361);
    let (bits, actual) = sdhci::clock_divider(250_000_000, 50_000_000);
    report.check("divider: 250 MHz to 50 MHz is 6, 41.7 MHz", bits == 0x0300 && actual == 41_666_666);
    let (bits, actual) = sdhci::clock_divider(25_000_000, 50_000_000);
    report.check("divider: a slow base clock is used undivided", bits == 0 && actual == 25_000_000);
}

fn voltage(report: &mut Report) {
    report.value("OCR: 2.7 to 3.6 V leaves 3.2 to 3.4 V", sdio::select_voltage(0x00FF_8000) as u64, 0x0030_0000);
    report.value("OCR: 3.2 to 3.3 V alone", sdio::select_voltage(0x0010_0000) as u64, 0x0010_0000);
    report.value("OCR: nothing the host has", sdio::select_voltage(0x000F_0000) as u64, 0);
}

fn credentials(report: &mut Report) {
    match config::parse(b"ssid=home net\npsk=correct horse\ncountry=GB\n") {
        Ok(c) => report.check(
            "wifi.conf: all three",
            c.ssid.matches(b"home net") && !c.ssid.matches(b"home") && c.passphrase.len() == 13 && c.country.0 == *b"GB",
        ),
        Err(_) => report.check("wifi.conf: all three", false),
    }
    match config::parse(b"ssid=a\r\npsk=12345678\r\n") {
        Ok(c) => report.check("wifi.conf: CRLF, and no country means JP", c.ssid.matches(b"a") && c.country.0 == *b"JP"),
        Err(_) => report.check("wifi.conf: CRLF, and no country means JP", false),
    }
    report.check("wifi.conf: a seven-character psk is refused", matches!(config::parse(b"ssid=a\npsk=1234567\n"), Err(config::Error::PassphraseLength)));
    report.check("wifi.conf: a missing psk is refused", matches!(config::parse(b"ssid=a\n"), Err(config::Error::NoPassphrase)));
    report.check("wifi.conf: another key is refused", matches!(config::parse(b"ssid=a\npsk=12345678\nkey=x\n"), Err(config::Error::UnknownKey(3))));
    report.check("wifi.conf: a lower-case country is refused", matches!(config::parse(b"ssid=a\npsk=12345678\ncountry=jp\n"), Err(config::Error::Country)));
}

/// A tree shaped like the part of a Pi 4's the driver reads: a `soc` bus with
/// the board's `dma-ranges`, two controller nodes at the same address with
/// the same compatible, the first disabled, and a mailbox.
fn sample_tree() -> Vec<u8> {
    let mut tree = Builder::new();
    tree.begin_node("");
    tree.prop_u32("#address-cells", 2);
    tree.prop_u32("#size-cells", 1);
    tree.begin_node("soc");
    tree.prop_u32("#address-cells", 1);
    tree.prop_u32("#size-cells", 1);
    tree.prop_cells("ranges", &[0x7e00_0000, 0x0, 0xfe00_0000, 0x0180_0000]);
    tree.prop_cells("dma-ranges", &[0xc000_0000, 0x0, 0x0, 0x4000_0000]);

    tree.begin_node("mailbox@7e00b880");
    tree.prop_str("compatible", "brcm,bcm2835-mbox");
    tree.prop_cells("reg", &[0x7e00_b880, 0x40]);
    tree.end_node();

    tree.begin_node("mmc@7e300000");
    tree.prop_str("compatible", "brcm,bcm2835-sdhci");
    tree.prop_cells("reg", &[0x7e30_0000, 0x100]);
    tree.prop_str("status", "disabled");
    tree.prop_u32("bus-width", 1);
    tree.end_node();

    tree.begin_node("mmcnr@7e300000");
    tree.prop_str("compatible", "brcm,bcm2835-sdhci");
    tree.prop_cells("reg", &[0x7e30_0000, 0x100]);
    tree.prop_str("status", "okay");
    tree.prop_u32("bus-width", 4);
    tree.begin_node("wifi@1");
    tree.prop_u32("reg", 1);
    tree.prop_str("compatible", "brcm,bcm4329-fmac");
    tree.end_node();
    tree.end_node();

    tree.end_node();
    tree.end_node();
    tree.finish()
}

fn device_tree(report: &mut Report) {
    let Some(phys) = place(&sample_tree()) else {
        report.check("a tree to walk", false);
        return;
    };
    match arch::fdt::find_enabled_compatible_in(phys, b"brcm,bcm2835-sdhci") {
        Some(node) => {
            report.value("the enabled controller, not the first", node.cell(b"bus-width").unwrap_or(0) as u64, 4);
            report.value("at the translated address", node.reg(0).map(|r| r.0).unwrap_or(0), 0xFE30_0000);
        }
        None => report.check("the enabled controller, not the first", false),
    }
    match arch::fdt::find_compatible_in(phys, b"brcm,bcm2835-sdhci") {
        Some(node) => report.value("the plain lookup still finds the first", node.cell(b"bus-width").unwrap_or(0) as u64, 1),
        None => report.check("the plain lookup still finds the first", false),
    }
    match arch::fdt::find_enabled_compatible_in(phys, b"brcm,bcm2835-mbox") {
        Some(node) => {
            report.value("memory at 0x1000 as a device on soc sees it", node.dma_address(0x1000).unwrap_or(0), 0xC000_1000);
            report.check("memory past the first gigabyte is not visible", node.dma_address(0x4000_0000).is_none());
        }
        None => report.check("memory at 0x1000 as a device on soc sees it", false),
    }
    report.check(
        "a compatible nothing has",
        arch::fdt::find_enabled_compatible_in(phys, b"brcm,nonesuch").is_none(),
    );
}
