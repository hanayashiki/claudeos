//! Checks on the random number generator.
//!
//! The ones that matter are the first three: the block function against the
//! vectors published with RFC 8439. A generator can pass every statistical
//! test there is while being a permutation anybody can invert, so what is
//! worth asserting is that this is the cipher it claims to be, not that its
//! output looks shapeless. The vectors are the only check here that would
//! notice a round count of nineteen or a rotation of the wrong distance.
//!
//! Next come the checks on how the generator uses it: that a request leaves
//! behind a key that is not the bytes it handed out, which is what stops
//! reading the output from giving up the state.
//!
//! The frequency checks at the end are the weakest thing here and are kept
//! because a generator that has stopped producing anything would show up in
//! them. Passing them says nothing about whether the output can be predicted.
//!
//! Run with `net=test` on the kernel command line, and counted into that
//! summary because the harness reads one line per boot.

use super::{block, Generator, Pool, BLOCK_LEN, KEY_LEN};

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
}

pub fn run(report: &mut Report) {
    published_vectors(report);
    key_erasure(report);
    the_pool(report);
    the_shape_of_the_output(report);
}

/// RFC 8439's own test vectors for the block function.
///
/// The first two are appendix A.1 vectors 1 and 2: the same all-zero key and
/// nonce at block counts 0 and 1, which together say the counter reaches the
/// state. The third is section 2.3.2, whose key, nonce and counter are all
/// different from each other, so a word put in the wrong place fails it.
fn published_vectors(report: &mut Report) {
    let mut out = [0u8; BLOCK_LEN];

    block(&[0; KEY_LEN], 0, &[0; 12], &mut out);
    report.check(
        "rfc 8439 a.1 vector 1: a zero key at block zero",
        out == [
            0x76, 0xb8, 0xe0, 0xad, 0xa0, 0xf1, 0x3d, 0x90, 0x40, 0x5d, 0x6a, 0xe5, 0x53, 0x86,
            0xbd, 0x28, 0xbd, 0xd2, 0x19, 0xb8, 0xa0, 0x8d, 0xed, 0x1a, 0xa8, 0x36, 0xef, 0xcc,
            0x8b, 0x77, 0x0d, 0xc7, 0xda, 0x41, 0x59, 0x7c, 0x51, 0x57, 0x48, 0x8d, 0x77, 0x24,
            0xe0, 0x3f, 0xb8, 0xd8, 0x4a, 0x37, 0x6a, 0x43, 0xb8, 0xf4, 0x15, 0x18, 0xa1, 0x1c,
            0xc3, 0x87, 0xb6, 0x69, 0xb2, 0xee, 0x65, 0x86,
        ],
    );

    block(&[0; KEY_LEN], 1, &[0; 12], &mut out);
    report.check(
        "rfc 8439 a.1 vector 2: the same key at block one",
        out == [
            0x9f, 0x07, 0xe7, 0xbe, 0x55, 0x51, 0x38, 0x7a, 0x98, 0xba, 0x97, 0x7c, 0x73, 0x2d,
            0x08, 0x0d, 0xcb, 0x0f, 0x29, 0xa0, 0x48, 0xe3, 0x65, 0x69, 0x12, 0xc6, 0x53, 0x3e,
            0x32, 0xee, 0x7a, 0xed, 0x29, 0xb7, 0x21, 0x76, 0x9c, 0xe6, 0x4e, 0x43, 0xd5, 0x71,
            0x33, 0xb0, 0x74, 0xd8, 0x39, 0xd5, 0x31, 0xed, 0x1f, 0x28, 0x51, 0x0a, 0xfb, 0x45,
            0xac, 0xe1, 0x0a, 0x1f, 0x4b, 0x79, 0x4d, 0x6f,
        ],
    );

    let mut key = [0u8; KEY_LEN];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let nonce = [0, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    block(&key, 1, &nonce, &mut out);
    report.check(
        "rfc 8439 section 2.3.2: a key, a nonce and a counter that all differ",
        out == [
            0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20,
            0x71, 0xc4, 0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a,
            0xc3, 0xd4, 0x6c, 0x4e, 0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2,
            0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2, 0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9,
            0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
        ],
    );
}

/// What a request does to the key behind it.
///
/// This is where "the output does not give away the state" is actually
/// asserted: the bytes handed out are the half of the block that does not
/// become the next key, and the key the request was answered from is gone.
/// Someone holding the output has thirty-two bytes of keystream and would have
/// to invert ChaCha20 to get either key back.
fn key_erasure(report: &mut Report) {
    let mut key = [0u8; KEY_LEN];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(7).wrapping_add(3);
    }
    let mut expected = [0u8; BLOCK_LEN];
    block(&key, 0, &[0; 12], &mut expected);

    let mut generator = Generator { key, seeded: true };
    let mut out = [0u8; KEY_LEN];
    generator.draw(&mut out);

    report.check("what a request hands out is the second half of the block", out == expected[KEY_LEN..]);
    report.check("the key left behind is the first half", generator.key == expected[..KEY_LEN]);
    report.check("which is not the half that was handed out", generator.key[..] != out[..]);
    report.check("and the key it was answered from is gone", generator.key != key);

    // A second request under the erased key produces something else again, so
    // holding the first answer says nothing about the second.
    let mut again = [0u8; KEY_LEN];
    generator.draw(&mut again);
    report.check("the next request answers with different bytes", again != out);
}

