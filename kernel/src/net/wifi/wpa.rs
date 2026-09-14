//! The WPA2 supplicant: the 4-way handshake and the group key handshake for a
//! network with PSK key management (AKM 00-0F-AC:2) and CCMP, run in the
//! kernel because the chip's standard firmware has no supplicant of its own.
//!
//! Follows IEEE Std 802.11-2020 clause 12.7 (12.7.2 for EAPOL-Key frames,
//! 12.7.6 for the 4-way handshake, 12.7.7 for the group key handshake) as
//! hostap implements it in `src/rsn_supp/wpa.c` at hostap_2_11. Every check
//! that function makes for this AKM and cipher is made here, in the same
//! order, and the function it comes from is named beside it. What hostap
//! supports and this network does not use (TKIP, WPA1, FT, SAE, PMF, OCV,
//! MLO, FILS) is left out, and a frame that would need it is refused. The one
//! exception is a TKIP group key's length, known only so the self-test can run
//! a published capture whose access point uses TKIP for group traffic; the
//! driver joins only networks whose group cipher is CCMP.
//!
//! Nothing here knows about the chip. A frame from the access point goes in;
//! the frame to send back and the keys to install come out, and the driver
//! sends the frame before it installs the keys, so that message 4 leaves
//! unencrypted as `brcmf_netdev_wait_pend8021x` arranges in brcmfmac.
//!
//! **Secrets.** The PMK, the PTK and the keys handed out are held in types with
//! no `Debug` or `Display`, and they zero their bytes when dropped. The error
//! values carry no key material, and neither does the log the driver makes
//! from them. The `hmac` and `sha1` crates keep the keyed hash state in values
//! that are dropped without being zeroed; that stays in freed stack memory
//! until it is overwritten.

use alloc::vec;
use alloc::vec::Vec;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// `PMK_LEN`: the PSK is the PMK, 256 bits.
pub const PMK_LEN: usize = 32;
/// `WPA_NONCE_LEN`.
pub const NONCE_LEN: usize = 32;
/// `WPA_REPLAY_COUNTER_LEN`.
const REPLAY_COUNTER_LEN: usize = 8;
/// `wpa_mic_len` for PSK: HMAC-SHA1 cut to 128 bits.
const MIC_LEN: usize = 16;
/// `wpa_kck_len`, `wpa_kek_len` and `wpa_cipher_key_len(CCMP)`.
const KCK_LEN: usize = 16;
const KEK_LEN: usize = 16;
pub const TK_LEN: usize = 16;
const PTK_LEN: usize = KCK_LEN + KEK_LEN + TK_LEN;
/// The size of `struct wpa_gtk_data`'s `gtk` field.
pub const GTK_MAX_LEN: usize = 32;
/// `RSN_CIPHER_SUITE_CCMP` and `RSN_CIPHER_SUITE_TKIP` in `wpa_common.h`, as
/// one number: OUI then type.
pub const SUITE_CCMP: u32 = 0x000F_AC04;
pub const SUITE_TKIP: u32 = 0x000F_AC02;

/// `wpa_cipher_key_len` for the two group ciphers a key frame can carry a key
/// for here. The driver joins only networks whose group cipher is CCMP; TKIP
/// is known so that the published capture, whose access point uses it for
/// group traffic, can be checked.
fn group_key_len(suite: u32) -> Option<usize> {
    match suite {
        SUITE_CCMP => Some(16),
        SUITE_TKIP => Some(32),
        _ => None,
    }
}

/// `wpa_cipher_rsc_len(WPA_CIPHER_CCMP)`.
pub const RSC_LEN: usize = 6;
/// The iteration count and output length `wpa_supplicant/config.c` gives
/// `pbkdf2_sha1` for a passphrase, from IEEE 802.11-2020 J.4.
const PBKDF2_ITERATIONS: u32 = 4096;

/// `struct ieee802_1x_hdr`: version, packet type, body length.
const EAPOL_HEADER_LEN: usize = 4;
/// `IEEE802_1X_TYPE_EAPOL_KEY` in `eapol_common.h`.
const EAPOL_TYPE_KEY: u8 = 3;
/// `EAPOL_KEY_TYPE_RSN` in `eapol_common.h`.
const DESCRIPTOR_RSN: u8 = 2;
/// `struct wpa_eapol_key` up to the MIC: descriptor type, key information,
/// key length, replay counter, nonce, IV, RSC and the reserved key ID.
const KEY_HEADER_LEN: usize = 1 + 2 + 2 + REPLAY_COUNTER_LEN + NONCE_LEN + 16 + 8 + 8;
/// The whole fixed part: the 802.1X header, the key header, the MIC and the
/// key data length. `keyhdrlen` in `wpa_sm_rx_eapol` is this less the 802.1X
/// header.
const FIXED_LEN: usize = EAPOL_HEADER_LEN + KEY_HEADER_LEN + MIC_LEN + 2;

// Offsets from the start of the 802.1X header.
const AT_KEY_INFO: usize = 5;
const AT_KEY_LENGTH: usize = 7;
const AT_REPLAY: usize = 9;
const AT_NONCE: usize = 17;
const AT_RSC: usize = 65;
const AT_MIC: usize = 81;
const AT_KEY_DATA_LENGTH: usize = 97;
const AT_KEY_DATA: usize = 99;

