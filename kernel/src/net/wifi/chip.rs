//! The chip behind the SDIO card, reached through function 1's window onto
//! its backplane, and getting its firmware running.
//!
//! Ported from Linux's brcmfmac in Raspberry Pi's `rpi-6.6.y`
//! (`drivers/net/wireless/broadcom/brcm80211/brcmfmac/`): `bcmsdh.c` for the
//! window and memory access, `chip.c` for finding the cores and resetting
//! them, and `sdio.c` for the clock requests and the order in which firmware
//! is put on the chip and started. Register offsets are that driver's
//! `sdio.h` and `sdio.c`, and `include/chipcommon.h` and `soc.h` beside it;
//! core ids and the wrapper registers are `include/linux/bcma/bcma.h` and
//! `bcma_regs.h`.
//!
//! Function 1 answers 17-bit addresses. The low 15 bits of one are an offset
//! into a 32 KiB window onto the chip's 32-bit backplane, and the window's
//! base is set with three function 1 registers. Bit 15 set asks for a 32-bit
//! access, so a register read is four bytes read at `offset | 0x8000`.

use super::delay::{now_us, sleep_ms, spin_us, Deadline};
use super::sdio::{Card, SdioError};
use alloc::vec::Vec;

pub const FUNC_BACKPLANE: u8 = 1;
pub const FUNC_WLAN: u8 = 2;

// ---------------------------------------------------------------------------
// Function 1's own registers, `sdio.h`
// ---------------------------------------------------------------------------

pub const SBSDIO_WATERMARK: u32 = 0x10008;
pub const SBSDIO_DEVICE_CTL: u32 = 0x10009;
const SBSDIO_FUNC1_SBADDRLOW: u32 = 0x1000A;
pub const SBSDIO_FUNC1_FRAMECTRL: u32 = 0x1000D;
pub const SBSDIO_FUNC1_CHIPCLKCSR: u32 = 0x1000E;
const SBSDIO_FUNC1_SDIOPULLUP: u32 = 0x1000F;
pub const SBSDIO_FUNC1_WFRAMEBCLO: u32 = 0x10019;
pub const SBSDIO_FUNC1_WFRAMEBCHI: u32 = 0x1001A;
pub const SBSDIO_FUNC1_RFRAMEBCLO: u32 = 0x1001B;
pub const SBSDIO_FUNC1_RFRAMEBCHI: u32 = 0x1001C;
const SBSDIO_FUNC1_MESBUSYCTRL: u32 = 0x1001D;
const SBSDIO_FUNC1_WAKEUPCTRL: u32 = 0x1001E;
pub const SBSDIO_FUNC1_SLEEPCSR: u32 = 0x1001F;

const SBSDIO_SB_OFT_ADDR_MASK: u32 = 0x07FFF;
const SBSDIO_SB_OFT_ADDR_LIMIT: u32 = 0x08000;
const SBSDIO_SB_ACCESS_2_4B_FLAG: u32 = 0x08000;
const SBSDIO_SBWINDOW_MASK: u32 = 0xFFFF_8000;

const SBSDIO_MESBUSYCTRL_ENAB: u8 = 0x80;
const SBSDIO_DEVCTL_F2WM_ENAB: u8 = 0x10;
const SBSDIO_FUNC1_WCTRL_HTWAIT_SHIFT: u8 = 1;
const SBSDIO_FUNC1_SLEEPCSR_KSO_MASK: u8 = 0x1;
pub const SBSDIO_FUNC1_SLEEPCSR_DEVON_MASK: u8 = 0x2;

// Chip clock control and status, `sdio.c`.
const SBSDIO_FORCE_ALP: u8 = 0x01;
const SBSDIO_FORCE_HT: u8 = 0x02;
const SBSDIO_ALP_AVAIL_REQ: u8 = 0x08;
const SBSDIO_HT_AVAIL_REQ: u8 = 0x10;
const SBSDIO_FORCE_HW_CLKREQ_OFF: u8 = 0x20;
const SBSDIO_ALP_AVAIL: u8 = 0x40;
const SBSDIO_HT_AVAIL: u8 = 0x80;
const SBSDIO_AVBITS: u8 = SBSDIO_HT_AVAIL | SBSDIO_ALP_AVAIL;
/// `BRCMF_INIT_CLKCTL1`.
const INIT_CLKCTL1: u8 = SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_ALP_AVAIL_REQ;
/// `PMU_MAX_TRANSITION_DLY` as `sdio.c` redefines it: a second, because the
/// first startup may compute a CRC before the PMU makes HT available.
const PMU_MAX_TRANSITION_DLY_US: u64 = 1_000_000;

// Function 0 registers particular to this vendor, `sdio.h`.
const SDIO_CCCR_BRCM_CARDCAP: u32 = 0xF0;
const SDIO_CCCR_BRCM_CARDCAP_CMD14_SUPPORT: u8 = 1 << 1;
const SDIO_CCCR_BRCM_CARDCAP_CMD14_EXT: u8 = 1 << 2;
const SDIO_CCCR_BRCM_CARDCTRL: u32 = 0xF1;
const SDIO_CCCR_BRCM_CARDCTRL_WLANRESET: u8 = 1 << 1;

// ---------------------------------------------------------------------------
// The backplane
// ---------------------------------------------------------------------------

/// `SI_ENUM_BASE_DEFAULT`: chipcommon, the first core, is here on every chip
/// this driver family knows.
pub const SI_ENUM_BASE: u32 = 0x1800_0000;

// Chipcommon registers, `struct chipcregs` in `chipcommon.h`.
const CC_CHIPID: u32 = 0x00;
const CC_CAPABILITIES: u32 = 0x04;
const CC_CAPABILITIES_EXT: u32 = 0xAC;
const CC_EROMPTR: u32 = 0xFC;
const CC_PMUCONTROL: u32 = 0x600;
const CC_PMUCAPABILITIES: u32 = 0x604;
const CC_CHIPCONTROL_ADDR: u32 = 0x650;
const CC_CHIPCONTROL_DATA: u32 = 0x654;

