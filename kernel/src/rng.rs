//! Random numbers.
//!
//! ChaCha20 run over a counter, keyed from an entropy pool the kernel fills at
//! boot and keeps adding to while it runs. It backs `/dev/random`,
//! `/dev/urandom`, `getrandom`, the `AT_RANDOM` bytes handed to a program's
//! libc, and TCP's per-boot initial sequence number secret.
//!
//! The cipher is here because the generator's state has to survive its own
//! output being read. Any program can read `/dev/urandom`, so whatever backs
//! it hands a reader as much output as they ask for, and a generator whose
//! next state is a function of the word it last produced -- which is what a
//! xorshift is -- gives up its whole past and future to sixty-four bits of
//! that. ChaCha20 is a permutation over a key and a counter, with the state it
//! started from added back at the end; recovering the key from keystream is
//! the problem the cipher is built to be hard at. It needs no dependency, no
//! tables and about eighty lines, it is what Linux uses for the same job, and
//! it has published test vectors, so the implementation can be checked against
//! the standard rather than against a histogram.
//!
//! Seeding is the weaker half, and `init` below says what it rests on. Where
//! the processor has a generator of its own -- RDSEED or RDRAND on x86-64,
//! FEAT_RNG on aarch64 -- that carries the seed. Where it has neither, which
//! includes the Raspberry Pi 4 this kernel targets, the seed is boot timing
//! and is worth tens of bits rather than hundreds. The README says what that
//! does and does not make this fit for.

use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub mod selftest;

// ---- ChaCha20, RFC 8439 ---------------------------------------------------

/// "expand 32-byte k", which is where the first four words of the state come
/// from.
const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

const KEY_LEN: usize = 32;
const BLOCK_LEN: usize = 64;
const NONCE_LEN: usize = 12;

#[inline]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One 64-byte block of keystream for a key, a block counter and a nonce.
///
/// The twenty rounds are a permutation and on their own would be invertible.
/// Adding the state the permutation started from is what makes the block
/// one-way in the key, and everything below rests on that: the output the
/// generator hands out, and the pool, which uses this as its mixing step.
fn block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN], out: &mut [u8; BLOCK_LEN]) {
    let mut start = [0u32; 16];
    start[0..4].copy_from_slice(&CONSTANTS);
    for (i, word) in start[4..12].iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    start[12] = counter;
    for (i, word) in start[13..16].iter_mut().enumerate() {
        *word =
            u32::from_le_bytes([nonce[4 * i], nonce[4 * i + 1], nonce[4 * i + 2], nonce[4 * i + 3]]);
    }

    let mut s = start;
    // Ten double rounds: the four columns, then the four diagonals.
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }

    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(start[i]).to_le_bytes());
    }
}

/// Overwrite bytes the compiler would otherwise be free to leave where they
/// are, because nothing reads them afterwards.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

// ---- the generator --------------------------------------------------------

/// The key the next request will be answered from.
///
/// A request runs the cipher from block zero, takes the first thirty-two bytes
/// of keystream as the next key, and hands the rest out. The key that produced
/// an answer is gone before the answer is returned, so someone who reads
/// kernel memory afterwards cannot work out what was handed out before they
/// looked. The nonce is zero throughout: a nonce keeps two messages under one
/// key apart, and no key here answers a second request.
struct Generator {
    key: [u8; KEY_LEN],
    seeded: bool,
}

static GENERATOR: Spinlock<Generator> =
    Spinlock::new(Generator { key: [0; KEY_LEN], seeded: false });

impl Generator {
    fn draw(&mut self, out: &mut [u8]) {
        let mut key = self.key;
        let nonce = [0u8; NONCE_LEN];
        let mut buffer = [0u8; BLOCK_LEN];

        block(&key, 0, &nonce, &mut buffer);
        self.key.copy_from_slice(&buffer[..KEY_LEN]);

        let first = out.len().min(BLOCK_LEN - KEY_LEN);
        out[..first].copy_from_slice(&buffer[KEY_LEN..KEY_LEN + first]);

        let mut done = first;
        let mut counter = 1u32;
        while done < out.len() {
            block(&key, counter, &nonce, &mut buffer);
            let take = (out.len() - done).min(BLOCK_LEN);
            out[done..done + take].copy_from_slice(&buffer[..take]);
            done += take;
            counter += 1;
        }

        wipe(&mut buffer);
        wipe(&mut key);
    }
}

