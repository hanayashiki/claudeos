//! Getting the link back: the order of scans and joins while the WiFi has no
//! link.
//!
//! The network can go away at any moment and for any reason, and there is no
//! supplicant in user space to notice and try again, so the kernel does. While
//! the stack's link is down, from bring-up on, the driver repeats one attempt:
//! scan, choose the strongest usable access point with the configured name
//! from that scan, join it, and run the handshake. An attempt ends with the
//! link up or with a failure, and every failure schedules the next attempt,
//! whatever it was: the network not in the scan, the firmware refusing a
//! request, the association failing, the handshake failing, or the attempt
//! running out of time. Only a link coming up stops the attempts.
//!
//! This file decides and the driver acts, the way the DHCP client does. It is
//! told what happened (`Input`) and the time, and it answers with what to do
//! (`Effect`): start a scan, join this access point, leave an association, or
//! print a line. It touches no hardware and reads no clock, so the self test
//! drives it with scripted sequences and a clock of its own.
//!
//! **The log.** A run is the time from a link lost, or from bring-up, until
//! the link has stayed up for `LINK_STABLE_MS`. Within a run, the first link
//! lost and the first failure of each kind each get a line. Later ones of the
//! same kind are counted, and the count is printed when `REPORT_INTERVAL_MS`
//! has passed since that kind's last line. The link coming back gets a line
//! when the loss before it had one. So a network that is gone for a day adds a
//! line every five minutes, and a link that keeps dropping adds two, not a
//! line per attempt. When the run ends with anything counted but not printed,
//! one line says how many losses and failed attempts it had.

use super::protocol;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How long a scan may take before the attempt is given up. A scan of both
/// bands with the firmware's own dwell times took about 2.4 s on the board;
/// brcmfmac's `BRCMF_ESCAN_TIMER_INTERVAL_MS` is ten seconds, and this allows
/// fifteen.
pub const SCAN_TIMEOUT_MS: u64 = 15_000;

/// How long a join request may go without the association. On the board the
/// link was up about 2.6 s after the request, association and handshake
/// together, and most of that is the firmware's own scan for the access point
/// before it associates. wpa_supplicant gives authentication and association
/// ten seconds (`wpa_supplicant_req_auth_timeout` when it asks the driver to
/// associate), several times what the board took, and this is the same.
pub const ASSOCIATE_TIMEOUT_MS: u64 = 10_000;

/// How long an association may go without the link, which comes up when the
/// 4-way handshake has finished and the keys are installed. wpa_supplicant
/// also allows ten seconds from the association for the first EAPOL frame.
/// An access point that loses a handshake message sends it again, about a
/// second later in hostapd and a few times over, before it gives up and
/// deauthenticates, and ten seconds covers those retries.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

/// The wait before the first attempt after a working link is lost. Not no
/// wait: a deauthentication often means the access point is restarting its
/// radio or changing channel, and a scan in the same instant can miss it. Not
/// a long one either, because every second of it is a second with no network.
/// With the 2.4 s scan and the 2.6 s join after it, the link is back about
/// seven seconds after it went, if the network is there.
pub const FIRST_RETRY_MS: u64 = 2_000;

/// The longest wait between attempts. The wait doubles with every attempt in a
/// row that fails, 4, 8 and 16 seconds after the first retry's 2, and then
/// stays at this for as long as the network stays away. A network that comes
/// back is joined within about half a minute. At the cap an attempt is a 2.4 s
/// scan every 32 seconds or so, which keeps the radio scanning less than a
/// tenth of the time.
pub const MAX_RETRY_MS: u64 = 30_000;

/// How long the link has to stay up to count as working. Losing a link that
/// was up this long starts the waits again from `FIRST_RETRY_MS`; losing one
/// sooner counts as one more failure in a row, so an access point that takes
/// the association and drops it every few seconds is asked at the growing
/// waits and not every two seconds. A minute is more than twenty times the
/// 2.6 s a join takes.
pub const LINK_STABLE_MS: u64 = 60_000;

