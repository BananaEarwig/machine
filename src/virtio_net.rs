//! The network card: a virtio-net device on PCI, QEMU's virtio-net-pci,
//! the same on x86_64 and AArch64. It moves Ethernet frames, whole, one to
//! a buffer: a queue of buffers the device fills with what arrives, and a
//! queue of frames to send. What arrives wakes a CPU, by an interrupt the
//! kernel routes (`irq`), where there is a way, and is polled; what speaks
//! IP and TCP over it is the kernel's.

use crate::irq::Irq;
use crate::pci;
use crate::virtio::{r8, Queue, Transport, DESC_WRITE};
use alloc::alloc::{alloc_zeroed, Layout};

/// VIRTIO_NET_F_MAC: the device has an address of its own for us
const F_MAC: u32 = 1 << 5;
/// the header before each frame, VERSION_1's: flags, gso, lengths and the
/// count of buffers, none of them used
const HEADER: usize = 12;
/// a buffer: the header and the largest frame, with no VLAN tag, rounded up
const BUF: usize = 2048;
/// the buffers of each queue
const BUFFERS: usize = 64;
/// the largest frame, the MTU of 1500 and its Ethernet header
pub const MAX_FRAME: usize = 1514;

pub struct Net {
    /// the card's Ethernet address
    pub mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_bufs: *mut u8,
    tx_bufs: *mut u8,
    /// which send buffers the device has not finished with, a bit each
    tx_busy: u64,
    /// the next send buffer to try
    tx_next: usize,
    /// what wakes a CPU when a frame has come, till the kernel takes it
    irq: Option<Irq>,
}

unsafe impl Send for Net {}

impl Net {
    unsafe fn new(f: &pci::Function) -> Option<Net> {
        let t = Transport::with_features(f, F_MAC)?;
        let rx = t.queue(0, BUFFERS)?;
        let tx = t.queue(1, BUFFERS)?;
        let layout = Layout::from_size_align(BUF * BUFFERS, 4096).ok()?;
        let rx_bufs = alloc_zeroed(layout);
        let tx_bufs = alloc_zeroed(layout);
        if rx_bufs.is_null() || tx_bufs.is_null() {
            return None;
        }
        let mut mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        if t.features & F_MAC != 0 {
            for (i, b) in mac.iter_mut().enumerate() {
                *b = r8(t.device_cfg + i);
            }
        }
        for i in 0..rx.len {
            rx.set_desc(i, rx_bufs.add(i * BUF) as u64, BUF as u32, DESC_WRITE, 0);
            rx.give(i as u16);
        }
        t.ready();
        rx.notify();
        // frames that come wake a CPU; frames sent are seen to when a
        // buffer is wanted, and wake none
        let irq = t.interrupt(f).filter(|i| t.listen(&rx, i));
        Some(Net { mac, rx, tx, rx_bufs, tx_bufs, tx_busy: 0, tx_next: 0, irq })
    }

    /// The interrupt that wakes a CPU when a frame has come, if there is
    /// one, for the kernel to claim for a CPU about to sleep, and release
    /// when it wakes (`irq`); then it is polled.
    pub fn take_irq(&mut self) -> Option<Irq> {
        self.irq.take()
    }

    /// The next frame that has come, to `f`, its buffer given back to the
    /// device after. False if none has.
    pub fn receive(&mut self, f: impl FnOnce(&[u8])) -> bool {
        unsafe {
            let Some((id, len)) = self.rx.take_len() else { return false };
            let id = id as usize % self.rx.len;
            let len = (len as usize).clamp(HEADER, BUF);
            let buf = core::slice::from_raw_parts(self.rx_bufs.add(id * BUF + HEADER), len - HEADER);
            f(buf);
            self.rx.give(id as u16);
            self.rx.notify();
            true
        }
    }

    /// the send buffers the device has finished with, free again
    fn reclaim(&mut self) {
        while let Some(id) = unsafe { self.tx.take() } {
            self.tx_busy &= !(1u64 << (id as usize % self.tx.len));
        }
    }

    /// Whether a frame can be sent now: a send buffer is free.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        let n = self.tx.len.min(64);
        self.tx_busy.count_ones() < n as u32
    }

    /// Send a frame of `len` bytes, at most MAX_FRAME, that `fill` writes.
    /// False, and `fill` not called, if every send buffer is busy.
    pub fn send(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        self.reclaim();
        let n = self.tx.len.min(64);
        let len = len.min(MAX_FRAME);
        for k in 0..n {
            let i = (self.tx_next + k) % n;
            if self.tx_busy & (1u64 << i) != 0 {
                continue;
            }
            unsafe {
                let buf = self.tx_bufs.add(i * BUF);
                core::ptr::write_bytes(buf, 0, HEADER);
                fill(core::slice::from_raw_parts_mut(buf.add(HEADER), len));
                self.tx.set_desc(i, buf as u64, (HEADER + len) as u32, 0, 0);
                self.tx_busy |= 1u64 << i;
                self.tx_next = (i + 1) % n;
                self.tx.give(i as u16);
                self.tx.notify();
            }
            return true;
        }
        false
    }
}

/// The first virtio-net device of these, started, if there is one.
pub fn init(funcs: &[pci::Function]) -> Option<Net> {
    funcs
        .iter()
        // vendor Red Hat, device virtio network: transitional, as QEMU
        // makes it by default, or 1.0 only
        .filter(|f| f.vendor == 0x1af4 && (f.device == 0x1000 || f.device == 0x1041))
        .find_map(|f| unsafe { Net::new(f) })
}
