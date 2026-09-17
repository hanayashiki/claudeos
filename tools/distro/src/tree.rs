//! The declared tree: directories, files and symbolic links, each with the
//! variants that have it and where its contents come from.
//!
//! A tree is declared once for all of its variants (src/images.rs,
//! src/alpine.rs). Building a variant's folder walks the nodes that variant
//! has, in the order they are declared.

use std::path::Path;

use crate::downloads::{Download, Member};
use crate::programs::Program;
use crate::variant::{Variant, Variants};

pub struct Node {
    /// One path component; empty for the top of a tree, which is the variant's
    /// folder itself.
    pub name: String,
    pub variants: Variants,
    /// What the node is, for `distro list`.
    pub note: &'static str,
    pub body: Body,
}

pub enum Body {
    Dir { mode: u32, contents: Contents, children: Vec<Node> },
    File { mode: u32, source: Source, optional: bool, checked: bool },
    Link { target: String },
}

/// Where a directory's contents come from, besides the children declared in it.
pub enum Contents {
    /// Nothing.
    Declared,
    /// A copy of a directory in the repository, by its path from the root: its
    /// files with mode 0755 when the repository's copy is executable and 0644
    /// otherwise, and its directories with 0755.
    Repo(&'static str),
    /// Everything in a pinned tar archive in gzip, with the modes, times and
    /// links it records.
    Unpack(&'static Download),
}

/// Where a file's bytes come from.
pub enum Source {
    /// A file in the repository, by its path from the root.
    Repo(&'static str),
    /// A file under build/ that the user provides, by its path from the root.
    /// Its contents are never printed.
    Input(&'static str),
    /// The text given.
    Text(&'static str),
    /// A pinned download.
    Download(&'static Download),
    /// A file taken out of a pinned download.
    Member(&'static Member),
    /// The file a program writes; the build runs the program.
    Program(Program),
    /// Content made in code, once every other file of the variant is in its
    /// folder, since it may read them.
    Generated(Generated),
}

pub struct Generated {
    /// What it is made of, for `distro list`.
    pub what: &'static str,
    pub make: fn(&Context) -> Result<Vec<u8>, String>,
}

/// What a generator can see.
pub struct Context<'a> {
    pub root: &'a Path,
    pub variant: Variant,
    /// The variant's folder, build/distro/VARIANT.
    pub folder: &'a Path,
    /// The tree the variant is declared in.
    pub tree: &'a Node,
}

pub fn dir(name: &str, variants: Variants) -> Node {
    Node {
        name: name.to_string(),
        variants,
        note: "",
        body: Body::Dir { mode: 0o755, contents: Contents::Declared, children: Vec::new() },
    }
}

pub fn file(name: &str, variants: Variants, mode: u32, source: Source) -> Node {
    Node {
        name: name.to_string(),
        variants,
        note: "",
        body: Body::File { mode, source, optional: false, checked: false },
    }
}

pub fn link(name: &str, variants: Variants, target: &str) -> Node {
    Node { name: name.to_string(), variants, note: "", body: Body::Link { target: target.to_string() } }
}

impl Node {
    pub fn note(mut self, note: &'static str) -> Node {
        self.note = note;
        self
    }

    /// The children of a directory.
    pub fn holding(mut self, nodes: Vec<Node>) -> Node {
        match &mut self.body {
            Body::Dir { children, .. } => children.extend(nodes),
            _ => panic!("{} is not a directory and cannot hold nodes", self.name),
        }
        self
    }

    /// The mode of a directory, 0755 unless given.
    pub fn mode(mut self, value: u32) -> Node {
        match &mut self.body {
            Body::Dir { mode, .. } => *mode = value,
            _ => panic!("{}: mode() is for directories; a file's mode is given with it", self.name),
        }
        self
    }

    pub fn contents(mut self, value: Contents) -> Node {
        match &mut self.body {
            Body::Dir { contents, .. } => *contents = value,
            _ => panic!("{} is not a directory", self.name),
        }
        self
    }

    /// A file whose source may fail to be made: a program that is not
    /// installed or does not build, or an input that does not exist. The file
    /// is then left out with a line saying so, and the build carries on.
    pub fn optional(mut self) -> Node {
        match &mut self.body {
            Body::File { optional, .. } => *optional = true,
            _ => panic!("{}: only a file can be optional", self.name),
        }
        self
    }

    /// A file the kernel checks at boot against /etc/claudeos/checksums.
    pub fn checked(mut self) -> Node {
        match &mut self.body {
            Body::File { checked, .. } => *checked = true,
            _ => panic!("{}: only a file can be checked", self.name),
        }
        self
    }

    pub fn children(&self) -> &[Node] {
        match &self.body {
            Body::Dir { children, .. } => children,
            _ => &[],
        }
    }

    /// The child named `name` that `variant` has.
    pub fn child(&self, name: &str, variant: Variant) -> Option<&Node> {
        self.children().iter().find(|node| node.name == name && node.variants.has(variant))
    }

    /// The node at `path`, a path of names separated by `/`, that `variant` has.
    pub fn find(&self, path: &str, variant: Variant) -> Option<&Node> {
        path.split('/').filter(|name| !name.is_empty()).try_fold(self, |node, name| node.child(name, variant))
    }

    /// Call `visit` with the path from this node and the node, for every node
    /// `variant` has under this one, in declaration order, a directory before
    /// what it holds.
    pub fn walk<'a>(&'a self, variant: Variant, visit: &mut dyn FnMut(&str, &'a Node)) {
        fn go<'a>(node: &'a Node, prefix: &str, variant: Variant, visit: &mut dyn FnMut(&str, &'a Node)) {
            for child in node.children().iter().filter(|child| child.variants.has(variant)) {
                let path = if prefix.is_empty() { child.name.clone() } else { format!("{prefix}/{}", child.name) };
                visit(&path, child);
                go(child, &path, variant, visit);
            }
        }
        go(self, "", variant, visit);
    }
}

/// Refuse a tree that could not be built as declared: a node with no
/// variants, a node in a variant its directory is not in, a name that is not
/// one path component, or two nodes with one path in one variant.
pub fn check(node: &Node) -> Result<(), String> {
    fn go(node: &Node, path: &str, errors: &mut Vec<String>) {
        for (index, child) in node.children().iter().enumerate() {
            let at = if path.is_empty() { child.name.clone() } else { format!("{path}/{}", child.name) };
            if child.name.is_empty() || child.name == "." || child.name == ".." || child.name.contains('/') {
                errors.push(format!("{at}: `{}` is not a name of one path component", child.name));
            }
            if child.variants.is_empty() {
                errors.push(format!("{at}: no variant has it"));
            }
            if !node.variants.covers(child.variants) {
                errors.push(format!("{at}: it is in a variant its directory is not in"));
            }
            for other in &node.children()[..index] {
                if other.name == child.name {
                    for variant in child.variants.list().filter(|v| other.variants.has(*v)) {
                        errors.push(format!("{at}: declared twice for {variant}"));
                    }
                }
            }
            if let Body::Link { target } = &child.body {
                if target.is_empty() {
                    errors.push(format!("{at}: a link with no target"));
                }
            }
            go(child, &at, errors);
        }
    }
    let mut errors = Vec::new();
    go(node, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
