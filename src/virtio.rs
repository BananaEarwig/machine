//! virtio 1.0 on PCI: the transport, its common configuration, and split
//! queues, polled, or with an interrupt that wakes a CPU when a queue has
//! something (`irq`). What the input and block drivers share.

use crate::irq::Irq;
use crate::pci::Function;
use alloc::alloc::{alloc_zeroed, Layout};
use core::cell::Cell;
use core::sync::atomic::{fence, Ordering};

pub unsafe fn w8(a: usize, v: u8) {
    (a as *mut u8).write_volatile(v)
}
pub unsafe fn r8(a: usize) -> u8 {
    (a as *const u8).read_volatile()
}
pub unsafe fn w16(a: usize, v: u16) {
    (a as *mut u16).write_volatile(v)
}
pub unsafe fn r16(a: usize) -> u16 {
    (a as *const u16).read_volatile()
}
pub unsafe fn w32(a: usize, v: u32) {
    (a as *mut u32).write_volatile(v)
}
pub unsafe fn r32(a: usize) -> u32 {
    (a as *const u32).read_volatile()
}
unsafe fn w64(a: usize, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

// the common configuration's fields
const DEVICE_FEATURE_SELECT: usize = 0;
const DEVICE_FEATURE: usize = 4;
const DRIVER_FEATURE_SELECT: usize = 8;
const DRIVER_FEATURE: usize = 12;
const MSIX_CONFIG: usize = 16;
const DEVICE_STATUS: usize = 20;
const QUEUE_SELECT: usize = 22;
const QUEUE_SIZE: usize = 24;
const QUEUE_MSIX_VECTOR: usize = 26;
const QUEUE_ENABLE: usize = 28;
const QUEUE_NOTIFY_OFF: usize = 30;
const QUEUE_DESC: usize = 32;
const QUEUE_DRIVER: usize = 40;
const QUEUE_DEVICE: usize = 48;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// no MSI-X vector
const NO_VECTOR: u16 = 0xffff;
/// the available ring's flag that asks the device for no interrupts
const AVAIL_NO_INTERRUPT: u16 = 1;

/// descriptor flags
pub const DESC_NEXT: u16 = 1;
pub const DESC_WRITE: u16 = 2;

/// A device found, reset and its features agreed: VERSION_1, and those of
/// the low 32 asked for that it has.
pub struct Transport {
    common: usize,
    notify: usize,
    mult: usize,
    /// the device's own configuration
    pub device_cfg: usize,
    /// the features of bits 0-31 agreed
    pub features: u32,
    /// the ISR, read to lower the device's interrupt line
    isr: usize,
    /// the MSI-X vector queues are given, if the device has an interrupt
    vector: Cell<u16>,
}

impl Transport {
    pub unsafe fn new(f: &Function) -> Option<Transport> {
        Self::with_features(f, 0)
    }

    /// as `new`, with the features of `low`, bits 0-31, the device has too
    pub unsafe fn with_features(f: &Function, low: u32) -> Option<Transport> {
        f.enable();
        let (mut common, mut notify, mut device_cfg, mut mult, mut isr) = (0, 0, 0, 0, 0);
        for (id, at) in f.capabilities() {
            if id != 0x09 {
                continue;
            }
            let kind = f.read8(at + 3);
            let addr = f.bar(f.read8(at + 4) as usize) + f.read32(at + 8) as usize;
            match kind {
                1 => common = addr,
                2 => {
                    notify = addr;
                    mult = f.read32(at + 16) as usize;
                }
                3 => isr = addr,
                4 => device_cfg = addr,
                _ => {}
            }
        }
        if common == 0 || notify == 0 || device_cfg == 0 {
            return None;
        }
        w8(common + DEVICE_STATUS, 0);
        while r8(common + DEVICE_STATUS) != 0 {
            core::hint::spin_loop();
        }
        w8(common + DEVICE_STATUS, STATUS_ACK | STATUS_DRIVER);
        w32(common + DEVICE_FEATURE_SELECT, 1);
        if r32(common + DEVICE_FEATURE) & 1 == 0 {
            return None;
        }
        w32(common + DEVICE_FEATURE_SELECT, 0);
        let has = r32(common + DEVICE_FEATURE);
        w32(common + DRIVER_FEATURE_SELECT, 0);
        w32(common + DRIVER_FEATURE, has & low);
        w32(common + DRIVER_FEATURE_SELECT, 1);
        w32(common + DRIVER_FEATURE, 1);
        w8(common + DEVICE_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if r8(common + DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            return None;
        }
        // no interrupts, till `interrupt` asks for them
        w16(common + MSIX_CONFIG, NO_VECTOR);
        Some(Transport { common, notify, mult, device_cfg, features: has & low, isr, vector: Cell::new(NO_VECTOR) })
    }

    /// An interrupt, which wakes this CPU till it is routed to another, for
    /// the queues `listen` then gives it: an MSI-X message on x86_64, the
    /// PCI line on QEMU's AArch64 virt machine. None if there is no way,
    /// and the queues stay polled.
    pub unsafe fn interrupt(&self, f: &Function) -> Option<Irq> {
        let me = crate::smp::this_cpu();
        let hw = crate::smp::CPUS[me].hw_id.load(Ordering::Relaxed);
        if let Some((addr, data)) = crate::plat::msi_message(hw) {
            let cap = f.capabilities().into_iter().find(|&(id, _)| id == 0x11)?.1;
            let table = f.read32(cap + 4);
            let bar = f.bar((table & 7) as usize);
            if bar == 0 {
                return None;
            }
            // entry 0, the queues' vector: the message, not masked
            let entry = bar + (table & !7) as usize;
            w32(entry, addr as u32);
            w32(entry + 4, (addr >> 32) as u32);
            w32(entry + 8, data);
            w32(entry + 12, 0);
            // MSI-X on, the function not masked
            f.write16(cap + 2, (f.read16(cap + 2) | 0x8000) & !0x4000);
            self.vector.set(0);
            return Some(Irq::msix(entry, me));
        }
        if self.isr == 0 {
            return None;
        }
        let intid = crate::plat::pci_line(f)?;
        if !crate::irq::take_line(intid) {
            return None;
        }
        // the line, not masked in the command register
        f.write16(0x04, f.read16(0x04) & !(1 << 10));
        r8(self.isr);
        crate::plat::line_enable(intid, hw);
        Some(Irq::line(intid, self.isr, me))
    }

    /// The interrupt `interrupt` gave, for this queue's news too: its
    /// MSI-X vector, if it is a message, and no longer quiet. False if the
    /// device would not have it, and the queue stays polled.
    pub unsafe fn listen(&self, q: &Queue) -> bool {
        let v = self.vector.get();
        if v != NO_VECTOR {
            w16(self.common + QUEUE_SELECT, q.index);
            w16(self.common + QUEUE_MSIX_VECTOR, v);
            if r16(self.common + QUEUE_MSIX_VECTOR) != v {
                return false;
            }
        }
        q.avail.write_volatile(0);
        fence(Ordering::SeqCst);
        true
    }

    /// queue `index`, of at most `max` descriptors, made and enabled
    pub unsafe fn queue(&self, index: u16, max: usize) -> Option<Queue> {
        w16(self.common + QUEUE_SELECT, index);
        let n = (r16(self.common + QUEUE_SIZE) as usize).min(max).min(256);
        if n == 0 {
            return None;
        }
        w16(self.common + QUEUE_SIZE, n as u16);
        // the descriptors, the available ring and the used ring, a page each
        let mem = alloc_zeroed(Layout::from_size_align(3 * 4096, 4096).ok()?);
        if mem.is_null() {
            return None;
        }
        let q = Queue {
            desc: mem as *mut Desc,
            avail: mem.add(4096) as *mut u16,
            used: mem.add(8192),
            len: n,
            last_used: 0,
            notify: self.notify + r16(self.common + QUEUE_NOTIFY_OFF) as usize * self.mult,
            index,
        };
        w64(self.common + QUEUE_DESC, q.desc as u64);
        w64(self.common + QUEUE_DRIVER, q.avail as u64);
        w64(self.common + QUEUE_DEVICE, q.used as u64);
        // polled, till `listen` says otherwise
        q.quiet();
        w16(self.common + QUEUE_ENABLE, 1);
        Some(q)
    }

    /// the driver is ready, the device may use its queues
    pub unsafe fn ready(&self) {
        w8(self.common + DEVICE_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
        fence(Ordering::SeqCst);
    }
}

#[repr(C)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// a split queue: descriptors, the ring of what is given to the device,
/// and the ring of what it has done
pub struct Queue {
    pub desc: *mut Desc,
    avail: *mut u16, // flags, idx, ring[len]
    used: *mut u8,   // flags, idx, ring[len] of (id u32, len u32)
    pub len: usize,
    last_used: u16,
    notify: usize,
    index: u16,
}

unsafe impl Send for Queue {}

impl Queue {
    pub unsafe fn set_desc(&self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        self.desc.add(i).write_volatile(Desc { addr, len, flags, next });
    }

    /// put a chain, by its first descriptor, on the available ring
    pub unsafe fn give(&self, head: u16) {
        let idx = self.avail.add(1).read_volatile();
        self.avail.add(2 + idx as usize % self.len).write_volatile(head);
        fence(Ordering::SeqCst);
        self.avail.add(1).write_volatile(idx.wrapping_add(1));
    }

    /// ask the device for no interrupts for this queue: it is polled
    pub unsafe fn quiet(&self) {
        self.avail.write_volatile(AVAIL_NO_INTERRUPT);
    }

    pub unsafe fn notify(&self) {
        fence(Ordering::SeqCst);
        w16(self.notify, self.index);
    }

    /// the head of the next chain the device has finished with, if any
    pub unsafe fn take(&mut self) -> Option<u16> {
        self.take_len().map(|(id, _)| id)
    }

    /// the same, with the bytes the device wrote to it
    pub unsafe fn take_len(&mut self) -> Option<(u16, u32)> {
        let idx = (self.used.add(2) as *const u16).read_volatile();
        if idx == self.last_used {
            return None;
        }
        fence(Ordering::SeqCst);
        let slot = self.last_used as usize % self.len;
        let id = (self.used.add(4 + slot * 8) as *const u32).read_volatile();
        let len = (self.used.add(8 + slot * 8) as *const u32).read_volatile();
        self.last_used = self.last_used.wrapping_add(1);
        Some((id as u16, len))
    }
}
