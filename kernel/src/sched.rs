//! Scheduler. Placeholder until the process layer lands.

use crate::cpu::idt::TrapFrame;

pub fn on_tick() {}

pub fn handle_user_page_fault(_addr: u64, _code: u64, _frame: &mut TrapFrame) -> bool {
    false
}

pub fn kill_current(_signal: i32) {
    panic!("user fault with no process layer yet");
}