const CID_ID_MASK: u32 = 0x0000_FFFF;
const CID_REV_MASK: u32 = 0x000F_0000;
const CID_REV_SHIFT: u32 = 16;
const CID_TYPE_MASK: u32 = 0xF000_0000;
const CID_TYPE_SHIFT: u32 = 28;
/// `SOCI_AI`, an AXI backplane. The other kind, `SOCI_SB`, only ever carried
/// the 4329.
const SOCI_AI: u32 = 1;
const CC_CAP_PMU: u32 = 0x1000_0000;
const PCAP_REV_MASK: u32 = 0x0000_00FF;
const BCMA_CC_CAP_EXT_AOB_PRESENT: u32 = 0x0000_0040;
const BCMA_CC_PMU_CTL_RES_SHIFT: u32 = 13;
const BCMA_CC_PMU_CTL_RES_RELOAD: u32 = 0x2;

// Core ids, `bcma.h`.
pub const BCMA_CORE_CHIPCOMMON: u16 = 0x800;
pub const BCMA_CORE_INTERNAL_MEM: u16 = 0x80E;
pub const BCMA_CORE_80211: u16 = 0x812;
pub const BCMA_CORE_PMU: u16 = 0x827;
pub const BCMA_CORE_SDIO_DEV: u16 = 0x829;
pub const BCMA_CORE_ARM_CM3: u16 = 0x82A;
pub const BCMA_CORE_ARM_CR4: u16 = 0x83E;
pub const BCMA_CORE_GCI: u16 = 0x840;

// Agent (wrapper) registers, `bcma_regs.h`.
const BCMA_IOCTL: u32 = 0x0408;
const BCMA_IOCTL_CLK: u32 = 0x0001;
const BCMA_IOCTL_FGC: u32 = 0x0002;
const BCMA_RESET_CTL: u32 = 0x0800;
const BCMA_RESET_CTL_RESET: u32 = 0x0001;

// Core-specific control bits, `chip.c`.
const ARMCR4_BCMA_IOCTL_CPUHALT: u32 = 0x0020;
const D11_BCMA_IOCTL_PHYCLOCKEN: u32 = 0x0004;
const D11_BCMA_IOCTL_PHYRESET: u32 = 0x0008;

// The CR4 core's memory description, `chip.c`.
const ARMCR4_CAP: u32 = 0x04;
const ARMCR4_BANKIDX: u32 = 0x40;
const ARMCR4_BANKINFO: u32 = 0x44;
const ARMCR4_TCBBNB_MASK: u32 = 0xF0;
const ARMCR4_TCBBNB_SHIFT: u32 = 4;
const ARMCR4_TCBANB_MASK: u32 = 0x0F;
const ARMCR4_BSZ_MASK: u32 = 0x7F;
const ARMCR4_BSZ_MULT: u32 = 8192;
const ARMCR4_BLK_1K_MASK: u32 = 0x200;
/// `BRCMF_CHIP_MAX_MEMSIZE`.
const MAX_MEMSIZE: u32 = 4 * 1024 * 1024;

// Enumeration ROM descriptors, `chip.c`.
const DMP_DESC_TYPE_MSK: u32 = 0x0000_000F;
const DMP_DESC_EMPTY: u32 = 0x0000_0000;
const DMP_DESC_VALID: u32 = 0x0000_0001;
const DMP_DESC_COMPONENT: u32 = 0x0000_0001;
const DMP_DESC_MASTER_PORT: u32 = 0x0000_0003;
const DMP_DESC_ADDRESS: u32 = 0x0000_0005;
const DMP_DESC_ADDRSIZE_GT32: u32 = 0x0000_0008;
const DMP_DESC_EOT: u32 = 0x0000_000F;
const DMP_COMP_PARTNUM: u32 = 0x000F_FF00;
const DMP_COMP_PARTNUM_S: u32 = 8;
const DMP_COMP_REVISION: u32 = 0xFF00_0000;
const DMP_COMP_REVISION_S: u32 = 24;
const DMP_COMP_NUM_SWRAP: u32 = 0x00F8_0000;
const DMP_COMP_NUM_SWRAP_S: u32 = 19;
const DMP_COMP_NUM_MWRAP: u32 = 0x0007_C000;
const DMP_COMP_NUM_MWRAP_S: u32 = 14;
const DMP_SLAVE_ADDR_BASE: u32 = 0xFFFF_F000;
const DMP_SLAVE_TYPE: u32 = 0x0000_00C0;
const DMP_SLAVE_TYPE_S: u32 = 6;
const DMP_SLAVE_TYPE_SLAVE: u32 = 0;
const DMP_SLAVE_TYPE_SWRAP: u32 = 2;
const DMP_SLAVE_TYPE_MWRAP: u32 = 3;
const DMP_SLAVE_SIZE_TYPE: u32 = 0x0000_0030;
const DMP_SLAVE_SIZE_TYPE_S: u32 = 4;
const DMP_SLAVE_SIZE_4K: u32 = 0;
const DMP_SLAVE_SIZE_8K: u32 = 1;
const DMP_SLAVE_SIZE_DESC: u32 = 3;

// The SDIO device core's registers, `struct sdpcmd_regs` in `sdio.h`.
pub const SD_INTSTATUS: u32 = 0x20;
pub const SD_HOSTINTMASK: u32 = 0x24;
pub const SD_TOSBMAILBOX: u32 = 0x40;
pub const SD_TOSBMAILBOXDATA: u32 = 0x48;
pub const SD_TOHOSTMAILBOXDATA: u32 = 0x4C;