// ---- the entropy pool -----------------------------------------------------

/// Everything the kernel has observed, folded into thirty-two bytes.
///
/// Folding uses the block function above: the pool is the key, the observation
/// is the nonce, and the first half of the keystream is the pool that comes
/// out. Because the block adds the state it started from, the pool that went
/// in cannot be worked back out of the pool that came out, so reading the pool
/// now says nothing about what was mixed into it before.
struct Pool {
    value: [u8; KEY_LEN],
    /// Counts the foldings, so mixing one observation twice does not give the
    /// same pool twice.
    step: u32,
}

static POOL: Spinlock<Pool> = Spinlock::new(Pool { value: [0; KEY_LEN], step: 0 });

impl Pool {
    fn mix(&mut self, data: &[u8]) {
        let mut buffer = [0u8; BLOCK_LEN];
        for chunk in data.chunks(NONCE_LEN) {
            let mut nonce = [0u8; NONCE_LEN];
            nonce[..chunk.len()].copy_from_slice(chunk);
            self.step = self.step.wrapping_add(1);
            block(&self.value, self.step, &nonce, &mut buffer);
            self.value.copy_from_slice(&buffer[..KEY_LEN]);
        }
        wipe(&mut buffer);
    }

    fn mix_u64(&mut self, value: u64) {
        self.mix(&value.to_le_bytes());
    }

    fn mix_words(&mut self, words: &[u64]) {
        for word in words {
            self.mix_u64(*word);
        }
    }

    /// Thirty-two bytes that depend on everything folded in so far. The pool
    /// moves on at the same time, so no two extractions see the same state.
    fn extract(&mut self) -> [u8; KEY_LEN] {
        let mut buffer = [0u8; BLOCK_LEN];
        self.step = self.step.wrapping_add(1);
        block(&self.value, self.step, &[0u8; NONCE_LEN], &mut buffer);
        self.value.copy_from_slice(&buffer[..KEY_LEN]);
        let mut out = [0u8; KEY_LEN];
        out.copy_from_slice(&buffer[KEY_LEN..]);
        wipe(&mut buffer);
        out
    }

    /// A value that names this boot and gives nothing about the pool away, for
    /// the boot line and for the check that two boots differ. Its nonce is a
    /// constant nothing else here passes, so the block it reads shares no
    /// keystream with a folding or an extraction, and unlike those two it
    /// leaves the pool where it was.
    fn boot_id(&self) -> u64 {
        let mut buffer = [0u8; BLOCK_LEN];
        block(&self.value, 0, b"claudeos-id\0", &mut buffer);
        let id = u64::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6], buffer[7],
        ]);
        wipe(&mut buffer);
        id
    }
}

// ---- observations from interrupt handlers ---------------------------------

/// Where an interrupt handler leaves what it saw.
///
/// A folding into the pool costs a ChaCha block per twelve bytes and is behind
/// a lock that masks interrupts, which is more than an interrupt handler
/// should pay. So handlers fold into these four words with plain arithmetic,
/// and the next reseed carries all four into the pool at once. The mixing here
/// is not cryptographic and does not need to be: it has to spread one
/// observation across a word and not cancel the last one out. What
/// one-wayness there is comes from the pool.
static FAST: [AtomicU64; 4] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static EVENTS: AtomicU32 = AtomicU32::new(0);