// `WPA_KEY_INFO_*` in `wpa_common.h`.
const KEY_INFO_TYPE_MASK: u16 = 0x0007;
const KEY_INFO_TYPE_HMAC_SHA1_AES: u16 = 2;
const KEY_INFO_KEY_TYPE: u16 = 1 << 3;
const KEY_INFO_KEY_INDEX_MASK: u16 = (1 << 4) | (1 << 5);
const KEY_INFO_INSTALL: u16 = 1 << 6;
const KEY_INFO_ACK: u16 = 1 << 7;
const KEY_INFO_MIC: u16 = 1 << 8;
const KEY_INFO_SECURE: u16 = 1 << 9;
const KEY_INFO_REQUEST: u16 = 1 << 11;
const KEY_INFO_ENCR_KEY_DATA: u16 = 1 << 12;
const KEY_INFO_SMK_MESSAGE: u16 = 1 << 13;

// Elements and KDEs, `ieee802_11_defs.h` and `wpa_common.h`.
const WLAN_EID_RSN: u8 = 48;
const WLAN_EID_VENDOR_SPECIFIC: u8 = 221;
const WLAN_EID_RSNX: u8 = 244;
const RSN_SELECTOR_LEN: usize = 4;
const WPA_OUI_TYPE: [u8; 4] = [0x00, 0x50, 0xF2, 1];
const RSN_KEY_DATA_GROUPKEY: [u8; 4] = [0x00, 0x0F, 0xAC, 1];
const RSN_KEY_DATA_PMKID: [u8; 4] = [0x00, 0x0F, 0xAC, 4];
const RSN_KEY_DATA_KEYID: [u8; 4] = [0x00, 0x0F, 0xAC, 10];
/// `PMKID_LEN`.
const PMKID_LEN: usize = 16;

/// `WLAN_REASON_UNSPECIFIED` and `WLAN_REASON_IE_IN_4WAY_DIFFERS`.
pub const REASON_UNSPECIFIED: u16 = 1;
pub const REASON_IE_IN_4WAY_DIFFERS: u16 = 17;

/// `RSN_VERSION` and `RSN_NUM_REPLAY_COUNTERS_16` in `wpa_common.h`, and the
/// PSK AKM, `RSN_AUTH_KEY_MGMT_PSK_OVER_802_1X`.
const RSN_VERSION: u16 = 1;
const RSN_NUM_REPLAY_COUNTERS_16: u16 = 3;
const AKM_PSK: [u8; 4] = [0x00, 0x0F, 0xAC, 2];