// `sdio.c`: interrupt status and the mailbox protocol.
pub const I_HMB_SW_MASK: u32 = 0x0000_00F0;
pub const I_HMB_FC_STATE: u32 = 1 << 4;
pub const I_HMB_FC_CHANGE: u32 = 1 << 5;
pub const I_HMB_FRAME_IND: u32 = 1 << 6;
pub const I_HMB_HOST_INT: u32 = 1 << 7;
pub const I_CHIPACTIVE: u32 = 1 << 29;
/// `HOSTINTMASK`.
pub const HOSTINTMASK: u32 = I_HMB_SW_MASK | I_CHIPACTIVE;
pub const SMB_NAK: u32 = 1 << 0;
pub const SMB_INT_ACK: u32 = 1 << 1;
const SMB_DATA_VERSION_SHIFT: u32 = 16;
pub const HMB_DATA_NAKHANDLED: u32 = 0x0001;
pub const HMB_DATA_DEVREADY: u32 = 0x0002;
pub const HMB_DATA_FC: u32 = 0x0004;
pub const HMB_DATA_FWREADY: u32 = 0x0008;
pub const HMB_DATA_FWHALT: u32 = 0x0010;
pub const HMB_DATA_VERSION_MASK: u32 = 0x00FF_0000;
pub const HMB_DATA_VERSION_SHIFT: u32 = 16;
/// `SDPCM_PROT_VERSION`.
pub const SDPCM_PROT_VERSION: u32 = 4;

// The 43455 in particular.
pub const BRCM_CC_4345_CHIP_ID: u16 = 0x4345;
const BRCM_CC_43454_CHIP_ID: u16 = 43454;
/// `brcmf_chip_tcm_rambase`.
const RAMBASE_4345: u32 = 0x198000;
/// `brcmf_sdio_firmware_callback`, `SDIO_DEVICE_ID_BROADCOM_43455`.
const CY_43455_F2_WATERMARK: u8 = 0x60;
const CY_43455_MES_WATERMARK: u8 = 0x50;
/// The watermark of the switch's default branch, `DEFAULT_F2_WATERMARK`.
const DEFAULT_F2_WATERMARK: u8 = 0x08;
/// SDIO device ids, `include/linux/mmc/sdio_ids.h`. The Pi 4's card reports
/// the first although its chip is a 43455.
pub const SDIO_DEVICE_ID_BROADCOM_43430: u16 = 0xa9a6;
pub const SDIO_DEVICE_ID_BROADCOM_43455: u16 = 0xa9bf;

/// `SDIO_FUNC1_BLOCKSIZE` and `SDIO_FUNC2_BLOCKSIZE` in `bcmsdh.c`; the 43455
/// takes the default for function 2.
pub const F1_BLOCK_SIZE: u16 = 64;
pub const F2_BLOCK_SIZE: u16 = 512;
/// `SDIO_WAIT_F2RDY`, the time function 2 is given to come ready.
pub const F2_READY_TIMEOUT_MS: u32 = 3000;

/// Function 1 as a window onto the backplane.
pub struct Backplane {
    card: Card,
    /// Where the window is, once set. `sdiodev->sbwad`.
    window: Option<u32>,
}

impl Backplane {
    pub fn new(card: Card) -> Backplane {
        Backplane { card, window: None }
    }

    pub fn card(&mut self) -> &mut Card {
        &mut self.card
    }

    /// A function 1 register, one byte. `brcmf_sdiod_readb`.
    pub fn read8(&mut self, register: u32) -> Result<u8, SdioError> {
        self.card.read_byte(FUNC_BACKPLANE, register)
    }

    pub fn write8(&mut self, register: u32, value: u8) -> Result<(), SdioError> {
        self.card.write_byte(FUNC_BACKPLANE, register, value)
    }

    /// Point the window at the part of the backplane holding `address`.
    /// `brcmf_sdiod_set_backplane_window`: the base shifted down a byte, into
    /// the three address registers from the low one up.
    fn set_window(&mut self, address: u32) -> Result<(), SdioError> {
        let base = address & SBSDIO_SBWINDOW_MASK;
        if self.window == Some(base) {
            return Ok(());
        }
        let mut value = base >> 8;
        for i in 0..3 {
            self.card.write_byte(FUNC_BACKPLANE, SBSDIO_FUNC1_SBADDRLOW + i, value as u8)?;
            value >>= 8;
        }
        self.window = Some(base);
        Ok(())
    }

