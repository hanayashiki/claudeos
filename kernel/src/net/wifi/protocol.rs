//! What crosses function 2 once the firmware runs: SDPCM frames, the BCDC
//! control messages and data headers inside them, the firmware's events, and
//! the payloads of the commands this driver sends.
//!
//! Everything here is arithmetic on byte slices, so it can be checked without
//! a chip. The layouts are brcmfmac's, in Raspberry Pi's `rpi-6.6.y`
//! (`drivers/net/wireless/broadcom/brcm80211/brcmfmac/`): `sdio.c` for the
//! SDPCM header and its padding rules, `bcdc.c` for the control and data
//! headers, `fweh.h` for events, `fwil.c` for iovars, and `fwil_types.h` and
//! `cfg80211.c` for the scan, country and join payloads. Every multi-byte
//! field is little-endian except in events, which are big-endian.

use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// SDPCM, `sdio.c`
// ---------------------------------------------------------------------------

/// Frame length and its complement.
pub const HWHDR_LEN: usize = 4;
/// Sequence, channel, next length, data offset, flow control and window.
pub const SWHDR_LEN: usize = 8;
pub const HDRLEN: usize = HWHDR_LEN + SWHDR_LEN;

pub const CHANNEL_CONTROL: u8 = 0;
pub const CHANNEL_EVENT: u8 = 1;
pub const CHANNEL_DATA: u8 = 2;
pub const CHANNEL_GLOM: u8 = 3;

/// `BRCMF_FIRSTREAD`: how much of a frame is read before its length is known.
pub const FIRST_READ: usize = 1 << 6;
/// `MAX_RX_DATASZ`: the longest event or data frame accepted.
pub const MAX_RX_DATASZ: usize = 2048;
/// `ALIGNMENT` on a 64-bit build, which is what `head_align` is.
pub const HEAD_ALIGN: usize = 8;
/// Function 2's block size, and `bus->roundup`, which is the smaller of it and
/// `max_roundup`, 512.
pub const BLOCK_SIZE: usize = 512;
pub const ROUNDUP: usize = 512;

/// `BRCMF_DCMD_MAXLEN`, the largest control buffer.
pub const DCMD_MAXLEN: usize = 8192;
/// `BRCMF_TX_IOCTL_MAX_MSG_SIZE`: `ETH_FRAME_LEN` plus `ETH_FCS_LEN`. A
/// control message longer than this is cut to it by `brcmf_proto_bcdc_msg`.
pub const TX_IOCTL_MAX_MSG_SIZE: usize = 1514 + 4;
/// `bus_if->maxctl`: `BRCMF_DCMD_MAXLEN` plus the dcmd header plus
/// `ROUND_UP_MARGIN`, plus the roundup `brcmf_sdio_bus_preinit` adds.
pub const MAX_CONTROL: usize = DCMD_MAXLEN + DCMD_LEN + 2048 + ROUNDUP;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub len: u16,
    pub seq: u8,
    pub channel: u8,
    pub next_len: u8,
    pub data_offset: u8,
    pub flow_control: u8,
    /// The highest sequence number the firmware will take, `tx_seq_max`.
    pub window: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderError {
    /// Length and check both zero: nothing to read.
    NoData,
    /// The length's complement does not match.
    Checksum,
    TooShort,
    TooLong,
    BadDataOffset,
}

/// `brcmf_sdio_hdparse` for an ordinary frame.
pub fn parse_header(bytes: &[u8]) -> Result<Header, HeaderError> {
    if bytes.len() < HDRLEN {
        return Err(HeaderError::TooShort);
    }
    let len = u16::from_le_bytes([bytes[0], bytes[1]]);
    let check = u16::from_le_bytes([bytes[2], bytes[3]]);
    if len | check == 0 {
        return Err(HeaderError::NoData);
    }
    if len ^ check != 0xFFFF {
        return Err(HeaderError::Checksum);
    }
    if (len as usize) < HDRLEN {
        return Err(HeaderError::TooShort);
    }
    let sw = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let channel = ((sw >> 8) & 0xF) as u8;
    if len as usize > MAX_RX_DATASZ && channel != CHANNEL_CONTROL {
        return Err(HeaderError::TooLong);
    }
    let data_offset = (sw >> 24) as u8;
    if (data_offset as usize) < HDRLEN || data_offset as u16 > len {
        return Err(HeaderError::BadDataOffset);
    }
    let fc = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    Ok(Header {
        len,
        seq: sw as u8,
        channel,
        next_len: (sw >> 16) as u8,
        data_offset,
        flow_control: fc as u8,
        window: (fc >> 8) as u8,
    })
}