/// The RSN element this station associates with, as `wpa_gen_wpa_ie_rsn`
/// writes it for WPA2-PSK with CCMP: version 1, CCMP as the group suite, one
/// pairwise suite, CCMP, one AKM, PSK, then the capabilities `rsn_supp_capab`
/// gives, which here is only 16 PTKSA replay counters when WMM is in use.
/// `wpa_supplicant_set_suites` turns WMM on when the access point's scan entry
/// has a WMM element. No PMKID list, since a PSK has no cached PMKSA.
pub fn station_rsn_element(wmm: bool) -> Vec<u8> {
    let capabilities: u16 = if wmm { RSN_NUM_REPLAY_COUNTERS_16 << 2 } else { 0 };
    let mut element = vec![WLAN_EID_RSN, 0];
    element.extend_from_slice(&RSN_VERSION.to_le_bytes());
    element.extend_from_slice(&SUITE_CCMP.to_be_bytes());
    element.extend_from_slice(&1u16.to_le_bytes());
    element.extend_from_slice(&SUITE_CCMP.to_be_bytes());
    element.extend_from_slice(&1u16.to_le_bytes());
    element.extend_from_slice(&AKM_PSK);
    element.extend_from_slice(&capabilities.to_le_bytes());
    element[1] = (element.len() - 2) as u8;
    element
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The pairwise master key, which for PSK is derived from the passphrase.
pub struct Pmk([u8; PMK_LEN]);

impl Pmk {
    /// PSK = PBKDF2(HMAC-SHA1, passphrase, SSID, 4096, 256), IEEE 802.11-2020
    /// J.4.1, as `wpa_supplicant/config.c` calls `pbkdf2_sha1`.
    pub fn from_passphrase(passphrase: &[u8], ssid: &[u8]) -> Pmk {
        let mut pmk = [0u8; PMK_LEN];
        pbkdf2::pbkdf2_hmac::<Sha1>(passphrase, ssid, PBKDF2_ITERATIONS, &mut pmk);
        Pmk(pmk)
    }

    /// Whether this is the given key, for the self-test's published vectors.
    pub fn equals(&self, other: &[u8]) -> bool {
        equal_in_constant_time(&self.0, other)
    }
}

impl Drop for Pmk {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// KCK, KEK and TK, in that order. `struct wpa_ptk`.
struct Ptk([u8; PTK_LEN]);

impl Ptk {
    fn kck(&self) -> &[u8] {
        &self.0[..KCK_LEN]
    }

    fn kek(&self) -> &[u8] {
        &self.0[KCK_LEN..KCK_LEN + KEK_LEN]
    }

    fn tk(&self) -> &[u8] {
        &self.0[KCK_LEN + KEK_LEN..]
    }
}

impl Drop for Ptk {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// The pairwise key to install: CCMP's 128-bit temporal key.
pub struct PairwiseKey([u8; TK_LEN]);

impl PairwiseKey {
    pub fn key(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PairwiseKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// A group key to install, with its index and the receive sequence counter to
/// start from. `struct wpa_gtk_data`.
pub struct GroupKey {
    index: u8,
    key: [u8; GTK_MAX_LEN],
    len: usize,
    rsc: [u8; RSC_LEN],
}

impl GroupKey {
    pub fn index(&self) -> u8 {
        self.index
    }

    pub fn key(&self) -> &[u8] {
        &self.key[..self.len]
    }

    /// The first six bytes of Key RSC, least significant first.
    pub fn rsc(&self) -> [u8; RSC_LEN] {
        self.rsc
    }
}

impl Drop for GroupKey {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

// ---------------------------------------------------------------------------
// The functions
// ---------------------------------------------------------------------------

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    // HMAC takes a key of any length, so this cannot fail.
    let Ok(mut mac) = <HmacSha1 as Mac>::new_from_slice(key) else {
        unreachable!("HMAC refused a key")
    };
    for part in parts {
        mac.update(part);
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// PRF-n from IEEE 802.11-2020 12.7.1.2: HMAC-SHA1 over the label, a zero
/// byte, the data and a counter byte from zero, concatenated and cut to the
/// length wanted. `sha1_prf` in hostap's `src/crypto/sha1-prf.c`.
pub fn prf_sha1(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mut counter = 0u8;
    let mut pos = 0;
    while pos < out.len() {
        let mut hash = hmac_sha1(key, &[label, &[0], data, &[counter]]);
        let n = (out.len() - pos).min(hash.len());
        out[pos..pos + n].copy_from_slice(&hash[..n]);
        hash.fill(0);
        pos += n;
        counter = counter.wrapping_add(1);
    }
}

/// `wpa_pmk_to_ptk` for this AKM: PRF-384 over the smaller address, the
/// larger, the smaller nonce and the larger, with the label "Pairwise key
/// expansion".
fn derive_ptk(pmk: &Pmk, own: &[u8; 6], aa: &[u8; 6], snonce: &[u8; NONCE_LEN], anonce: &[u8; NONCE_LEN]) -> Ptk {
    let mut data = [0u8; 2 * 6 + 2 * NONCE_LEN];
    let (first, second) = if own < aa { (own, aa) } else { (aa, own) };
    data[..6].copy_from_slice(first);
    data[6..12].copy_from_slice(second);
    let (first, second) = if snonce < anonce { (snonce, anonce) } else { (anonce, snonce) };
    data[12..44].copy_from_slice(first);
    data[44..76].copy_from_slice(second);
    let mut ptk = Ptk([0; PTK_LEN]);
    prf_sha1(&pmk.0, b"Pairwise key expansion", &data, &mut ptk.0);
    ptk
}

/// `wpa_eapol_key_mic` for descriptor version 2: HMAC-SHA1 of the whole
/// EAPOL frame, with its MIC field zero, cut to 128 bits.
fn mic(kck: &[u8], frame: &[u8]) -> [u8; MIC_LEN] {
    let hash = hmac_sha1(kck, &[frame]);
    let mut out = [0u8; MIC_LEN];
    out.copy_from_slice(&hash[..MIC_LEN]);
    out
}

/// `os_memcmp_const`: the time taken does not depend on where the first
/// difference is.
fn equal_in_constant_time(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    difference == 0
}

/// RFC 3394 key unwrap with a 128-bit KEK, `aes_unwrap`. `out` is eight bytes
/// shorter than `wrapped`.
pub fn aes_unwrap(kek: &[u8; 16], wrapped: &[u8], out: &mut [u8]) -> bool {
    aes_kw::KekAes128::from(*kek).unwrap(wrapped, out).is_ok()
}

/// RFC 3394 key wrap with a 128-bit KEK, `aes_wrap`. Only the self-test uses
/// it; the supplicant never wraps anything.
pub fn aes_wrap(kek: &[u8; 16], plain: &[u8], out: &mut [u8]) -> bool {
    aes_kw::KekAes128::from(*kek).wrap(plain, out).is_ok()
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// What a frame from the access point was, for the log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Message 1 of the 4-way handshake; the reply is message 2.
    Message1,
    /// Message 3; the reply is message 4, and the keys come with it.
    Message3,
    /// Message 1 of the group key handshake; the reply is message 2, and a
    /// new group key comes with it.
    GroupMessage1,
}

/// The result of a frame that was accepted.
pub struct Outcome {
    pub step: Step,
    /// The EAPOL frame to send to the access point, before any key below is
    /// installed.
    pub reply: Vec<u8>,
    pub pairwise: Option<PairwiseKey>,
    pub group: Option<GroupKey>,
}

/// Why a frame was not accepted. None carries key material.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    // Frames `wpa_sm_rx_eapol` drops without changing anything.
    TooShort,
    NotKeyFrame,
    BadLength,
    NotRsnDescriptor,
    KeyDataOverflow,
    ReplayCounterNotIncreased,
    SmkBit,
    NoAckBit,
    RequestBit,
    DescriptorVersion(u16),
    MicMismatch,
    NoKeyForMic,
    EncryptedWithoutMic,
    KeyDataNotBlocks,
    UnwrapFailed,
    PairwiseKeyIndex,
    GroupWithoutMic,
    // Failures after which hostap deauthenticates.
    KeyDataElements,
    GtkUnencrypted,
    NoApRsnElement,
    RsnElementDiffers,
    RsnxElementDiffers,
    ExtendedKeyId,
    AnonceDiffers,
    KeyLength(u16),
    NoGtk,
    GtkLength,
    GroupBeforePairwise,
}

impl Error {
    /// The reason code hostap deauthenticates with after this failure, or
    /// `None` when it only drops the frame. `wpa_sm_deauthenticate` from the
    /// `failed:` paths, and `wpa_report_ie_mismatch` for the elements.
    pub fn deauthenticate(&self) -> Option<u16> {
        use Error::*;
        match self {
            KeyDataElements | GtkUnencrypted | NoApRsnElement | ExtendedKeyId | AnonceDiffers | KeyLength(_)
            | NoGtk | GtkLength | GroupBeforePairwise => Some(REASON_UNSPECIFIED),
            RsnElementDiffers | RsnxElementDiffers => Some(REASON_IE_IN_4WAY_DIFFERS),
            _ => None,
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use Error::*;
        match self {
            TooShort => write!(f, "too short to be an EAPOL-Key frame"),
            NotKeyFrame => write!(f, "an EAPOL frame that is not a key frame"),
            BadLength => write!(f, "the EAPOL length does not fit the frame"),
            NotRsnDescriptor => write!(f, "not an RSN key descriptor"),
            KeyDataOverflow => write!(f, "the key data length runs past the frame"),
            ReplayCounterNotIncreased => write!(f, "the replay counter did not increase"),
            SmkBit => write!(f, "the SMK bit is set"),
            NoAckBit => write!(f, "the ACK bit is not set"),
            RequestBit => write!(f, "the request bit is set"),
            DescriptorVersion(v) => write!(f, "key descriptor version {} where CCMP with PSK needs 2", v),
            MicMismatch => write!(f, "the MIC does not match"),
            NoKeyForMic => write!(f, "a MIC before any key to check it with"),
            EncryptedWithoutMic => write!(f, "encrypted key data without a MIC"),
            KeyDataNotBlocks => write!(f, "encrypted key data that is not whole 8-byte blocks"),
            UnwrapFailed => write!(f, "the key data did not unwrap"),
            PairwiseKeyIndex => write!(f, "a pairwise key frame with a key index"),
            GroupWithoutMic => write!(f, "a group key frame without a MIC"),
            KeyDataElements => write!(f, "the key data's elements do not parse"),
            GtkUnencrypted => write!(f, "a group key in unencrypted key data"),
            NoApRsnElement => write!(f, "the scan gave no RSN element for this access point"),
            RsnElementDiffers => write!(f, "message 3's RSN element differs from the beacon's"),
            RsnxElementDiffers => write!(f, "message 3's RSNX element differs from the beacon's"),
            ExtendedKeyId => write!(f, "a non-zero extended key ID"),
            AnonceDiffers => write!(f, "message 3's ANonce differs from message 1's"),
            KeyLength(n) => write!(f, "key length {} where CCMP needs 16", n),
            NoGtk => write!(f, "no group key"),
            GtkLength => write!(f, "a group key whose length is not the group cipher's"),
            GroupBeforePairwise => write!(f, "a group key handshake before the 4-way handshake finished"),
        }
    }
}

/// The elements and KDEs `wpa_parse_kde_ies` and `wpa_parse_generic` find in
/// key data that this AKM uses.
#[derive(Default)]
struct KeyData<'a> {
    rsn: Option<&'a [u8]>,
    rsnx: Option<&'a [u8]>,
    wpa: Option<&'a [u8]>,
    gtk: Option<&'a [u8]>,
    key_id: Option<&'a [u8]>,
}

/// `wpa_parse_kde_ies`: elements until padding, where one that runs past the
/// end is an error.
fn parse_key_data(data: &[u8]) -> Result<KeyData<'_>, Error> {
    let mut found = KeyData::default();
    let mut pos = 0;
    while data.len() - pos > 1 {
        if data[pos] == WLAN_EID_VENDOR_SPECIFIC && data[pos + 1] == 0 {
            // Padding.
            break;
        }
        let len = 2 + data[pos + 1] as usize;
        if len > data.len() - pos {
            return Err(Error::KeyDataElements);
        }
        let element = &data[pos..pos + len];
        match element[0] {
            WLAN_EID_RSN => found.rsn = Some(element),
            WLAN_EID_RSNX => found.rsnx = Some(element),
            WLAN_EID_VENDOR_SPECIFIC => parse_generic(element, &mut found),
            _ => {}
        }
        pos += len;
    }
    Ok(found)
}

/// `wpa_parse_generic` for the KDEs this AKM uses. A KDE of any other kind is
/// passed over, as hostap passes over one it does not recognise.
fn parse_generic<'a>(element: &'a [u8], found: &mut KeyData<'a>) {
    if element.len() < 2 + RSN_SELECTOR_LEN {
        return;
    }
    let selector = &element[2..2 + RSN_SELECTOR_LEN];
    let body = &element[2 + RSN_SELECTOR_LEN..];
    if selector == WPA_OUI_TYPE && body.len() >= 2 && body[0] == 1 && body[1] == 0 {
        found.wpa = Some(element);
    } else if selector == RSN_KEY_DATA_PMKID && body.len() >= PMKID_LEN {
        // A PSK has no PMKSA to look up, so the PMKID is not used.
    } else if selector == RSN_KEY_DATA_KEYID && body.len() >= 2 {
        found.key_id = Some(body);
    } else if selector == RSN_KEY_DATA_GROUPKEY && body.len() > 2 {
        found.gtk = Some(body);
    }
}

/// An EAPOL-Key frame's fixed fields.
struct KeyFrame {
    version: u8,
    key_info: u16,
    key_length: u16,
    replay: [u8; REPLAY_COUNTER_LEN],
    nonce: [u8; NONCE_LEN],
    rsc: [u8; 8],
}

impl KeyFrame {
    fn read(frame: &[u8]) -> KeyFrame {
        let mut replay = [0u8; REPLAY_COUNTER_LEN];
        replay.copy_from_slice(&frame[AT_REPLAY..AT_REPLAY + REPLAY_COUNTER_LEN]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&frame[AT_NONCE..AT_NONCE + NONCE_LEN]);
        let mut rsc = [0u8; 8];
        rsc.copy_from_slice(&frame[AT_RSC..AT_RSC + 8]);
        KeyFrame {
            version: frame[0],
            key_info: u16::from_be_bytes([frame[AT_KEY_INFO], frame[AT_KEY_INFO + 1]]),
            key_length: u16::from_be_bytes([frame[AT_KEY_LENGTH], frame[AT_KEY_LENGTH + 1]]),
            replay,
            nonce,
            rsc,
        }
    }
}

/// An EAPOL-Key frame from this station, with its MIC zero. The key length,
/// IV, RSC and key ID are zero, as hostap writes them for RSN. The version is
/// the one `wpa_alloc_eapol` in `wpas_glue.c` writes, from the configuration.
fn build(version: u8, key_info: u16, replay: &[u8; REPLAY_COUNTER_LEN], nonce: &[u8; NONCE_LEN], key_data: &[u8]) -> Vec<u8> {
    let body = FIXED_LEN - EAPOL_HEADER_LEN + key_data.len();
    let mut frame = vec![0u8; EAPOL_HEADER_LEN + body];
    frame[0] = version;
    frame[1] = EAPOL_TYPE_KEY;
    frame[2..4].copy_from_slice(&(body as u16).to_be_bytes());
    frame[4] = DESCRIPTOR_RSN;
    frame[AT_KEY_INFO..AT_KEY_INFO + 2].copy_from_slice(&key_info.to_be_bytes());
    frame[AT_REPLAY..AT_REPLAY + REPLAY_COUNTER_LEN].copy_from_slice(replay);
    frame[AT_NONCE..AT_NONCE + NONCE_LEN].copy_from_slice(nonce);
    frame[AT_KEY_DATA_LENGTH..AT_KEY_DATA].copy_from_slice(&(key_data.len() as u16).to_be_bytes());
    frame[AT_KEY_DATA..].copy_from_slice(key_data);
    frame
}

/// Put the MIC into a frame built with a zero one. `wpa_eapol_key_send`.
fn sign(frame: &mut [u8], kck: &[u8]) {
    let value = mic(kck, frame);
    frame[AT_MIC..AT_MIC + MIC_LEN].copy_from_slice(&value);
}

// ---------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------

/// `DEFAULT_EAPOL_VERSION` in `wpa_supplicant/config.h`. IEEE 802.1X-2004 is
/// version 2, but hostap sends 1 because some access points drop frames with
/// the newer number.
pub const EAPOL_VERSION: u8 = 1;

/// What the supplicant was told about the association.
pub struct Association {
    pub own: [u8; 6],
    pub aa: [u8; 6],
    /// The RSN element this station put in its association request, which
    /// message 2 must carry unchanged. `sm->assoc_wpa_ie`.
    pub own_rsn: Vec<u8>,
    /// The access point's RSN and RSNX elements from its beacon or probe
    /// response, which message 3's must match. `sm->ap_rsn_ie`, `ap_rsnxe`.
    pub ap_rsn: Option<Vec<u8>>,
    pub ap_rsnx: Option<Vec<u8>>,
    /// The group cipher suite in the access point's RSN element, which
    /// decides the group key's length.
    pub group_cipher: u32,
    /// The EAPOL header version this station sends.
    pub eapol_version: u8,
}

/// `struct wpa_sm`, for this AKM and cipher.
pub struct Supplicant {
    association: Association,
    pmk: Pmk,
    random: fn(&mut [u8]),
    snonce: [u8; NONCE_LEN],
    renew_snonce: bool,
    anonce: [u8; NONCE_LEN],
    tptk: Option<Ptk>,
    ptk: Option<Ptk>,
    ptk_installed: bool,
    rx_replay: Option<[u8; REPLAY_COUNTER_LEN]>,
    msg_3_of_4_ok: bool,
    /// The group key in use, so that the same one is not installed twice.
    /// `sm->gtk`.
    gtk: Option<([u8; GTK_MAX_LEN], usize)>,
}

impl Drop for Supplicant {
    fn drop(&mut self) {
        if let Some((gtk, _)) = self.gtk.as_mut() {
            gtk.fill(0);
        }
    }
}

impl Supplicant {
    /// `wpa_sm_init` starts with `renew_snonce` set, so the first message 1
    /// draws a nonce. `random` fills a buffer with bytes from a
    /// cryptographic generator.
    pub fn new(association: Association, pmk: Pmk, random: fn(&mut [u8])) -> Supplicant {
        Supplicant {
            association,
            pmk,
            random,
            snonce: [0; NONCE_LEN],
            renew_snonce: true,
            anonce: [0; NONCE_LEN],
            tptk: None,
            ptk: None,
            ptk_installed: false,
            rx_replay: None,
            msg_3_of_4_ok: false,
            gtk: None,
        }
    }

    /// Whether the 4-way handshake has finished.
    pub fn completed(&self) -> bool {
        self.msg_3_of_4_ok
    }

    /// The MIC a frame would carry under the key this supplicant would check
    /// it with now: the temporary PTK from message 1 if there is one, else
    /// the PTK. For the self-test, which uses it to compare frames with a
    /// published capture; a MIC gives away nothing about the key.
    pub fn mic_under_current_key(&self, frame: &[u8]) -> Option<[u8; MIC_LEN]> {
        let ptk = self.tptk.as_ref().or(self.ptk.as_ref())?;
        if frame.len() < FIXED_LEN {
            return None;
        }
        let mut copy = frame.to_vec();
        copy[AT_MIC..AT_MIC + MIC_LEN].fill(0);
        Some(mic(ptk.kck(), &copy))
    }

    /// Message 1 of a group key handshake as an access point would build it
    /// under this supplicant's PTK: a GTK KDE wrapped with the KEK. For the
    /// self-test only, since no published capture has a rekey.
    pub fn group_message_for_test(&self, replay: [u8; 8], index: u8, gtk: &[u8], rsc: [u8; 8]) -> Option<Vec<u8>> {
        let ptk = self.ptk.as_ref()?;
        if gtk.len() > GTK_MAX_LEN || (2 + RSN_SELECTOR_LEN + 2 + gtk.len()) % 8 != 0 {
            return None;
        }
        let mut plain = vec![WLAN_EID_VENDOR_SPECIFIC, (RSN_SELECTOR_LEN + 2 + gtk.len()) as u8];
        plain.extend_from_slice(&RSN_KEY_DATA_GROUPKEY);
        plain.extend_from_slice(&[index & 0x03, 0]);
        plain.extend_from_slice(gtk);
        let mut wrapped = vec![0u8; plain.len() + 8];
        let mut kek = [0u8; KEK_LEN];
        kek.copy_from_slice(ptk.kek());
        let ok = aes_wrap(&kek, &plain, &mut wrapped);
        kek.fill(0);
        plain.fill(0);
        if !ok {
            return None;
        }
        let key_info = KEY_INFO_TYPE_HMAC_SHA1_AES
            | ((index as u16 & 0x03) << 4)
            | KEY_INFO_ACK
            | KEY_INFO_MIC
            | KEY_INFO_SECURE
            | KEY_INFO_ENCR_KEY_DATA;
        let mut frame = build(2, key_info, &replay, &[0; NONCE_LEN], &wrapped);
        frame[AT_RSC..AT_RSC + 8].copy_from_slice(&rsc);
        sign(&mut frame, ptk.kck());
        Some(frame)
    }

    /// One EAPOL frame from the access point. `wpa_sm_rx_eapol`.
    pub fn receive(&mut self, received: &[u8]) -> Result<Outcome, Error> {
        if received.len() < FIXED_LEN {
            return Err(Error::TooShort);
        }
        let plen = u16::from_be_bytes([received[2], received[3]]) as usize;
        if received[1] != EAPOL_TYPE_KEY {
            return Err(Error::NotKeyFrame);
        }
        if plen > received.len() - EAPOL_HEADER_LEN || plen < FIXED_LEN - EAPOL_HEADER_LEN {
            return Err(Error::BadLength);
        }
        // Bytes after the 802.1X body are ignored. The copy is modified while
        // the MIC is checked and the key data decrypted, and zeroed after.
        let mut frame = received[..EAPOL_HEADER_LEN + plen].to_vec();
        let result = self.receive_copy(&mut frame);
        frame.fill(0);
        result
    }

    fn receive_copy(&mut self, frame: &mut [u8]) -> Result<Outcome, Error> {
        if frame[4] != DESCRIPTOR_RSN {
            return Err(Error::NotRsnDescriptor);
        }
        let mut key_data_len = u16::from_be_bytes([frame[AT_KEY_DATA_LENGTH], frame[AT_KEY_DATA_LENGTH + 1]]) as usize;
        if key_data_len > frame.len() - FIXED_LEN {
            return Err(Error::KeyDataOverflow);
        }
        let key = KeyFrame::read(frame);
        if let Some(last) = self.rx_replay {
            if key.replay <= last {
                return Err(Error::ReplayCounterNotIncreased);
            }
        }
        if key.key_info & KEY_INFO_SMK_MESSAGE != 0 {
            return Err(Error::SmkBit);
        }
        if key.key_info & KEY_INFO_ACK == 0 {
            return Err(Error::NoAckBit);
        }
        if key.key_info & KEY_INFO_REQUEST != 0 {
            return Err(Error::RequestBit);
        }
        // hostap accepts version 3 with CCMP as an interoperability
        // workaround and then checks the MIC with AES-CMAC. This supplicant
        // has no AES-CMAC, so it refuses that frame instead.
        let ver = key.key_info & KEY_INFO_TYPE_MASK;
        if ver != KEY_INFO_TYPE_HMAC_SHA1_AES {
            return Err(Error::DescriptorVersion(ver));
        }

        if key.key_info & KEY_INFO_MIC != 0 {
            self.verify_mic(frame, &key)?;
        }

        if key.key_info & KEY_INFO_ENCR_KEY_DATA != 0 {
            if key.key_info & KEY_INFO_MIC == 0 {
                return Err(Error::EncryptedWithoutMic);
            }
            key_data_len = self.decrypt_key_data(frame, key_data_len)?;
        }
        let key_data = &frame[AT_KEY_DATA..AT_KEY_DATA + key_data_len];

        if key.key_info & KEY_INFO_KEY_TYPE != 0 {
            if key.key_info & KEY_INFO_KEY_INDEX_MASK != 0 {
                return Err(Error::PairwiseKeyIndex);
            }
            if key.key_info & (KEY_INFO_MIC | KEY_INFO_ENCR_KEY_DATA) != 0 {
                self.process_3_of_4(&key, key_data)
            } else {
                self.process_1_of_4(&key, key_data)
            }
        } else if key.key_info & KEY_INFO_MIC != 0 {
            self.process_1_of_2(&key, key_data)
        } else {
            Err(Error::GroupWithoutMic)
        }
    }

    /// `wpa_supplicant_verify_eapol_key_mic`: the temporary PTK from message
    /// 1 first, which becomes the PTK when it checks; then the PTK already in
    /// use.
    fn verify_mic(&mut self, frame: &mut [u8], key: &KeyFrame) -> Result<(), Error> {
        let mut received = [0u8; MIC_LEN];
        received.copy_from_slice(&frame[AT_MIC..AT_MIC + MIC_LEN]);
        frame[AT_MIC..AT_MIC + MIC_LEN].fill(0);
        let mut ok = false;
        if let Some(tptk) = &self.tptk {
            if equal_in_constant_time(&mic(tptk.kck(), frame), &received) {
                ok = true;
                self.ptk = self.tptk.take();
                self.ptk_installed = false;
                self.renew_snonce = true;
            }
        }
        if !ok {
            let Some(ptk) = &self.ptk else { return Err(Error::NoKeyForMic) };
            if !equal_in_constant_time(&mic(ptk.kck(), frame), &received) {
                return Err(Error::MicMismatch);
            }
        }
        self.rx_replay = Some(key.replay);
        Ok(())
    }

    /// `wpa_supplicant_decrypt_key_data` for AES key wrap: the key data is
    /// unwrapped in place and its length shortened by the eight bytes of
    /// integrity check.
    fn decrypt_key_data(&self, frame: &mut [u8], len: usize) -> Result<usize, Error> {
        let Some(ptk) = &self.ptk else { return Err(Error::NoKeyForMic) };
        if len < 8 || len % 8 != 0 {
            return Err(Error::KeyDataNotBlocks);
        }
        let mut kek = [0u8; KEK_LEN];
        kek.copy_from_slice(ptk.kek());
        let mut plain = vec![0u8; len - 8];
        let unwrapped = aes_unwrap(&kek, &frame[AT_KEY_DATA..AT_KEY_DATA + len], &mut plain);
        kek.fill(0);
        if !unwrapped {
            plain.fill(0);
            return Err(Error::UnwrapFailed);
        }
        frame[AT_KEY_DATA..AT_KEY_DATA + plain.len()].copy_from_slice(&plain);
        plain.fill(0);
        Ok(len - 8)
    }

    /// `wpa_supplicant_process_1_of_4`.
    fn process_1_of_4(&mut self, key: &KeyFrame, key_data: &[u8]) -> Result<Outcome, Error> {
        parse_key_data(key_data)?;
        if self.renew_snonce {
            (self.random)(&mut self.snonce);
            self.renew_snonce = false;
        }
        let tptk = derive_ptk(&self.pmk, &self.association.own, &self.association.aa, &self.snonce, &key.nonce);

        // `wpa_supplicant_send_2_of_4`.
        let mut key_info = KEY_INFO_TYPE_HMAC_SHA1_AES | KEY_INFO_KEY_TYPE | KEY_INFO_MIC;
        if self.ptk.is_some() {
            key_info |= KEY_INFO_SECURE;
        }
        let mut reply = build(self.association.eapol_version, key_info, &key.replay, &self.snonce, &self.association.own_rsn);
        sign(&mut reply, tptk.kck());
        self.tptk = Some(tptk);
        self.anonce = key.nonce;
        Ok(Outcome { step: Step::Message1, reply, pairwise: None, group: None })
    }

    /// `wpa_supplicant_process_3_of_4`.
    fn process_3_of_4(&mut self, key: &KeyFrame, key_data: &[u8]) -> Result<Outcome, Error> {
        let ie = parse_key_data(key_data)?;
        if ie.gtk.is_some() && key.key_info & KEY_INFO_ENCR_KEY_DATA == 0 {
            return Err(Error::GtkUnencrypted);
        }
        self.validate_ie(&ie)?;
        // `wpa_handle_ext_key_id` without extended key IDs.
        if let Some(key_id) = ie.key_id {
            if key_id[0] & 0x03 != 0 {
                return Err(Error::ExtendedKeyId);
            }
        }
        if key.nonce != self.anonce {
            return Err(Error::AnonceDiffers);
        }
        if key.key_length as usize != TK_LEN {
            return Err(Error::KeyLength(key.key_length));
        }
        let Some(ptk) = &self.ptk else { return Err(Error::NoKeyForMic) };

        // `wpa_supplicant_send_4_of_4`.
        let key_info = (key.key_info & KEY_INFO_SECURE) | KEY_INFO_TYPE_HMAC_SHA1_AES | KEY_INFO_KEY_TYPE | KEY_INFO_MIC;
        let mut reply = build(self.association.eapol_version, key_info, &key.replay, &[0; NONCE_LEN], &[]);
        sign(&mut reply, ptk.kck());
        self.renew_snonce = true;

        // `wpa_supplicant_install_ptk`, once per PTK. hostap installs it before
        // it looks at the GTK and deauthenticates if that fails; here nothing
        // is handed out unless the whole message is good, which ends the same
        // way for the association.
        let mut pairwise = None;
        if key.key_info & KEY_INFO_INSTALL != 0 && !self.ptk_installed {
            let mut tk = [0u8; TK_LEN];
            tk.copy_from_slice(ptk.tk());
            pairwise = Some(PairwiseKey(tk));
        }

        // `wpa_supplicant_pairwise_gtk`.
        let Some(gtk) = ie.gtk else { return Err(Error::NoGtk) };
        let group = self.take_gtk(gtk, &key.rsc)?;
        if pairwise.is_some() {
            self.ptk_installed = true;
        }
        self.msg_3_of_4_ok = true;
        Ok(Outcome { step: Step::Message3, reply, pairwise, group })
    }

    /// `wpa_supplicant_process_1_of_2`.
    fn process_1_of_2(&mut self, key: &KeyFrame, key_data: &[u8]) -> Result<Outcome, Error> {
        if !self.msg_3_of_4_ok {
            return Err(Error::GroupBeforePairwise);
        }
        let ie = parse_key_data(key_data)?;
        if ie.gtk.is_some() && key.key_info & KEY_INFO_ENCR_KEY_DATA == 0 {
            return Err(Error::GtkUnencrypted);
        }
        let Some(gtk) = ie.gtk else { return Err(Error::NoGtk) };
        let group = self.take_gtk(gtk, &key.rsc)?;
        let Some(ptk) = &self.ptk else { return Err(Error::NoKeyForMic) };

        // `wpa_supplicant_send_2_of_2`.
        let key_info = (key.key_info & KEY_INFO_KEY_INDEX_MASK) | KEY_INFO_TYPE_HMAC_SHA1_AES | KEY_INFO_SECURE | KEY_INFO_MIC;
        let mut reply = build(self.association.eapol_version, key_info, &key.replay, &[0; NONCE_LEN], &[]);
        sign(&mut reply, ptk.kck());
        Ok(Outcome { step: Step::GroupMessage1, reply, pairwise: None, group })
    }

    /// `wpa_supplicant_validate_ie` without FT: message 3's RSN element must
    /// be the beacon's byte for byte (`wpa_compare_rsn_ie`), and the RSNX
    /// element must be in both or neither and the same.
    fn validate_ie(&self, ie: &KeyData<'_>) -> Result<(), Error> {
        let Some(ap_rsn) = &self.association.ap_rsn else { return Err(Error::NoApRsnElement) };
        if ie.wpa.is_none() && ie.rsn.is_none() {
            return Err(Error::RsnElementDiffers);
        }
        if let Some(rsn) = ie.rsn {
            if rsn != ap_rsn.as_slice() {
                return Err(Error::RsnElementDiffers);
            }
        }
        match (&self.association.ap_rsnx, ie.rsnx) {
            (None, None) => {}
            (Some(ap), Some(received)) if ap.as_slice() == received => {}
            _ => return Err(Error::RsnxElementDiffers),
        }
        Ok(())
    }

    /// The GTK KDE into a key to install: `wpa_supplicant_pairwise_gtk` and
    /// the group handshake's equivalent, `wpa_supplicant_check_group_cipher`,
    /// `wpa_supplicant_gtk_tx_bit_workaround`, `wpa_supplicant_rsc_relaxation`
    /// and the reinstallation check in `wpa_supplicant_install_gtk`. Returns
    /// `None` when this key is the one already installed.
    fn take_gtk(&mut self, kde: &[u8], rsc: &[u8; 8]) -> Result<Option<GroupKey>, Error> {
        // KeyID in bits 0-1 and Tx in bit 2 of the first byte, a reserved
        // byte, then the key. The Tx bit is ignored, since a pairwise key is
        // in use.
        if kde.len() < 2 || kde.len() - 2 > GTK_MAX_LEN {
            return Err(Error::GtkLength);
        }
        let index = kde[0] & 0x03;
        let bytes = &kde[2..];
        if Some(bytes.len()) != group_key_len(self.association.group_cipher) {
            return Err(Error::GtkLength);
        }
        // `DEFAULT_WPA_RSC_RELAXATION` is 1: an RSC whose bytes look swapped is
        // taken as zero.
        let relax = (rsc[5] != 0 && rsc[0] == 0) || rsc[6] != 0 || rsc[7] != 0;
        let mut start = [0u8; RSC_LEN];
        if !relax {
            start.copy_from_slice(&rsc[..RSC_LEN]);
        }
        let len = bytes.len();
        let mut key = [0u8; GTK_MAX_LEN];
        key[..len].copy_from_slice(bytes);
        if let Some((current, current_len)) = &self.gtk {
            if equal_in_constant_time(&current[..*current_len], &key[..len]) {
                key.fill(0);
                return Ok(None);
            }
        }
        if let Some((old, _)) = self.gtk.as_mut() {
            old.fill(0);
        }
        self.gtk = Some((key, len));
        Ok(Some(GroupKey { index, key, len, rsc: start }))
    }
}