    /// A 32-bit backplane register. `brcmf_sdiod_readl`.
    pub fn read32(&mut self, address: u32) -> Result<u32, SdioError> {
        self.set_window(address)?;
        let offset = (address & SBSDIO_SB_OFT_ADDR_MASK) | SBSDIO_SB_ACCESS_2_4B_FLAG;
        let mut bytes = [0u8; 4];
        self.card.read(FUNC_BACKPLANE, offset, true, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// `brcmf_sdiod_writel`.
    pub fn write32(&mut self, address: u32, value: u32) -> Result<(), SdioError> {
        self.set_window(address)?;
        let offset = (address & SBSDIO_SB_OFT_ADDR_MASK) | SBSDIO_SB_ACCESS_2_4B_FLAG;
        self.card.write(FUNC_BACKPLANE, offset, true, &value.to_le_bytes())
    }

    /// Write `data` into chip memory. `brcmf_sdiod_ramrw`: the first piece
    /// runs to the end of the window it starts in, and every piece after that
    /// is a whole window or the rest. Each piece is rounded up to four bytes,
    /// as `brcmf_sdiod_skbuff_write` rounds its request, with zeroes.
    pub fn ram_write(&mut self, mut address: u32, data: &[u8]) -> Result<(), SdioError> {
        let mut done = 0usize;
        let mut piece = Vec::with_capacity(SBSDIO_SB_OFT_ADDR_LIMIT as usize + 4);
        while done < data.len() {
            let offset = address & SBSDIO_SB_OFT_ADDR_MASK;
            let room = (SBSDIO_SB_OFT_ADDR_LIMIT - offset) as usize;
            let size = (data.len() - done).min(room);
            self.set_window(address)?;
            piece.clear();
            piece.extend_from_slice(&data[done..done + size]);
            while piece.len() % 4 != 0 {
                piece.push(0);
            }
            self.card.write(FUNC_BACKPLANE, offset | SBSDIO_SB_ACCESS_2_4B_FLAG, true, &piece)?;
            done += size;
            address = address.wrapping_add(size as u32);
        }
        Ok(())
    }

    /// Read chip memory into `out`, in the same pieces.
    pub fn ram_read(&mut self, mut address: u32, out: &mut [u8]) -> Result<(), SdioError> {
        let mut done = 0usize;
        let mut piece = Vec::with_capacity(SBSDIO_SB_OFT_ADDR_LIMIT as usize + 4);
        while done < out.len() {
            let offset = address & SBSDIO_SB_OFT_ADDR_MASK;
            let room = (SBSDIO_SB_OFT_ADDR_LIMIT - offset) as usize;
            let size = (out.len() - done).min(room);
            self.set_window(address)?;
            piece.clear();
            piece.resize((size + 3) & !3, 0);
            self.card.read(FUNC_BACKPLANE, offset | SBSDIO_SB_ACCESS_2_4B_FLAG, true, &mut piece)?;
            out[done..done + size].copy_from_slice(&piece[..size]);
            done += size;
            address = address.wrapping_add(size as u32);
        }
        Ok(())
    }

    /// Read from function 2's frame FIFO. `brcmf_sdiod_recv_pkt`: the window
    /// at chipcommon, the 32-bit flag on, and a fixed address (`sdio_readsb`).
    pub fn f2_read(&mut self, buf: &mut [u8]) -> Result<(), SdioError> {
        self.set_window(SI_ENUM_BASE)?;
        let address = (SI_ENUM_BASE & SBSDIO_SB_OFT_ADDR_MASK) | SBSDIO_SB_ACCESS_2_4B_FLAG;
        self.card.read(FUNC_WLAN, address, false, buf)
    }

    /// Write to function 2. `brcmf_sdiod_send_buf`, which goes through
    /// `sdio_memcpy_toio` and so increments the address.
    pub fn f2_write(&mut self, data: &[u8]) -> Result<(), SdioError> {
        self.set_window(SI_ENUM_BASE)?;
        let address = (SI_ENUM_BASE & SBSDIO_SB_OFT_ADDR_MASK) | SBSDIO_SB_ACCESS_2_4B_FLAG;
        self.card.write(FUNC_WLAN, address, true, data)
    }
}

// ---------------------------------------------------------------------------
// The cores
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Core {
    pub id: u16,
    pub rev: u8,
    pub base: u32,
    pub wrap: u32,
}

pub struct Chip {
    pub id: u16,
    pub rev: u8,
    pub cores: Vec<Core>,
    pub rambase: u32,
    pub ramsize: u32,
    pub cc_caps: u32,
    pub cc_caps_ext: u32,
    pub pmurev: u32,
    pub pmucaps: u32,
}

fn plain(what: &'static str) -> SdioError {
    SdioError { what, host: None }
}

impl Chip {
    pub fn core(&self, id: u16) -> Option<Core> {
        self.cores.iter().copied().find(|core| core.id == id)
    }

    fn d11(&self, unit: usize) -> Option<Core> {
        self.cores.iter().copied().filter(|core| core.id == BCMA_CORE_80211).nth(unit)
    }

    /// `brcmf_chip_get_pmu`: a separate PMU core from chipcommon revision 35
    /// on, where the extended capabilities say there is one.
    pub fn pmu(&self) -> Option<Core> {
        let cc = self.core(BCMA_CORE_CHIPCOMMON)?;
        if cc.rev >= 35 && self.cc_caps_ext & BCMA_CC_CAP_EXT_AOB_PRESENT != 0 {
            if let Some(pmu) = self.core(BCMA_CORE_PMU) {
                return Some(pmu);
            }
        }
        Some(cc)
    }

    pub fn sdio_core(&self) -> Result<Core, SdioError> {
        self.core(BCMA_CORE_SDIO_DEV).ok_or(plain("the chip has no SDIO device core"))
    }
}

/// `brcmf_chip_dmp_get_desc`.
fn get_desc(bp: &mut Backplane, address: &mut u32) -> Result<(u32, u32), SdioError> {
    let value = bp.read32(*address)?;
    *address = address.wrapping_add(4);
    let mut kind = value & DMP_DESC_TYPE_MSK;
    if kind & !DMP_DESC_ADDRSIZE_GT32 == DMP_DESC_ADDRESS {
        kind = DMP_DESC_ADDRESS;
    }
    Ok((value, kind))
}

/// `brcmf_chip_dmp_get_regaddr`: the core's register base and its wrapper,
/// or `None` where the reference returns an error and the scan moves on.
fn get_regaddr(bp: &mut Backplane, address: &mut u32) -> Result<Option<(u32, u32)>, SdioError> {
    let mut regbase = 0u32;
    let mut wrapbase = 0u32;
    let (_, desc) = get_desc(bp, address)?;
    let wraptype = if desc == DMP_DESC_MASTER_PORT {
        DMP_SLAVE_TYPE_MWRAP
    } else if desc == DMP_DESC_ADDRESS {
        *address = address.wrapping_sub(4);
        DMP_SLAVE_TYPE_SWRAP
    } else {
        *address = address.wrapping_sub(4);
        return Ok(None);
    };
    loop {
        let (value, desc) = loop {
            let (value, desc) = get_desc(bp, address)?;
            if desc == DMP_DESC_EOT {
                *address = address.wrapping_sub(4);
                return Ok(None);
            }
            if desc == DMP_DESC_ADDRESS || desc == DMP_DESC_COMPONENT {
                break (value, desc);
            }
        };
        if desc == DMP_DESC_COMPONENT {
            *address = address.wrapping_sub(4);
            return Ok(Some((regbase, wrapbase)));
        }
        if value & DMP_DESC_ADDRSIZE_GT32 != 0 {
            get_desc(bp, address)?;
        }
        let sztype = (value & DMP_SLAVE_SIZE_TYPE) >> DMP_SLAVE_SIZE_TYPE_S;
        if sztype == DMP_SLAVE_SIZE_DESC {
            let (size, _) = get_desc(bp, address)?;
            if size & DMP_DESC_ADDRSIZE_GT32 != 0 {
                get_desc(bp, address)?;
            }
        }
        if sztype == DMP_SLAVE_SIZE_4K || sztype == DMP_SLAVE_SIZE_8K {
            let stype = (value & DMP_SLAVE_TYPE) >> DMP_SLAVE_TYPE_S;
            if regbase == 0 && stype == DMP_SLAVE_TYPE_SLAVE {
                regbase = value & DMP_SLAVE_ADDR_BASE;
            }
            if wrapbase == 0 && stype == wraptype {
                wrapbase = value & DMP_SLAVE_ADDR_BASE;
            }
        }
        if regbase != 0 && wrapbase != 0 {
            return Ok(Some((regbase, wrapbase)));
        }
    }
}

/// `brcmf_chip_dmp_erom_scan`. The table is a few hundred words; the bound is
/// there so that a chip answering garbage cannot keep the scan going.
fn erom_scan(bp: &mut Backplane) -> Result<Vec<Core>, SdioError> {
    let mut address = bp.read32(SI_ENUM_BASE + CC_EROMPTR)?;
    let mut cores = Vec::new();
    let mut desc_type = 0;
    let mut descriptors = 0;
    while desc_type != DMP_DESC_EOT {
        descriptors += 1;
        if descriptors > 4096 {
            return Err(plain("the enumeration ROM never ended"));
        }
        let (value, kind) = get_desc(bp, &mut address)?;
        desc_type = kind;
        if value & DMP_DESC_VALID == 0 || desc_type == DMP_DESC_EMPTY || desc_type != DMP_DESC_COMPONENT {
            continue;
        }
        let id = ((value & DMP_COMP_PARTNUM) >> DMP_COMP_PARTNUM_S) as u16;
        let (value, kind) = get_desc(bp, &mut address)?;
        desc_type = kind;
        if value & DMP_DESC_TYPE_MSK != DMP_DESC_COMPONENT {
            return Err(plain("a component descriptor was not followed by another"));
        }
        let nmw = (value & DMP_COMP_NUM_MWRAP) >> DMP_COMP_NUM_MWRAP_S;
        let nsw = (value & DMP_COMP_NUM_SWRAP) >> DMP_COMP_NUM_SWRAP_S;
        let rev = ((value & DMP_COMP_REVISION) >> DMP_COMP_REVISION_S) as u8;
        if nmw + nsw == 0 && id != BCMA_CORE_PMU && id != BCMA_CORE_GCI {
            continue;
        }
        if let Some((base, wrap)) = get_regaddr(bp, &mut address)? {
            cores.push(Core { id, rev, base, wrap });
        }
    }
    Ok(cores)
}

/// `brcmf_chip_ai_iscoreup`.
pub fn core_is_up(bp: &mut Backplane, core: Core) -> Result<bool, SdioError> {
    let ioctl = bp.read32(core.wrap + BCMA_IOCTL)?;
    let reset = bp.read32(core.wrap + BCMA_RESET_CTL)?;
    Ok(ioctl & (BCMA_IOCTL_FGC | BCMA_IOCTL_CLK) == BCMA_IOCTL_CLK && reset & BCMA_RESET_CTL_RESET == 0)
}

/// `brcmf_chip_ai_coredisable`.
fn core_disable(bp: &mut Backplane, core: Core, prereset: u32, reset: u32) -> Result<(), SdioError> {
    if bp.read32(core.wrap + BCMA_RESET_CTL)? & BCMA_RESET_CTL_RESET == 0 {
        bp.write32(core.wrap + BCMA_IOCTL, prereset | BCMA_IOCTL_FGC | BCMA_IOCTL_CLK)?;
        bp.read32(core.wrap + BCMA_IOCTL)?;
        bp.write32(core.wrap + BCMA_RESET_CTL, BCMA_RESET_CTL_RESET)?;
        spin_us(20);
        // SPINWAIT for up to 300 microseconds in steps of ten.
        for _ in 0..30 {
            if bp.read32(core.wrap + BCMA_RESET_CTL)? == BCMA_RESET_CTL_RESET {
                break;
            }
            spin_us(10);
        }
    }
    bp.write32(core.wrap + BCMA_IOCTL, reset | BCMA_IOCTL_FGC | BCMA_IOCTL_CLK)?;
    bp.read32(core.wrap + BCMA_IOCTL)?;
    Ok(())
}

/// `brcmf_chip_ai_resetcore`, including its handling of a chip with two
/// 802.11 cores, which are reset together.
fn core_reset(bp: &mut Backplane, chip: &Chip, core: Core, prereset: u32, reset: u32, postreset: u32) -> Result<(), SdioError> {
    let second = if core.id == BCMA_CORE_80211 { chip.d11(1) } else { None };
    core_disable(bp, core, prereset, reset)?;
    if let Some(second) = second {
        core_disable(bp, second, prereset, reset)?;
    }
    for target in core_and(second, core) {
        let mut count = 0;
        while bp.read32(target.wrap + BCMA_RESET_CTL)? & BCMA_RESET_CTL_RESET != 0 {
            bp.write32(target.wrap + BCMA_RESET_CTL, 0)?;
            count += 1;
            if count > 50 {
                break;
            }
            spin_us(50);
        }
    }
    for target in core_and(second, core) {
        bp.write32(target.wrap + BCMA_IOCTL, postreset | BCMA_IOCTL_CLK)?;
        bp.read32(target.wrap + BCMA_IOCTL)?;
    }
    Ok(())
}

fn core_and(second: Option<Core>, core: Core) -> impl Iterator<Item = Core> {
    core::iter::once(core).chain(second)
}

/// `brcmf_chip_cr4_set_passive`: the ARM held in reset with its CPU halted,
/// and the 802.11 cores disabled for the firmware to bring up itself.
fn set_passive(bp: &mut Backplane, chip: &Chip) -> Result<(), SdioError> {
    let cr4 = chip.core(BCMA_CORE_ARM_CR4).ok_or(plain("the chip has no ARM CR4 core"))?;
    // `brcmf_chip_disable_arm`: every IOCTL bit but the halt cleared.
    let halt = bp.read32(cr4.wrap + BCMA_IOCTL)? & ARMCR4_BCMA_IOCTL_CPUHALT;
    core_reset(bp, chip, cr4, halt, ARMCR4_BCMA_IOCTL_CPUHALT, ARMCR4_BCMA_IOCTL_CPUHALT)?;
    let mut unit = 0;
    while let Some(d11) = chip.d11(unit) {
        core_disable(bp, d11, D11_BCMA_IOCTL_PHYRESET | D11_BCMA_IOCTL_PHYCLOCKEN, D11_BCMA_IOCTL_PHYCLOCKEN)?;
        unit += 1;
    }
    Ok(())
}

/// `brcmf_chip_tcm_ramsize`: the CR4's A and B banks, each described in
/// 8 KiB or 1 KiB blocks.
fn tcm_ramsize(bp: &mut Backplane, cr4: Core) -> Result<u32, SdioError> {
    let corecap = bp.read32(cr4.base + ARMCR4_CAP)?;
    let nab = corecap & ARMCR4_TCBANB_MASK;
    let nbb = (corecap & ARMCR4_TCBBNB_MASK) >> ARMCR4_TCBBNB_SHIFT;
    let mut memsize = 0u32;
    for index in 0..nab + nbb {
        bp.write32(cr4.base + ARMCR4_BANKIDX, index)?;
        let info = bp.read32(cr4.base + ARMCR4_BANKINFO)?;
        let mut block = ARMCR4_BSZ_MULT;
        if info & ARMCR4_BLK_1K_MASK != 0 {
            block >>= 3;
        }
        memsize += ((info & ARMCR4_BSZ_MASK) + 1) * block;
    }
    Ok(memsize)
}

/// Find the chip and its cores, halt its processor and learn its memory.
/// `brcmf_sdio_probe_attach` up to `brcmf_chip_attach`, then that function
/// (`brcmf_sdio_buscoreprep`, `brcmf_chip_recognition`, `brcmf_chip_setup`).
pub fn attach(bp: &mut Backplane) -> Result<Chip, SdioError> {
    let signature = bp.read32(SI_ENUM_BASE)?;
    crate::println!("wifi: backplane signature at {:#010x} is {:#010x}", SI_ENUM_BASE, signature);

    // Force the PLL off until the chip is attached.
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, INIT_CLKCTL1)?;
    let clock = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    if clock & !SBSDIO_AVBITS != INIT_CLKCTL1 {
        crate::println!("wifi: ChipClkCSR wrote {:#04x} read {:#04x}", INIT_CLKCTL1, clock);
        return Err(plain("ChipClkCSR did not take the value written"));
    }

    // `brcmf_sdio_buscoreprep`.
    let request = SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_ALP_AVAIL_REQ;
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, request)?;
    let clock = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    if clock & !SBSDIO_AVBITS != request {
        return Err(plain("ChipClkCSR did not take the ALP request"));
    }
    let start = now_us();
    let mut clock = clock;
    while clock & SBSDIO_AVBITS == 0 {
        if now_us() - start > PMU_MAX_TRANSITION_DLY_US {
            crate::println!("wifi: ALP never became available, ChipClkCSR {:#04x}", clock);
            return Err(plain("ALP clock never became available"));
        }
        spin_us(10);
        clock = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    }
    crate::println!("wifi: ALP clock available after {} us, ChipClkCSR {:#04x}", now_us() - start, clock);
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_FORCE_ALP)?;
    spin_us(65);
    bp.write8(SBSDIO_FUNC1_SDIOPULLUP, 0)?;

    // `brcmf_chip_recognition`.
    let chipid = bp.read32(SI_ENUM_BASE + CC_CHIPID)?;
    if chipid == 0xFFFF_FFFF {
        return Err(plain("reading the chip id failed"));
    }
    let id = (chipid & CID_ID_MASK) as u16;
    let rev = ((chipid & CID_REV_MASK) >> CID_REV_SHIFT) as u8;
    let socitype = (chipid & CID_TYPE_MASK) >> CID_TYPE_SHIFT;
    crate::println!(
        "wifi: chip id {:#06x} revision {}, {} backplane",
        id,
        rev,
        if socitype == SOCI_AI { "AXI" } else { "SB" }
    );
    if socitype != SOCI_AI {
        return Err(plain("the chip's backplane is not AXI"));
    }
    let cores = erom_scan(bp)?;
    for core in &cores {
        crate::println!(
            "wifi:   core {:#05x} rev {:2} base {:#010x} wrap {:#010x}",
            core.id,
            core.rev,
            core.base,
            core.wrap
        );
    }
    let mut chip = Chip {
        id,
        rev,
        cores,
        rambase: 0,
        ramsize: 0,
        cc_caps: 0,
        cc_caps_ext: 0,
        pmurev: 0,
        pmucaps: 0,
    };
    // `brcmf_chip_cores_check`: this driver knows only the CR4 layout.
    if chip.core(BCMA_CORE_ARM_CR4).is_none() {
        return Err(plain("no ARM CR4 core, which is the only processor this driver starts"));
    }
    set_passive(bp, &chip)?;

    // `brcmf_chip_get_raminfo`.
    let cr4 = chip.core(BCMA_CORE_ARM_CR4).ok_or(plain("the chip has no ARM CR4 core"))?;
    chip.ramsize = tcm_ramsize(bp, cr4)?;
    chip.rambase = match id {
        BRCM_CC_4345_CHIP_ID | BRCM_CC_43454_CHIP_ID => RAMBASE_4345,
        _ => return Err(plain("no RAM base is known for this chip")),
    };
    crate::println!("wifi: RAM at {:#x}, {} KiB", chip.rambase, chip.ramsize / 1024);
    if chip.ramsize == 0 || chip.ramsize > MAX_MEMSIZE {
        return Err(plain("the chip's RAM size is not believable"));
    }

    // `brcmf_chip_setup`.
    let cc = chip.core(BCMA_CORE_CHIPCOMMON).ok_or(plain("the chip has no chipcommon core"))?;
    chip.cc_caps = bp.read32(cc.base + CC_CAPABILITIES)?;
    chip.cc_caps_ext = bp.read32(cc.base + CC_CAPABILITIES_EXT)?;
    let pmu = chip.pmu().ok_or(plain("the chip has no PMU"))?;
    if chip.cc_caps & CC_CAP_PMU != 0 {
        let caps = bp.read32(pmu.base + CC_PMUCAPABILITIES)?;
        chip.pmurev = caps & PCAP_REV_MASK;
        chip.pmucaps = caps;
    }
    crate::println!(
        "wifi: chipcommon rev {}, pmu rev {}, pmu caps {:#010x}",
        cc.rev,
        chip.pmurev,
        chip.pmucaps
    );

    // Back in `brcmf_sdio_probe_attach`.
    let sdio_core = chip.sdio_core()?;
    // `brcmf_sdio_kso_init`: keep the SDIO interface awake, from SDIO core
    // revision 12 on.
    if sdio_core.rev >= 12 {
        let sleep = bp.read8(SBSDIO_FUNC1_SLEEPCSR)?;
        if sleep & SBSDIO_FUNC1_SLEEPCSR_KSO_MASK == 0 {
            bp.write8(SBSDIO_FUNC1_SLEEPCSR, sleep | SBSDIO_FUNC1_SLEEPCSR_KSO_MASK)?;
        }
    }
    // `brcmf_sdio_drivestrengthinit` has no table for this chip, so nothing.
    // An SDIO reset should reset the backplane too.
    let control = bp.card().read_byte(0, SDIO_CCCR_BRCM_CARDCTRL)?;
    bp.card().write_byte(0, SDIO_CCCR_BRCM_CARDCTRL, control | SDIO_CCCR_BRCM_CARDCTRL_WLANRESET)?;
    // A backplane reset should reload the PMU's power-on values.
    let pmucontrol = bp.read32(pmu.base + CC_PMUCONTROL)?;
    bp.write32(pmu.base + CC_PMUCONTROL, pmucontrol | (BCMA_CC_PMU_CTL_RES_RELOAD << BCMA_CC_PMU_CTL_RES_SHIFT))?;

    // `brcmf_sdio_probe`: function 2 off to clear any half-transferred frame,
    // and the clock request dropped until a firmware needs it.
    bp.card().disable_function(FUNC_WLAN)?;
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, 0)?;
    Ok(chip)
}

