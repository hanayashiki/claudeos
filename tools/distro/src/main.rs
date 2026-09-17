//! distro: the userland images, the card and the Alpine fixtures, declared as
//! trees in src/images.rs and src/alpine.rs, and the folders and cpio archives
//! built from them.
//!
//! A variant is built in two stages. Its folder, build/distro/VARIANT, holds
//! the image's tree in root/ (and, for the board, the card's data/ and boot/),
//! and can be looked at or changed; the image, build/distro/VARIANT/
//! initramfs.cpio, is then packed from root/. Downloads are kept in
//! build/cache (src/cache.rs), and programs are built into target/.

mod alpine;
mod applets;
mod cache;
mod checksums;
mod cpio;
mod downloads;
mod folder;
mod images;
mod programs;
mod tree;
mod variant;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cache::Cache;
use tree::{Body, Contents, Node, Source};
use variant::Variant;

const USAGE: &str = "\
usage: cargo run --release -p distro -- COMMAND

  build VARIANT|all          build the folder, then pack the image from it
  folder VARIANT|all         build the folder build/distro/VARIANT only
  cpio VARIANT|all           pack build/distro/VARIANT/initramfs.cpio from the
                             folder as it is
  list VARIANT|all           print the declared tree and where each node comes from
  fetch [VARIANT|all]        fill the download cache, build/cache, and nothing else

  pack DIR OUT               pack any directory into a cpio archive, as for a folder
  kernel-digest ELF          print the digest of the kernel's checked bytes
  flip-code-byte ELF IMAGE OUT
                             copy the kernel image IMAGE to OUT with the last byte
                             of .text changed, for the boot check's test

variants: test-x86_64, test-aarch64, board-aarch64, alpine-x86_64, alpine-aarch64";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("distro: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The repository this program was built from.
fn repository() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().and_then(Path::parent).expect("tools/distro is two levels below the root").to_path_buf()
}

fn variants(name: Option<&String>) -> Result<Vec<Variant>, String> {
    match name.map(String::as_str) {
        None | Some("all") => Ok(Variant::ALL.to_vec()),
        Some(name) => Variant::parse(name).map(|v| vec![v]).ok_or_else(|| format!("no variant named {name}\n\n{USAGE}")),
    }
}

/// The tree a variant is declared in, checked.
fn tree_for(root: &Path, variant: Variant) -> Result<Node, String> {
    let tree = match variant {
        Variant::TestX86_64 | Variant::TestAarch64 | Variant::BoardAarch64 => images::tree(root)?,
        Variant::AlpineX86_64 | Variant::AlpineAarch64 => alpine::tree(),
    };
    tree::check(&tree)?;
    Ok(tree)
}

