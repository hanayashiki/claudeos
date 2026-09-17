//! The userland images, the card's /data and its boot files, as one tree.
//!
//! There are three variants. test-x86_64 and test-aarch64 are what
//! scripts/test.sh boots under QEMU: the suites, the programs they drive and
//! the rtest applet. board-aarch64 is what a Raspberry Pi 4 boots, and it holds
//! only what the kernel and init need to boot and to reach the network and the
//! console without the card's data partition. Every other program a board runs
//! is on that partition, /data, which the kernel makes /usr (README.md,
//! "Programs on the card"). Only the board variant has a card, because the
//! board is a Raspberry Pi 4.
//!
//! A variant's folder, build/distro/VARIANT, holds:
//!
//!   root/   the image, packed into build/distro/VARIANT/initramfs.cpio
//!   data/   board only: what a new card's /data starts with, which
//!           scripts/mkcard.sh --new copies there and --usr copies the usr
//!           part of
//!   boot/   board only: the card's boot partition but for the two files
//!           scripts/mkcard.sh adds, the kernel and the image
//!
//! Moving a node from one variant to another is a change to its variants.
//! Nothing gets into a folder that is not declared here, except the contents
//! of a directory copied or unpacked whole.

use std::path::Path;

use crate::applets;
use crate::checksums::{sha1_hex, Elf};
use crate::downloads::*;
use crate::programs::Program;
use crate::tree::{dir, file, link, Body, Context, Contents, Generated, Node, Source};
use crate::variant::*;

const EXEC: u32 = 0o755;
const READ: u32 = 0o644;

/// The features cbox is built with, and the variants it is built with each
/// for. An applet the table in user/cbox/src/main.rs compiles only with a
/// feature gets its link in exactly these variants.
const CBOX_FEATURES: &[(&str, Variants)] = &[
    // The rtest applet, the Rust standard library suite.
    ("rtest", TESTS),
];

/// The name the board image has on the card's boot partition, which
/// config.txt gives the firmware and scripts/mkcard.sh reads out of it.
const BOARD_IMAGE_ON_CARD: &str = "initramfs-aarch64-board.cpio";

pub fn tree(root: &Path) -> Result<Node, String> {
    let applets = applets::read(&root.join("user/cbox/src/main.rs"))?;
    let tree = dir("", IMAGES).holding(vec![
        dir("root", IMAGES).note("the image, packed into initramfs.cpio").holding(vec![
            dir("bin", IMAGES).holding(bin(&applets)),
            dir("dev", IMAGES),
            dir("etc", IMAGES).holding(etc()),
            dir("lib", AARCH64).holding(lib()),
            dir("proc", IMAGES),
            // Empty, as /usr is absent: once the card is mounted the kernel
            // replaces both with links into /data, and whatever the image
            // held there would be out of reach. check_image refuses anything
            // under either.
            dir("root", IMAGES).note("the home directory, empty until the card replaces it"),
            dir("tests", TESTS).holding(tests()),
            dir("tmp", IMAGES),
        ]),
        dir("data", BOARD).note("what a new card's /data starts with").holding(data()),
        dir("boot", BOARD).note("the card's boot partition, without kernel8.img and the image").holding(boot()),
    ]);
    check_image(&tree)?;
    Ok(tree)
}

