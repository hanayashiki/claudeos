//! The coreutils applets.

mod fileops;
pub mod rtest;
mod misc;
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
        "stat" => fileops::stat(args),
        "find" => fileops::find(args),
        "du" => fileops::du(args),
        "df" => fileops::df(args),
        "pwd" => fileops::pwd(args),
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
        "rtest" => rtest::main(args),

        _ => return None,
    };
    Some(code)
}

/// Report an error the way a Unix tool does and return a failure status.
pub fn fail(program: &str, context: &str, err: std::io::Error) -> i32 {
    eprintln!("{}: {}: {}", program, context, err);
    1
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

/// Read every named file, or standard input when there are none.
pub fn read_inputs(program: &str, paths: &[String]) -> (Vec<(String, String)>, i32) {
    use std::io::Read;
    let mut out = Vec::new();
    let mut status = 0;
    if paths.is_empty() {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_ok() {
            out.push(("-".to_string(), text));
        }
        return (out, status);
    }
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(text) => out.push((path.clone(), text)),
            Err(err) => status = fail(program, path, err),
        }
    }
    (out, status)
}