/// `brcmf_sdio_htclk`, for a bus without save/restore: ask for the ALP or HT
/// clock and poll for it for up to a second, or drop the request.
fn backplane_clock(bp: &mut Backplane, on: bool, alp_only: bool) -> Result<u8, SdioError> {
    if !on {
        bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, 0)?;
        return Ok(0);
    }
    let request = if alp_only { SBSDIO_ALP_AVAIL_REQ } else { SBSDIO_HT_AVAIL_REQ };
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, request)?;
    let available = |value: u8| {
        value & SBSDIO_AVBITS != 0 && (alp_only || value & SBSDIO_AVBITS == SBSDIO_AVBITS)
    };
    let deadline = Deadline::after_us(PMU_MAX_TRANSITION_DLY_US);
    let mut clock = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    while !available(clock) {
        if deadline.expired() {
            crate::println!("wifi: {} clock timeout, ChipClkCSR {:#04x}", if alp_only { "ALP" } else { "HT" }, clock);
            return Err(plain("the backplane clock never became available"));
        }
        sleep_ms(5);
        clock = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    }
    Ok(clock)
}

/// Read back a few pieces of what was written and say whether they match.
fn verify(bp: &mut Backplane, what: &str, address: u32, data: &[u8]) -> Result<bool, SdioError> {
    const SAMPLE: usize = 64;
    if data.len() < SAMPLE {
        let mut back = alloc::vec![0u8; data.len()];
        bp.ram_read(address, &mut back)?;
        return Ok(back == data);
    }
    let mut ok = true;
    let mut back = [0u8; SAMPLE];
    for offset in [0, (data.len() / 3) & !3, (data.len() * 2 / 3) & !3, (data.len() - SAMPLE) & !3] {
        bp.ram_read(address + offset as u32, &mut back)?;
        if back[..] != data[offset..offset + SAMPLE] {
            crate::println!("wifi: {} differs at offset {:#x} after writing", what, offset);
            ok = false;
        }
    }
    Ok(ok)
}

