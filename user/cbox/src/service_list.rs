//! The lists of services init starts at boot, and how a line of one is read.
//!
//! One service per line: a name, a policy, any options, and the command.
//!
//! ```text
//! # name  policy  options                          command
//! ntpd    always  needs=/etc/ntp.conf every=6h limit=180s backoff=30s-10m  /bin/busybox ntpd -n -q
//! site    always  /bin/busybox httpd -f -p 8080 -h /data/site
//! ```
//!
//! The command starts at the first word that begins with `/`, and every word
//! between the policy and it has to be an option, so an option can never be
//! taken for the program or the program for an option.
//!
//! Nothing here touches a file or a process. Text comes in as bytes, and
//! services and reasons go out, so each rule is checked by the unit tests on
//! the Mac, including one that feeds random bytes to the reader to check that
//! nothing in a list makes it panic.

use std::time::Duration;

/// The most of a list that is read. A list of a hundred services is a few
/// kilobytes; a file much larger than this is not a list someone wrote.
pub const MAX_BYTES: usize = 64 * 1024;

/// The most lines read, comments and blank lines included. Without it a file
/// of 64 KiB of newlines would be 65536 lines to go through.
pub const MAX_LINES: usize = 1000;

/// The longest line taken. A command with its arguments fits in well under
/// this, and an error message never has to quote more than this.
pub const MAX_LINE: usize = 1024;

/// The most services one list may hold. Each runs as two processes, the
/// service and the process keeping it, with a pipe between them.
pub const MAX_SERVICES: usize = 32;

const MAX_NAME: usize = 32;

/// The longest duration an option takes: a week. It is there to refuse a typo
/// such as `every=6000000h` rather than because a longer wait cannot be kept.
const MAX_DURATION: u64 = 7 * 24 * 3600;

/// How much of a word taken from the list an error message quotes.
const QUOTED: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Started at boot, and started again whenever it ends.
    Always,
    /// Started at boot and never again.
    Once,
    /// Listed, and not started.
    Off,
}

impl Policy {
    pub fn word(self) -> &'static str {
        match self {
            Policy::Always => "always",
            Policy::Once => "once",
            Policy::Off => "off",
        }
    }
}

/// The waits between failed runs: `first` after the first failure in a row,
/// doubling with each one after it, and never more than `cap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub cap: Duration,
}

impl Backoff {
    /// A service that ends at once is started again a second later, and one
    /// that keeps ending is started about once a minute, which keeps its log
    /// readable and costs the machine nothing it would notice.
    pub const DEFAULT: Backoff = Backoff { first: Duration::from_secs(1), cap: Duration::from_secs(60) };

    /// The wait after `failures` failed runs in a row, the first of them 1.
    pub fn wait(&self, failures: u32) -> Duration {
        let mut wait = self.first;
        for _ in 1..failures {
            if wait >= self.cap {
                break;
            }
            wait = wait.saturating_mul(2);
        }
        wait.min(self.cap)
    }

    /// Whether a run that lasted `ran` starts the count of failures again. A
    /// service that keeps failing is started at most once every `cap`, so a
    /// run at least that long is not part of such a loop. A run limit below
    /// the cap means a run never lasts that long, and the count is then reset
    /// only by an exit with status 0 under `every=`.
    pub fn resets_after(&self, ran: Duration) -> bool {
        ran >= self.cap
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub policy: Policy,
    /// `needs=PATH`: started only when PATH exists at boot.
    pub needs: Option<String>,
    /// `every=D`: after an exit with status 0, the next start is D later,
    /// rather than after a backoff wait.
    pub every: Option<Duration>,
    /// `limit=D`: a run still going after D is killed and counts as a failure.
    pub limit: Option<Duration>,
    /// `backoff=FIRST-CAP`, or `Backoff::DEFAULT`.
    pub backoff: Backoff,
    /// The program's absolute path, then its arguments.
    pub argv: Vec<String>,
    /// Where it is in its list, counting from 1.
    pub line: usize,
}

/// A line that holds something and was not taken as a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct List {
    pub services: Vec<Service>,
    pub skipped: Vec<Skipped>,
    /// Why part of the file was not read at all, when the byte or line cap
    /// stopped the reading.
    pub unread: Option<String>,
}

