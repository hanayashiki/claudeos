//! Interrupt descriptor table and the common trap frame.

use super::gdt;
use core::arch::asm;
use core::ptr::addr_of;

/// Register state saved by the stubs in interrupts.s, in memory order.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub error_code: u64,
    // Pushed by the CPU.
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl IdtEntry {
    fn set(&mut self, handler: u64, selector: u16, ist: u8, dpl: u8, trap: bool) {
        self.offset_low = handler as u16;
        self.selector = selector;
        self.ist = ist & 0x7;
        // 0xE = interrupt gate (clears IF), 0xF = trap gate (leaves IF).
        let gate = if trap { 0xF } else { 0xE };
        self.type_attr = 0x80 | ((dpl & 3) << 5) | gate;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.zero = 0;
    }
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry {
    offset_low: 0,
    selector: 0,
    ist: 0,
    type_attr: 0,
    offset_mid: 0,
    offset_high: 0,
    zero: 0,
}; 256];

extern "C" {
    static isr_stub_table: [u64; 256];
    pub fn enter_user_mode(frame: *const TrapFrame) -> !;
}

pub type Handler = fn(&mut TrapFrame);

// Written only during single-threaded init, read on every interrupt. A lock
// here would be taken with interrupts already disabled on every trap, and a
// fault taken inside it would deadlock.
static mut HANDLERS: [Option<Handler>; 256] = [None; 256];

pub fn register(vector: u8, handler: Handler) {
    unsafe {
        let handlers = &mut *core::ptr::addr_of_mut!(HANDLERS);
        handlers[vector as usize] = Some(handler);
    }
}

pub fn init() {
    unsafe {
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        let stubs = &*core::ptr::addr_of!(isr_stub_table);
        for (i, entry) in idt.iter_mut().enumerate() {
            entry.set(stubs[i], gdt::KERNEL_CODE, 0, 0, false);
        }
        // Faults that can happen on a corrupt stack get their own stack.
        idt[8].set(stubs[8], gdt::KERNEL_CODE, gdt::IST_DOUBLE_FAULT, 0, false);
        idt[2].set(stubs[2], gdt::KERNEL_CODE, gdt::IST_NMI, 0, false);
        // Legacy Linux syscall entry must be reachable from ring 3.
        idt[0x80].set(stubs[0x80], gdt::KERNEL_CODE, 0, 3, false);

        let pointer = DescriptorTablePointer {
            limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: addr_of!(IDT) as u64,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

pub const EXCEPTION_NAMES: [&str; 32] = [
    "divide error",
    "debug",
    "non-maskable interrupt",
    "breakpoint",
    "overflow",
    "bound range exceeded",
    "invalid opcode",
    "device not available",
    "double fault",
    "coprocessor segment overrun",
    "invalid TSS",
    "segment not present",
    "stack-segment fault",
    "general protection fault",
    "page fault",
    "reserved",
    "x87 floating-point exception",
    "alignment check",
    "machine check",
    "SIMD floating-point exception",
    "virtualization exception",
    "control protection exception",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "hypervisor injection exception",
    "VMM communication exception",
    "security exception",
    "reserved",
    "reserved",
];

#[no_mangle]
pub extern "C" fn interrupt_dispatch(frame: &mut TrapFrame) {
    let vector = frame.vector as usize;
    let handler = unsafe { (*core::ptr::addr_of!(HANDLERS))[vector] };
    if let Some(handler) = handler {
        handler(frame);
        return;
    }
    crate::trap::unhandled(frame);
}