fn bin(applets: &[applets::Applet]) -> Vec<Node> {
    let mut nodes = vec![
        file("cbox", IMAGES, EXEC, Source::Program(Program::Cargo { package: "cbox", features: CBOX_FEATURES }))
            .note("the shell, init and the applets")
            .checked(),
    ];
    // A link for every applet the variant's cbox has.
    for applet in applets.iter().filter(|applet| applet.name != "cbox") {
        let variants = match &applet.feature {
            None => IMAGES,
            Some(feature) => match CBOX_FEATURES.iter().find(|(name, _)| name == feature) {
                Some((_, variants)) => *variants,
                None => continue,
            },
        };
        nodes.push(link(&applet.name, variants, "cbox").note("an applet of cbox"));
    }
    // "[" cannot appear in the applet table's identifier list.
    nodes.push(link("[", IMAGES, "cbox").note("the test applet"));

    nodes.extend([
        // A board runs /usr/bin/busybox from the card.
        file("busybox", TEST_X86_64, EXEC, Source::Download(&BUSYBOX_X86_64))
            .note("upstream busybox, for the suites"),
        file("busybox", TEST_AARCH64, EXEC, Source::Member(&BUSYBOX_AARCH64))
            .note("upstream busybox, for the suites"),
        file("busybox-extras", TEST_AARCH64, EXEC, Source::Member(&BUSYBOX_EXTRAS_AARCH64))
            .note("httpd, telnetd and nc from Alpine"),
        // The web server the suites name as /bin/httpd on either machine.
        // busybox.net's x86-64 busybox has httpd of its own; Alpine's aarch64
        // one does not.
        link("httpd", TEST_X86_64, "busybox").note("the web server"),
        link("httpd", TEST_AARCH64, "busybox-extras").note("the web server"),
        file("cloudflared", TEST_X86_64, EXEC, Source::Download(&CLOUDFLARED_X86_64))
            .note("the Cloudflare tunnel client"),
        file("cloudflared", TEST_AARCH64, EXEC, Source::Download(&CLOUDFLARED_AARCH64))
            .note("the Cloudflare tunnel client"),
        // A C program built against musl, to show the ABI is not Rust-specific.
        file("hello_c", TESTS, EXEC, Source::Program(Program::MuslC { source: "user/c/hello.c", name: "hello_c" }))
            .note("a C program built against musl, left out without clang")
            .optional(),
        // Sockets, through the standard library rather than through this
        // project's own code: `inet` on its own is the socket suite, `inet
        // serve` an HTTP server.
        file("inet", TESTS, EXEC, Source::Program(Program::Cargo { package: "inet", features: &[] }))
            .note("the socket suite and a small HTTP server"),
    ]);
    nodes
}

fn etc() -> Vec<Node> {
    vec![
        dir("claudeos", IMAGES).holding(vec![
            file("checksums", IMAGES, READ, Source::Generated(Generated { what: "the boot check's manifest", make: manifest }))
                .note("what the kernel checks at boot"),
            // The system services init starts at boot, the same list in every
            // image. It decides what runs at every boot, so it is checked.
            file("services", IMAGES, READ, Source::Repo("user/services"))
                .note("the system services init starts")
                .checked(),
        ]),
        file("hostname", IMAGES, READ, Source::Text("claudeos\n")).note("the machine name"),
        file("motd", IMAGES, READ, Source::Generated(Generated { what: "the welcome text, naming commands this image has", make: motd }))
            .note("the welcome text"),
        // The servers BusyBox ntpd asks. The ntpd line in user/services says
        // needs=/etc/ntp.conf, so init starts ntpd only in an image that has
        // this file, which is how a board with no battery-backed clock learns
        // the date. The test images the suites boot never wait on servers
        // across the internet; the network time section of scripts/test.sh
        // adds this file to a copy.
        file("ntp.conf", BOARD, READ, Source::Text("server ntp.nict.jp\nserver time.cloudflare.com\n"))
            .note("the time servers ntpd asks to set the clock"),
        file("passwd", IMAGES, READ, Source::Text("root:x:0:0:root:/root:/bin/sh\n")).note("the root account"),
        // No resolv.conf: the kernel writes it once it has a network
        // configuration, from `nameserver=` or from the DHCP lease, so a fixed
        // one here would name a server the machine may not be able to reach.
        dir("ssl", IMAGES).note("the certificate store, for programs with their own TLS").holding(vec![
            link("cert.pem", IMAGES, "certs/ca-certificates.crt"),
            dir("certs", IMAGES).holding(vec![
                file("ca-certificates.crt", IMAGES, READ, Source::Member(&CA_CERTIFICATES)),
            ]),
        ]),
        // The network to join, if build/wifi.conf exists. It holds a
        // passphrase, so it is copied and never printed, and the kernel
        // removes it from the running system once it has read it.
        file("wifi.conf", AARCH64, 0o600, Source::Input("build/wifi.conf"))
            .note("the network to join, left out when build/wifi.conf does not exist")
            .optional(),
    ]
}

fn lib() -> Vec<Node> {
    vec![
        // The WiFi chip's firmware, NVRAM and regulatory data, under the names
        // and the directory Linux's brcmfmac uses. The kernel loads them
        // before there is a network, from the image, so they are checked.
        dir("firmware", AARCH64).holding(vec![dir("brcm", AARCH64).holding(vec![
            file("brcmfmac43455-sdio.bin", AARCH64, READ, Source::Download(&WIFI_FIRMWARE_BIN)).checked(),
            file("brcmfmac43455-sdio.clm_blob", AARCH64, READ, Source::Download(&WIFI_FIRMWARE_CLM_BLOB)).checked(),
            file("brcmfmac43455-sdio.txt", AARCH64, READ, Source::Download(&WIFI_FIRMWARE_TXT)).checked(),
        ])]),
        // The musl dynamic loader, which is also musl's libc, at the path
        // busybox-extras's program header names. The test image holds it; on a
        // board it is on the card, and the path is a link there.
        file("ld-musl-aarch64.so.1", TEST_AARCH64, EXEC, Source::Member(&MUSL_LOADER_AARCH64))
            .note("the loader and libc busybox-extras runs under"),
        link("ld-musl-aarch64.so.1", BOARD, "/usr/lib/ld-musl-aarch64.so.1")
            .note("where the loader busybox-extras names is on a board"),
    ]
}