impl List {
    /// Read a list. `text` is at most `MAX_BYTES` from the start of the file,
    /// and `more` says the file went on past them.
    pub fn parse(text: &[u8], more: bool) -> List {
        let mut list = List::default();
        let more = more || text.len() > MAX_BYTES;
        let text = &text[..text.len().min(MAX_BYTES)];
        // A list saved by Notepad can start with the UTF-8 byte order mark.
        let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
        // Past the byte cap the last line can be cut short, and a line cut
        // short could still read as a command with arguments missing, so it
        // is not read.
        let whole = if more {
            match text.iter().rposition(|&byte| byte == b'\n') {
                Some(end) => &text[..=end],
                None => &text[..0],
            }
        } else {
            text
        };
        let body = whole.strip_suffix(b"\n").unwrap_or(whole);
        let mut count = 0;
        if !whole.is_empty() {
            for raw in body.split(|&byte| byte == b'\n') {
                count += 1;
                if count > MAX_LINES {
                    list.unread = Some(format!(
                        "only the first {} lines were read, so lines from {} on were not",
                        MAX_LINES, count
                    ));
                    return list;
                }
                list.take_line(count, raw);
            }
        }
        if more {
            list.unread = Some(format!(
                "only the first {} bytes were read, so lines from {} on were not",
                MAX_BYTES,
                count + 1
            ));
        }
        list
    }

    fn take_line(&mut self, number: usize, raw: &[u8]) {
        let service = match parse_line(raw) {
            Ok(None) => return,
            Ok(Some(service)) => service,
            Err(reason) => {
                self.skipped.push(Skipped { line: number, reason });
                return;
            }
        };
        if let Some(first) = self.services.iter().find(|s| s.name == service.name) {
            let reason = format!("the name {} is already used on line {}", service.name, first.line);
            self.skipped.push(Skipped { line: number, reason });
        } else if self.services.len() >= MAX_SERVICES {
            let reason = format!("a list may hold {} services, and this would be one more", MAX_SERVICES);
            self.skipped.push(Skipped { line: number, reason });
        } else {
            self.services.push(Service { line: number, ..service });
        }
    }

    /// Take out every service whose name one in `system` has, with a line
    /// saying so. Every name in the system list counts, `off` or not, so a
    /// user list cannot run something in place of a system service or turn
    /// one off.
    pub fn refuse_system_names(&mut self, system: &List) {
        let taken = |service: &Service| system.services.iter().any(|s| s.name == service.name);
        for service in self.services.iter().filter(|s| taken(s)) {
            self.skipped.push(Skipped {
                line: service.line,
                reason: format!(
                    "{} is the name of a system service, which runs as the system list has it, and a user service cannot replace or disable it",
                    service.name
                ),
            });
        }
        self.services.retain(|s| !taken(s));
        self.skipped.sort_by_key(|skipped| skipped.line);
    }
}

/// One line, without its newline: a service, nothing for a blank or comment
/// line, or why it is neither.
fn parse_line(raw: &[u8]) -> Result<Option<Service>, String> {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.len() > MAX_LINE {
        return Err(format!("it is {} bytes long, and a line may be {}", raw.len(), MAX_LINE));
    }
    let line = std::str::from_utf8(raw).map_err(|_| String::from("it is not UTF-8 text"))?;
    // What a line holds is quoted in the errors file and the status files,
    // which are read on a terminal, where a control character would be taken
    // as part of an escape sequence.
    if line.chars().any(|c| (c < ' ' && c != '\t') || c == '\x7f') {
        return Err(String::from("it holds a control character"));
    }
    let mut words = split_words(line)?.into_iter();
    let Some(name) = words.next() else {
        return Ok(None);
    };
    if name.is_empty()
        || name.len() > MAX_NAME
        || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "the name {} is not 1 to {} of the characters a-z, 0-9, _ and -",
            quoted(&name),
            MAX_NAME
        ));
    }
    let policy = match words.next().as_deref() {
        Some("always") => Policy::Always,
        Some("once") => Policy::Once,
        Some("off") => Policy::Off,
        Some(other) => {
            return Err(format!("the policy {} is not always, once or off", quoted(other)));
        }
        None => return Err(format!("{} has no policy: always, once or off comes after the name", name)),
    };

    let rest: Vec<String> = words.collect();
    let mut service = Service {
        name,
        policy,
        needs: None,
        every: None,
        limit: None,
        backoff: Backoff::DEFAULT,
        argv: Vec::new(),
        line: 0,
    };
    let mut backoff_given = false;
    let mut at = 0;
    while at < rest.len() && !rest[at].starts_with('/') {
        let word = &rest[at];
        let Some((key, value)) = word.split_once('=') else {
            return Err(format!(
                "{} is neither an option nor the absolute path of a program",
                quoted(word)
            ));
        };
        let twice = || format!("{}= is given twice", key);
        match key {
            "needs" => {
                if service.needs.is_some() {
                    return Err(twice());
                }
                if !value.starts_with('/') {
                    return Err(format!("needs= takes an absolute path, and {} is not one", quoted(value)));
                }
                service.needs = Some(value.to_string());
            }
            "every" => {
                if service.every.is_some() {
                    return Err(twice());
                }
                service.every = Some(duration(key, value)?);
            }
            "limit" => {
                if service.limit.is_some() {
                    return Err(twice());
                }
                service.limit = Some(duration(key, value)?);
            }
            "backoff" => {
                if backoff_given {
                    return Err(twice());
                }
                let Some((first, cap)) = value.split_once('-') else {
                    return Err(format!(
                        "backoff= takes the first wait and the longest, such as backoff=30s-10m, and {} is not that",
                        quoted(value)
                    ));
                };
                let backoff = Backoff { first: duration(key, first)?, cap: duration(key, cap)? };
                if backoff.first > backoff.cap {
                    return Err(format!("backoff={} starts above the longest wait it names", value));
                }
                service.backoff = backoff;
                backoff_given = true;
            }
            _ => {
                return Err(format!(
                    "{} is not an option; the options are needs=, every=, limit= and backoff=",
                    quoted(&format!("{}=", key))
                ));
            }
        }
        at += 1;
    }
    if at == rest.len() {
        return Err(String::from(
            "there is no command: the absolute path of the program comes after the policy and any options",
        ));
    }
    if policy == Policy::Once && (service.every.is_some() || backoff_given) {
        return Err(String::from("every= and backoff= are for always, and a once service is never started again"));
    }
    service.argv = rest[at..].to_vec();
    Ok(Some(service))
}

