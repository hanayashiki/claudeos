//! The programs the build runs to make a file of a tree.
//!
//! Each is a list of commands and the file the last one writes. Nothing here
//! decides whether to run them: a cargo build that has nothing to do costs
//! cargo's own check, and Go and clang builds are small.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::variant::{Arch, Variant, Variants};

pub enum Program {
    /// The binary of a workspace package of the same name, built for the
    /// variant's machine with the `user` profile, with each feature listed
    /// for the variants given beside it.
    Cargo { package: &'static str, features: &'static [(&'static str, Variants)] },
    /// A C file, compiled by clang and linked by rust-lld against the musl
    /// libc and C runtime objects in rustup's target for the variant's
    /// machine.
    MuslC { source: &'static str, name: &'static str },
    /// A Go package, by its directory, built with cgo off for Linux on the
    /// variant's machine.
    Go { dir: &'static str, name: &'static str },
}

/// A command to run.
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub dir: PathBuf,
}

/// The commands that make a file, and the file. The programs cargo does not
/// build write theirs under target/TARGET/distro.
pub struct Plan {
    pub steps: Vec<Step>,
    pub output: PathBuf,
}

/// What the plans need to know about the toolchain on this machine.
pub struct Toolchain {
    sysroot: PathBuf,
    host: String,
}

fn toolchain() -> Result<&'static Toolchain, String> {
    static TOOLCHAIN: OnceLock<Result<Toolchain, String>> = OnceLock::new();
    TOOLCHAIN
        .get_or_init(|| {
            let output = |args: &[&str]| -> Result<String, String> {
                let out = Command::new("rustc").args(args).output().map_err(|e| format!("could not run rustc: {e}"))?;
                if !out.status.success() {
                    return Err(format!("rustc {} failed", args.join(" ")));
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            };
            let sysroot = PathBuf::from(output(&["--print", "sysroot"])?.trim());
            let host = output(&["-vV"])?
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .ok_or("rustc -vV names no host")?
                .to_string();
            Ok(Toolchain { sysroot, host })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

impl Program {
    pub fn plan(&self, root: &Path, variant: Variant) -> Result<Plan, String> {
        let arch = variant.arch();
        let target = arch.musl_target();
        match self {
            Program::Cargo { package, features } => {
                let wanted: Vec<&str> =
                    features.iter().filter(|(_, variants)| variants.has(variant)).map(|(name, _)| *name).collect();
                let mut args: Vec<String> =
                    ["build", "-p", package, "--profile", "user", "--target", target].iter().map(|s| s.to_string()).collect();
                if !wanted.is_empty() {
                    args.push("--features".into());
                    args.push(wanted.join(","));
                }
                Ok(Plan {
                    steps: vec![Step { program: "cargo".into(), args, env: Vec::new(), dir: root.to_path_buf() }],
                    output: root.join("target").join(target).join("user").join(package),
                })
            }
            Program::MuslC { source, name } => {
                let toolchain = toolchain()?;
                let rustlib = toolchain.sysroot.join("lib/rustlib");
                let musl = rustlib.join(target).join("lib/self-contained");
                let work = root.join("target").join(target).join("distro");
                let object = work.join(format!("{name}.o"));
                let output = work.join(name);
                let path = |p: PathBuf| p.to_string_lossy().into_owned();
                let mut link = vec!["-flavor".into(), "gnu".into(), "-o".into(), path(output.clone())];
                link.extend(["--no-pie", "-e", "_start"].map(String::from));
                let mut inputs = vec![path(musl.join("crt1.o")), path(musl.join("crti.o")), path(object.clone()), path(musl.join("libc.a"))];
                if arch == Arch::Aarch64 {
                    // `long double` is 128-bit on aarch64 and the processor has
                    // no instructions for it, so musl's printf calls out to the
                    // soft-float helpers (__addtf3 and the rest). x86-64 has
                    // those in hardware, which is why it needs nothing beyond
                    // libc. The only build of them on this machine is rustc's
                    // own compiler_builtins; it carries a reference to
                    // rust_eh_personality from an unwinding table that nothing
                    // here executes, so the symbol is defined away.
                    link.extend(["--defsym", "rust_eh_personality=0"].map(String::from));
                    inputs.push(path(compiler_builtins(&rustlib.join(target).join("lib"))?));
                }
                inputs.push(path(musl.join("crtn.o")));
                link.extend(inputs);
                Ok(Plan {
                    steps: vec![
                        Step {
                            program: "clang".into(),
                            args: [
                                &format!("--target={target}"),
                                "-O2",
                                "-ffreestanding",
                                "-nostdinc",
                                "-fno-stack-protector",
                                "-fno-builtin",
                                "-c",
                                &path(root.join(source)),
                                "-o",
                                &path(object.clone()),
                            ]
                            .map(String::from)
                            .to_vec(),
                            env: Vec::new(),
                            dir: root.to_path_buf(),
                        },
                        Step {
                            program: path(rustlib.join(&toolchain.host).join("bin/rust-lld")),
                            args: link,
                            env: Vec::new(),
                            dir: root.to_path_buf(),
                        },
                    ],
                    output,
                })
            }
            Program::Go { dir, name } => {
                let output = root.join("target").join(target).join("distro").join(name);
                Ok(Plan {
                    steps: vec![Step {
                        program: "go".into(),
                        args: vec!["build".into(), "-trimpath".into(), "-o".into(), output.to_string_lossy().into_owned(), ".".into()],
                        env: vec![
                            ("CGO_ENABLED".into(), "0".into()),
                            ("GOOS".into(), "linux".into()),
                            ("GOARCH".into(), arch.goarch().into()),
                        ],
                        dir: root.join(dir),
                    }],
                    output,
                })
            }
        }
    }
}

/// The compiler_builtins rlib in a target's library directory.
fn compiler_builtins(lib: &Path) -> Result<PathBuf, String> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(lib)
        .map_err(|e| format!("{}: {e}; is the target installed? rustup target add it", lib.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("libcompiler_builtins-") && name.ends_with(".rlib")
        })
        .collect();
    found.sort();
    found.into_iter().next().ok_or_else(|| format!("{} has no libcompiler_builtins-*.rlib", lib.display()))
}

impl Plan {
    /// Run every step in order, stopping at the first that fails.
    pub fn run(&self) -> Result<(), String> {
        let dir = self.output.parent().unwrap();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for step in &self.steps {
            let status = Command::new(&step.program)
                .args(&step.args)
                .envs(step.env.iter().map(|(k, v)| (k, v)))
                .current_dir(&step.dir)
                .status()
                .map_err(|e| format!("could not run {}: {e}", step.program))?;
            if !status.success() {
                return Err(format!("{step} failed ({status})"));
            }
        }
        Ok(())
    }
}

impl Step {
    /// The command as a line to read: paths in the repository from its root,
    /// and paths in the toolchain from $SYSROOT.
    pub fn show(&self, root: &Path) -> String {
        let mut text = self.to_string();
        if let Ok(toolchain) = toolchain() {
            text = text.replace(&format!("{}/", toolchain.sysroot.display()), "$SYSROOT/");
        }
        text = text.replace(&format!("{}/", root.display()), "");
        if self.dir != root {
            text = format!("(cd {} && {text})", self.dir.strip_prefix(root).unwrap_or(&self.dir).display());
        }
        text
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (key, value) in &self.env {
            write!(f, "{key}={value} ")?;
        }
        write!(f, "{}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}
