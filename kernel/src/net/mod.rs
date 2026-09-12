//! Networking.
//!
//! The card and the protocols meet here and nowhere else. A driver implements
//! `Interface`, registers itself with `attach`, and hands every frame it
//! receives to `receive`. The stack parses those frames and sends its replies
//! back out through whatever was attached.
//!
//! Frames arrive at `receive` from ordinary task context, not from the
//! interrupt that took them off the card. Protocol work is far too much to do
//! with interrupts masked, so the driver's handler does nothing but move the
//! frame off the ring and wake the task that calls in here.

use crate::abi::Errno;
use crate::sync::Spinlock;

/// A network card the stack can send through.
pub trait Interface: Sync {
    /// The card's hardware address.
    fn mac(&self) -> [u8; 6];

    /// Queue one complete Ethernet frame, headers included. Called from task
    /// context, and must not sleep.
    fn transmit(&self, frame: &[u8]) -> Result<(), Errno>;
}

static INTERFACE: Spinlock<Option<&'static dyn Interface>> = Spinlock::new(None);

/// Register the card the stack sends through. Called once, by the driver.
pub fn attach(nic: &'static dyn Interface) {
    *INTERFACE.lock() = Some(nic);
}

pub fn interface() -> Option<&'static dyn Interface> {
    *INTERFACE.lock()
}

/// Send one frame, if there is a card to send it through.
pub fn transmit(frame: &[u8]) -> Result<(), Errno> {
    match interface() {
        Some(nic) => nic.transmit(frame),
        None => Err(Errno::ENODEV),
    }
}

/// One received Ethernet frame.
pub fn receive(_frame: &[u8]) {}

/// Anything that has come due: retransmissions, timeouts. Called once per
/// timer tick from the network task.
pub fn tick() {}