/// Split a line into words at spaces and tabs. Single and double quotes keep
/// what is between them in one word, as they are, and a quoted part joins the
/// text on either side of it; nothing is expanded, and a backslash is an
/// ordinary character. A `#` that starts a word outside quotes starts a
/// comment.
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '#' if !in_word => break,
            '\'' | '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some(end) if end == c => break,
                        Some(inner) => word.push(inner),
                        None => {
                            let kind = if c == '\'' { "single" } else { "double" };
                            return Err(format!("a {} quote is not closed", kind));
                        }
                    }
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// `30s`, `10m` or `6h`.
fn duration(key: &str, value: &str) -> Result<Duration, String> {
    let bad = || {
        format!(
            "{}= takes a whole number of seconds, minutes or hours, such as 30s, 10m or 6h, from 1s to 168h, and {} is not that",
            key,
            quoted(value)
        )
    };
    let (digits, scale) = if let Some(digits) = value.strip_suffix('s') {
        (digits, 1)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60)
    } else if let Some(digits) = value.strip_suffix('h') {
        (digits, 3600)
    } else {
        return Err(bad());
    };
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let seconds = digits.parse::<u64>().map_err(|_| bad())? * scale;
    if seconds == 0 || seconds > MAX_DURATION {
        return Err(bad());
    }
    Ok(Duration::from_secs(seconds))
}

