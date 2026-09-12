//! pid 1: bring the system up, start a shell, and reap orphans.

use crate::sys;

pub fn main(args: &[String]) -> i32 {
    println!();
    println!("  claudeos");
    println!("  a Rust operating system that runs Linux binaries");
    println!();

    for dir in ["/tmp", "/root", "/var", "/var/log", "/usr", "/usr/bin"] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::env::set_current_dir("/root");

    // Anything after "--" on the kernel command line is run instead of a shell.
    let script: Option<&String> = args.iter().skip(1).find(|a| !a.starts_with('-'));

    let mut restarts = 0;
    loop {
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

        // Reap everything; stop when the shell itself is gone.
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
                    // A shell that exited on its own ends the session, whatever
                    // status it reports; `exit` after a failed command is still
                    // the user asking to leave.
                    None => {
                        println!("init: session ended");
                        return 0;
                    }
                    Some(signal) => {
                        restarts += 1;
                        if restarts > 3 {
                            println!("init: shell keeps dying (signal {}); giving up", signal);
                            return code;
                        }
                        println!("init: shell died on signal {}; restarting", signal);
                    }
                }
                break;
            }
        }
    }
}
