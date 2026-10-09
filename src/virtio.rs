//! virtio 1.0 on PCI: the transport, its common configuration, and split
//! queues, polled. What the input and block drivers share.

use crate::pci::Function;
use alloc::alloc::{alloc_zeroed, Layout};
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
const DEVICE_STATUS: usize = 20;
const QUEUE_SELECT: usize = 22;
const QUEUE_SIZE: usize = 24;
const QUEUE_ENABLE: usize = 28;
const QUEUE_NOTIFY_OFF: usize = 30;
const QUEUE_DESC: usize = 32;
const QUEUE_DRIVER: usize = 40;
const QUEUE_DEVICE: usize = 48;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// descriptor flags
pub const DESC_NEXT: u16 = 1;
pub const DESC_WRITE: u16 = 2;

/// A device found, reset and its features agreed, VERSION_1 and no other.
pub struct Transport {
    common: usize,
    notify: usize,
    mult: usize,
    /// the device's own configuration
    pub device_cfg: usize,
}

impl Transport {
    pub unsafe fn new(f: &Function) -> Option<Transport> {
        f.enable();
        let (mut common, mut notify, mut device_cfg, mut mult) = (0, 0, 0, 0);
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
        w32(common + DRIVER_FEATURE_SELECT, 0);
        w32(common + DRIVER_FEATURE, 0);
        w32(common + DRIVER_FEATURE_SELECT, 1);
        w32(common + DRIVER_FEATURE, 1);
        w8(common + DEVICE_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if r8(common + DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            return None;
        }
        Some(Transport { common, notify, mult, device_cfg })
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

    pub unsafe fn notify(&self) {
        fence(Ordering::SeqCst);
        w16(self.notify, self.index);
    }

    /// the head of the next chain the device has finished with, if any
    pub unsafe fn take(&mut self) -> Option<u16> {
        let idx = (self.used.add(2) as *const u16).read_volatile();
        if idx == self.last_used {
            return None;
        }
        fence(Ordering::SeqCst);
        let slot = self.last_used as usize % self.len;
        let id = (self.used.add(4 + slot * 8) as *const u32).read_volatile();
        self.last_used = self.last_used.wrapping_add(1);
        Some(id as u16)
    }
}
