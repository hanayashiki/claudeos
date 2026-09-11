//! 8259A programmable interrupt controller.

use crate::io::{inb, io_wait, outb};

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x11; // init + expect ICW4
const ICW4_8086: u8 = 0x01;
const EOI: u8 = 0x20;

/// Vector the first PIC's IRQ0 is remapped to.
pub const PIC1_OFFSET: u8 = 32;
pub const PIC2_OFFSET: u8 = 40;

pub fn init() {
    unsafe {
        let mask1 = inb(PIC1_DATA);
        let mask2 = inb(PIC2_DATA);

        outb(PIC1_COMMAND, ICW1_INIT);
        io_wait();
        outb(PIC2_COMMAND, ICW1_INIT);
        io_wait();
        outb(PIC1_DATA, PIC1_OFFSET);
        io_wait();
        outb(PIC2_DATA, PIC2_OFFSET);
        io_wait();
        outb(PIC1_DATA, 4); // slave is on IRQ2
        io_wait();
        outb(PIC2_DATA, 2); // slave identity
        io_wait();
        outb(PIC1_DATA, ICW4_8086);
        io_wait();
        outb(PIC2_DATA, ICW4_8086);
        io_wait();

        outb(PIC1_DATA, mask1);
        outb(PIC2_DATA, mask2);
    }
    mask_all();
}

pub fn mask_all() {
    unsafe {
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

pub fn unmask(irq: u8) {
    unsafe {
        if irq < 8 {
            let mask = inb(PIC1_DATA) & !(1 << irq);
            outb(PIC1_DATA, mask);
        } else {
            let mask = inb(PIC2_DATA) & !(1 << (irq - 8));
            outb(PIC2_DATA, mask);
            // The cascade line must be open for the slave to be heard.
            let mask1 = inb(PIC1_DATA) & !(1 << 2);
            outb(PIC1_DATA, mask1);
        }
    }
}

pub fn mask(irq: u8) {
    unsafe {
        if irq < 8 {
            let value = inb(PIC1_DATA) | (1 << irq);
            outb(PIC1_DATA, value);
        } else {
            let value = inb(PIC2_DATA) | (1 << (irq - 8));
            outb(PIC2_DATA, value);
        }
    }
}

pub fn end_of_interrupt(irq: u8) {
    unsafe {
        if irq >= 8 {
            outb(PIC2_COMMAND, EOI);
        }
        outb(PIC1_COMMAND, EOI);
    }
}
