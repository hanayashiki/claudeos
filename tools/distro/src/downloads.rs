//! Every file the trees take from outside the repository, pinned by sha256.
//!
//! A download is fetched once into the cache (src/cache.rs) and checked
//! against its sha256 before anything uses it; a file taken out of an archive
//! is checked against its own. A rerun gets the same bytes or stops.

/// A file fetched from `url`.
pub struct Download {
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The file at `path` inside the tar archive `archive`.
pub struct Member {
    pub archive: &'static Download,
    pub path: &'static str,
    pub sha256: &'static str,
}

// ---- busybox ---------------------------------------------------------------
//
// An upstream static busybox, to test against a binary this project did not
// build. busybox.net publishes prebuilt binaries for x86-64 and for 32-bit ARM,
// but none for aarch64, so that one comes from Alpine's busybox-static package.
// An .apk is a tarball of concatenated gzip streams, here with the binary at
// bin/busybox.static.
//
// busybox.net's x86-64 build has httpd. Alpine's busybox-static does not:
// Alpine builds httpd, telnetd and nc into busybox-extras, and publishes that
// only dynamically linked. So on aarch64 bin/busybox-extras comes out of that
// package, and lib/ld-musl-aarch64.so.1, the interpreter its program header
// names, which is musl's libc as well, out of Alpine's musl package, from the
// same Alpine release. The kernel hands a program with an interpreter to it.

pub static BUSYBOX_X86_64: Download = Download {
    url: "https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox",
    sha256: "6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348",
};

static BUSYBOX_STATIC_APK: Download = Download {
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.19/main/aarch64/busybox-static-1.36.1-r21.apk",
    sha256: "d574d8c6434223367f140bd16d34f09bd01a4ea11e480c2e735ec2e2cfcc3346",
};

pub static BUSYBOX_AARCH64: Member = Member {
    archive: &BUSYBOX_STATIC_APK,
    path: "bin/busybox.static",
    sha256: "f83aa5afa7ea458a57a5ee1e1d2c80a4f2754e11221b12236e18500426cce7b4",
};

static BUSYBOX_EXTRAS_APK: Download = Download {
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.19/main/aarch64/busybox-extras-1.36.1-r21.apk",
    sha256: "8771a6f036f628925167b076e6c78eecf1424efd607416eb3776eff6d313da18",
};

pub static BUSYBOX_EXTRAS_AARCH64: Member = Member {
    archive: &BUSYBOX_EXTRAS_APK,
    path: "bin/busybox-extras",
    sha256: "23df02a3eb3278ddbeb65dbeb317a73efd7cf5bdae606d34b10d3bec8b91a545",
};

static MUSL_APK: Download = Download {
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.19/main/aarch64/musl-1.2.4_git20230717-r6.apk",
    sha256: "6685d4a557c963a198426edbf9ecc66fa1f12bfe51185df69173984d67bd1b40",
};

pub static MUSL_LOADER_AARCH64: Member = Member {
    archive: &MUSL_APK,
    path: "lib/ld-musl-aarch64.so.1",
    sha256: "7d605adcd1cdc4d37251f0e8e3099954e13554323aabfd5e9cf47c10ff63b76c",
};

// ---- cloudflared -----------------------------------------------------------
//
// Cloudflare's own static cloudflared build, version 2026.9.1. It is a static
// Go program, and Go brings its own threads, its own resolver and its own TLS
// rather than calling a libc for any of them, so it asks for things nothing
// built against musl here has asked for. Cloudflare names the binaries by Go's
// architecture words, and lists the same sha256s in the release notes.

pub static CLOUDFLARED_X86_64: Download = Download {
    url: "https://github.com/cloudflare/cloudflared/releases/download/2026.9.1/cloudflared-linux-amd64",
    sha256: "03f1f25d1cc93b9ad6c60569d44060bc4f17ed97075760ed8cfca4b12dcd68cc",
};

pub static CLOUDFLARED_AARCH64: Download = Download {
    url: "https://github.com/cloudflare/cloudflared/releases/download/2026.9.1/cloudflared-linux-arm64",
    sha256: "3d97437c71848bd8df68041e12436b484a661d95073ea1937f01a845ce88faa3",
};

// ---- Alpine ----------------------------------------------------------------
//
// Alpine's minirootfs, release 3.19.1. Everything in it is dynamically linked
// against musl and loaded by Alpine's own ld-musl, so booting it exercises the
// program interpreter path with a userland this project had no hand in
// building. The sha256s are the ones Alpine publishes beside the archives. The
// images take their certificate store from it, which is the same file for both
// machines.

pub static ALPINE_MINIROOTFS_X86_64: Download = Download {
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.19/releases/x86_64/alpine-minirootfs-3.19.1-x86_64.tar.gz",
    sha256: "185123ceb6e7d08f2449fff5543db206ffb79decd814608d399ad447e08fa29e",
};

pub static ALPINE_MINIROOTFS_AARCH64: Download = Download {
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.19/releases/aarch64/alpine-minirootfs-3.19.1-aarch64.tar.gz",
    sha256: "7ef5eef3a5b1d198dfb1610cde1ef5b0755ff5d838fb1e5e1b9f42b59214820f",
};

pub static CA_CERTIFICATES: Member = Member {
    archive: &ALPINE_MINIROOTFS_AARCH64,
    path: "etc/ssl/certs/ca-certificates.crt",
    sha256: "824cefcee69de918c76b7b92776f304c3a4b7f6281539118bc1d41a9dd8476d9",
};

// ---- the Pi 4's WiFi firmware ----------------------------------------------
//
// The firmware, NVRAM and regulatory data for the Raspberry Pi 4's WiFi chip,
// a Cypress CYW43455. They are the ones Raspberry Pi OS installs, from
// github.com/RPi-Distro/firmware-nonfree at one commit of the bookworm branch,
// which is what Raspberry Pi OS bookworm installs.
//
// Linux's brcmfmac asks for brcmfmac43455-sdio.raspberrypi,4-model-b.bin,
// .clm_blob and .txt on this board. In that repository those three names are
// symbolic links:
//
//   .bin       -> ../cypress/cyfmac43455-sdio.bin
//   .clm_blob  -> ../cypress/cyfmac43455-sdio.clm_blob
//   .txt       -> brcmfmac43455-sdio.txt
//
// and cyfmac43455-sdio.bin is not a file there either: the package's postinst
// makes it an alternative between -standard.bin, at priority 50, and
// -minimal.bin, at priority 10, so an installed system gets -standard. Those
// are the real files below, which the images hold under the names brcmfmac
// asks for.

pub static WIFI_FIRMWARE_BIN: Download = Download {
    url: "https://raw.githubusercontent.com/RPi-Distro/firmware-nonfree/c91cd2804cf7463aab913e7247c176049f16bbd6/debian/config/brcm80211/cypress/cyfmac43455-sdio-standard.bin",
    sha256: "d608f866582519c0a28d86db43040f4f1b98dd1d153e72e9752586546b4a36c3",
};

pub static WIFI_FIRMWARE_CLM_BLOB: Download = Download {
    url: "https://raw.githubusercontent.com/RPi-Distro/firmware-nonfree/c91cd2804cf7463aab913e7247c176049f16bbd6/debian/config/brcm80211/cypress/cyfmac43455-sdio.clm_blob",
    sha256: "9823842cae9fb9a5dd1e5fb31f595516ec7deee341354bef30bb3026eee29cc1",
};

pub static WIFI_FIRMWARE_TXT: Download = Download {
    url: "https://raw.githubusercontent.com/RPi-Distro/firmware-nonfree/c91cd2804cf7463aab913e7247c176049f16bbd6/debian/config/brcm80211/brcm/brcmfmac43455-sdio.txt",
    sha256: "ca709be81a78bdb6932936374f39943acbd7af07fae6151011127599a3ce9e3d",
};

// ---- the Pi 4's boot firmware ----------------------------------------------
//
// From github.com/raspberrypi/firmware at commit ae2a7dc5 of 2026-09-11,
// "firmware: Fix broken IMX500 auto-detection", the master the card has booted
// with since 2026-09-13 (firmware build a089929a of Sep 11 2026). start4.elf is
// the firmware the EEPROM bootloader loads, fixup4.dat tells it how to split
// memory with the video core, and the two device tree files describe the board
// and move the serial port to the header pins.

pub static PI_START4_ELF: Download = Download {
    url: "https://raw.githubusercontent.com/raspberrypi/firmware/ae2a7dc5330b7ea2c7107e5c4cb6b2691355bb9c/boot/start4.elf",
    sha256: "4468c39acb5cab9578cbbcf5f1dfc38c41df61b4f621a3e24e027d1dd20192ff",
};

pub static PI_FIXUP4_DAT: Download = Download {
    url: "https://raw.githubusercontent.com/raspberrypi/firmware/ae2a7dc5330b7ea2c7107e5c4cb6b2691355bb9c/boot/fixup4.dat",
    sha256: "b6adae406e8ff1a0478ed4a8812ea55dd30f384f7415224ef4391a78b7b6b4fa",
};

pub static PI_BCM2711_RPI_4_B_DTB: Download = Download {
    url: "https://raw.githubusercontent.com/raspberrypi/firmware/ae2a7dc5330b7ea2c7107e5c4cb6b2691355bb9c/boot/bcm2711-rpi-4-b.dtb",
    sha256: "75761b73c284e26623e4d1624bff13e67bce2ae620880efd81d6571a3739fcfb",
};

pub static PI_DISABLE_BT_DTBO: Download = Download {
    url: "https://raw.githubusercontent.com/raspberrypi/firmware/ae2a7dc5330b7ea2c7107e5c4cb6b2691355bb9c/boot/overlays/disable-bt.dtbo",
    sha256: "ea69d22dedc607fee75eec57d8a4cc0f0eab93cd75393e61a64c49fbac912d02",
};
