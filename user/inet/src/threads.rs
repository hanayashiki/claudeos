//! What a thread shares with the task that started it.
//!
//! A thread is a clone that asked for CLONE_VM, CLONE_FILES and CLONE_FS. The
//! address space is the one a program notices first, but the other two are
//! just as visible: a descriptor one thread opens is one its siblings can
//! read, and a directory one changes into is the one a relative path resolves
//! against everywhere else in the process.

use crate::sys;
use crate::Report;
use std::os::unix::io::IntoRawFd;

pub fn run(report: &mut Report) {
    a_descriptor_opened_in_a_thread(report);
    a_directory_changed_in_a_thread(report);
}

fn a_descriptor_opened_in_a_thread(report: &mut Report) {
    const PATH: &str = "/tmp/shared-descriptor";
    const BODY: &[u8] = b"shared";
    if let Err(err) = std::fs::write(PATH, BODY) {
        report.check("a file to open", false, format!("{}", err));
        return;
    }

    // The handle is given up rather than closed, so the descriptor outlives
    // the thread that opened it.
    let opened = std::thread::spawn(|| match std::fs::File::open(PATH) {
        Ok(file) => file.into_raw_fd(),
        Err(_) => -1,
    })
    .join();
    let fd = opened.unwrap_or(-1);

    let mut buf = [0u8; 6];
    let n = if fd < 0 { -1 } else { sys::read(fd, &mut buf) };
    report.check(
        "a descriptor a thread opened is one its siblings have",
        n == BODY.len() as i64 && &buf[..] == BODY,
        format!("read on descriptor {} returned {}", fd, n),
    );
    if fd >= 0 {
        sys::close(fd);
    }
    let _ = std::fs::remove_file(PATH);
}

fn a_directory_changed_in_a_thread(report: &mut Report) {
    let Ok(original) = std::env::current_dir() else {
        report.check("a working directory to start from", false, String::new());
        return;
    };
    let changed = std::thread::spawn(|| std::env::set_current_dir("/tmp").is_ok())
        .join()
        .unwrap_or(false);
    let now = std::env::current_dir().ok();
    report.check(
        "a directory a thread changed into is the one its siblings resolve against",
        changed && now.as_deref() == Some(std::path::Path::new("/tmp")),
        format!("the working directory is {:?}", now),
    );
    let _ = std::env::set_current_dir(original);
}
