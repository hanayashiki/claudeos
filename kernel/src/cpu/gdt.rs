//! Global descriptor table and task state segment.
//!
//! The selector order is fixed by what `syscall`/`sysret` require: STAR's
//! kernel field implies SS = CS + 8, and its user field implies
//! CS = base + 16 and SS = base + 8.

use core::arch::asm;
use core::ptr::addr_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS_SELECTOR: u16 = 0x28;

/// STAR[47:32]; sysret derives the user selectors from STAR[63:48].
pub const STAR_KERNEL_BASE: u64 = 0x08;
pub const STAR_USER_BASE: u64 = 0x10;

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tss {
    reserved0: u32,
    /// Stack pointers for rings 0..2, used on a privilege-raising interrupt.
    pub rsp: [u64; 3],
    reserved1: u64,
    /// Interrupt stack table.
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: core::mem::size_of::<Tss>() as u16,
        }
    }
}

const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut NMI_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);

static mut TSS: Tss = Tss::new();
static mut GDT: [u64; 7] = [0; 7];

pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;

fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = (limit as u64 & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (0x89u64 << 40) // present, available 64-bit TSS
        | (((limit as u64 >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = (base >> 32) & 0xFFFF_FFFF;
    (low, high)
}

pub fn init() {
    unsafe {
        let tss_ptr = addr_of!(TSS) as u64;
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        tss.ist[(IST_DOUBLE_FAULT - 1) as usize] =
            addr_of!(DOUBLE_FAULT_STACK) as u64 + IST_STACK_SIZE as u64;
        tss.ist[(IST_NMI - 1) as usize] = addr_of!(NMI_STACK) as u64 + IST_STACK_SIZE as u64;
        tss.iomap_base = core::mem::size_of::<Tss>() as u16;

        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        gdt[0] = 0;
        gdt[1] = 0x00AF_9B00_0000_FFFF; // kernel code, 64-bit
        gdt[2] = 0x00CF_9300_0000_FFFF; // kernel data
        gdt[3] = 0x00CF_F300_0000_FFFF; // user data, DPL3
        gdt[4] = 0x00AF_FB00_0000_FFFF; // user code, 64-bit, DPL3
        let (low, high) = tss_descriptor(tss_ptr, core::mem::size_of::<Tss>() as u32 - 1);
        gdt[5] = low;
        gdt[6] = high;

        let pointer = DescriptorTablePointer {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

        // Reload CS with a far return, then the data segments.
        asm!(
            "push {sel}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = const KERNEL_CODE as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
        asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov ss, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) KERNEL_DATA,
            options(nostack, preserves_flags),
        );

        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
    }
}

/// Stack the CPU switches to when an interrupt raises privilege to ring 0.
pub fn set_kernel_stack(rsp: u64) {
    unsafe {
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        tss.rsp[0] = rsp;
    }
}

pub fn kernel_stack() -> u64 {
    unsafe { (*core::ptr::addr_of!(TSS)).rsp[0] }
}