fn run(args: &[String]) -> Result<(), String> {
    let root = repository();
    let cache = Cache::new(root.join("build/cache"));
    let command = args.first().map(String::as_str).unwrap_or("");
    let arity = |count: usize| {
        if args.len() == count + 1 {
            Ok(())
        } else {
            Err(format!("{command} takes {count} argument{}\n\n{USAGE}", if count == 1 { "" } else { "s" }))
        }
    };
    match command {
        "build" | "folder" | "cpio" | "list" => {
            arity(1)?;
            for variant in variants(args.get(1))? {
                match command {
                    "build" => {
                        build_folder(&root, &cache, variant)?;
                        pack_image(&root, variant)?;
                    }
                    "folder" => build_folder(&root, &cache, variant)?,
                    "cpio" => pack_image(&root, variant)?,
                    _ => print!("{}", list(&root, variant)?),
                }
            }
            Ok(())
        }
        "fetch" => {
            if args.len() > 2 {
                return Err(format!("fetch takes at most one argument\n\n{USAGE}"));
            }
            fetch(&root, &cache, &variants(args.get(1))?)
        }
        "pack" => {
            arity(2)?;
            let (source, target) = (Path::new(&args[1]), Path::new(&args[2]));
            let packed = cpio::pack(source, target)?;
            println!("{}: {} entries, {} bytes of file data", target.display(), packed.entries, packed.data_bytes);
            Ok(())
        }
        "kernel-digest" => {
            arity(1)?;
            println!("{}", checksums::Elf::read(Path::new(&args[1]))?.kernel_digest()?);
            Ok(())
        }
        "flip-code-byte" => {
            arity(3)?;
            let elf = checksums::Elf::read(Path::new(&args[1]))?;
            println!("{}", checksums::flip_code_byte(&elf, Path::new(&args[2]), Path::new(&args[3]))?);
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

fn build_folder(root: &Path, cache: &Cache, variant: Variant) -> Result<(), String> {
    println!("== {variant}: build/distro/{variant}");
    let tree = tree_for(root, variant)?;
    let folder = folder::build(root, cache, &tree, variant)?;
    let manifest = folder.join("root/etc/claudeos/checksums");
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        println!("/etc/claudeos/checksums in build/distro/{variant}/root:");
        for line in text.lines() {
            println!("  {line}");
        }
    }
    Ok(())
}

fn pack_image(root: &Path, variant: Variant) -> Result<(), String> {
    let folder = folder::folder_path(root, variant);
    if !folder.join("root").is_dir() {
        return Err(format!("build/distro/{variant}/root does not exist; run: distro folder {variant}"));
    }
    let target = folder.join("initramfs.cpio");
    let packed = cpio::pack(&folder.join("root"), &target)?;
    println!("build/distro/{variant}/initramfs.cpio: {} entries, {} bytes of file data", packed.entries, packed.data_bytes);
    Ok(())
}

fn fetch(root: &Path, cache: &Cache, variants: &[Variant]) -> Result<(), String> {
    // Every download and member the variants name, once each, by sha256.
    let mut wanted: BTreeMap<&'static str, Source> = BTreeMap::new();
    let mut trees = Vec::new();
    for &variant in variants {
        trees.push((variant, tree_for(root, variant)?));
    }
    for (variant, tree) in &trees {
        tree.walk(*variant, &mut |_, node| match &node.body {
            Body::File { source: Source::Download(d), .. } | Body::Dir { contents: Contents::Unpack(d), .. } => {
                wanted.entry(d.sha256).or_insert(Source::Download(d));
            }
            Body::File { source: Source::Member(m), .. } => {
                wanted.entry(m.sha256).or_insert(Source::Member(m));
            }
            _ => {}
        });
    }
    for (sha256, source) in &wanted {
        let had = cache.has(sha256)?;
        let what = match source {
            Source::Download(d) => {
                cache.download(d)?;
                d.url.to_string()
            }
            Source::Member(m) => {
                cache.member(m)?;
                format!("{} in {}", m.path, m.archive.url)
            }
            _ => unreachable!(),
        };
        println!("{}  {sha256}  {what}", if had { "have   " } else { "fetched" });
    }
    println!("in {}", cache.path("").display());
    Ok(())
}

/// The declared tree of a variant, one node per line: its path, its mode or
/// link target, where it comes from, and what it is.
fn list(root: &Path, variant: Variant) -> Result<String, String> {
    let tree = tree_for(root, variant)?;
    let mut out = format!("{variant}, in build/distro/{variant}:\n");
    let mut failure = None;
    tree.walk(variant, &mut |path, node| {
        let depth = path.matches('/').count();
        let mut name = format!("{}{}", "  ".repeat(depth), node.name);
        let (mode, source) = match &node.body {
            Body::Dir { mode, contents, .. } => {
                name.push('/');
                let source = match contents {
                    Contents::Declared => String::new(),
                    Contents::Repo(dir) => format!("a copy of {dir}"),
                    Contents::Unpack(d) => format!("unpacked from {} (sha256 {})", d.url, short(d.sha256)),
                };
                (format!("{mode:04o}"), source)
            }
            Body::Link { target } => (format!("-> {target}"), String::new()),
            Body::File { mode, source, optional, checked } => {
                let mut text = match source {
                    Source::Repo(file) => file.to_string(),
                    Source::Input(file) => format!("{file}, if it exists"),
                    Source::Text(text) => format!("text {text:?}"),
                    Source::Download(d) => format!("{} (sha256 {})", d.url, short(d.sha256)),
                    Source::Member(m) => {
                        format!("{} (sha256 {}) in {} (sha256 {})", m.path, short(m.sha256), m.archive.url, short(m.archive.sha256))
                    }
                    Source::Program(program) => match program.plan(root, variant) {
                        Ok(plan) => plan.steps.iter().map(|step| step.show(root)).collect::<Vec<_>>().join(" && "),
                        Err(e) => {
                            failure = Some(e);
                            String::new()
                        }
                    },
                    Source::Generated(generated) => format!("generated: {}", generated.what),
                };
                if *optional {
                    text += "; optional";
                }
                if *checked {
                    text += "; checked at boot";
                }
                (format!("{mode:04o}"), text)
            }
        };
        let mut line = format!("{name:<34} {mode:<6}");
        if !source.is_empty() {
            let _ = write!(line, " {source}");
        }
        if !node.note.is_empty() {
            let _ = write!(line, "  # {}", node.note);
        }
        out += line.trim_end();
        out.push('\n');
    });
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

fn short(sha256: &str) -> &str {
    &sha256[..12]
}
