//! Stopping and restarting the machine: the `reboot` system call and the
//! machine's watchdog.
//!
//! One word on the command line is read here. `watchdog=off` leaves the
//! machine's watchdog stopped, which is what a processor held still in a
//! debugger needs.

use crate::abi::*;
use crate::arch;

/// Read `watchdog=` off the command line, and start the watchdog unless the
/// line says not to.
///
/// Called first thing in boot, before there is a heap. That is why this reads
/// the line itself rather than taking the result of `parse_cmdline`: the
/// earlier the watchdog starts, the more of boot it covers.
pub fn configure(cmdline: &str) {
    let mut watchdog = true;
    for word in cmdline.split_whitespace() {
        // Every word after this one is init's.
        if word == "--" {
            break;
        }
        if word == "watchdog=off" {
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
            println!("claudeos: restarting");
            arch::restart()
        }
        LINUX_REBOOT_CMD_HALT => {
            println!("claudeos: halting");
            arch::power_off()
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            println!("claudeos: powering off");
            arch::power_off()
        }
        // Whether Ctrl-Alt-Del restarts the machine. Nothing restarts it on a
        // key here in either setting, so there is nothing to change.
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}