/// splitmix64's finalizer: every input bit reaches the whole word.
fn stir(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Record that something happened, and when.
///
/// `what` says which kind of event it was; the entropy is in the cycle
/// counter, because when an interrupt arrives varies and what it is does not.
/// On both emulated machines the gap between two timer ticks measured about
/// six bits over three hundred boots, which is what makes this worth doing at
/// 100 Hz.
///
/// Called from interrupt handlers, so it takes no lock and allocates nothing.
/// Two handlers racing here lose one observation, which costs nothing.
pub fn observe(what: u64) {
    let index = EVENTS.fetch_add(1, Ordering::Relaxed) as usize & 3;
    let slot = &FAST[index];
    let sample = crate::arch::cycle_counter() ^ what.rotate_left(32);
    slot.store(stir(slot.load(Ordering::Relaxed) ^ sample), Ordering::Relaxed);
}

/// What the fast pool holds, leaving it to fill again.
fn drain() -> [u64; 4] {
    let mut out = [0u64; 4];
    for (slot, word) in FAST.iter().zip(out.iter_mut()) {
        *word = slot.swap(0, Ordering::Relaxed);
    }
    EVENTS.store(0, Ordering::Relaxed);
    out
}

// ---- reseeding ------------------------------------------------------------

/// How long the generator keeps one key. A second is short enough that a state
/// somebody somehow learned stops being worth having quickly, and long enough
/// that the cost does not show: a reseed is a dozen ChaCha blocks, measured at
/// 8.5 microseconds on the emulated x86-64 machine.
const RESEED_INTERVAL_NS: u64 = 1_000_000_000;
/// How many interrupts have to have been seen for a reseed to fold in
/// something new. At the 100 Hz tick this is passed several times a second, so
/// the interval above is what decides in practice; it is here to stop a reseed
/// that would fold in nothing on a machine sitting with interrupts masked.
const RESEED_EVENTS: u32 = 16;

static LAST_RESEED_NS: AtomicU64 = AtomicU64::new(0);

/// Give the generator a key drawn from the pool.
///
/// The key it had goes into the pool first, so a reseed cannot make things
/// worse: a pool that has learnt nothing new still leaves the generator no
/// weaker than the one it replaces.
fn reseed(now_ns: u64) {
    let fast = drain();
    let mut pool = POOL.lock();
    pool.mix_words(&fast);
    pool.mix_u64(crate::arch::cycle_counter());
    pool.mix_u64(now_ns);
    // The processor's generator again, where there is one, so a machine that
    // has one does not depend on the boot seed for the rest of its uptime.
    if let Some(word) = crate::arch::hardware_random() {
        pool.mix_u64(word);
    }

    let mut generator = GENERATOR.lock();
    pool.mix(&generator.key);
    generator.key = pool.extract();
    generator.seeded = true;
    LAST_RESEED_NS.store(now_ns, Ordering::Relaxed);
}

/// Whether enough has happened, and enough time has passed, to be worth a new
/// key. Two relaxed loads and a comparison on the path that decides not to.
fn reseed_if_due() {
    if EVENTS.load(Ordering::Relaxed) < RESEED_EVENTS {
        return;
    }
    let now = crate::time::monotonic_ns();
    if now.wrapping_sub(LAST_RESEED_NS.load(Ordering::Relaxed)) < RESEED_INTERVAL_NS {
        return;
    }
    reseed(now);
}

// ---- what the rest of the kernel calls ------------------------------------

pub fn fill(buf: &mut [u8]) {
    // A draw before the pool has been filled would come from a key of zeroes,
    // which is the same stream on every machine. Nothing in the boot sequence
    // asks for one, and this is what says so rather than producing it quietly.
    let seeded = GENERATOR.lock().seeded;
    if !seeded {
        crate::println!("random: asked for bytes before the generator was seeded");
        reseed(crate::time::monotonic_ns());
    }
    reseed_if_due();
    GENERATOR.lock().draw(buf);
}

pub fn next_u64() -> u64 {
    let mut bytes = [0u8; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

// ---- seeding at boot ------------------------------------------------------

/// How many words to take from a generator the processor has of its own.
const HARDWARE_WORDS: usize = 8;
/// How many timing samples the jitter loop takes. Over three hundred boots of
/// each emulated machine, sixty-three samples taken together were worth about
/// ten bits on x86-64 and fourteen on the Pi -- far less than the per-sample
/// figure times the count, because most samples repeat the last one. Taking
/// more of them buys little, so this is as many as fits in the time.
const JITTER_SAMPLES: usize = 128;

/// Time a fixed block of work against the cycle counter, over and over,
/// folding the results together the cheap way.
///
/// What varies is how the work lines up with the counter's own steps and
/// whatever else the machine did in between. It is the only source that needs
/// no device, so it is what a board with no generator of its own is left with.
fn jitter() -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut sink = 0u64;
    for i in 0..JITTER_SAMPLES {
        let begin = crate::arch::cycle_counter();
        for step in 0..64u64 {
            sink = sink.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(step);
            core::hint::black_box(sink);
        }
        let slot = &mut out[i & 3];
        *slot = stir(*slot ^ crate::arch::cycle_counter().wrapping_sub(begin));
    }
    core::hint::black_box(sink);
    out
}

/// Fill the pool, take the first key from it, and say on the console what the
/// seed rests on.
///
/// Called once, after the clock is running and before anything can ask for a
/// random number. The sources are gathered before the pool is locked, because
/// the lock masks interrupts and the gathering is the slow part.
pub fn init(boot: &crate::boot::BootInfo) {
    let entry = crate::arch::cycle_counter();

    // The processor's own generator, where there is one. It is mixed into the
    // pool rather than taken as the key: a generator built into a chip is a
    // single source whose workings cannot be inspected, and folding it in
    // leaves a machine that has one no worse off than one that does not if it
    // should turn out to be worth nothing.
    let mut hardware = [0u64; HARDWARE_WORDS];
    let mut hardware_words = 0;
    for slot in hardware.iter_mut() {
        match crate::arch::hardware_random() {
            Some(word) => {
                *slot = word;
                hardware_words += 1;
            }
            None => break,
        }
    }

    let timing = jitter();

    // The date, on a machine that keeps one. Two boots a second apart differ
    // here even if every other source repeats. A board with no clock reports
    // the epoch, and this is worth nothing there.
    let clock = crate::arch::read_wall_clock();
    let date = (clock.year as u64) << 40
        | (clock.month as u64) << 32
        | (clock.day as u64) << 24
        | (clock.hour as u64) << 16
        | (clock.minute as u64) << 8
        | clock.second as u64;

    // Whatever the interrupt handlers have seen since the machine started.
    // The clock has been ticking through the calibration loop, so there is
    // something here even though no process has run yet.
    let seen = drain();

    let id;
    {
        let mut pool = POOL.lock();
        pool.mix_words(&hardware[..hardware_words]);
        pool.mix_words(&seen);
        // When this boot happened, to whatever resolution the counter has, and
        // how long it has taken to get this far.
        pool.mix_u64(entry);
        pool.mix_u64(crate::trap::ticks());
        pool.mix_u64(crate::arch::cycle_counter());
        pool.mix_u64(date);
        pool.mix_words(&timing);
        // Not entropy: two machines of the same kind booted the same way get
        // the same bytes from it. It is here so that two configured
        // differently do not start from the same pool when every source above
        // has told them the same thing.
        pool.mix(boot.cmdline().as_bytes());

        let mut generator = GENERATOR.lock();
        generator.key = pool.extract();
        generator.seeded = true;
        id = pool.boot_id();
    }
    LAST_RESEED_NS.store(crate::time::monotonic_ns(), Ordering::Relaxed);

    // What the seed rests on, said out loud, because it is the difference
    // between a generator fit to make a key with and one that is not. The boot
    // id is a function of the pool that gives nothing about it away; it is
    // printed so that two boots producing one stream is something a test can
    // see rather than something nobody would notice.
    if hardware_words == 0 {
        crate::println!(
            "random: chacha20 seeded from boot timing alone -- this cpu has no generator, \
             boot id {:016x}",
            id
        );
    } else {
        crate::println!(
            "random: chacha20 seeded from boot timing and {} words from the cpu's own \
             generator, boot id {:016x}",
            hardware_words,
            id
        );
    }
}

