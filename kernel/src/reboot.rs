//! Stopping and restarting the machine: the `reboot` system call, the
//! machine's watchdog, and what a kernel panic ends in.
//!
//! Two words on the command line are read here. `panic=N` restarts the
//! machine N seconds after a panic, at once when N is negative, and never when
//! it is zero, which leaves the machine stopped with the message on the
//! console. `watchdog=off` leaves the machine's watchdog stopped, which is
//! what a processor held still in a debugger needs.

use crate::abi::*;
use crate::arch;
use core::sync::atomic::{AtomicI64, Ordering};

/// Seconds from a panic to the restart, as `panic=` sets it.
///
/// Ten unless the command line says otherwise, which is the value Linux
/// systems commonly boot with. On the board it means a panic is recovered from
/// without anyone at the power supply. Under QEMU, which the scripts run with
/// `-no-reboot`, the restart ends the run, so a suite whose kernel panics is
/// reported ten seconds later rather than when its timeout runs out.
static PANIC_RESTART_SECONDS: AtomicI64 = AtomicI64::new(10);

/// Read `panic=` and `watchdog=` off the command line, and start the watchdog
/// unless the line says not to.
///
/// Called first thing in boot, before there is a heap. That is why this reads
/// the line itself rather than taking the result of `parse_cmdline`: the
/// earlier the watchdog starts, the more of boot it covers, and a panic in
/// memory set-up is handled the way the line asked.
pub fn configure(cmdline: &str) {
    let mut watchdog = true;
    for word in cmdline.split_whitespace() {
        // Every word after this one is init's.
        if word == "--" {
            break;
        }
        if let Some(value) = word.strip_prefix("panic=") {
            if let Ok(seconds) = value.parse::<i64>() {
                PANIC_RESTART_SECONDS.store(seconds, Ordering::Relaxed);
            }
        } else if word == "watchdog=off" {
            watchdog = false;
        }
    }

    if !watchdog {
        // Stopped rather than only not started, in case whatever ran before
        // this kernel left it running.
        arch::watchdog_stop();
        println!("watchdog: off");
    } else if let Some(seconds) = arch::watchdog_start() {
        println!("watchdog: resets the machine after {} s without a timer interrupt", seconds);
    } else {
        println!("watchdog: none found");
    }
    let seconds = PANIC_RESTART_SECONDS.load(Ordering::Relaxed);
    if seconds > 0 {
        println!("panic: restart after {} s", seconds);
    } else if seconds < 0 {
        println!("panic: restart at once");
    } else {
        println!("panic: stay stopped");
    }
}

/// `reboot`: restart or stop the machine.
///
/// The magic numbers are checked before the command, and a call with the wrong
/// ones fails whatever it asks for, which is what makes a stray call with
/// whatever happened to be in the registers unable to stop anything. Linux
/// checks for CAP_SYS_BOOT before either, and every process here is root.
///
/// Halt stops the machine the same way power off does. Linux halts the
/// processor and leaves the power on, but the board has one way to stop, the
/// halt partition, and nothing on QEMU's x86 machine could tell a program the
/// difference. RESTART2, which carries a string for the firmware, KEXEC and
/// SW_SUSPEND have nothing behind them here and get EINVAL, which is Linux's
/// answer to a command it does not know.
pub fn reboot(magic1: u32, magic2: u32, command: u32) -> SysResult {
    let magic2_known = matches!(
        magic2,
        LINUX_REBOOT_MAGIC2 | LINUX_REBOOT_MAGIC2A | LINUX_REBOOT_MAGIC2B | LINUX_REBOOT_MAGIC2C
    );
    if magic1 != LINUX_REBOOT_MAGIC1 || !magic2_known {
        return Err(Errno::EINVAL);
    }
    match command {
        LINUX_REBOOT_CMD_RESTART => {
            sync_data();
            println!("claudeos: restarting");
            arch::restart()
        }
        LINUX_REBOOT_CMD_HALT => {
            sync_data();
            println!("claudeos: halting");
            arch::power_off()
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            sync_data();
            println!("claudeos: powering off");
            arch::power_off()
        }
        // Whether Ctrl-Alt-Del restarts the machine. Nothing restarts it on a
        // key here in either setting, so there is nothing to change.
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// Before the machine stops on request: the data volume unmounted, which
/// writes FSInfo and marks the volume dismounted cleanly, so the card reads as
/// unmounted properly wherever it is put next. Every write is on the card
/// already. The wait is bounded (`storage::unmount`), so a card that stops
/// answering leaves the volume marked in use and does not keep the machine
/// from stopping. The panic path does not come here.
fn sync_data() {
    if let Err(why) = crate::fs::data::unmount() {
        println!("data: {}", why);
    }
}

/// What a panic ends in, once its message has been printed.
pub fn after_panic() -> ! {
    let seconds = PANIC_RESTART_SECONDS.load(Ordering::Relaxed);
    if seconds == 0 {
        // The watchdog is no longer fed, and left running it would restart
        // the machine that `panic=0` asked to stay stopped.
        arch::watchdog_stop();
        println!("claudeos: stopped; panic=0 leaves the machine this way");
        crate::serial::drain();
        loop {
            arch::halt();
        }
    }
    if seconds > 0 {
        println!("claudeos: restarting in {} seconds", seconds);
        // Out on the wire before the wait, in case the wait never ends and
        // the watchdog is what restarts the machine.
        crate::serial::drain();
        wait_seconds(seconds as u64);
    }
    println!("claudeos: restarting");
    arch::restart()
}

/// Spin for `seconds`, measured on the cycle counter, and feed the watchdog
/// each time one of them has passed.
///
/// Interrupts are masked on the panic path, so the tick has stopped and cannot
/// be what measures this; the counter runs whatever the interrupt mask is.
///
/// The watchdog is fed so that the restart at the end is this code's rather
/// than the watchdog's, which matters for a wait longer than the watchdog's
/// own time, and for a panic that came late in a long stretch with interrupts
/// masked. It is fed once per second the counter says has passed rather than
/// on every turn of the loop, so a counter that has stopped starves the
/// watchdog, and the watchdog then restarts the machine this wait would have.
///
/// A machine whose counter has no known rate waits for nothing.
fn wait_seconds(seconds: u64) {
    let per_second = crate::time::counter_rate();
    if per_second == 0 {
        return;
    }
    arch::watchdog_feed();
    let start = arch::cycle_counter();
    for elapsed in 1..=seconds {
        let due = per_second.saturating_mul(elapsed);
        while arch::cycle_counter().wrapping_sub(start) < due {
            core::hint::spin_loop();
        }
        arch::watchdog_feed();
    }
}