/// `brcmf_sdio_hdpack` without glomming: length and complement, then
/// sequence, channel and data offset, then a zero word.
pub fn pack_header(out: &mut [u8], len: u16, seq: u8, channel: u8, data_offset: u8) {
    out[0..2].copy_from_slice(&len.to_le_bytes());
    out[2..4].copy_from_slice(&(!len).to_le_bytes());
    let sw = seq as u32 | ((channel as u32 & 0xF) << 8) | ((data_offset as u32) << 24);
    out[4..8].copy_from_slice(&sw.to_le_bytes());
    out[8..12].copy_from_slice(&0u32.to_le_bytes());
}

/// The padding `brcmf_sdio_tx_ctrlframe` adds: up to a whole block when the
/// frame is longer than one and that costs no more than `roundup`, otherwise
/// up to the head alignment.
pub const fn tx_pad(len: usize) -> usize {
    if len > BLOCK_SIZE {
        let pad = BLOCK_SIZE - len % BLOCK_SIZE;
        if pad > ROUNDUP || pad >= BLOCK_SIZE {
            0
        } else {
            pad
        }
    } else if len % HEAD_ALIGN != 0 {
        HEAD_ALIGN - len % HEAD_ALIGN
    } else {
        0
    }
}

/// How many more bytes to read for a frame whose first `FIRST_READ` bytes are
/// in. `brcmf_sdio_pad`, applied to what is left.
pub const fn rx_remaining(len: usize) -> usize {
    let left = if len > FIRST_READ { len - FIRST_READ } else { 0 };
    if left > BLOCK_SIZE {
        let pad = BLOCK_SIZE - left % BLOCK_SIZE;
        if pad <= ROUNDUP && pad < BLOCK_SIZE && left + pad + FIRST_READ < MAX_RX_DATASZ {
            left + pad
        } else {
            left
        }
    } else if left % HEAD_ALIGN != 0 {
        left + HEAD_ALIGN - left % HEAD_ALIGN
    } else {
        left
    }
}

/// The same for a control frame, `brcmf_sdio_read_control`, whose bound is
/// the control buffer instead.
pub const fn control_remaining(len: usize) -> usize {
    if len <= FIRST_READ {
        return 0;
    }
    let left = len - FIRST_READ;
    if left > BLOCK_SIZE {
        let pad = BLOCK_SIZE - left % BLOCK_SIZE;
        if pad <= ROUNDUP && pad < BLOCK_SIZE && len + pad < MAX_CONTROL {
            left + pad
        } else {
            left
        }
    } else if left % HEAD_ALIGN != 0 {
        left + HEAD_ALIGN - left % HEAD_ALIGN
    } else {
        left
    }
}

// ---------------------------------------------------------------------------
// BCDC, `bcdc.c`
// ---------------------------------------------------------------------------

/// `struct brcmf_proto_bcdc_dcmd`: command, buffer length, flags, status.
pub const DCMD_LEN: usize = 16;
const BCDC_DCMD_ERROR: u32 = 0x01;
const BCDC_DCMD_SET: u32 = 0x02;
const BCDC_DCMD_IF_SHIFT: u32 = 12;
const BCDC_DCMD_ID_SHIFT: u32 = 16;

/// `struct brcmf_proto_bcdc_header`: flags, priority, interface, and the
/// data offset in words.
pub const BCDC_HEADER_LEN: usize = 4;
const BCDC_PROTO_VER: u8 = 2;
const BCDC_FLAG_VER_SHIFT: u8 = 4;
const BCDC_FLAG_VER_MASK: u8 = 0xF0;
const BCDC_FLAG2_IF_MASK: u8 = 0x0F;

/// Bytes between the SDPCM header and the BCDC header of a data frame. WHD
/// puts two there and says the firmware appends two zero bytes to a frame
/// sent without them; brcmfmac's alignment of an Ethernet frame whose IP
/// header is on a four-byte boundary comes out the same.
pub const DATA_PAD: usize = 2;