/// Put the firmware and NVRAM into chip memory and start the processor.
/// `brcmf_sdio_download_firmware`.
pub fn download(bp: &mut Backplane, chip: &Chip, firmware: &[u8], nvram: &[u8]) -> Result<(), SdioError> {
    if firmware.len() < 4 || firmware.len() + nvram.len() > chip.ramsize as usize {
        return Err(plain("the firmware and NVRAM do not fit the chip's RAM"));
    }
    backplane_clock(bp, true, true)?;

    let rstvec = u32::from_le_bytes([firmware[0], firmware[1], firmware[2], firmware[3]]);
    crate::println!("wifi: firmware reset vector {:#010x}", rstvec);

    let start = now_us();
    bp.ram_write(chip.rambase, firmware)?;
    let took = now_us() - start;
    crate::println!(
        "wifi: {} bytes of firmware written in {} ms ({} KiB/s)",
        firmware.len(),
        took / 1000,
        (firmware.len() as u64 * 1_000_000 / took.max(1)) / 1024
    );
    if !verify(bp, "firmware", chip.rambase, firmware)? {
        return Err(plain("the firmware did not read back as written"));
    }

    // `brcmf_sdio_download_nvram`: at the very top of RAM, its length word
    // last.
    let address = chip.rambase + chip.ramsize - nvram.len() as u32;
    bp.ram_write(address, nvram)?;
    if !verify(bp, "nvram", address, nvram)? {
        return Err(plain("the NVRAM did not read back as written"));
    }
    crate::println!("wifi: {} bytes of NVRAM at {:#x}", nvram.len(), address);

    // `brcmf_chip_cr4_set_active` and `brcmf_sdio_buscore_activate`: every
    // interrupt cleared, the reset vector at address zero, and the processor
    // let out of halt.
    let sdio_core = chip.sdio_core()?;
    bp.write32(sdio_core.base + SD_INTSTATUS, 0xFFFF_FFFF)?;
    bp.ram_write(0, &rstvec.to_le_bytes())?;
    let cr4 = chip.core(BCMA_CORE_ARM_CR4).ok_or(plain("the chip has no ARM CR4 core"))?;
    core_reset(bp, chip, cr4, ARMCR4_BCMA_IOCTL_CPUHALT, 0, 0)?;

    backplane_clock(bp, false, true)?;
    Ok(())
}