/// How often a line that keeps happening is counted in the log.
pub const REPORT_INTERVAL_MS: u64 = 300_000;

/// What the machine needs to know about the access point it hands to `Join`.
/// The driver's is a scan result; the self test's is a channel number.
pub trait Target: Clone {
    fn channel(&self) -> u8;
}

/// Why an attempt ended without a link.
#[derive(Clone, PartialEq, Eq)]
pub enum Failure {
    /// Nothing in the scan carried the configured name.
    NotFound,
    /// Something did, but none of it offers security this driver can join.
    Unsuitable(&'static str),
    /// The firmware refused to scan, and what it answered.
    ScanRefused(String),
    ScanTimedOut,
    /// The firmware refused a request that sets up the join.
    JoinRefused(String),
    /// The firmware's SET_SSID event with a status other than success.
    AssociationFailed { status: u32, reason: u32 },
    NoAssociation,
    NoHandshake,
    /// The supplicant refused a handshake frame and disassociated.
    HandshakeFailed { reason: u16 },
    /// A deauthentication, a disassociation or the link going down, while the
    /// attempt was still associating or in the handshake.
    Ended { cause: &'static str, reason: u32 },
}

/// How many kinds of `Failure` there are.
const FAILURE_KINDS: usize = 10;
/// The kinds of line a run counts: each kind of failure, then a link lost,
/// then a link up.
const LOST: usize = FAILURE_KINDS;
const UP: usize = FAILURE_KINDS + 1;
const KINDS: usize = FAILURE_KINDS + 2;

impl Failure {
    /// Failures of the same kind are counted together, whatever their codes.
    fn kind(&self) -> usize {
        match self {
            Failure::NotFound => 0,
            Failure::Unsuitable(_) => 1,
            Failure::ScanRefused(_) => 2,
            Failure::ScanTimedOut => 3,
            Failure::JoinRefused(_) => 4,
            Failure::AssociationFailed { .. } => 5,
            Failure::NoAssociation => 6,
            Failure::NoHandshake => 7,
            Failure::HandshakeFailed { .. } => 8,
            Failure::Ended { .. } => 9,
        }
    }
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::NotFound => write!(f, "the configured network was not in the scan"),
            Failure::Unsuitable(why) => write!(f, "{}", why),
            Failure::ScanRefused(why) => write!(f, "the scan was refused: {}", why),
            Failure::ScanTimedOut => write!(f, "the scan did not finish within {} s", SCAN_TIMEOUT_MS / 1000),
            Failure::JoinRefused(why) => write!(f, "asking to join failed: {}", why),
            Failure::AssociationFailed { status, reason } => {
                write!(f, "the join failed: SET_SSID status {} reason {}", status, reason)
            }
            Failure::NoAssociation => {
                write!(f, "not associated within {} s of the join request", ASSOCIATE_TIMEOUT_MS / 1000)
            }
            Failure::NoHandshake => {
                write!(f, "associated, but the handshake did not finish within {} s", HANDSHAKE_TIMEOUT_MS / 1000)
            }
            Failure::HandshakeFailed { reason } => {
                write!(f, "associated, but the handshake failed; disassociated with reason {}", reason)
            }
            Failure::Ended { cause, reason } => write!(f, "the attempt was ended by {} reason {}", cause, reason),
        }
    }
}