/// A word from the list in backquotes, cut to `QUOTED` characters.
fn quoted(word: &str) -> String {
    let mut shown: String = word.chars().take(QUOTED).collect();
    if shown.len() < word.len() {
        shown.push_str("...");
    }
    format!("`{}`", shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Result<Option<Service>, String> {
        parse_line(line.as_bytes())
    }

    fn argv(line: &str) -> Vec<String> {
        one(line).unwrap().unwrap().argv
    }

    fn error(line: &str) -> String {
        one(line).unwrap_err()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_system_list_ntpd_line() {
        let service = one("ntpd  always  needs=/etc/ntp.conf every=6h limit=180s backoff=30s-10m  /bin/busybox ntpd -n -q")
            .unwrap()
            .unwrap();
        assert_eq!(service.name, "ntpd");
        assert_eq!(service.policy, Policy::Always);
        assert_eq!(service.needs.as_deref(), Some("/etc/ntp.conf"));
        assert_eq!(service.every, Some(secs(6 * 3600)));
        assert_eq!(service.limit, Some(secs(180)));
        assert_eq!(service.backoff, Backoff { first: secs(30), cap: secs(600) });
        assert_eq!(service.argv, ["/bin/busybox", "ntpd", "-n", "-q"]);
    }

    #[test]
    fn the_ntpd_backoff_is_the_old_time_keepers() {
        // The time keeper init had before the lists: 30 s doubling to 600 s.
        let old = |failures: u32| (secs(30) * (1u32 << failures.saturating_sub(1).min(8))).min(secs(600));
        let backoff = Backoff { first: secs(30), cap: secs(600) };
        for failures in 1..40 {
            assert_eq!(backoff.wait(failures), old(failures), "after {} failures", failures);
        }
        // Its runs are killed at 180 s, so none lasts long enough to reset
        // the count.
        assert!(!backoff.resets_after(secs(180)));
    }

    #[test]
    fn the_default_backoff() {
        let waits: Vec<u64> = (1..=9).map(|n| Backoff::DEFAULT.wait(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(Backoff::DEFAULT.wait(0), secs(1));
        assert_eq!(Backoff::DEFAULT.wait(u32::MAX), secs(60));
        assert!(!Backoff::DEFAULT.resets_after(secs(59)));
        assert!(Backoff::DEFAULT.resets_after(secs(60)));
        let long = Backoff { first: secs(168 * 3600), cap: secs(168 * 3600) };
        assert_eq!(long.wait(u32::MAX), secs(168 * 3600));
    }

    #[test]
    fn quotes() {
        assert_eq!(
            argv(r#"a always /bin/sh -c 'echo "hi there"; exit 1'"#),
            ["/bin/sh", "-c", r#"echo "hi there"; exit 1"#]
        );
        assert_eq!(argv(r#"a always /bin/echo "it's" 'say "x"'"#), ["/bin/echo", "it's", r#"say "x""#]);
        // Quoted parts join the text beside them, and empty quotes are a word.
        assert_eq!(argv(r#"a always /bin/echo x"y z"w '' """#), ["/bin/echo", "xy zw", "", ""]);
        // Nothing is expanded and a backslash is an ordinary character.
        assert_eq!(argv(r#"a always /bin/echo $HOME \n "\" *"#), ["/bin/echo", "$HOME", "\\n", "\\", "*"]);
        // A quoted program path still starts the command.
        assert_eq!(argv(r#"a always "/bin/my prog" x"#), ["/bin/my prog", "x"]);
        assert_eq!(error("a always /bin/echo 'open"), "a single quote is not closed");
        assert_eq!(error(r#"a always /bin/echo "open"#), "a double quote is not closed");
    }

    #[test]
    fn comments_blank_lines_and_whitespace() {
        assert_eq!(one(""), Ok(None));
        assert_eq!(one("   \t  "), Ok(None));
        assert_eq!(one("# a comment"), Ok(None));
        assert_eq!(one("   # indented"), Ok(None));
        assert_eq!(argv("a always /bin/echo x # trailing comment"), ["/bin/echo", "x"]);
        // Inside a word, or quoted, # is text.
        assert_eq!(argv("a always /bin/echo a#b '#c'"), ["/bin/echo", "a#b", "#c"]);
        assert_eq!(argv("\ta\talways\t/bin/echo\tx   \t "), ["/bin/echo", "x"]);
    }

    #[test]
    fn crlf_and_a_byte_order_mark() {
        let list = List::parse(b"\xEF\xBB\xBFa always /bin/echo a\r\n# note\r\n\r\nb once /bin/echo b\r\n", false);
        assert!(list.skipped.is_empty(), "{:?}", list.skipped);
        assert_eq!(list.services.len(), 2);
        assert_eq!(list.services[0].argv, ["/bin/echo", "a"]);
        assert_eq!(list.services[1].argv, ["/bin/echo", "b"]);
        assert_eq!(list.services[1].line, 4);
        // A carriage return anywhere but the end of a line is a control
        // character.
        assert_eq!(error("a always /bin/echo a\rb"), "it holds a control character");
        // No newline at the end of the file.
        let list = List::parse(b"a always /bin/echo a", false);
        assert_eq!(list.services.len(), 1);
    }

    #[test]
    fn bad_names() {
        for name in ["Ntpd", "web.site", "a/b", "caf\u{e9}", "''", "x_y_z_0123456789_0123456789_abcde"] {
            let reason = error(&format!("{} always /bin/true", name));
            assert!(reason.starts_with("the name `"), "{}: {}", name, reason);
        }
        // 32 characters is the most.
        assert!(one("x_y_z_0123456789_0123456789_abcd always /bin/true").unwrap().is_some());
        assert!(one("a-b_c-9 always /bin/true").unwrap().is_some());
    }

    #[test]
    fn bad_policies() {
        assert_eq!(error("web sometimes /bin/true"), "the policy `sometimes` is not always, once or off");
        assert_eq!(error("web Always /bin/true"), "the policy `Always` is not always, once or off");
        assert_eq!(error("web"), "web has no policy: always, once or off comes after the name");
        assert_eq!(error("web /bin/true"), "the policy `/bin/true` is not always, once or off");
    }

    #[test]
    fn options() {
        assert_eq!(
            error("web always port=80 /bin/true"),
            "`port=` is not an option; the options are needs=, every=, limit= and backoff="
        );
        assert_eq!(error("web always every=1h every=2h /bin/true"), "every= is given twice");
        assert_eq!(error("web always backoff=1s-2s backoff=1s-2s /bin/true"), "backoff= is given twice");
        assert_eq!(
            error("web always needs=etc/x /bin/true"),
            "needs= takes an absolute path, and `etc/x` is not one"
        );
        for bad in ["0s", "10", "10x", "s", "-1s", "1.5h", "169h", "10081m", "9999999s", "", "\u{3042}"] {
            let reason = error(&format!("web always limit={} /bin/true", bad));
            assert!(reason.starts_with("limit= takes a whole number"), "{}: {}", bad, reason);
        }
        assert_eq!(one("web always limit=168h /bin/true").unwrap().unwrap().limit, Some(secs(168 * 3600)));
        assert_eq!(
            error("web always backoff=10m-30s /bin/true"),
            "backoff=10m-30s starts above the longest wait it names"
        );
        assert!(error("web always backoff=30s /bin/true").starts_with("backoff= takes the first wait"));
        assert_eq!(
            error("web once every=1h /bin/true"),
            "every= and backoff= are for always, and a once service is never started again"
        );
        let once = one("web once limit=5s needs=/data/site /bin/true").unwrap().unwrap();
        assert_eq!((once.limit, once.needs.as_deref()), (Some(secs(5)), Some("/data/site")));
        // Options under off are read and checked, so a line can be turned off
        // by changing one word.
        let off = one("web off every=1h backoff=2s-4s /bin/true").unwrap().unwrap();
        assert_eq!(off.policy, Policy::Off);
        assert_eq!(off.backoff, Backoff { first: secs(2), cap: secs(4) });
    }

    #[test]
    fn the_command() {
        assert_eq!(
            error("web always busybox httpd"),
            "`busybox` is neither an option nor the absolute path of a program"
        );
        assert_eq!(
            error("web always"),
            "there is no command: the absolute path of the program comes after the policy and any options"
        );
        assert_eq!(
            error("web always limit=5s # the command went missing"),
            "there is no command: the absolute path of the program comes after the policy and any options"
        );
        // Words after the program are its arguments, = or not.
        assert_eq!(argv("web always /bin/env A=1 every=2"), ["/bin/env", "A=1", "every=2"]);
    }

    #[test]
    fn bytes_that_are_not_text() {
        assert_eq!(parse_line(b"web always /bin/echo \xff"), Err(String::from("it is not UTF-8 text")));
        assert_eq!(error("web always /bin/echo \x1b[2J"), "it holds a control character");
        assert_eq!(error("web always /bin/echo \x7f"), "it holds a control character");
        assert_eq!(error("web always /bin/echo \0"), "it holds a control character");
    }

    #[test]
    fn long_lines() {
        let fits = format!("web always /bin/echo {}", "x".repeat(MAX_LINE - 21));
        assert_eq!(fits.len(), MAX_LINE);
        assert!(one(&fits).unwrap().is_some());
        // The carriage return of a CRLF line is not counted.
        assert!(parse_line(format!("{}\r", fits).as_bytes()).unwrap().is_some());
        let over = format!("{}x", fits);
        assert_eq!(error(&over), "it is 1025 bytes long, and a line may be 1024");
        // A long word is quoted cut short.
        let name = "n".repeat(500);
        assert_eq!(
            error(&format!("{} always /bin/true", name)),
            format!("the name `{}...` is not 1 to 32 of the characters a-z, 0-9, _ and -", "n".repeat(40))
        );
        // A huge line among good ones costs only itself.
        let text = format!("a always /bin/true\n{}\nb once /bin/true\n", "y".repeat(50_000));
        let list = List::parse(text.as_bytes(), false);
        assert_eq!(list.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(list.skipped, [Skipped { line: 2, reason: String::from("it is 50000 bytes long, and a line may be 1024") }]);
    }

    #[test]
    fn many_lines() {
        let mut text = String::new();
        for _ in 0..MAX_LINES - 1 {
            text.push_str("# filler\n");
        }
        text.push_str("last once /bin/true\n");
        text.push_str("past always /bin/true\n");
        let list = List::parse(text.as_bytes(), false);
        assert_eq!(list.services.len(), 1);
        assert_eq!(list.services[0].line, MAX_LINES);
        assert_eq!(list.unread.as_deref(), Some("only the first 1000 lines were read, so lines from 1001 on were not"));
        // Exactly the cap, with the newline at the end, is all read.
        let exact = text.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n") + "\n";
        assert_eq!(List::parse(exact.as_bytes(), false).unread, None);
    }

    #[test]
    fn the_byte_cap() {
        // What a reader hands over when the file is longer than the cap: the
        // first MAX_BYTES, ending inside a line.
        // Filler lines of 100 bytes to within 120 bytes of the cap, then a
        // line of 200 bytes, which the cap falls inside.
        let mut text = String::from("a always /bin/true\n");
        while text.len() + 100 <= MAX_BYTES - 20 {
            text.push_str(&format!("# {}\n", "z".repeat(97)));
        }
        let cut_at = text.lines().count() + 1;
        text.push_str(&format!("b always /bin/echo {}\n", "cut".repeat(60)));
        let read = &text.as_bytes()[..MAX_BYTES];
        let list = List::parse(read, true);
        assert_eq!(list.services.len(), 1);
        assert!(list.skipped.is_empty(), "{:?}", list.skipped);
        assert_eq!(
            list.unread,
            Some(format!("only the first 65536 bytes were read, so lines from {} on were not", cut_at))
        );
        // More than the cap handed over is cut the same way.
        let list = List::parse(text.as_bytes(), false);
        assert_eq!(list.services.len(), 1);
        assert!(list.unread.is_some());
        // One line longer than the cap: nothing is read.
        let list = List::parse(&[b'x'; MAX_BYTES], true);
        assert!(list.services.is_empty() && list.skipped.is_empty());
        assert_eq!(list.unread.as_deref(), Some("only the first 65536 bytes were read, so lines from 1 on were not"));
    }

    #[test]
    fn too_many_services() {
        let text: String = (0..MAX_SERVICES + 2).map(|n| format!("s{} once /bin/true\n", n)).collect();
        let list = List::parse(text.as_bytes(), false);
        assert_eq!(list.services.len(), MAX_SERVICES);
        assert_eq!(list.skipped.len(), 2);
        assert_eq!(list.skipped[0].line, MAX_SERVICES + 1);
        assert_eq!(list.skipped[0].reason, "a list may hold 32 services, and this would be one more");
    }

    #[test]
    fn duplicate_names() {
        let list = List::parse(b"web always /bin/true\nweb once /bin/false\nweb off /bin/true\n", false);
        assert_eq!(list.services.len(), 1);
        assert_eq!(list.services[0].argv, ["/bin/true"]);
        assert_eq!(list.skipped.len(), 2);
        assert_eq!(list.skipped[0], Skipped { line: 2, reason: String::from("the name web is already used on line 1") });
    }

    #[test]
    fn duplicate_names_across_lists() {
        let system = List::parse(b"ntpd always /bin/busybox ntpd -n -q\nspare off /bin/true\n", false);
        let mut user = List::parse(
            b"web always /bin/true\nntpd always /bin/sh -c 'echo impostor'\nbad Name\nspare always /bin/true\n",
            false,
        );
        user.refuse_system_names(&system);
        assert_eq!(user.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["web"]);
        assert_eq!(user.skipped.iter().map(|s| s.line).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(
            user.skipped[0].reason,
            "ntpd is the name of a system service, which runs as the system list has it, and a user service cannot replace or disable it"
        );
    }

    #[test]
    fn random_bytes_never_panic() {
        // Lines made of the characters the reader treats specially, and of any
        // byte at all.
        let alphabet = b" \t\r\n#'\"=/-sm0h9aZ\xff\xe3\x82\xbfalwaysonceoffneedseverylimitbackoff";
        let mut state = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..4000 {
            let length = (next() % 1500) as usize;
            let text: Vec<u8> = (0..length)
                .map(|_| {
                    let pick = next();
                    if round % 2 == 0 { alphabet[(pick % alphabet.len() as u64) as usize] } else { pick as u8 }
                })
                .collect();
            let list = List::parse(&text, round % 3 == 0);
            for service in &list.services {
                assert!(service.argv[0].starts_with('/'));
            }
        }
    }
}
