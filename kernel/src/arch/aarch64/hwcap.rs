//! What the processor can do, in the two words a program is told it in:
//! AT_HWCAP and AT_HWCAP2.
//!
//! The ID registers that describe the processor are readable only from EL1, so
//! a program cannot ask them itself. Linux answers such a read by emulating it
//! and says so with HWCAP_CPUID; this kernel does not, and a read from a
//! program is an undefined instruction here. The auxiliary vector is therefore
//! the whole of what a program learns, and a bit left clear is a feature the
//! program will not use.
//!
//! Each bit below is computed by the row of Linux's `arm64_elf_hwcaps` table
//! in arch/arm64/kernel/cpufeature.c that sets it, from the field positions in
//! arch/arm64/tools/sysreg, and has the number arch/arm64/include/uapi/asm/
//! hwcap.h gives it. A bit is only set when this kernel lets a program use
//! what it describes. Left out, with the reason:
//!
//! - HWCAP_EVTSTRM: the timer event stream is never turned on.
//! - HWCAP_CPUID: reads of the ID registers from EL0 are not emulated.
//! - HWCAP_DCPOP, HWCAP2_DCPODP: `dc cvap` and `dc cvadp` from EL0 trap unless
//!   SCTLR_EL1.UCI is set. `boot.s` sets only M, C and I in that register and
//!   leaves the rest as the firmware did, and the trap is sent as SIGILL.
//! - HWCAP2_WFXT: `wfet` and `wfit` from EL0 trap unless SCTLR_EL1.nTWE and
//!   nTWI are set, which the kernel does not do either, and nothing emulates
//!   the trapped instruction as Linux's `wfi_handler` does.
//! - HWCAP2_ECV: the counter registers it adds are read from EL0 only when
//!   CNTKCTL_EL1 allows it, and nothing here sets that register.
//! - HWCAP_DIT, HWCAP_SSBS: both are PSTATE bits a program sets. `rt_sigreturn`
//!   here keeps only the condition flags of the saved PSTATE, so returning
//!   from any handler clears them; Linux's `valid_user_regs` keeps both.
//! - HWCAP2_AFP, HWCAP2_RPRES: AFP is FPCR's AH, FIZ and NEP bits, and
//!   `rt_sigreturn` here keeps only the rounding mode and exception masks of
//!   FPCR, so a handler's return clears them. RPRES describes results under
//!   FPCR.AH.
//! - HWCAP2_MOPS: the instructions trap unless SCTLR_EL1.MSCEn is set, and the
//!   exception one of them raises when it is resumed in a state it did not
//!   stop in is handled in Linux by `do_el0_mops`, which has no counterpart
//!   here.
//! - HWCAP2_FPMR and the six HWCAP2_F8 bits: FP8 is controlled through FPMR,
//!   which needs SCTLR_EL1.EnFPM and a context switch that carries it.
//! - SVE and SME, every bit of either: both need their enable bits in
//!   CPACR_EL1 and state larger than the vector registers a switch carries.
//! - HWCAP_PACA, HWCAP_PACG: pointer authentication needs keys installed per
//!   process and the enable bits in SCTLR_EL1.
//! - HWCAP2_BTI: needs the guarded-page bit in the page tables and SCTLR_EL1.BT0.
//! - HWCAP2_MTE, HWCAP2_MTE3: need tagged memory and the tag check controls.
//! - HWCAP2_POE: needs POR_EL0 carried across a context switch.

use core::arch::asm;

const HWCAP_FP: u64 = 1 << 0;
const HWCAP_ASIMD: u64 = 1 << 1;
const HWCAP_AES: u64 = 1 << 3;
const HWCAP_PMULL: u64 = 1 << 4;
const HWCAP_SHA1: u64 = 1 << 5;
const HWCAP_SHA2: u64 = 1 << 6;
const HWCAP_CRC32: u64 = 1 << 7;
const HWCAP_ATOMICS: u64 = 1 << 8;
const HWCAP_FPHP: u64 = 1 << 9;
const HWCAP_ASIMDHP: u64 = 1 << 10;
const HWCAP_ASIMDRDM: u64 = 1 << 12;
const HWCAP_JSCVT: u64 = 1 << 13;
const HWCAP_FCMA: u64 = 1 << 14;
const HWCAP_LRCPC: u64 = 1 << 15;
const HWCAP_SHA3: u64 = 1 << 17;
const HWCAP_SM3: u64 = 1 << 18;
const HWCAP_SM4: u64 = 1 << 19;
const HWCAP_ASIMDDP: u64 = 1 << 20;
const HWCAP_SHA512: u64 = 1 << 21;
const HWCAP_ASIMDFHM: u64 = 1 << 23;
const HWCAP_USCAT: u64 = 1 << 25;
const HWCAP_ILRCPC: u64 = 1 << 26;
const HWCAP_FLAGM: u64 = 1 << 27;
const HWCAP_SB: u64 = 1 << 29;