/// What happened, for `step`.
pub enum Input<B> {
    /// Nothing but the time moving on.
    Tick,
    /// The scan finished: the access point to join, or why there is none.
    Scanned(Result<B, Failure>),
    /// The firmware did not take the scan request.
    ScanRefused(String),
    /// The firmware did not take a request that sets up the join.
    JoinRefused(String),
    AssociationFailed { status: u32, reason: u32 },
    Associated,
    /// The supplicant refused a frame and the driver disassociated.
    HandshakeFailed { reason: u16 },
    /// The handshake finished and the keys are in: the stack's link is up.
    LinkUp,
    /// The firmware said the association ended, or the driver ended it.
    LinkLost { cause: &'static str, reason: u32 },
}

/// What to do, in the order `step` returns it.
pub enum Effect<B> {
    /// Start a scan. Its end comes back as `Input::Scanned`.
    Scan,
    /// Ask to join this access point.
    Join(B),
    /// Leave whatever association the attempt made, so the next attempt
    /// starts from none.
    Disassociate,
    /// A line for the log.
    Log(String),
}

enum State<B> {
    /// The next attempt starts at `at`.
    Waiting { at: u64 },
    Scanning { deadline: u64 },
    /// A join asked for at `requested`, not associated yet.
    Associating { target: B, requested: u64, deadline: u64 },
    /// Associated, and the handshake running.
    Handshaking { target: B, requested: u64, deadline: u64 },
    Up { target: B, since: u64 },
}

/// The log's record of one kind of line within a run.
#[derive(Clone, Copy, Default)]
struct Tally {
    /// Whether this kind has had its first line.
    reported: bool,
    /// How many since this kind's last line.
    unreported: u32,
    last_report: u64,
}

/// Losses and failures from a link lost, or bring-up, until the link has
/// stayed up for `LINK_STABLE_MS`.
struct Run {
    began: u64,
    losses: u32,
    failed: u32,
    tallies: [Tally; KINDS],
}

impl Run {
    fn new(now: u64) -> Run {
        Run { began: now, losses: 0, failed: 0, tallies: [Tally::default(); KINDS] }
    }
}

pub struct Reconnect<B> {
    state: State<B>,
    /// Failed attempts in a row, counting a link lost before it had stayed up
    /// for `LINK_STABLE_MS` as one. It sets the wait before the next attempt.
    failures: u32,
    /// Attempts since the link was last up, the one in progress included.
    attempts: u32,
    /// Attempts since the machine was made.
    started: u32,
    ever_up: bool,
    /// When the link was lost, or when the machine was made.
    down_since: u64,
    /// Whether the last link lost had a line, or, before any, true.
    loss_reported: bool,
    run: Option<Run>,
    effects: Vec<Effect<B>>,
}

/// The wait after `failures` failed attempts in a row.
pub fn retry_delay(failures: u32) -> u64 {
    // Sixteen doublings of two seconds are past the cap already, and the
    // shift stays well inside 64 bits.
    (FIRST_RETRY_MS << failures.min(16)).min(MAX_RETRY_MS)
}

impl<B: Target> Reconnect<B> {
    /// A machine whose first attempt's scan is already running, which is what
    /// bring-up's scan is: its result is the first `Input::Scanned`.
    pub fn new(now: u64) -> Reconnect<B> {
        Reconnect {
            state: State::Scanning { deadline: now + SCAN_TIMEOUT_MS },
            failures: 0,
            attempts: 1,
            started: 1,
            ever_up: false,
            down_since: now,
            loss_reported: true,
            run: Some(Run::new(now)),
            effects: Vec::new(),
        }
    }

    /// Whether the attempt in progress is the first since the machine was
    /// made and no link has come up yet. The driver describes that attempt
    /// step by step, and later ones only through this machine's lines.
    pub fn first_attempt(&self) -> bool {
        self.started == 1 && !self.ever_up
    }

    pub fn link_up(&self) -> bool {
        matches!(self.state, State::Up { .. })
    }

    /// When a `Tick` next has something to do, or `None` when only an input
    /// can change anything.
    pub fn deadline(&self) -> Option<u64> {
        match &self.state {
            State::Waiting { at } => Some(*at),
            State::Scanning { deadline }
            | State::Associating { deadline, .. }
            | State::Handshaking { deadline, .. } => Some(*deadline),
            State::Up { since, .. } => self.run.as_ref().map(|_| since + LINK_STABLE_MS),
        }
    }