/// A control request, as `brcmf_proto_bcdc_msg` and then
/// `brcmf_sdio_tx_ctrlframe` build it. `buflen` is the length the firmware is
/// told the buffer has, which for a query is how much it may answer with;
/// `payload` is what is actually sent, cut at `TX_IOCTL_MAX_MSG_SIZE`.
///
/// Returns the frame, padded, in `out`.
pub fn control_frame(out: &mut Vec<u8>, seq: u8, reqid: u16, cmd: u32, set: bool, ifidx: u8, buflen: usize, payload: &[u8]) {
    let body = payload.len().min(TX_IOCTL_MAX_MSG_SIZE - DCMD_LEN);
    let len = HDRLEN + DCMD_LEN + body;
    let pad = tx_pad(len);
    out.clear();
    out.resize(len + pad, 0);
    pack_header(out, len as u16, seq, CHANNEL_CONTROL, HDRLEN as u8);
    let mut flags = (reqid as u32) << BCDC_DCMD_ID_SHIFT;
    if set {
        flags |= BCDC_DCMD_SET;
    }
    flags |= (ifidx as u32 & 0xF) << BCDC_DCMD_IF_SHIFT;
    let dcmd = HDRLEN;
    out[dcmd..dcmd + 4].copy_from_slice(&cmd.to_le_bytes());
    out[dcmd + 4..dcmd + 8].copy_from_slice(&(buflen as u32).to_le_bytes());
    out[dcmd + 8..dcmd + 12].copy_from_slice(&flags.to_le_bytes());
    out[dcmd + 12..dcmd + 16].copy_from_slice(&0u32.to_le_bytes());
    out[dcmd + DCMD_LEN..dcmd + DCMD_LEN + body].copy_from_slice(&payload[..body]);
}

/// An Ethernet frame for the firmware to send: SDPCM header, two bytes, BCDC
/// header, frame. `brcmf_sdio_txpkt` without glomming adds no padding at the
/// tail; the only rounding is `brcmf_sdiod_skbuff_write`'s, up to four bytes,
/// and the header's length leaves it out.
pub fn data_frame(out: &mut Vec<u8>, seq: u8, frame: &[u8]) {
    let offset = HDRLEN + DATA_PAD;
    let len = offset + BCDC_HEADER_LEN + frame.len();
    let pad = (4 - len % 4) % 4;
    out.clear();
    out.resize(len + pad, 0);
    pack_header(out, len as u16, seq, CHANNEL_DATA, offset as u8);
    out[offset] = BCDC_PROTO_VER << BCDC_FLAG_VER_SHIFT;
    out[offset + 1] = 0;
    out[offset + 2] = 0;
    out[offset + 3] = 0;
    out[offset + BCDC_HEADER_LEN..len].copy_from_slice(frame);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DcmdResponse {
    pub cmd: u32,
    pub len: u32,
    pub id: u16,
    pub ifidx: u8,
    /// The firmware's error code, when it flagged one.
    pub error: Option<i32>,
}

/// The dcmd header at the start of a control frame's payload, and what
/// follows it. `brcmf_proto_bcdc_query_dcmd` and `_set_dcmd`.
pub fn parse_dcmd(payload: &[u8]) -> Option<(DcmdResponse, &[u8])> {
    if payload.len() < DCMD_LEN {
        return None;
    }
    let word = |at: usize| u32::from_le_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]]);
    let flags = word(8);
    let response = DcmdResponse {
        cmd: word(0),
        len: word(4),
        id: (flags >> BCDC_DCMD_ID_SHIFT) as u16,
        ifidx: ((flags >> BCDC_DCMD_IF_SHIFT) & 0xF) as u8,
        error: if flags & BCDC_DCMD_ERROR != 0 { Some(word(12) as i32) } else { None },
    };
    Some((response, &payload[DCMD_LEN..]))
}

/// What follows a data or event frame's BCDC header, and the interface it is
/// for. `brcmf_proto_bcdc_hdrpull`: the version has to be 2, and the data
/// starts past the header and `data_offset` words of firmware signals.
pub fn strip_bcdc(payload: &[u8]) -> Option<(u8, &[u8])> {
    if payload.len() <= BCDC_HEADER_LEN {
        return None;
    }
    if (payload[0] & BCDC_FLAG_VER_MASK) >> BCDC_FLAG_VER_SHIFT != BCDC_PROTO_VER {
        return None;
    }
    let start = BCDC_HEADER_LEN + payload[3] as usize * 4;
    if start > payload.len() {
        return None;
    }
    Some((payload[2] & BCDC_FLAG2_IF_MASK, &payload[start..]))
}

// ---------------------------------------------------------------------------
// Commands and iovars, `fwil.h` and `fwil.c`
// ---------------------------------------------------------------------------

pub const C_UP: u32 = 2;
pub const C_GET_INFRA: u32 = 19;
pub const C_SET_INFRA: u32 = 20;
pub const C_SET_AUTH: u32 = 22;
pub const C_SET_SSID: u32 = 26;
pub const C_GET_REVINFO: u32 = 98;
pub const C_GET_VAR: u32 = 262;
pub const C_SET_VAR: u32 = 263;
pub const C_SET_WSEC_PMK: u32 = 268;

