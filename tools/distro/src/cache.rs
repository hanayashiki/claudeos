//! The download cache: build/cache/sha256/HEX holds the file whose sha256 is
//! HEX.
//!
//! A download is stored under the sha256 recorded for it, and a file taken
//! out of an archive under its own, so a member is taken out once and the
//! archive is not needed again while the member's copy is intact. Every read
//! hashes the file again: a copy whose bytes no longer match is fetched or
//! taken out again, and a fetch or a member that does not match its record
//! stops the build with nothing stored.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};

use crate::downloads::{Download, Member};

pub struct Cache {
    dir: PathBuf,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Cache {
    pub fn new(dir: PathBuf) -> Cache {
        Cache { dir }
    }

    pub fn path(&self, sha256: &str) -> PathBuf {
        self.dir.join("sha256").join(sha256)
    }

    /// The stored copy with this sha256, if there is one and it still has it.
    fn stored(&self, sha256: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.path(sha256);
        match fs::read(&path) {
            Ok(data) if sha256_hex(&data) == sha256 => Ok(Some(data)),
            Ok(_) => {
                eprintln!("note: {} no longer has its sha256; replacing it", path.display());
                fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Store `data` under `sha256`, renamed into place so that no reader sees
    /// half of it.
    fn store(&self, sha256: &str, data: &[u8]) -> Result<(), String> {
        let path = self.path(sha256);
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let partial = dir.join(format!("{sha256}.partial.{}", std::process::id()));
        fs::write(&partial, data).map_err(|e| format!("{}: {e}", partial.display()))?;
        fs::rename(&partial, &path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The bytes of a download, fetched if the cache has no intact copy.
    pub fn download(&self, download: &Download) -> Result<Vec<u8>, String> {
        if let Some(data) = self.stored(download.sha256)? {
            return Ok(data);
        }
        let dir = self.dir.join("sha256");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let partial = dir.join(format!("{}.fetch.{}", download.sha256, std::process::id()));
        eprintln!("fetching {}", download.url);
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "2", "--max-time", "900", "-o"])
            .arg(&partial)
            .arg(download.url)
            .status()
            .map_err(|e| format!("could not run curl: {e}"))?;
        let data = if status.success() { fs::read(&partial).ok() } else { None };
        let _ = fs::remove_file(&partial);
        let data = data.ok_or_else(|| format!("could not fetch {}; check the network and try again", download.url))?;
        let got = sha256_hex(&data);
        if got != download.sha256 {
            return Err(format!(
                "{} has sha256 {got}, not the {} recorded in tools/distro/src/downloads.rs; refusing to use it",
                download.url, download.sha256
            ));
        }
        self.store(download.sha256, &data)?;
        Ok(data)
    }

    /// The bytes of a file inside a downloaded archive, taken out if the cache
    /// has no intact copy.
    pub fn member(&self, member: &Member) -> Result<Vec<u8>, String> {
        if let Some(data) = self.stored(member.sha256)? {
            return Ok(data);
        }
        let archive = self.download(member.archive)?;
        let data = read_member(&archive, member.path)
            .map_err(|e| format!("{}: {e}", member.archive.url))?
            .ok_or_else(|| format!("{} holds no {}", member.archive.url, member.path))?;
        let got = sha256_hex(&data);
        if got != member.sha256 {
            return Err(format!(
                "{} from {} has sha256 {got}, not the {} recorded in tools/distro/src/downloads.rs; refusing to use it",
                member.path, member.archive.url, member.sha256
            ));
        }
        self.store(member.sha256, &data)?;
        Ok(data)
    }

    /// Whether the cache holds an intact copy of this sha256, for `distro fetch`.
    pub fn has(&self, sha256: &str) -> Result<bool, String> {
        Ok(self.stored(sha256)?.is_some())
    }
}

/// The path of an archive entry without a leading `./`.
fn entry_path(entry: &tar::Entry<impl Read>) -> std::io::Result<String> {
    let path = entry.path()?;
    let text = path.to_string_lossy();
    Ok(text.trim_start_matches("./").to_string())
}

/// The regular file at `path` in a tar archive in gzip. An Alpine package is
/// three gzip streams, the signature, the control files and the data, each a
/// piece of a tar archive, which read one after another make one archive.
fn read_member(archive: &[u8], path: &str) -> std::io::Result<Option<Vec<u8>>> {
    let mut tar = tar::Archive::new(MultiGzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry_path(&entry)? == path {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// Unpack a tar archive in gzip into `dir`, with the modes, times and links it
/// records.
pub fn unpack(archive: &[u8], dir: &Path) -> Result<(), String> {
    let mut tar = tar::Archive::new(MultiGzDecoder::new(archive));
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_overwrite(true);
    tar.unpack(dir).map_err(|e| format!("unpacking into {}: {e}", dir.display()))
}
