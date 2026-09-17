//! The applet names declared in the APPLETS table of user/cbox/src/main.rs.
//!
//! An entry with `#[cfg(feature = "NAME")]` on the line above it is compiled
//! into cbox only with that cargo feature, so an image gets a link for it only
//! when its cbox is built with NAME. Any other attribute in the table stops the
//! build rather than being guessed at.

use std::fs;
use std::path::Path;

pub struct Applet {
    pub name: String,
    /// The cargo feature the entry is compiled with, if it needs one.
    pub feature: Option<String>,
}

pub fn read(main_rs: &Path) -> Result<Vec<Applet>, String> {
    let text = fs::read_to_string(main_rs).map_err(|e| format!("{}: {e}", main_rs.display()))?;
    let mut lines = text.lines().skip_while(|line| !line.starts_with("pub const APPLETS"));
    if lines.next().is_none() {
        return Err(format!("{} has no `pub const APPLETS` table", main_rs.display()));
    }
    let mut applets = Vec::new();
    let mut wanted: Option<String> = None;
    for line in lines {
        if line.starts_with("];") {
            return Ok(applets);
        }
        if let Some(rest) = line.strip_prefix("    #") {
            let feature = rest
                .strip_prefix("[cfg(feature = \"")
                .and_then(|rest| rest.strip_suffix("\")]"))
                .filter(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'));
            match feature {
                Some(name) => wanted = Some(name.to_string()),
                None => return Err(format!("{}: cannot read this attribute: {line}", main_rs.display())),
            }
        } else if let Some(rest) = line.strip_prefix("    (\"") {
            let name = rest.split('"').next().unwrap_or("");
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'[') {
                return Err(format!("{}: cannot read this applet name: {line}", main_rs.display()));
            }
            applets.push(Applet { name: name.to_string(), feature: wanted.take() });
        }
    }
    Err(format!("{}: the APPLETS table has no end", main_rs.display()))
}