fn tests() -> Vec<Node> {
    vec![
        file("busybox.sh", TESTS, EXEC, Source::Repo("tests/busybox.sh")).note("the upstream busybox suite"),
        // Only the Pi 4 has a card slot.
        file("data.sh", TEST_AARCH64, EXEC, Source::Repo("tests/data.sh"))
            .note("the /data suite, run with a card in the emulated slot"),
        file("demo.sh", TESTS, EXEC, Source::Repo("tests/demo.sh")).note("the scripted tour"),
        // Built from source for the variant's machine rather than copied, so
        // it is never the wrong architecture or a stale build. Go needs no libc
        // and links statically with cgo off.
        file("go_main", TESTS, EXEC, Source::Program(Program::Go { dir: "user/go", name: "go_main" }))
            .note("the Go program under user/go, left out when it does not build")
            .optional(),
        file("hello.txt", TESTS, READ, Source::Text("This file came from the initramfs, unpacked by the kernel at boot.\n"))
            .note("a file to read back"),
        file("suite.sh", TESTS, EXEC, Source::Repo("tests/suite.sh")).note("the userland suite"),
    ]
}

/// What a new card's /data starts with. FAT has no symbolic links, so an
/// applet name such as httpd cannot be a link to busybox-extras.
/// /usr/bin/httpd is a two-line script that runs the applet instead: a copy
/// would be a second busybox-extras to keep in step with the first, and a link
/// in the image would put the card's layout in the image and give the program
/// a name outside /usr/bin.
fn data() -> Vec<Node> {
    vec![
        dir("root", BOARD).note("the home directory, /root"),
        file("services.txt", BOARD, READ, Source::Repo("user/data/services.txt")).note("the user services list"),
        dir("site", BOARD).contents(Contents::Repo("user/data/site")).note("the page the site service serves"),
        dir("usr", BOARD).holding(vec![
            dir("bin", BOARD).holding(vec![
                file("busybox", BOARD, EXEC, Source::Member(&BUSYBOX_AARCH64))
                    .note("upstream busybox: ntpd, which the system list starts, and the other applets"),
                file("busybox-extras", BOARD, EXEC, Source::Member(&BUSYBOX_EXTRAS_AARCH64))
                    .note("httpd, telnetd and nc from Alpine"),
                file("cloudflared", BOARD, EXEC, Source::Download(&CLOUDFLARED_AARCH64))
                    .note("the Cloudflare tunnel client"),
                file("httpd", BOARD, EXEC, Source::Repo("user/data/usr/bin/httpd")).note("runs busybox-extras httpd"),
            ]),
            dir("lib", BOARD).holding(vec![
                file("ld-musl-aarch64.so.1", BOARD, EXEC, Source::Member(&MUSL_LOADER_AARCH64))
                    .note("the musl loader busybox-extras names, through the image's link at /lib"),
            ]),
        ]),
    ]
}

/// The card's boot partition. A Pi 4's bootloader lives in an EEPROM on the
/// board and reads the first FAT partition and nothing else, finding files by
/// name. scripts/mkcard.sh adds kernel8.img and the board image.
fn boot() -> Vec<Node> {
    vec![
        file("bcm2711-rpi-4-b.dtb", BOARD, READ, Source::Download(&PI_BCM2711_RPI_4_B_DTB))
            .note("the device tree describing this board"),
        file("cmdline.txt", BOARD, READ, Source::Generated(Generated { what: "the kernel's command line", make: cmdline }))
            .note("the kernel's command line"),
        file("config.txt", BOARD, READ, Source::Generated(Generated { what: "the firmware's settings", make: config }))
            .note("what the firmware reads before anything else"),
        file("fixup4.dat", BOARD, READ, Source::Download(&PI_FIXUP4_DAT))
            .note("how the firmware splits memory with the video core"),
        dir("overlays", BOARD).holding(vec![
            file("disable-bt.dtbo", BOARD, READ, Source::Download(&PI_DISABLE_BT_DTBO))
                .note("moves the full serial port to the header pins"),
        ]),
        file("start4.elf", BOARD, READ, Source::Download(&PI_START4_ELF)).note("the firmware the EEPROM bootloader loads"),
    ]
}