    /// Take in what happened at `now`, in milliseconds, and say what to do.
    pub fn step(&mut self, now: u64, input: Input<B>) -> Vec<Effect<B>> {
        match input {
            Input::Tick => self.on_tick(now),
            Input::Scanned(result) => {
                if let State::Scanning { .. } = self.state {
                    match result {
                        Ok(target) => {
                            self.state = State::Associating {
                                target: target.clone(),
                                requested: now,
                                deadline: now + ASSOCIATE_TIMEOUT_MS,
                            };
                            self.effects.push(Effect::Join(target));
                        }
                        Err(failure) => self.fail(now, failure),
                    }
                }
            }
            Input::ScanRefused(why) => {
                if let State::Scanning { .. } = self.state {
                    self.fail(now, Failure::ScanRefused(why));
                }
            }
            Input::JoinRefused(why) => {
                if let State::Associating { .. } = self.state {
                    self.fail(now, Failure::JoinRefused(why));
                }
            }
            Input::AssociationFailed { status, reason } => {
                if let State::Associating { .. } = self.state {
                    self.fail(now, Failure::AssociationFailed { status, reason });
                }
            }
            Input::Associated => self.on_associated(now),
            Input::HandshakeFailed { reason } => match self.state {
                State::Associating { .. } | State::Handshaking { .. } => {
                    self.fail(now, Failure::HandshakeFailed { reason })
                }
                // A group key handshake while the link is up, which the driver
                // ends by disassociating, as it does the 4-way handshake.
                State::Up { .. } => self.on_link_lost(now, "a refused handshake frame, disassociated with", reason as u32),
                State::Waiting { .. } | State::Scanning { .. } => {}
            },
            Input::LinkUp => self.on_link_up(now),
            Input::LinkLost { cause, reason } => self.on_link_lost(now, cause, reason),
        }
        core::mem::take(&mut self.effects)
    }

    fn on_tick(&mut self, now: u64) {
        match self.state {
            State::Waiting { at } if now >= at => {
                self.attempts += 1;
                self.started += 1;
                self.state = State::Scanning { deadline: now + SCAN_TIMEOUT_MS };
                self.effects.push(Effect::Scan);
            }
            State::Scanning { deadline } if now >= deadline => self.fail(now, Failure::ScanTimedOut),
            State::Associating { deadline, .. } if now >= deadline => {
                // The firmware may still be trying, and an association it
                // made after this would have no supplicant behind it.
                self.effects.push(Effect::Disassociate);
                self.fail(now, Failure::NoAssociation);
            }
            State::Handshaking { deadline, .. } if now >= deadline => {
                self.effects.push(Effect::Disassociate);
                self.fail(now, Failure::NoHandshake);
            }
            State::Up { since, .. } => self.end_run_if_stable(now, since),
            _ => {}
        }
    }

    fn on_associated(&mut self, now: u64) {
        match &self.state {
            State::Associating { target, requested, .. } => {
                let (target, requested) = (target.clone(), *requested);
                self.state = State::Handshaking { target, requested, deadline: now + HANDSHAKE_TIMEOUT_MS };
            }
            // An association no attempt is waiting for: one that finished
            // after its attempt was given up.
            State::Waiting { .. } | State::Scanning { .. } => self.effects.push(Effect::Disassociate),
            State::Handshaking { .. } | State::Up { .. } => {}
        }
    }

    fn on_link_up(&mut self, now: u64) {
        let (target, requested) = match &self.state {
            State::Associating { target, requested, .. } | State::Handshaking { target, requested, .. } => {
                (target.clone(), *requested)
            }
            State::Waiting { .. } | State::Scanning { .. } | State::Up { .. } => return,
        };
        let channel = target.channel();
        let attempts = self.attempts;
        self.state = State::Up { target, since: now };
        self.ever_up = true;
        self.attempts = 0;
        // A loss that had a line gets a line for the link coming back, so the
        // log never ends on a loss the link has since recovered from; one
        // that was only counted is only counted coming back.
        if self.loss_reported {
            self.log(format!(
                "wifi: joined on channel {}: link up {} ms after the join request, on attempt {}, {} ms without a link",
                channel,
                now - requested,
                attempts,
                now - self.down_since
            ));
        } else {
            self.run.get_or_insert_with(|| Run::new(now)).tallies[UP].unreported += 1;
        }
    }