const HWCAP2_FLAGM2: u64 = 1 << 7;
const HWCAP2_FRINT: u64 = 1 << 8;
const HWCAP2_I8MM: u64 = 1 << 13;
const HWCAP2_BF16: u64 = 1 << 14;
const HWCAP2_DGH: u64 = 1 << 15;
const HWCAP2_RNG: u64 = 1 << 16;
const HWCAP2_EBF16: u64 = 1 << 32;
const HWCAP2_CSSC: u64 = 1 << 34;
const HWCAP2_RPRFM: u64 = 1 << 35;
const HWCAP2_HBC: u64 = 1 << 44;
const HWCAP2_LRCPC3: u64 = 1 << 46;
const HWCAP2_LSE128: u64 = 1 << 47;
const HWCAP2_LUT: u64 = 1 << 49;
const HWCAP2_FAMINMAX: u64 = 1 << 50;

/// The ID registers the rows below read.
#[derive(Clone, Copy)]
enum Register {
    Pfr0,
    Isar0,
    Isar1,
    Isar2,
    Isar3,
    Mmfr2,
}

/// Which of the two words a row sets its bit in.
#[derive(Clone, Copy)]
enum Word {
    Hwcap,
    Hwcap2,
}

/// One row of `arm64_elf_hwcaps`: the bit is set when the four-bit field at
/// `shift` holds at least `min`.
///
/// Most fields are unsigned. A signed one reads 0b1111 as -1, which is how
/// the floating point and Advanced SIMD fields say "not implemented", and
/// `ARM64_CPUID_FIELDS` limits a signed match to the positive half, so such a
/// field matches from `min` to 7. That is `feature_matches`.
#[derive(Clone, Copy)]
struct Rule {
    register: Register,
    shift: u32,
    signed: bool,
    min: i64,
    word: Word,
    bit: u64,
}

const fn unsigned(register: Register, shift: u32, min: i64, word: Word, bit: u64) -> Rule {
    Rule { register, shift, signed: false, min, word, bit }
}

const fn signed(register: Register, shift: u32, min: i64, word: Word, bit: u64) -> Rule {
    Rule { register, shift, signed: true, min, word, bit }
}

use Register::*;
use Word::*;

/// The rows, in the order `arm64_elf_hwcaps` lists them, with the field and
/// the value it names as a comment.
const RULES: &[Rule] = &[
    unsigned(Isar0, 4, 2, Hwcap, HWCAP_PMULL),      // AES, PMULL
    unsigned(Isar0, 4, 1, Hwcap, HWCAP_AES),        // AES, AES
    unsigned(Isar0, 8, 1, Hwcap, HWCAP_SHA1),       // SHA1, IMP
    unsigned(Isar0, 12, 1, Hwcap, HWCAP_SHA2),      // SHA2, SHA256
    unsigned(Isar0, 12, 2, Hwcap, HWCAP_SHA512),    // SHA2, SHA512
    unsigned(Isar0, 16, 1, Hwcap, HWCAP_CRC32),     // CRC32, IMP
    unsigned(Isar0, 20, 2, Hwcap, HWCAP_ATOMICS),   // ATOMIC, IMP
    unsigned(Isar0, 20, 3, Hwcap2, HWCAP2_LSE128),  // ATOMIC, FEAT_LSE128
    unsigned(Isar0, 28, 1, Hwcap, HWCAP_ASIMDRDM),  // RDM, IMP
    unsigned(Isar0, 32, 1, Hwcap, HWCAP_SHA3),      // SHA3, IMP
    unsigned(Isar0, 36, 1, Hwcap, HWCAP_SM3),       // SM3, IMP
    unsigned(Isar0, 40, 1, Hwcap, HWCAP_SM4),       // SM4, IMP
    unsigned(Isar0, 44, 1, Hwcap, HWCAP_ASIMDDP),   // DP, IMP
    unsigned(Isar0, 48, 1, Hwcap, HWCAP_ASIMDFHM),  // FHM, IMP
    unsigned(Isar0, 52, 1, Hwcap, HWCAP_FLAGM),     // TS, FLAGM
    unsigned(Isar0, 52, 2, Hwcap2, HWCAP2_FLAGM2),  // TS, FLAGM2
    unsigned(Isar0, 60, 1, Hwcap2, HWCAP2_RNG),     // RNDR, IMP
    signed(Pfr0, 16, 0, Hwcap, HWCAP_FP),           // FP, IMP
    signed(Pfr0, 16, 1, Hwcap, HWCAP_FPHP),         // FP, FP16
    signed(Pfr0, 20, 0, Hwcap, HWCAP_ASIMD),        // AdvSIMD, IMP
    signed(Pfr0, 20, 1, Hwcap, HWCAP_ASIMDHP),      // AdvSIMD, FP16
    unsigned(Isar1, 12, 1, Hwcap, HWCAP_JSCVT),     // JSCVT, IMP
    unsigned(Isar1, 16, 1, Hwcap, HWCAP_FCMA),      // FCMA, IMP
    unsigned(Isar1, 20, 1, Hwcap, HWCAP_LRCPC),     // LRCPC, IMP
    unsigned(Isar1, 20, 2, Hwcap, HWCAP_ILRCPC),    // LRCPC, LRCPC2
    unsigned(Isar1, 20, 3, Hwcap2, HWCAP2_LRCPC3),  // LRCPC, LRCPC3
    unsigned(Isar1, 32, 1, Hwcap2, HWCAP2_FRINT),   // FRINTTS, IMP
    unsigned(Isar1, 36, 1, Hwcap, HWCAP_SB),        // SB, IMP
    unsigned(Isar1, 44, 1, Hwcap2, HWCAP2_BF16),    // BF16, IMP
    unsigned(Isar1, 44, 2, Hwcap2, HWCAP2_EBF16),   // BF16, EBF16
    unsigned(Isar1, 48, 1, Hwcap2, HWCAP2_DGH),     // DGH, IMP
    unsigned(Isar1, 52, 1, Hwcap2, HWCAP2_I8MM),    // I8MM, IMP
    unsigned(Isar2, 56, 1, Hwcap2, HWCAP2_LUT),     // LUT, IMP
    unsigned(Isar3, 4, 1, Hwcap2, HWCAP2_FAMINMAX), // FAMINMAX, IMP
    unsigned(Mmfr2, 32, 1, Hwcap, HWCAP_USCAT),     // AT, IMP
    unsigned(Isar2, 52, 1, Hwcap2, HWCAP2_CSSC),    // CSSC, IMP
    unsigned(Isar2, 48, 1, Hwcap2, HWCAP2_RPRFM),   // RPRFM, IMP
    unsigned(Isar2, 20, 1, Hwcap2, HWCAP2_HBC),     // BC, IMP
];