/// `brcmf_create_iovar`: the name, its terminator, and the data.
pub fn iovar(name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 1 + data.len());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(data);
    out
}

// ---------------------------------------------------------------------------
// Downloads, `common.c` and `fwil_types.h`
// ---------------------------------------------------------------------------

/// `MAX_CHUNK_LEN` in `common.c`.
pub const CLM_CHUNK: usize = 1400;
const DLOAD_HANDLER_VER: u16 = 1;
const DLOAD_FLAG_VER_SHIFT: u16 = 12;
pub const DL_BEGIN: u16 = 0x0002;
pub const DL_END: u16 = 0x0004;
const DL_TYPE_CLM: u16 = 2;

/// One chunk of a CLM download, `struct brcmf_dload_data_le`: flag, type,
/// length and a CRC of zero, then the data. `brcmf_c_download`.
pub fn clm_chunk(flag: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    let flag = flag | (DLOAD_HANDLER_VER << DLOAD_FLAG_VER_SHIFT);
    out.extend_from_slice(&flag.to_le_bytes());
    out.extend_from_slice(&DL_TYPE_CLM.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// `struct brcmf_fil_country_le` the way `brcmf_translate_country_code` fills
/// it for a 43455, which uses the ISO 3166 code with revision zero: the two
/// letters as the abbreviation and as the code.
pub fn country(code: [u8; 2]) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0] = code[0];
    out[1] = code[1];
    out[8] = code[0];
    out[9] = code[1];
    out
}

// ---------------------------------------------------------------------------
// Joining, `brcmu_wifi.h`, `fwil_types.h` and `cfg80211.c`
// ---------------------------------------------------------------------------

/// `wpa_auth` values, `brcmu_wifi.h`.
pub const WPA2_AUTH_UNSPECIFIED: u32 = 0x0040;
pub const WPA2_AUTH_PSK: u32 = 0x0080;
/// `wsec` for CCMP, `AES_ENABLED` in `brcmu_wifi.h`.
pub const AES_ENABLED: u32 = 0x0004;
/// `BRCMF_WSEC_PASSPHRASE` in `fwil_types.h`: the key field holds a
/// passphrase for the firmware to derive the PMK from.
pub const WSEC_PASSPHRASE: u16 = 1 << 0;
/// `struct brcmf_wsec_pmk_le`: key length, flags, and
/// `BRCMF_WSEC_MAX_SAE_PASSWORD_LEN` (128) bytes of key.
pub const WSEC_PMK_LEN: usize = 4 + 128;
/// `struct brcmf_ext_join_params_le` up to its chanspec list, which is what
/// `brcmf_cfg80211_connect` sends when no channel is named.
pub const EXT_JOIN_PARAMS_LEN: usize = 68;
/// `struct brcmf_ssid_le`, which is what the WLC_SET_SSID fallback in
/// `brcmf_cfg80211_connect` sends when no channel is named.
pub const SSID_LE_LEN: usize = 36;

/// A WLC_SET_WSEC_PMK request, as `brcmf_set_wsec` builds it. `fill` writes
/// the key straight into the field, so this module never holds a copy; the
/// caller zeroes the result once it is sent.
pub fn wsec_pmk(key_len: usize, flags: u16, fill: impl FnOnce(&mut [u8])) -> [u8; WSEC_PMK_LEN] {
    let mut out = [0u8; WSEC_PMK_LEN];
    out[0..2].copy_from_slice(&(key_len as u16).to_le_bytes());
    out[2..4].copy_from_slice(&flags.to_le_bytes());
    fill(&mut out[4..4 + key_len.min(128)]);
    out
}

/// The "join" iovar's data as `brcmf_cfg80211_connect` fills it with a BSSID
/// and no channel: the SSID, a scan type of -1 and every timing -1, the BSSID,
/// and no chanspecs. The offsets follow the structures' natural alignment:
/// `scan_type` at 36 then three bytes of padding, the four timings from 40,
/// the BSSID at 56 then two bytes, and `chanspec_num` at 64.
///
/// The BSSID is the access point the driver chose from its own scan, as
/// wpa_supplicant hands brcmfmac the one it chose from its. The firmware then
/// joins that access point and no other with the name, so the RSN element the
/// scan recorded for it is the one message 3 is checked against. With no
/// chanspec the firmware still finds the access point's channel with its own
/// scan.
pub fn ext_join_params(ssid_len: usize, bssid: [u8; 6], fill: impl FnOnce(&mut [u8])) -> [u8; EXT_JOIN_PARAMS_LEN] {
    let mut out = [0u8; EXT_JOIN_PARAMS_LEN];
    out[0..4].copy_from_slice(&(ssid_len as u32).to_le_bytes());
    fill(&mut out[4..4 + ssid_len.min(32)]);
    out[36] = 0xFF;
    for field in 0..4 {
        out[40 + field * 4..44 + field * 4].copy_from_slice(&(-1i32).to_le_bytes());
    }
    out[56..62].copy_from_slice(&bssid);
    out
}

