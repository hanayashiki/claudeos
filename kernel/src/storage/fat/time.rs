//! FAT timestamps.
//!
//! A directory entry holds a date and a time with no time zone (fatgen103,
//! "FAT Directory Structure", DIR_WrtDate and DIR_WrtTime, and "Date and Time
//! Formats"). The board has no time zone either, so a timestamp here is the
//! kernel's clock taken as UTC. Linux does the same when mounted with `tz=UTC`,
//! and macOS shows such a stamp shifted by the Mac's own offset from UTC.
//!
//! The date is a 16-bit field: years since 1980 in bits 15 to 9, the month in
//! 8 to 5, the day in 4 to 0. The time is: hours in bits 15 to 11, minutes in
//! 10 to 5, and seconds divided by two in 4 to 0. So a stamp covers 1980-01-01
//! to 2107-12-31 in two-second steps.

/// 1980-01-01 00:00:00 UTC and 2107-12-31 23:59:58 UTC as seconds since 1970:
/// the first and last moments a FAT stamp can hold.
pub const FAT_EPOCH: i64 = 315_532_800;
pub const FAT_END: i64 = 4_354_819_198;

/// A date, a time and the creation time's hundredths, as they are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub date: u16,
    pub time: u16,
    /// DIR_CrtTimeTenth: despite its name, hundredths of a second, 0 to 199,
    /// which here only ever carry the odd second the time field drops.
    pub hundredths: u8,
}

/// Howard Hinnant's `civil_from_days`: the Gregorian date `days` after
/// 1970-01-01. Only called with days inside FAT's range, so none of the
/// arithmetic can leave i64 or u64.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// The inverse, `days_from_civil`, for a year from 1980 to 2107, a month from
/// 1 to 12 and a day from 1 to 31.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// The stamp for `unix` seconds since 1970, held to the range FAT has.
pub fn encode(unix: i64) -> Stamp {
    let unix = unix.clamp(FAT_EPOCH, FAT_END);
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    let seconds = unix.rem_euclid(86_400);
    // Year is 1980..=2107, month 1..=12, day 1..=31, seconds 0..86400: every
    // field fits its bits.
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((seconds / 3600) as u16) << 11) | (((seconds / 60 % 60) as u16) << 5) | ((seconds % 60 / 2) as u16);
    let hundredths = if seconds % 2 == 1 { 100 } else { 0 };
    Stamp { date, time, hundredths }
}

/// Seconds since 1970 for a date and time read off the card. The fields are
/// whatever the entry holds, so a month of 13 or a day of 0 is possible, and
/// anything out of range reads as the start of FAT's epoch.
pub fn decode(date: u16, time: u16) -> i64 {
    let year = 1980 + (date >> 9) as i64;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = (time >> 11) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    let second = ((time & 0x1F) * 2) as i64;
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 59 {
        return FAT_EPOCH;
    }
    days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
}
