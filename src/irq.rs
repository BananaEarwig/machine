//! Interrupts from devices, so that a CPU waiting for a device can sleep
//! till it has something, rather than look again and again.
//!
//! As every interrupt here, one from a device only wakes a CPU: a sleeping
//! one at once, a busy one at its next sleep, which then ends at once. No
//! handler of the device's runs; the driver looks at its queues when the
//! CPU wakes, as it would have when polling.
//!
//! * On x86_64 a PCI device sends an MSI-X message to the local APIC of
//!   the CPU it is routed to, on a vector of its own.
//! * On AArch64 a device raises its PCI interrupt line, an SPI of the GIC,
//!   which is routed to one CPU and taken on its rising edge, as QEMU's
//!   virt machine has no MSIs without an ITS. The device keeps the line up
//!   till its ISR is read: `rearm` lowers it, so that its next news raises
//!   it again. Lines are found only on QEMU's virt machine, where they are
//!   known without reading ACPI's AML; two devices on one line, the second
//!   is polled.
//!
//! A device that has neither has no `Irq`, and is polled, as before.

use crate::{plat, smp};
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct Irq {
    how: How,
    /// the CPU it is routed to; the lock is held while it is changed
    cpu: spin::Mutex<usize>,
    seen: AtomicUsize,
}

enum How {
    /// the address of the device's MSI-X table entry
    Msix(usize),
    /// the GIC's INTID for the line, and the device's ISR, which lowers it
    Line(u32, usize),
}

impl Irq {
    pub(crate) fn msix(entry: usize, cpu: usize) -> Irq {
        Irq { how: How::Msix(entry), cpu: spin::Mutex::new(cpu), seen: AtomicUsize::new(cpu) }
    }

    pub(crate) fn line(intid: u32, isr: usize, cpu: usize) -> Irq {
        Irq { how: How::Line(intid, isr), cpu: spin::Mutex::new(cpu), seen: AtomicUsize::new(cpu) }
    }

    /// the CPU it wakes
    pub fn cpu(&self) -> usize {
        self.seen.load(Ordering::Acquire)
    }

    /// Wake CPU n from now on. What the device sent before may have gone
    /// to the CPU it was routed to: a CPU that routes it to itself looks
    /// at the device after.
    pub fn route(&self, n: usize) {
        if n >= smp::cpus() || self.cpu() == n {
            return;
        }
        let mut c = self.cpu.lock();
        if *c == n {
            return;
        }
        let hw = smp::CPUS[n].hw_id.load(Ordering::Relaxed);
        match self.how {
            How::Msix(entry) => unsafe { plat::msi_route(entry, hw) },
            How::Line(intid, _) => plat::line_route(intid, hw),
        }
        *c = n;
        self.seen.store(n, Ordering::Release);
    }

    /// Ready to wake a CPU again, before the driver looks at the device: a
    /// line is lowered, so that what comes after raises it. Nothing for a
    /// message, which comes for each piece of news.
    pub fn rearm(&self) {
        if let How::Line(_, isr) = self.how {
            unsafe { crate::virtio::r8(isr) };
        }
    }

    /// what it is, for a line on the console
    pub fn describe(&self) -> alloc::string::String {
        match self.how {
            How::Msix(_) => alloc::string::String::from("MSI-X"),
            How::Line(intid, _) => alloc::format!("line {}", intid),
        }
    }
}

/// the lines taken, one device to a line
static LINES: spin::Mutex<alloc::vec::Vec<u32>> = spin::Mutex::new(alloc::vec::Vec::new());

/// a line for one device: false if another has it
pub(crate) fn take_line(intid: u32) -> bool {
    let mut l = LINES.lock();
    if l.contains(&intid) {
        return false;
    }
    l.push(intid);
    true
}
