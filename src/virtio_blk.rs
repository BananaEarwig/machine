//! Disks: virtio-blk devices on PCI, read and written a request at a time,
//! waited for to the end: asleep till the disk's interrupt, which the
//! request routes to its own CPU, if it has one (`irq`), else polled. A
//! disk is given to ChrysaLisp as a file, `dev/` and its serial number, as QEMU's `-device virtio-blk-pci,serial=data` has it,
//! so ChrysaLisp's own exFAT or FAT32 can mount it. A disk with no serial,
//! the one this was booted from, is left alone.

use crate::irq::Irq;
use crate::pci;
use crate::virtio::{r32, Queue, Transport, DESC_NEXT, DESC_WRITE};
use alloc::alloc::{alloc_zeroed, Layout};
use alloc::string::String;
use alloc::vec::Vec;

const SECTOR: u64 = 512;
/// the most one request moves
const BOUNCE: usize = 64 << 10;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_GET_ID: u32 = 8;

pub struct Disk {
    pub name: String,
    /// in bytes
    pub size: u64,
    queue: Queue,
    /// the request's header at 0, its status at 16
    req: *mut u8,
    bounce: *mut u8,
    /// the interrupt that ends a wait, if there is one
    irq: Option<Irq>,
}

unsafe impl Send for Disk {}

impl Disk {
    unsafe fn new(f: &pci::Function) -> Option<Disk> {
        let t = Transport::new(f)?;
        let queue = t.queue(0, 4)?;
        t.ready();
        let req = alloc_zeroed(Layout::from_size_align(4096, 4096).ok()?);
        let bounce = alloc_zeroed(Layout::from_size_align(BOUNCE, 4096).ok()?);
        if req.is_null() || bounce.is_null() {
            return None;
        }
        // the capacity, in sectors, is the first of its configuration
        let sectors = r32(t.device_cfg) as u64 | (r32(t.device_cfg + 4) as u64) << 32;
        let mut d = Disk { name: String::new(), size: sectors * SECTOR, queue, req, bounce, irq: None };
        // its serial, NUL padded
        if d.request(T_GET_ID, 0, 20, false) {
            let id = core::slice::from_raw_parts(d.bounce, 20);
            let n = id.iter().position(|&b| b == 0).unwrap_or(20);
            d.name = String::from_utf8_lossy(&id[..n]).into_owned();
        }
        // an interrupt, for a disk that is kept, one with a serial
        if !d.name.is_empty() {
            if let Some(irq) = t.interrupt(f) {
                if t.listen(&d.queue) {
                    d.irq = Some(irq);
                }
            }
        }
        Some(d)
    }

    /// one request, of `len` bytes from or to the bounce buffer, done
    unsafe fn request(&mut self, kind: u32, sector: u64, len: usize, write: bool) -> bool {
        // the interrupt to this CPU, and ready, before the disk can answer
        if let Some(irq) = &self.irq {
            irq.route(crate::smp::this_cpu());
            irq.rearm();
        }
        let q = &mut self.queue;
        (self.req as *mut u32).write(kind);
        (self.req.add(4) as *mut u32).write(0);
        (self.req.add(8) as *mut u64).write(sector);
        self.req.add(16).write_volatile(0xff);
        q.set_desc(0, self.req as u64, 16, DESC_NEXT, 1);
        q.set_desc(1, self.bounce as u64, len as u32, DESC_NEXT | if write { 0 } else { DESC_WRITE }, 2);
        q.set_desc(2, self.req.add(16) as u64, 1, DESC_WRITE, 0);
        q.give(0);
        q.notify();
        loop {
            if q.take().is_some() {
                break;
            }
            if self.irq.is_some() {
                // a millisecond at most, should the interrupt not come
                crate::cpu::nap_us(1000);
            } else {
                core::hint::spin_loop();
            }
        }
        // the line down again, so that it does not hold up another's
        if let Some(irq) = &self.irq {
            irq.rearm();
        }
        self.req.add(16).read_volatile() == 0
    }

    /// read `buf.len()` bytes at `off`, the number read, or -1
    pub fn read(&mut self, off: u64, buf: &mut [u8]) -> i64 {
        let len = (buf.len() as u64).min(self.size.saturating_sub(off)) as usize;
        let mut done = 0;
        while done < len {
            let at = off + done as u64;
            let first = at / SECTOR;
            let skip = (at % SECTOR) as usize;
            let n = (len - done).min(BOUNCE - skip);
            let span = (skip + n).div_ceil(SECTOR as usize) * SECTOR as usize;
            unsafe {
                if !self.request(T_IN, first, span, false) {
                    return -1;
                }
                core::ptr::copy_nonoverlapping(self.bounce.add(skip), buf.as_mut_ptr().add(done), n);
            }
            done += n;
        }
        len as i64
    }

    /// write `data` at `off`, the number written, or -1. A sector only
    /// part of which is written is read first.
    pub fn write(&mut self, off: u64, data: &[u8]) -> i64 {
        let len = (data.len() as u64).min(self.size.saturating_sub(off)) as usize;
        let mut done = 0;
        while done < len {
            let at = off + done as u64;
            let first = at / SECTOR;
            let skip = (at % SECTOR) as usize;
            let n = (len - done).min(BOUNCE - skip);
            let span = (skip + n).div_ceil(SECTOR as usize) * SECTOR as usize;
            unsafe {
                if (skip != 0 || n % SECTOR as usize != 0) && !self.request(T_IN, first, span, false) {
                    return -1;
                }
                core::ptr::copy_nonoverlapping(data.as_ptr().add(done), self.bounce.add(skip), n);
                if !self.request(T_OUT, first, span, true) {
                    return -1;
                }
            }
            done += n;
        }
        len as i64
    }
}

pub static DISKS: spin::Mutex<Vec<Disk>> = spin::Mutex::new(Vec::new());

/// start every virtio-blk device of these that has a serial
pub fn init(funcs: &[pci::Function]) -> usize {
    let mut disks = DISKS.lock();
    for f in funcs {
        // virtio block, transitional or 1.0
        if f.vendor == 0x1af4 && (f.device == 0x1001 || f.device == 0x1042) {
            if let Some(d) = unsafe { Disk::new(f) } {
                if !d.name.is_empty() {
                    let how = d.irq.as_ref().map_or(alloc::string::String::from("polled"), |i| i.describe());
                    crate::kprintln!("[machine] disk dev/{}, {} MB, {}", d.name, d.size >> 20, how);
                    disks.push(d);
                }
            }
        }
    }
    disks.len()
}

/// the disk of a path, dev/<serial>
pub fn find(key: &[u8]) -> Option<usize> {
    let name = key.strip_prefix(b"dev/")?;
    DISKS.lock().iter().position(|d| d.name.as_bytes() == name)
}
