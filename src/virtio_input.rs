//! The keyboard and the mouse: virtio-input devices on PCI, QEMU's
//! virtio-keyboard-pci and virtio-tablet-pci (or virtio-mouse-pci), the
//! same on x86_64 and AArch64. Each gives Linux input events, a type, a
//! code and a value, in buffers it fills from a queue of them. Nothing is
//! taken as an interrupt: the queues are polled when the GUI asks for an
//! event.

use crate::pci;
use crate::virtio::{r32, r8, w8, Queue, Transport, DESC_WRITE};
use alloc::alloc::{alloc_zeroed, Layout};
use alloc::vec::Vec;

const QUEUE: usize = 64;

/// a virtio_input_event
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: u32,
}

struct Device {
    queue: Queue,
    events: *mut InputEvent,
    /// the range of each absolute axis, x then y, if it has them
    abs: [(i32, i32); 2],
}

unsafe impl Send for Device {}

impl Device {
    /// Set a virtio-input function going, with its event queue full of
    /// buffers for the device to fill. None if it will not.
    unsafe fn new(f: &pci::Function) -> Option<Device> {
        let t = Transport::new(f)?;
        let queue = t.queue(0, QUEUE)?;
        let events = alloc_zeroed(Layout::from_size_align(4096, 4096).ok()?) as *mut InputEvent;
        if events.is_null() {
            return None;
        }
        for i in 0..queue.len {
            queue.set_desc(i, events.add(i) as u64, core::mem::size_of::<InputEvent>() as u32, DESC_WRITE, 0);
            queue.give(i as u16);
        }
        t.ready();
        queue.notify();
        let mut abs = [(0, 0); 2];
        for (axis, a) in abs.iter_mut().enumerate() {
            // VIRTIO_INPUT_CFG_ABS_INFO, its min and max
            w8(t.device_cfg, 0x12);
            w8(t.device_cfg + 1, axis as u8);
            if r8(t.device_cfg + 2) >= 8 {
                *a = (r32(t.device_cfg + 8) as i32, r32(t.device_cfg + 12) as i32);
            }
        }
        Some(Device { queue, events, abs })
    }

    /// the events the device has written since last time, each buffer
    /// given back to it
    unsafe fn poll(&mut self, out: &mut impl FnMut(InputEvent, &[(i32, i32); 2])) {
        let mut given = false;
        while let Some(id) = self.queue.take() {
            let e = self.events.add(id as usize).read_volatile();
            out(e, &self.abs);
            self.queue.give(id);
            given = true;
        }
        if given {
            self.queue.notify();
        }
    }
}

static DEVICES: spin::Mutex<Vec<Device>> = spin::Mutex::new(Vec::new());

/// start every virtio-input device of these
pub fn init(funcs: &[pci::Function]) -> usize {
    let mut devs = DEVICES.lock();
    for f in funcs {
        // vendor Red Hat, device virtio 1.0 input
        if f.vendor == 0x1af4 && f.device == 0x1052 {
            if let Some(d) = unsafe { Device::new(f) } {
                devs.push(d);
            }
        }
    }
    devs.len()
}

/// every event the devices have, to `out`, with the axes' ranges of the
/// device it came from
pub fn poll(mut out: impl FnMut(InputEvent, &[(i32, i32); 2])) {
    for d in DEVICES.lock().iter_mut() {
        unsafe { d.poll(&mut out) };
    }
}