/// What `brcmf_sdio_firmware_callback` does once the processor is running,
/// up to the point the bus is handed to the protocol layer: the HT clock,
/// function 2 enabled and ready, the interrupt mask, this chip's watermarks,
/// and save/restore where the chip has it. Returns whether save/restore is
/// on.
pub fn start(bp: &mut Backplane, chip: &Chip, device: u16) -> Result<bool, SdioError> {
    let sdio_core = chip.sdio_core()?;

    backplane_clock(bp, true, false)?;
    let saved = bp.read8(SBSDIO_FUNC1_CHIPCLKCSR)?;
    bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, saved | SBSDIO_FORCE_HT)?;

    bp.write32(sdio_core.base + SD_TOSBMAILBOXDATA, SDPCM_PROT_VERSION << SMB_DATA_VERSION_SHIFT)?;

    let waited = bp.card().enable_function(FUNC_WLAN, Some(F2_READY_TIMEOUT_MS))?;
    crate::println!("wifi: function 2 ready after {} ms", waited / 1000);

    bp.write32(sdio_core.base + SD_HOSTINTMASK, HOSTINTMASK)?;
    // `brcmf_sdio_firmware_callback` chooses the watermarks by the SDIO
    // device id function 1 reports (`sdiod->func1->device`), not by the chip
    // id. The Pi 4's chip reads 0x4345 from chipcommon, but its CIS reports
    // 0xa9a6, which `sdio_ids.h` names SDIO_DEVICE_ID_BROADCOM_43430, and
    // that id has no case in the switch. So under Raspberry Pi OS this board
    // takes the default branch, which sets the F2 watermark and nothing
    // else, and so does this. The 43455 branch is kept for a card that
    // reports 0xa9bf.
    if device == SDIO_DEVICE_ID_BROADCOM_43455 {
        bp.write8(SBSDIO_WATERMARK, CY_43455_F2_WATERMARK)?;
        let devctl = bp.read8(SBSDIO_DEVICE_CTL)?;
        bp.write8(SBSDIO_DEVICE_CTL, devctl | SBSDIO_DEVCTL_F2WM_ENAB)?;
        bp.write8(SBSDIO_FUNC1_MESBUSYCTRL, CY_43455_MES_WATERMARK | SBSDIO_MESBUSYCTRL_ENAB)?;
    } else {
        bp.write8(SBSDIO_WATERMARK, DEFAULT_F2_WATERMARK)?;
    }
    crate::println!(
        "wifi: function 1 reports device {:#06x}, so F2 watermark {:#04x}",
        device,
        if device == SDIO_DEVICE_ID_BROADCOM_43455 { CY_43455_F2_WATERMARK } else { DEFAULT_F2_WATERMARK }
    );

    let save_restore = sr_capable(bp, chip)?;
    if save_restore {
        // `brcmf_sdio_sr_init`.
        let wake = bp.read8(SBSDIO_FUNC1_WAKEUPCTRL)?;
        bp.write8(SBSDIO_FUNC1_WAKEUPCTRL, wake | (1 << SBSDIO_FUNC1_WCTRL_HTWAIT_SHIFT))?;
        bp.card().write_byte(0, SDIO_CCCR_BRCM_CARDCAP, SDIO_CCCR_BRCM_CARDCAP_CMD14_SUPPORT | SDIO_CCCR_BRCM_CARDCAP_CMD14_EXT)?;
        bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, SBSDIO_FORCE_HT)?;
    } else {
        bp.write8(SBSDIO_FUNC1_CHIPCLKCSR, saved)?;
    }

    // `brcmf_sdiod_intr_register` claims both functions' interrupts, which
    // is their enable bits and the master enable in CCCR IENx. Nothing takes
    // the host's interrupt yet; the chip is polled.
    let enables = bp.card().read_byte(0, super::sdio::CCCR_IENX)?;
    bp.card().write_byte(0, super::sdio::CCCR_IENX, enables | (1 << FUNC_BACKPLANE) | (1 << FUNC_WLAN) | 1)?;
    Ok(save_restore)
}

/// `brcmf_chip_sr_capable` for this chip: PMU revision 17 or later, and the
/// save/restore engine bit in PMU chip control register 3.
fn sr_capable(bp: &mut Backplane, chip: &Chip) -> Result<bool, SdioError> {
    if chip.pmurev < 17 {
        return Ok(false);
    }
    let pmu = chip.pmu().ok_or(plain("the chip has no PMU"))?;
    match chip.id {
        BRCM_CC_4345_CHIP_ID | BRCM_CC_43454_CHIP_ID => {
            bp.write32(pmu.base + CC_CHIPCONTROL_ADDR, 3)?;
            let value = bp.read32(pmu.base + CC_CHIPCONTROL_DATA)?;
            Ok(value & (1 << 2) != 0)
        }
        _ => Ok(false),
    }
}
