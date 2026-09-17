//! Building a variant's folder, build/distro/VARIANT, from its tree.
//!
//! The folder is made again from nothing each time. Nodes are made in the
//! order they are declared, a directory before what it holds. Generated files
//! are made after every other node, since they read the folder, and the modes
//! of the declared directories are set last, so that a directory without write
//! permission still receives what it holds. Every file and link made here is
//! written at build time; a file unpacked from an archive keeps the time the
//! archive records.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::cache::{self, Cache};
use crate::tree::{Body, Context, Contents, Node, Source};
use crate::variant::Variant;

pub fn folder_path(root: &Path, variant: Variant) -> PathBuf {
    root.join("build/distro").join(variant.name())
}

struct Build<'a> {
    root: &'a Path,
    cache: &'a Cache,
    variant: Variant,
    folder: PathBuf,
    /// Generated files, made once everything else is in.
    generated: Vec<(String, &'a Node)>,
    /// Declared directories and their modes, set at the end.
    dirs: Vec<(String, u32)>,
}

pub fn build(root: &Path, cache: &Cache, tree: &Node, variant: Variant) -> Result<PathBuf, String> {
    let folder = folder_path(root, variant);
    remove_tree(&folder)?;
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let mut build = Build { root, cache, variant, folder: folder.clone(), generated: Vec::new(), dirs: Vec::new() };

    let mut nodes = Vec::new();
    tree.walk(variant, &mut |path, node| nodes.push((path.to_string(), node)));
    for (path, node) in nodes {
        build.make(&path, node)?;
    }

    let context = Context { root, variant, folder: &folder, tree };
    for (path, node) in std::mem::take(&mut build.generated) {
        if let Body::File { mode, source: Source::Generated(generated), .. } = &node.body {
            let data = (generated.make)(&context).map_err(|e| format!("{path}: {e}"))?;
            write_file(&folder.join(&path), &data, *mode)?;
        }
    }
    for (path, mode) in build.dirs.iter().rev() {
        let at = folder.join(path);
        fs::set_permissions(&at, fs::Permissions::from_mode(*mode)).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    Ok(folder)
}

impl<'a> Build<'a> {
    fn make(&mut self, path: &str, node: &'a Node) -> Result<(), String> {
        let at = self.folder.join(path);
        match &node.body {
            Body::Dir { mode, contents, .. } => {
                match fs::symlink_metadata(&at) {
                    Ok(info) if info.is_dir() => {}
                    Ok(_) => return Err(format!("{path}: declared as a directory, and something else is there")),
                    Err(_) => fs::create_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?,
                }
                match contents {
                    Contents::Declared => {}
                    Contents::Repo(source) => copy_repo_dir(&self.root.join(source), &at)?,
                    Contents::Unpack(download) => cache::unpack(&self.cache.download(download)?, &at)?,
                }
                self.dirs.push((path.to_string(), *mode));
            }
            Body::Link { target } => {
                remove_existing(&at)?;
                symlink(target, &at).map_err(|e| format!("{}: {e}", at.display()))?;
            }
            Body::File { mode, source, optional, .. } => {
                let data = match source {
                    Source::Generated(_) => {
                        self.generated.push((path.to_string(), node));
                        return Ok(());
                    }
                    Source::Repo(file) => fs::read(self.root.join(file)).map_err(|e| format!("{file}: {e}")),
                    Source::Input(file) => match fs::read(self.root.join(file)) {
                        Err(e) if e.kind() == ErrorKind::NotFound && *optional => {
                            println!("note: {file} does not exist, so {path} is left out");
                            return Ok(());
                        }
                        other => other.map_err(|e| format!("{file}: {e}")),
                    },
                    Source::Text(text) => Ok(text.as_bytes().to_vec()),
                    Source::Download(download) => self.cache.download(download),
                    Source::Member(member) => self.cache.member(member),
                    Source::Program(program) => program.plan(self.root, self.variant).and_then(|plan| {
                        for step in &plan.steps {
                            println!("+ {}", step.show(self.root));
                        }
                        plan.run()?;
                        fs::read(&plan.output).map_err(|e| format!("{}: {e}", plan.output.display()))
                    }),
                };
                match data {
                    Ok(data) => write_file(&at, &data, *mode)?,
                    Err(e) if *optional => println!("warning: {e}\nwarning: {path} is left out"),
                    Err(e) => return Err(format!("{path}: {e}")),
                }
            }
        }
        Ok(())
    }
}

fn remove_existing(at: &Path) -> Result<(), String> {
    match fs::symlink_metadata(at) {
        Ok(info) if info.is_dir() => Err(format!("{}: a directory is in the way", at.display())),
        Ok(_) => fs::remove_file(at).map_err(|e| format!("{}: {e}", at.display())),
        Err(_) => Ok(()),
    }
}

fn write_file(at: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    remove_existing(at)?;
    fs::write(at, data).map_err(|e| format!("{}: {e}", at.display()))?;
    fs::set_permissions(at, fs::Permissions::from_mode(mode)).map_err(|e| format!("{}: {e}", at.display()))
}

/// Copy the repository directory `source` into `dest`: files with mode 0755
/// when executable and 0644 otherwise, directories with 0755. Links and
/// anything else stop the build, since a repository copy should hold neither.
fn copy_repo_dir(source: &Path, dest: &Path) -> Result<(), String> {
    let mut names: Vec<_> = fs::read_dir(source)
        .map_err(|e| format!("{}: {e}", source.display()))?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", source.display()))?;
    names.sort();
    for name in names {
        let from = source.join(&name);
        let to = dest.join(&name);
        let info = fs::symlink_metadata(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        if info.is_dir() {
            fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            copy_repo_dir(&from, &to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o755)).map_err(|e| format!("{}: {e}", to.display()))?;
        } else if info.is_file() {
            let data = fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
            let mode = if info.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
            write_file(&to, &data, mode)?;
        } else {
            return Err(format!("{} is neither a file nor a directory", from.display()));
        }
    }
    Ok(())
}

/// Remove a folder made before, giving its directories write permission first,
/// since an unpacked archive can hold directories without it.
pub fn remove_tree(at: &Path) -> Result<(), String> {
    fn open_up(at: &Path) -> std::io::Result<()> {
        let info = fs::symlink_metadata(at)?;
        if info.is_dir() {
            if info.permissions().mode() & 0o700 != 0o700 {
                fs::set_permissions(at, fs::Permissions::from_mode(info.permissions().mode() | 0o700))?;
            }
            for entry in fs::read_dir(at)? {
                open_up(&entry?.path())?;
            }
        }
        Ok(())
    }
    match fs::symlink_metadata(at) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", at.display())),
        Ok(_) => {}
    }
    open_up(at).and_then(|_| fs::remove_dir_all(at)).map_err(|e| format!("removing {}: {e}", at.display()))
}