/// The pool's two properties: folding something in moves it, and it never
/// hands the same bytes out twice.
fn the_pool(report: &mut Report) {
    let mut pool = Pool { value: [0; KEY_LEN], step: 0 };
    let before = pool.value;
    pool.mix_u64(0x0123_4567_89ab_cdef);
    report.check("folding an observation in moves the pool", pool.value != before);

    let first = pool.value;
    pool.mix_u64(0x0123_4567_89ab_cdef);
    report.check("folding the same one in again moves it somewhere else", pool.value != first);

    let a = pool.extract();
    let b = pool.extract();
    report.check("two extractions from one pool differ", a != b);
    report.check("and neither is the pool itself", a != pool.value && b != pool.value);

    // Two pools that were told different things must not agree. Without the
    // observation reaching the state this would pass anyway, so it is worth
    // little on its own; it is here because the opposite result would mean the
    // mixing step had dropped its input.
    let mut left = Pool { value: [0; KEY_LEN], step: 0 };
    let mut right = Pool { value: [0; KEY_LEN], step: 0 };
    left.mix_u64(1);
    right.mix_u64(2);
    report.check("two pools told different things do not agree", left.value != right.value);
}

/// Frequency checks on a real draw. See the note at the top of this file for
/// what they are and are not worth.
fn the_shape_of_the_output(report: &mut Report) {
    const BYTES: usize = 64 * 1024;
    let mut sample = alloc::vec![0u8; BYTES];
    super::fill(&mut sample);

    let bits = BYTES * 8;
    let set: usize = sample.iter().map(|byte| byte.count_ones() as usize).sum();
    // The standard deviation of the count is sqrt(bits)/2, which is 362 here.
    // Five of those is a band a fair generator falls outside of about once in
    // two million runs.
    let deviation = set.abs_diff(bits / 2);
    report.check("bits are set about half the time", deviation < 5 * 362);

    // Chi-squared over the byte values, in whole numbers: 256 buckets holding
    // 256 each on average, so 255 degrees of freedom, mean 255 and standard
    // deviation 22.6. The bounds are five of those either way, and the lower
    // one is what would catch a generator producing every value exactly as
    // often as every other, which is not what a random one does.
    let mut counts = [0u32; 256];
    for byte in sample.iter() {
        counts[*byte as usize] += 1;
    }
    let expected = (BYTES / 256) as i64;
    let spread: i64 =
        counts.iter().map(|c| (*c as i64 - expected).pow(2)).sum::<i64>() / expected;
    report.check("byte values are spread the way chance spreads them", (142..=368).contains(&spread));

    // Two draws in a row must differ. A generator that had stopped advancing
    // would pass both checks above and fail this one.
    let mut first = [0u8; 32];
    let mut second = [0u8; 32];
    super::fill(&mut first);
    super::fill(&mut second);
    report.check("two draws in a row differ", first != second);
}