/// `BRCMF_C_DISASSOC` in `fwil.h`.
pub const C_DISASSOC: u32 = 52;

/// `struct brcmf_scb_val_le` as `brcmf_cfg80211_disconnect` fills it for
/// WLC_DISASSOC: the reason code and the access point's address, padded to
/// twelve bytes by the structure's alignment.
pub fn scb_val(reason: u32, address: [u8; 6]) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&reason.to_le_bytes());
    out[4..10].copy_from_slice(&address);
    out
}

/// `CRYPTO_ALGO_AES_CCM` in `brcmu_wifi.h`.
pub const CRYPTO_ALGO_AES_CCM: u32 = 4;
/// `BRCMF_PRIMARY_KEY` in `fwil_types.h`.
pub const PRIMARY_KEY: u32 = 1 << 1;
/// `sizeof(struct brcmf_wsec_key_le)`: index, length, 32 bytes of key, 72 of
/// padding, algorithm, flags, 12 of padding, `iv_initialized`, 4 of padding,
/// the receive IV as a 32-bit and a 16-bit field padded to 8, 8 of padding,
/// and the peer address, padded to a multiple of 4. WHD's `wl_wsec_key_t` has
/// the same offsets.
pub const WSEC_KEY_LEN: usize = 164;

/// The "wsec_key" iovar's data for a CCMP key, as `brcmf_cfg80211_add_key`
/// fills `struct brcmf_wsec_key` and `convert_key_from_CPU` lays it out.
/// wpa_supplicant's nl80211 driver names the access point for a pairwise key
/// and no address for a group key, so a pairwise key is an "ext_key" with the
/// peer's address and no flags, and a group key has `BRCMF_PRIMARY_KEY`. Both
/// come with a six-byte sequence counter, which becomes the receive IV with
/// `iv_initialized` set. `fill` writes the key into the data field.
pub fn wsec_key(
    index: u32,
    key_len: usize,
    peer: Option<[u8; 6]>,
    rsc: [u8; 6],
    fill: impl FnOnce(&mut [u8]),
) -> [u8; WSEC_KEY_LEN] {
    let mut out = [0u8; WSEC_KEY_LEN];
    out[0..4].copy_from_slice(&index.to_le_bytes());
    out[4..8].copy_from_slice(&(key_len as u32).to_le_bytes());
    fill(&mut out[8..8 + key_len.min(32)]);
    out[112..116].copy_from_slice(&CRYPTO_ALGO_AES_CCM.to_le_bytes());
    let flags = if peer.is_some() { 0 } else { PRIMARY_KEY };
    out[116..120].copy_from_slice(&flags.to_le_bytes());
    out[132..136].copy_from_slice(&1u32.to_le_bytes());
    let hi = u32::from_le_bytes([rsc[2], rsc[3], rsc[4], rsc[5]]);
    let lo = u16::from_le_bytes([rsc[0], rsc[1]]);
    out[140..144].copy_from_slice(&hi.to_le_bytes());
    out[144..146].copy_from_slice(&lo.to_le_bytes());
    if let Some(peer) = peer {
        out[156..162].copy_from_slice(&peer);
    }
    out
}

/// The WLC_SET_SSID fallback's data: the SSID alone.
pub fn ssid_le(ssid_len: usize, fill: impl FnOnce(&mut [u8])) -> [u8; SSID_LE_LEN] {
    let mut out = [0u8; SSID_LE_LEN];
    out[0..4].copy_from_slice(&(ssid_len as u32).to_le_bytes());
    fill(&mut out[4..4 + ssid_len.min(32)]);
    out
}

// ---------------------------------------------------------------------------
// Events, `fweh.h`
// ---------------------------------------------------------------------------

pub const E_SET_SSID: u32 = 0;
pub const E_AUTH: u32 = 3;
pub const E_DEAUTH: u32 = 5;
pub const E_DEAUTH_IND: u32 = 6;
pub const E_ASSOC: u32 = 7;
pub const E_REASSOC: u32 = 9;
pub const E_DISASSOC: u32 = 11;
pub const E_DISASSOC_IND: u32 = 12;
pub const E_LINK: u32 = 16;
pub const E_PRUNE: u32 = 23;
pub const E_IF: u32 = 54;
pub const E_PSK_SUP: u32 = 46;
pub const E_ESCAN_RESULT: u32 = 69;