/// Refuse a node of an image under /usr or /root: once the card is mounted the
/// kernel replaces both with links into /data, and whatever the image held
/// there is out of reach.
fn check_image(tree: &Node) -> Result<(), String> {
    let mut errors = Vec::new();
    for variant in IMAGES.list() {
        let Some(image) = tree.find("root", variant) else { continue };
        for top in ["usr", "root"] {
            if let Some(node) = image.child(top, variant) {
                if !matches!(&node.body, Body::Dir { contents: Contents::Declared, children, .. } if children.is_empty()) {
                    errors.push(format!(
                        "root/{top} in {variant} holds something; the kernel replaces /{top} with a link into /data once the card is mounted"
                    ));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// /etc/claudeos/checksums: the digest of the kernel, then of every file of the
/// image declared checked(), in the order they are declared.
///
/// The programs on the card are not in it: the check runs before the card is
/// mounted, and what is on /data is the user's to change. /etc/wifi.conf is
/// not either: it is the user's configuration, not software.
fn manifest(context: &Context) -> Result<Vec<u8>, String> {
    let mut lines = String::new();
    let kernel = context.root.join(context.variant.arch().kernel_elf());
    if kernel.is_file() {
        lines += &format!("{}  kernel\n", Elf::read(&kernel)?.kernel_digest()?);
    } else {
        println!(
            "warning: {} is missing, so the manifest has no kernel line; build the kernel before the userland",
            context.variant.arch().kernel_elf()
        );
    }
    let image = context.tree.find("root", context.variant).ok_or("the variant has no root")?;
    let mut failure = None;
    image.walk(context.variant, &mut |path, node| {
        if let Body::File { checked: true, .. } = &node.body {
            let at = context.folder.join("root").join(path);
            match std::fs::read(&at) {
                Ok(data) => lines += &format!("{}  /{path}\n", sha1_hex(&data)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("note: /{path} is not in this image, so the manifest has no line for it")
                }
                Err(e) => failure = Some(format!("{}: {e}", at.display())),
            }
        }
    });
    match failure {
        Some(e) => Err(e),
        None => Ok(lines.into_bytes()),
    }
}

/// /etc/motd, suggesting only commands the image has.
fn motd(context: &Context) -> Result<Vec<u8>, String> {
    let image = context.folder.join("root");
    let mut text = format!(
        "Welcome to claudeos.

This is a kernel written from scratch in Rust that implements enough of the
Linux system call interface to run unmodified static Linux binaries. The
userland you are talking to was built for {}.

Try:  ls -l /bin | head      ps      free      cat /proc/cpuinfo
",
        context.variant.arch().musl_target()
    );
    if image.join("tests/demo.sh").is_file() {
        text += "      echo hi | tr a-z A-Z   sh /tests/demo.sh\n";
    } else {
        text += "      echo hi | tr a-z A-Z   cat /proc/claudeos/integrity\n";
    }
    if image.join("bin/rtest").is_symlink() && image.join("bin/hello_c").is_file() {
        text += "      rtest                  hello_c 60\n";
    }
    Ok(text.into_bytes())
}

/// config.txt, which the firmware reads before it loads anything else.
///
/// arm_64bit    start the processor in 64-bit mode and look for kernel8.img.
/// enable_uart  turn the serial console on and hold the clock steady.
/// dtoverlay    move the full serial port to the pins on the header. Without
///              it that port is wired to the Bluetooth radio and a cable on
///              the header sees nothing, whatever the kernel writes.
/// initramfs    load the ram disk and tell the kernel where it landed. The
///              word takes a space rather than an equals sign, which is a
///              quirk of this file rather than a mistake here. `followkernel`
///              places it directly after the kernel image.
fn config(_: &Context) -> Result<Vec<u8>, String> {
    Ok(format!(
        "arm_64bit=1\nenable_uart=1\ndtoverlay=disable-bt\nkernel=kernel8.img\ninitramfs {BOARD_IMAGE_ON_CARD} followkernel\n"
    )
    .into_bytes())
}

/// cmdline.txt, which the firmware passes to the kernel as its command line
/// through the device tree.
///
/// net=wifi  bring up the WiFi rather than the wired port, when the image
///           carries a network to join in /etc/wifi.conf. The kernel's
///           network stack holds one interface, and without this word it
///           takes the wired port whether or not a cable is plugged in.
fn cmdline(context: &Context) -> Result<Vec<u8>, String> {
    let wifi = context.folder.join("root/etc/wifi.conf").is_file();
    Ok(if wifi { "net=wifi init=/bin/init\n" } else { "init=/bin/init\n" }.as_bytes().to_vec())
}
