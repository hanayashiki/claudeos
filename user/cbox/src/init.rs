//! pid 1: bring the system up, start the services and a shell, and reap
//! orphans.

use crate::services;
use crate::sys;

pub fn main(args: &[String]) -> i32 {
    println!();
    println!("  claudeos");
    println!("  a Rust operating system that runs Linux binaries");
    println!();

    for dir in ["/tmp", "/root", "/var", "/var/log", "/run", "/usr", "/usr/bin"] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::env::set_current_dir("/root");

    // The services the two lists name, /etc/claudeos/services and then
    // /data/services.txt. This returns once the starter has printed its
    // summary, or after at most ten seconds without one, so the shell below
    // starts whatever the lists hold.
    services::start();

    // An argument from the kernel command line names a script to run instead
    // of a shell.
    let script: Option<&String> = args.iter().skip(1).find(|a| !a.starts_with('-'));

    // What a shell ending on its own does. On a board the machine is reached
    // only through this shell, over the serial cable or the telnet console, and
    // a board that powered off stays off until someone unplugs it, so the
    // default is to start a new shell. `shell_exit=poweroff` on the kernel
    // command line, which reaches this process as its environment, keeps the
    // older behaviour for an emulator, where leaving the shell is how a session
    // ends.
    let power_off_on_exit = std::env::var("shell_exit").map(|v| v == "poweroff").unwrap_or(false);

    // Shells that died on a signal in a row, each soon after it started.
    let mut deaths = 0;
    loop {
        let started = std::time::Instant::now();
        let child = sys::fork();
        if child == 0 {
            // New process group, and make it the terminal's foreground group.
            sys::setpgid(0, 0);
            sys::set_foreground_group(sys::getpid() as i32);
            let argv: Vec<String> = match script {
                Some(path) => vec!["/bin/sh".into(), path.clone()],
                None => vec!["/bin/sh".into()],
            };
            let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
            sys::execve("/bin/sh", &argv, &envp);
            eprintln!("init: cannot start /bin/sh");
            sys::exit_group(1);
        }
        if child < 0 {
            eprintln!("init: fork failed");
            return 1;
        }

        // Reap everything; stop when the shell itself is gone. The services'
        // keepers are children too once their starter has exited, and a
        // keeper ends on its own only for a `once` service, so one is reaped
        // here when it does; the services they run are their own children and
        // never reach this loop.
        loop {
            let (pid, status) = sys::wait4(-1, 0);
            if pid < 0 {
                break;
            }
            if pid == child {
                let code = sys::exit_code_of(status);
                if script.is_some() {
                    println!("init: script finished with status {}", code);
                    return code;
                }
                match sys::signal_of(status) {
                    // A shell that exited on its own is the user leaving,
                    // whatever status it reports; `exit` after a failed
                    // command still is.
                    None if power_off_on_exit => {
                        println!("init: session ended");
                        return 0;
                    }
                    None => {
                        println!("init: shell exited; starting a new one");
                        deaths = 0;
                    }
                    Some(signal) => {
                        // A shell that ran a while before it died is one
                        // death, not the start of a loop.
                        if started.elapsed() > std::time::Duration::from_secs(10) {
                            deaths = 0;
                        }
                        deaths += 1;
                        if deaths > 3 {
                            if power_off_on_exit {
                                println!("init: shell keeps dying (signal {}); giving up", signal);
                                return code;
                            }
                            // With no shell there is no way to type `reboot`,
                            // and powering off would leave the board off, so
                            // restart the machine. Booted over the network,
                            // that also fetches whatever build is served now.
                            println!(
                                "init: shell keeps dying (signal {}); restarting the machine",
                                signal
                            );
                            sys::reboot(sys::REBOOT_MAGIC1, sys::REBOOT_MAGIC2, sys::REBOOT_CMD_RESTART);
                            eprintln!("init: the kernel refused to restart; starting a new shell");
                            deaths = 0;
                        } else {
                            println!("init: shell died on signal {}; restarting", signal);
                        }
                    }
                }
                break;
            }
        }
    }
}
