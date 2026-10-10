//! Sound: a virtio-snd device on PCI, QEMU's virtio-sound-pci, the same on
//! x86_64 and AArch64. One output stream, 16-bit stereo at 44,100 frames a
//! second, played from a ring of buffers the device gives back as it plays
//! them. Polled: `feed` fills each buffer it has given back, from what the
//! kernel's mixer makes, and gives it again; called often enough, every
//! few tens of milliseconds, the sound does not break up.

use crate::pci;
use crate::virtio::{r32, Queue, Transport, DESC_NEXT, DESC_WRITE};
use alloc::alloc::{alloc_zeroed, Layout};

/// the frames a second, and the channels
pub const RATE: u32 = 44100;
pub const CHANNELS: usize = 2;
/// the frames in a buffer, about 23 ms
pub const PERIOD: usize = 1024;
/// the buffers in the ring, about 186 ms of sound
const BUFFERS: usize = 8;

const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_START: u32 = 0x0104;
const S_OK: u32 = 0x8000;
const FMT_S16: u8 = 5;
const RATE_44100: u8 = 6;

/// a buffer: the stream's number, its frames, and the status the device
/// writes when it has played them
const PCM_AT: usize = 16;
const PCM_BYTES: usize = PERIOD * CHANNELS * 2;
const STATUS_AT: usize = PCM_AT + PCM_BYTES;
const SLOT: usize = STATUS_AT + 16;

pub struct Sound {
    ctrl: Queue,
    tx: Queue,
    /// a control request and its answer
    req: *mut u8,
    stream: u32,
    bufs: *mut u8,
    /// which buffers are the kernel's to fill, a bit each
    free: u32,
}

unsafe impl Send for Sound {}

impl Sound {
    /// a control request of `len` bytes, built in req, and its answer, of
    /// `answer` bytes, after it; the answer's status
    unsafe fn control(&mut self, len: usize, answer: usize) -> u32 {
        let resp = self.req.add(256);
        core::ptr::write_bytes(resp, 0, answer.max(4));
        self.ctrl.set_desc(0, self.req as u64, len as u32, DESC_NEXT, 1);
        self.ctrl.set_desc(1, resp as u64, answer.max(4) as u32, DESC_WRITE, 0);
        self.ctrl.give(0);
        self.ctrl.notify();
        let t0 = crate::cpu::now_us();
        while self.ctrl.take().is_none() {
            if crate::cpu::now_us() > t0 + 1_000_000 {
                return 0;
            }
            core::hint::spin_loop();
        }
        (resp as *const u32).read_volatile()
    }

    unsafe fn put32(&self, at: usize, v: u32) {
        (self.req.add(at) as *mut u32).write_unaligned(v);
    }

    /// a request that is the stream's header alone
    unsafe fn stream_request(&mut self, code: u32) -> bool {
        self.put32(0, code);
        self.put32(4, self.stream);
        self.control(8, 4) == S_OK
    }

    unsafe fn new(f: &pci::Function) -> Option<Sound> {
        let t = Transport::new(f)?;
        let streams = r32(t.device_cfg + 4);
        if streams == 0 {
            return None;
        }
        let ctrl = t.queue(0, 4)?;
        let tx = t.queue(2, 3 * BUFFERS)?;
        let req = alloc_zeroed(Layout::from_size_align(4096, 4096).ok()?);
        let bufs = alloc_zeroed(Layout::from_size_align(SLOT * BUFFERS, 4096).ok()?);
        if req.is_null() || bufs.is_null() {
            return None;
        }
        t.ready();
        let mut s = Sound { ctrl, tx, req, stream: 0, bufs, free: (1 << BUFFERS) - 1 };
        // the streams, to find one that plays 16-bit stereo at 44,100
        let n = streams.min(4);
        s.put32(0, R_PCM_INFO);
        s.put32(4, 0);
        s.put32(8, n);
        s.put32(12, 32);
        if s.control(16, 4 + 32 * n as usize) != S_OK {
            return None;
        }
        let info = s.req.add(256 + 4);
        let mut found = None;
        for i in 0..n as usize {
            let p = info.add(32 * i);
            let formats = (p.add(8) as *const u64).read_unaligned();
            let rates = (p.add(16) as *const u64).read_unaligned();
            let direction = p.add(24).read();
            let (cmin, cmax) = (p.add(25).read(), p.add(26).read());
            if direction == 0 && formats & (1 << FMT_S16) != 0 && rates & (1 << RATE_44100) != 0 && cmin <= 2 && cmax >= 2 {
                found = Some(i as u32);
                break;
            }
        }
        s.stream = found?;
        // its parameters, then prepared and started
        s.put32(0, R_PCM_SET_PARAMS);
        s.put32(4, s.stream);
        s.put32(8, (PCM_BYTES * BUFFERS) as u32);
        s.put32(12, PCM_BYTES as u32);
        s.put32(16, 0);
        s.req.add(20).write(2);
        s.req.add(21).write(FMT_S16);
        s.req.add(22).write(RATE_44100);
        s.req.add(23).write(0);
        if s.control(24, 4) != S_OK || !s.stream_request(R_PCM_PREPARE) {
            return None;
        }
        // silence in every buffer to start with, then playing
        s.feed(|b| b.fill(0));
        if !s.stream_request(R_PCM_START) {
            return None;
        }
        Some(s)
    }

    /// Every buffer the device has played, filled by `fill`, PERIOD frames
    /// of 16-bit stereo, and given back to it. How many were.
    pub fn feed(&mut self, mut fill: impl FnMut(&mut [i16])) -> usize {
        unsafe {
            while let Some(id) = self.tx.take() {
                let k = id as usize / 3;
                if k < BUFFERS {
                    self.free |= 1 << k;
                }
            }
            let mut n = 0;
            for k in 0..BUFFERS {
                if self.free & (1 << k) == 0 {
                    continue;
                }
                let b = self.bufs.add(k * SLOT);
                (b as *mut u32).write_volatile(self.stream);
                let pcm = core::slice::from_raw_parts_mut(b.add(PCM_AT) as *mut i16, PERIOD * CHANNELS);
                fill(pcm);
                let d = (3 * k) as u16;
                self.tx.set_desc(d as usize, b as u64, 4, DESC_NEXT, d + 1);
                self.tx.set_desc(d as usize + 1, b.add(PCM_AT) as u64, PCM_BYTES as u32, DESC_NEXT, d + 2);
                self.tx.set_desc(d as usize + 2, b.add(STATUS_AT) as u64, 8, DESC_WRITE, 0);
                self.tx.give(d);
                self.free &= !(1 << k);
                n += 1;
            }
            if n > 0 {
                self.tx.notify();
            }
            n
        }
    }
}

/// The first virtio-snd device of these that plays 16-bit stereo at
/// 44,100, started and playing silence, if there is one.
pub fn init(funcs: &[pci::Function]) -> Option<Sound> {
    funcs
        .iter()
        // vendor Red Hat, device virtio 1.0 sound (25)
        .filter(|f| f.vendor == 0x1af4 && f.device == 0x1059)
        .find_map(|f| unsafe { Sound::new(f) })
}