/// The name `fweh.h` gives an event this driver asks for, for the log.
pub fn event_name(event_type: u32) -> &'static str {
    match event_type {
        E_SET_SSID => "SET_SSID",
        E_AUTH => "AUTH",
        E_DEAUTH => "DEAUTH",
        E_DEAUTH_IND => "DEAUTH_IND",
        E_ASSOC => "ASSOC",
        E_REASSOC => "REASSOC",
        E_DISASSOC => "DISASSOC",
        E_DISASSOC_IND => "DISASSOC_IND",
        E_LINK => "LINK",
        E_PRUNE => "PRUNE",
        E_PSK_SUP => "PSK_SUP",
        E_IF => "IF",
        E_ESCAN_RESULT => "ESCAN_RESULT",
        _ => "other",
    }
}
/// `BRCMF_E_LAST`.
pub const E_LAST: u32 = 191;
/// `BRCMF_EVENTING_MASK_LEN`.
pub const EVENTING_MASK_LEN: usize = (E_LAST as usize).div_ceil(8);

pub const E_STATUS_SUCCESS: u32 = 0;
pub const E_STATUS_FAIL: u32 = 1;
pub const E_STATUS_NO_NETWORKS: u32 = 3;
pub const E_STATUS_PARTIAL: u32 = 8;
/// `BRCMF_E_STATUS_FWSUP_COMPLETED`, which a PSK_SUP event carries once the
/// firmware's supplicant has installed the keys.
pub const E_STATUS_FWSUP_COMPLETED: u32 = 6;
/// `BRCMF_EVENT_MSG_LINK`.
pub const EVENT_MSG_LINK: u16 = 0x01;

/// `ETH_P_LINK_CTL`, the ethertype events arrive under.
const ETH_P_LINK_CTL: u16 = 0x886C;
/// `BRCM_OUI`.
const BRCM_OUI: [u8; 3] = [0x00, 0x10, 0x18];
/// `BCMILCP_BCM_SUBTYPE_EVENT`.
const BCMILCP_BCM_SUBTYPE_EVENT: u16 = 1;
/// `struct brcmf_event`: Ethernet header 14, Broadcom header 10, message 48.
pub const EVENT_PACKET_LEN: usize = 14 + 10 + 48;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    pub flags: u16,
    pub event_type: u32,
    pub status: u32,
    pub reason: u32,
    pub auth_type: u32,
    pub datalen: u32,
    pub addr: [u8; 6],
    pub ifidx: u8,
    pub bsscfgidx: u8,
}

/// An event packet: the checks `brcmf_fweh_process_skb` makes before handing
/// it on, and the length check `brcmf_fweh_process_event` makes on its data.
pub fn parse_event(packet: &[u8]) -> Option<(Event, &[u8])> {
    if packet.len() < EVENT_PACKET_LEN {
        return None;
    }
    let be16 = |at: usize| u16::from_be_bytes([packet[at], packet[at + 1]]);
    let be32 = |at: usize| u32::from_be_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]]);
    if be16(12) != ETH_P_LINK_CTL {
        return None;
    }
    let hdr = 14;
    if packet[hdr + 5..hdr + 8] != BRCM_OUI || be16(hdr + 8) != BCMILCP_BCM_SUBTYPE_EVENT {
        return None;
    }
    let msg = hdr + 10;
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&packet[msg + 24..msg + 30]);
    let event = Event {
        flags: be16(msg + 2),
        event_type: be32(msg + 4),
        status: be32(msg + 8),
        reason: be32(msg + 12),
        auth_type: be32(msg + 16),
        datalen: be32(msg + 20),
        addr,
        ifidx: packet[msg + 46],
        bsscfgidx: packet[msg + 47],
    };
    let datalen = event.datalen as usize;
    if datalen > DCMD_MAXLEN || EVENT_PACKET_LEN + datalen > packet.len() {
        return None;
    }
    Some((event, &packet[EVENT_PACKET_LEN..EVENT_PACKET_LEN + datalen]))
}

/// The mask for `event_msgs`, one bit per event.
pub fn event_mask(events: &[u32]) -> [u8; EVENTING_MASK_LEN] {
    let mut mask = [0u8; EVENTING_MASK_LEN];
    for &event in events {
        if (event as usize) < EVENTING_MASK_LEN * 8 {
            mask[event as usize / 8] |= 1 << (event % 8);
        }
    }
    mask
}

// ---------------------------------------------------------------------------
// Scanning, `fwil_types.h` and `cfg80211.c`
// ---------------------------------------------------------------------------

