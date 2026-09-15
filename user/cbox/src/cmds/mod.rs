//! The coreutils applets.

mod fileops;
#[cfg(feature = "rtest")]
pub mod rtest;
mod misc;
mod sed;
mod sysinfo;
mod textops;

pub fn run(name: &str, args: &[String]) -> Option<i32> {
    let code = match name {
        // files and directories
        "ls" => fileops::ls(args),
        "cat" => fileops::cat(args),
        "cp" => fileops::cp(args),
        "mv" => fileops::mv(args),
        "rm" => fileops::rm(args),
        "mkdir" => fileops::mkdir(args),
        "rmdir" => fileops::rmdir(args),
        "touch" => fileops::touch(args),
        "ln" => fileops::ln(args),
        "mkfifo" => fileops::mkfifo(args),
        "stat" => fileops::stat(args),
        "find" => fileops::find(args),
        "du" => fileops::du(args),
        "df" => fileops::df(args),
        "pwd" => fileops::pwd(args),
        "readlink" => fileops::readlink(args),
        "hexdump" | "xxd" => fileops::hexdump(args),

        // text
        "echo" => textops::echo(args),
        "wc" => textops::wc(args),
        "head" => textops::head(args),
        "tail" => textops::tail(args),
        "grep" => textops::grep(args),
        "sort" => textops::sort(args),
        "uniq" => textops::uniq(args),
        "cut" => textops::cut(args),
        "tr" => textops::tr(args),
        "tee" => textops::tee(args),
        "seq" => textops::seq(args),
        "rev" => textops::rev(args),

        // system
        "uname" => sysinfo::uname(args),
        "ps" => sysinfo::ps(args),
        "dmesg" => sysinfo::dmesg(args),
        "sed" => sed::main(args),
        "xargs" => misc::xargs(args),
        "free" => sysinfo::free(args),
        "uptime" => sysinfo::uptime(args),
        "date" => sysinfo::date(args),
        "env" => sysinfo::env(args),
        "id" => sysinfo::id(args),
        "whoami" => sysinfo::whoami(args),
        "hostname" => sysinfo::hostname(args),
        "mount" => sysinfo::mount(args),
        "sleep" => sysinfo::sleep(args),
        "kill" => sysinfo::kill(args),
        "sync" => sysinfo::sync(args),
        "reboot" => sysinfo::reboot(args),
        "halt" => sysinfo::halt(args),
        "poweroff" => sysinfo::poweroff(args),
        "clear" => sysinfo::clear(args),

        // small things
        "true" => 0,
        "false" => 1,
        "yes" => misc::yes(args),
        "basename" => misc::basename(args),
        "dirname" => misc::dirname(args),
        "test" | "[" => misc::test(args),
        "printf" => misc::printf(args),
        "chmod" => misc::chmod(args),
        "expr" => misc::expr(args),
        #[cfg(feature = "rtest")]
        "rtest" => rtest::main(args),

        _ => return None,
    };
    Some(code)
}

/// Report an error the way a Unix tool does and return a failure status.
pub fn fail(program: &str, context: &str, err: std::io::Error) -> i32 {
    eprintln!("{}: {}: {}", program, context, describe(&err));
    1
}

/// The message without the "(os error N)" tail that Rust appends, which is
/// not what a Unix tool prints.
pub fn describe(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.find(" (os error") {
        Some(index) => text[..index].to_string(),
        None => text,
    }
}

/// Split arguments into flag characters and positional operands.
pub fn split_flags(args: &[String]) -> (String, Vec<String>) {
    let mut flags = String::new();
    let mut operands = Vec::new();
    let mut no_more_flags = false;
    for arg in args.iter().skip(1) {
        if arg == "--" {
            no_more_flags = true;
        } else if !no_more_flags && arg.starts_with('-') && arg.len() > 1 {
            flags.push_str(&arg[1..]);
        } else {
            operands.push(arg.clone());
        }
    }
    (flags, operands)
}

/// The lines of `data`, each one keeping the newline that ended it. The last
/// line has none when the input did not end in one, which is how a tool that
/// copies lines through knows not to add a newline the input had not got.
pub fn lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            out.push(&data[start..=index]);
            start = index + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// A line without the newline that ended it, if it had one.
pub fn without_newline(line: &[u8]) -> &[u8] {
    match line.strip_suffix(b"\n") {
        Some(rest) => rest,
        None => line,
    }
}

/// Read every named file, or standard input when there are none.
///
/// What comes back is bytes. Reading into a String instead asks the tool to
/// decide whether its input was text, and answers no for a file of arbitrary
/// bytes: the read fails, and the applet prints nothing at all for a file it
/// was asked to count or to copy.
pub fn read_inputs(program: &str, paths: &[String]) -> (Vec<(String, Vec<u8>)>, i32) {
    use std::io::Read;
    let mut out = Vec::new();
    let mut status = 0;
    if paths.is_empty() {
        let mut data = Vec::new();
        match std::io::stdin().read_to_end(&mut data) {
            Ok(_) => out.push(("-".to_string(), data)),
            Err(err) => status = fail(program, "-", err),
        }
        return (out, status);
    }
    for path in paths {
        match std::fs::read(path) {
            Ok(data) => out.push((path.clone(), data)),
            Err(err) => status = fail(program, path, err),
        }
    }
    (out, status)
}