/// The registers, read once.
struct IdRegisters {
    pfr0: u64,
    isar0: u64,
    isar1: u64,
    isar2: u64,
    isar3: u64,
    mmfr2: u64,
}

impl IdRegisters {
    /// Read them on the processor this runs on, which is the only one this
    /// kernel runs programs on: the others are parked at boot.
    ///
    /// The three newer registers are spelled by their encodings because the
    /// assembler only knows their names with features turned on that this
    /// kernel is not built with. A processor older than a register reads it as
    /// zero rather than faulting: every unallocated encoding in this block of
    /// the ID register space is RAZ, which is why Linux reads all of them on
    /// every processor.
    fn read() -> IdRegisters {
        let (pfr0, isar0, isar1, isar2, isar3, mmfr2): (u64, u64, u64, u64, u64, u64);
        unsafe {
            asm!(
                "mrs {pfr0}, id_aa64pfr0_el1",
                "mrs {isar0}, id_aa64isar0_el1",
                "mrs {isar1}, id_aa64isar1_el1",
                "mrs {isar2}, s3_0_c0_c6_2",
                "mrs {isar3}, s3_0_c0_c6_3",
                "mrs {mmfr2}, s3_0_c0_c7_2",
                pfr0 = out(reg) pfr0,
                isar0 = out(reg) isar0,
                isar1 = out(reg) isar1,
                isar2 = out(reg) isar2,
                isar3 = out(reg) isar3,
                mmfr2 = out(reg) mmfr2,
                options(nomem, nostack, preserves_flags),
            );
        }
        IdRegisters { pfr0, isar0, isar1, isar2, isar3, mmfr2 }
    }

    fn get(&self, register: Register) -> u64 {
        match register {
            Pfr0 => self.pfr0,
            Isar0 => self.isar0,
            Isar1 => self.isar1,
            Isar2 => self.isar2,
            Isar3 => self.isar3,
            Mmfr2 => self.mmfr2,
        }
    }
}

impl Rule {
    fn matches(&self, registers: &IdRegisters) -> bool {
        let field = (registers.get(self.register) >> self.shift) & 0xF;
        if self.signed {
            // Sign-extend the four bits, as `cpuid_feature_extract_signed_field`
            // does, and stop at the largest positive value.
            let value = ((field << 60) as i64) >> 60;
            value >= self.min && value <= 7
        } else {
            field as i64 >= self.min
        }
    }
}

/// AT_HWCAP and AT_HWCAP2 for a program on this processor.
pub fn elf_hwcaps() -> (u64, u64) {
    let registers = IdRegisters::read();
    let (mut hwcap, mut hwcap2) = (0u64, 0u64);
    for rule in RULES {
        if rule.matches(&registers) {
            match rule.word {
                Hwcap => hwcap |= rule.bit,
                Hwcap2 => hwcap2 |= rule.bit,
            }
        }
    }
    (hwcap, hwcap2)
}