const BRCMF_ESCAN_REQ_VERSION: u32 = 1;
const WL_ESCAN_ACTION_START: u16 = 1;
const DOT11_BSSTYPE_ANY: u8 = 2;
const BRCMF_SCANTYPE_ACTIVE: u8 = 0;
/// `sizeof(struct brcmf_escan_result_le)` less its first BSS.
pub const ESCAN_RESULTS_FIXED_SIZE: usize = 12;
/// The part of `struct brcmf_bss_info_le` read here, through `ie_length`,
/// which with the structure's natural alignment ends at byte 124.
const BSS_INFO_READ: usize = 124;

/// An escan request with version 1 parameters, as `brcmf_run_escan` builds it
/// for firmware without `scan_ver`: every channel, any BSS type, active, the
/// firmware's own timings, and no SSID. `brcmf_escan_prep` and
/// `brcmf_scan_params_v2_to_v1`.
pub fn escan_request(sync_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 64);
    out.extend_from_slice(&BRCMF_ESCAN_REQ_VERSION.to_le_bytes());
    out.extend_from_slice(&WL_ESCAN_ACTION_START.to_le_bytes());
    out.extend_from_slice(&sync_id.to_le_bytes());
    // `struct brcmf_scan_params_le`.
    out.extend_from_slice(&[0u8; 36]); // ssid_le: length zero
    out.extend_from_slice(&[0xFF; 6]); // bssid: broadcast
    out.push(DOT11_BSSTYPE_ANY);
    out.push(BRCMF_SCANTYPE_ACTIVE);
    for _ in 0..4 {
        out.extend_from_slice(&(-1i32).to_le_bytes()); // nprobes, active, passive, home
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // channel_num
    out
}

/// The security a network advertises in its RSN information element (IEEE
/// 802.11-2020, 9.4.2.24), and whether it also has the older WPA one. Suites
/// are the OUI and type as one number, so 0x000FAC04 is CCMP and 0x000FAC02
/// is PSK.
#[derive(Clone, Copy, Default)]
pub struct Security {
    pub rsn: bool,
    pub wpa: bool,
    pub group: u32,
    pub pairwise: [u32; 4],
    pub pairwise_count: usize,
    pub akm: [u32; 4],
    pub akm_count: usize,
    pub capabilities: Option<u16>,
}

/// `RSN_CAP_MFPR_MASK` and `RSN_CAP_MFPC_MASK` as `cfg80211.c` reads them.
pub const RSN_CAP_MFPR: u16 = 1 << 6;
pub const RSN_CAP_MFPC: u16 = 1 << 7;
pub const SUITE_CCMP: u32 = 0x000F_AC04;
pub const AKM_PSK: u32 = 0x000F_AC02;
pub const AKM_PSK_SHA256: u32 = 0x000F_AC06;
pub const AKM_SAE: u32 = 0x000F_AC08;

const WLAN_EID_RSN: u8 = 48;
const WLAN_EID_VENDOR_SPECIFIC: u8 = 221;
/// The WPA element is vendor-specific: Microsoft's OUI and type 1.
const WPA_OUI_TYPE: [u8; 4] = [0x00, 0x50, 0xF2, 0x01];

/// Read the RSN and WPA elements out of a run of information elements. A
/// truncated element or list is read as far as it goes.
pub fn parse_security(ies: &[u8]) -> Security {
    let mut security = Security::default();
    let mut at = 0;
    while at + 2 <= ies.len() {
        let id = ies[at];
        let len = ies[at + 1] as usize;
        let Some(body) = ies.get(at + 2..at + 2 + len) else { break };
        if id == WLAN_EID_VENDOR_SPECIFIC && body.starts_with(&WPA_OUI_TYPE) {
            security.wpa = true;
        }
        if id == WLAN_EID_RSN && body.len() >= 2 {
            security.rsn = true;
            let suite = |at: usize| body.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
            let count = |at: usize| body.get(at..at + 2).map(|c| u16::from_le_bytes([c[0], c[1]]) as usize);
            let mut p = 2;
            if let Some(group) = suite(p) {
                security.group = group;
                p += 4;
                if let Some(n) = count(p) {
                    p += 2;
                    for i in 0..n {
                        if let Some(s) = suite(p + i * 4) {
                            if security.pairwise_count < 4 {
                                security.pairwise[security.pairwise_count] = s;
                                security.pairwise_count += 1;
                            }
                        }
                    }
                    p += n * 4;
                    if let Some(n) = count(p) {
                        p += 2;
                        for i in 0..n {
                            if let Some(s) = suite(p + i * 4) {
                                if security.akm_count < 4 {
                                    security.akm[security.akm_count] = s;
                                    security.akm_count += 1;
                                }
                            }
                        }
                        p += n * 4;
                        security.capabilities = count(p).map(|c| c as u16);
                    }
                }
            }
        }
        at += 2 + len;
    }
    security
}