    fn on_link_lost(&mut self, now: u64, cause: &'static str, reason: u32) {
        match self.state {
            State::Up { since, .. } => {
                self.end_run_if_stable(now, since);
                // A run still open means the link did not stay up long enough
                // to count as working.
                if self.run.is_some() {
                    self.failures = self.failures.saturating_add(1);
                }
                let delay = retry_delay(self.failures);
                self.down_since = now;
                self.state = State::Waiting { at: now + delay };
                self.run.get_or_insert_with(|| Run::new(now)).losses += 1;
                let due = self.due(now, LOST);
                self.loss_reported = due.is_some();
                match due {
                    Some(0) => self.log(format!(
                        "wifi: link lost: {} reason {}, after {} s up; scanning again in {} s",
                        cause,
                        reason,
                        (now - since) / 1000,
                        delay / 1000
                    )),
                    Some(count) => self.log(format!(
                        "wifi: link lost {} more times since the last report, each within {} s of coming up; the last by {} reason {}; scanning again in {} s",
                        count,
                        LINK_STABLE_MS / 1000,
                        cause,
                        reason,
                        delay / 1000
                    )),
                    None => {}
                }
            }
            State::Associating { .. } | State::Handshaking { .. } => self.fail(now, Failure::Ended { cause, reason }),
            // Nothing is associated, so this is the end of an association
            // already given up, arriving late.
            State::Waiting { .. } | State::Scanning { .. } => {}
        }
    }

    /// End the attempt in progress and schedule the next.
    fn fail(&mut self, now: u64, failure: Failure) {
        self.failures = self.failures.saturating_add(1);
        let delay = retry_delay(self.failures);
        self.state = State::Waiting { at: now + delay };
        self.run.get_or_insert_with(|| Run::new(now)).failed += 1;
        match self.due(now, failure.kind()) {
            Some(0) => self.log(format!("wifi: attempt {}: {}; trying again in {} s", self.attempts, failure, delay / 1000)),
            Some(count) => self.log(format!(
                "wifi: {} {} more times since the last report; attempt {}, no link for {} s; trying again in {} s",
                failure,
                count,
                self.attempts,
                (now - self.down_since) / 1000,
                delay / 1000
            )),
            None => {}
        }
    }

    /// Once the link has stayed up for `LINK_STABLE_MS`, the run is over: the
    /// waits start again from the shortest, and what the run counted without
    /// printing is summed up in one line.
    fn end_run_if_stable(&mut self, now: u64, since: u64) {
        if now - since < LINK_STABLE_MS {
            return;
        }
        let Some(run) = self.run.take() else { return };
        self.failures = 0;
        if run.tallies.iter().any(|tally| tally.unreported > 0) {
            self.log(format!(
                "wifi: the link has stayed up {} s, after {} losses and {} failed attempts in {} s",
                LINK_STABLE_MS / 1000,
                run.losses,
                run.failed,
                (now - run.began) / 1000
            ));
        }
    }

    /// Whether a line of this kind is due now: `Some(0)` for the first of its
    /// kind in the run, `Some(count)` when `REPORT_INTERVAL_MS` has passed
    /// since its last line and `count` more have happened, and `None` when this
    /// one is only counted.
    fn due(&mut self, now: u64, kind: usize) -> Option<u32> {
        let tally = &mut self.run.get_or_insert_with(|| Run::new(now)).tallies[kind];
        if !tally.reported {
            tally.reported = true;
            tally.last_report = now;
            return Some(0);
        }
        tally.unreported += 1;
        if now - tally.last_report < REPORT_INTERVAL_MS {
            return None;
        }
        let count = tally.unreported;
        tally.unreported = 0;
        tally.last_report = now;
        Some(count)
    }

    fn log(&mut self, line: String) {
        self.effects.push(Effect::Log(line));
    }
}

impl Target for protocol::Bss {
    fn channel(&self) -> u8 {
        self.channel
    }
}
