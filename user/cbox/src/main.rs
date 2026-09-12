//! A multicall binary: the program it runs is chosen by the name it is
//! invoked under, the way busybox does it.

mod cmds;
mod edit;
mod regex;
mod init;
mod shell;
mod sys;

use std::process::exit;

/// Every applet, with a one-line description used by `help`.
pub const APPLETS: &[(&str, &str)] = &[
    ("basename", "strip directory from a path"),
    ("cat", "concatenate files to standard output"),
    ("cbox", "list the applets in this binary"),
    ("cd", "(shell builtin) change directory"),
    ("chmod", "change file permission bits"),
    ("clear", "clear the terminal"),
    ("cp", "copy files"),
    ("cut", "select fields from each line"),
    ("date", "print the current time"),
    ("df", "report filesystem space"),
    ("dirname", "strip the last component from a path"),
    ("du", "summarise disk usage"),
    ("echo", "write arguments to standard output"),
    ("env", "print the environment"),
    ("expr", "evaluate an integer expression"),
    ("false", "exit with a failure status"),
    ("find", "walk a directory tree"),
    ("dmesg", "print the kernel log"),
    ("sed", "edit a stream of text"),
    ("xargs", "build a command line from input"),
    ("free", "report memory use"),
    ("grep", "select lines matching a pattern"),
    ("head", "print the first lines of a file"),
    ("hexdump", "dump a file in hex"),
    ("history", "(shell builtin) list recent commands"),
    ("hostname", "print the machine name"),
    ("id", "print the user identity"),
    ("init", "system startup, runs as pid 1"),
    ("kill", "send a signal to a process"),
    ("ln", "create a link"),
    ("mkfifo", "create a named pipe"),
    ("ls", "list directory contents"),
    ("mkdir", "create directories"),
    ("mount", "show mounted filesystems"),
    ("mv", "move or rename files"),
    ("printf", "format and print arguments"),
    ("ps", "list running processes"),
    ("pwd", "print the working directory"),
    ("readlink", "print what a symbolic link points at"),
    ("rm", "remove files"),
    ("rtest", "exercise the Rust standard library"),
    ("rmdir", "remove empty directories"),
    ("rev", "reverse each line"),
    ("seq", "print a sequence of numbers"),
    ("sh", "the shell"),
    ("sleep", "pause for a number of seconds"),
    ("sort", "sort lines of text"),
    ("stat", "show file status"),
    ("sync", "flush filesystem buffers"),
    ("tail", "print the last lines of a file"),
    ("tee", "copy standard input to files and stdout"),
    ("test", "evaluate a condition"),
    ("touch", "create empty files"),
    ("tr", "translate or delete characters"),
    ("true", "exit with a success status"),
    ("uname", "print system information"),
    ("uniq", "collapse repeated lines"),
    ("uptime", "how long the system has been running"),
    ("wc", "count lines, words and bytes"),
    ("whoami", "print the current user"),
    ("yes", "repeat a string forever"),
];

pub fn help() {
    println!("claudeos userland -- {} applets", APPLETS.len());
    println!();
    let width = APPLETS.iter().map(|(name, _)| name.len()).max().unwrap_or(8);
    for (name, description) in APPLETS {
        println!("  {:width$}  {}", name, description, width = width);
    }
    println!();
    println!("The shell supports pipelines, && || ; &, redirection with");
    println!("> >> < 2>, globbing, $VARIABLE expansion and quoting.");
}

/// Shell-style pattern match, shared with `find -name`.
pub fn shell_glob(name: &str, pattern: &str) -> bool {
    shell::matches_pattern(name, pattern)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let program = args
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_else(|| "cbox".into());

    // Invoked as "cbox <applet> ...": shift the arguments along.
    let (name, args) = if program == "cbox" && args.len() > 1 {
        (args[1].clone(), args[1..].to_vec())
    } else {
        (program, args)
    };

    exit(dispatch(&name, &args));
}

fn dispatch(name: &str, args: &[String]) -> i32 {
    match name {
        "init" => init::main(args),
        "sh" | "shell" => shell::Shell::new().run(args),
        "cbox" | "help" => {
            help();
            0
        }
        _ => match cmds::run(name, args) {
            Some(code) => code,
            None => {
                eprintln!("cbox: {}: unknown applet (try 'help')", name);
                127
            }
        },
    }
}