/// One network from a scan.
#[derive(Clone)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: [u8; 32],
    pub ssid_len: usize,
    pub chanspec: u16,
    pub channel: u8,
    pub rssi: i16,
    pub capability: u16,
    pub security: Security,
    /// The RSN and RSNX elements whole, header included, which the
    /// supplicant compares with message 3's. `sm->ap_rsn_ie` and
    /// `sm->ap_rsnxe`.
    pub rsn_element: Option<Vec<u8>>,
    pub rsnx_element: Option<Vec<u8>>,
    /// Whether the network has a WMM element, which decides the replay
    /// counters this station advertises.
    pub wmm: bool,
}

/// `WLAN_EID_RSNX` in hostap's `ieee802_11_defs.h`.
const WLAN_EID_RSNX: u8 = 244;
/// `WMM_IE_VENDOR_TYPE`: Microsoft's OUI and type 2, any subtype, which
/// `wpa_bss_get_vendor_ie` matches on.
const WMM_OUI_TYPE: [u8; 4] = [0x00, 0x50, 0xF2, 0x02];

/// The first RSN and RSNX elements whole, and whether there is a WMM element,
/// in a run of information elements.
pub fn find_elements(ies: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>, bool) {
    let mut rsn = None;
    let mut rsnx = None;
    let mut wmm = false;
    let mut at = 0;
    while at + 2 <= ies.len() {
        let len = ies[at + 1] as usize;
        let Some(element) = ies.get(at..at + 2 + len) else { break };
        match element[0] {
            WLAN_EID_RSN if rsn.is_none() => rsn = Some(element.to_vec()),
            WLAN_EID_RSNX if rsnx.is_none() => rsnx = Some(element.to_vec()),
            WLAN_EID_VENDOR_SPECIFIC if element[2..].starts_with(&WMM_OUI_TYPE) => wmm = true,
            _ => {}
        }
        at += 2 + len;
    }
    (rsn, rsnx, wmm)
}

impl Bss {
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..self.ssid_len]
    }
}

/// The BSS in an escan result event. `brcmf_cfg80211_escan_handler`: the
/// buffer length has to fit the event, one BSS per event, and the BSS's own
/// length has to account for the rest. The channel is `ctl_ch`, or when that
/// is zero the channel number in the chanspec's low byte, which is the
/// control channel of a 20 MHz chanspec.
pub fn parse_escan_result(data: &[u8]) -> Option<Bss> {
    if data.len() < ESCAN_RESULTS_FIXED_SIZE + BSS_INFO_READ {
        return None;
    }
    let le16 = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
    let le32 = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let buflen = le32(0) as usize;
    if buflen > data.len() || buflen < ESCAN_RESULTS_FIXED_SIZE + BSS_INFO_READ {
        return None;
    }
    if le16(10) != 1 {
        return None;
    }
    let bss = ESCAN_RESULTS_FIXED_SIZE;
    let length = le32(bss + 4) as usize;
    if length != buflen - ESCAN_RESULTS_FIXED_SIZE {
        return None;
    }
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&data[bss + 8..bss + 14]);
    let ssid_len = (data[bss + 18] as usize).min(32);
    let mut ssid = [0u8; 32];
    ssid[..ssid_len].copy_from_slice(&data[bss + 19..bss + 19 + ssid_len]);
    let chanspec = le16(bss + 72);
    let ctl_ch = data[bss + 88];
    // `brcmf_inform_single_bss`: the elements are `ie_length` bytes from
    // `ie_offset`, both measured from the start of the BSS. Elements that do
    // not fit inside the event are not read.
    let ie_offset = le16(bss + 116) as usize;
    let ie_length = le32(bss + 120) as usize;
    let (security, (rsn_element, rsnx_element, wmm)) =
        match data.get(bss + ie_offset..(bss + ie_offset).saturating_add(ie_length)) {
            Some(ies) if bss + ie_offset + ie_length <= buflen => (parse_security(ies), find_elements(ies)),
            _ => (Security::default(), (None, None, false)),
        };
    Some(Bss {
        bssid,
        ssid,
        ssid_len,
        chanspec,
        channel: if ctl_ch != 0 { ctl_ch } else { chanspec as u8 },
        rssi: le16(bss + 78) as i16,
        capability: le16(bss + 16),
        security,
        rsn_element,
        rsnx_element,
        wmm,
    })
}
