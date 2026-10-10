//! The console on the screen, for a machine with no serial port, a laptop
//! say: while the firmware is there, what is printed goes to its text
//! output; once it has gone, it is drawn into the framebuffer, in an 8x8
//! font (the `font8x8` crate's), doubled on a screen 1024 or more wide.
//! Text is UTF-8: ASCII and Latin-1 are drawn, £ and é say, and any other
//! character as a '?'.
//!
//! It is on when `boot::start` finds no serial port at COM1 (on x86_64;
//! on AArch64 there is always the PL011 the firmware names), or when a
//! kernel asks for it (`force`). On QEMU, which has a serial port, it is
//! off, and everything is as it was.
//!
//! The ANSI sequences a terminal program sends are followed as far as a
//! TUI needs: the cursor moved and put, the screen and the line cleared.
//! Colours and the rest are let be. A kernel whose desktop takes the
//! screen calls `hand_over`, and the console draws no more.

use crate::display::{self, Framebuffer};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// the console is on the screen
static ON: AtomicBool = AtomicBool::new(false);
/// the firmware is there, and has the screen's text
static FIRMWARE: AtomicBool = AtomicBool::new(false);

const FG: u32 = 0x00c0_c0c0;
const BG: u32 = 0x0000_0000;

enum Escape {
    None,
    Esc,
    /// in a CSI, the numbers so far
    Csi(Vec<u16>, bool),
}

struct Text {
    fb: Framebuffer,
    scale: usize,
    cols: usize,
    rows: usize,
    x: usize,
    y: usize,
    /// what is in each cell, a character up to U+00FF, so the screen can be
    /// drawn again when it scrolls
    cells: Vec<u8>,
    escape: Escape,
    /// a UTF-8 character so far, and how many bytes more it wants
    utf8: (u32, u8),
}

static TEXT: spin::Mutex<Option<Text>> = spin::Mutex::new(None);

/// what was said through the firmware, said again on the framebuffer when
/// it has gone, as leaving it clears the screen: in a buffer of its own,
/// as the first of it comes before there is a heap
static EARLY: spin::Mutex<([u8; 4096], usize)> = spin::Mutex::new(([0; 4096], 0));

/// on, if there is no serial port
pub fn init(serial: bool) {
    if !serial {
        force();
    }
}

/// the console on the screen, serial port or not
pub fn force() {
    ON.store(true, Ordering::Release);
    FIRMWARE.store(true, Ordering::Release);
}

pub fn on() -> bool {
    ON.load(Ordering::Acquire)
}

/// The firmware is going: the screen's framebuffer, as it is now if no
/// one has set it, is drawn into from here on.
pub fn leaving_firmware() {
    if !on() {
        return;
    }
    FIRMWARE.store(false, Ordering::Release);
    if display::framebuffer().is_none() {
        display::current();
    }
    let Some(fb) = display::framebuffer() else {
        ON.store(false, Ordering::Release);
        return;
    };
    let scale = if fb.width >= 1024 { 2 } else { 1 };
    let cols = fb.width / (8 * scale);
    let rows = fb.height / (8 * scale);
    let mut t = Text { fb, scale, cols, rows, x: 0, y: 0, cells: vec![b' '; cols * rows], escape: Escape::None, utf8: (0, 0) };
    clear(&t);
    let early = EARLY.lock();
    for &b in &early.0[..early.1] {
        t.put(b);
    }
    *TEXT.lock() = Some(t);
}

/// The desktop has the screen: the console draws no more.
pub fn hand_over() {
    ON.store(false, Ordering::Release);
}

/// bytes, from the console's writer, under its lock
pub fn write(bytes: &[u8]) {
    if !on() {
        return;
    }
    if FIRMWARE.load(Ordering::Acquire) {
        let mut early = EARLY.lock();
        let (buf, n) = &mut *early;
        let k = bytes.len().min(buf.len() - *n);
        buf[*n..*n + k].copy_from_slice(&bytes[..k]);
        *n += k;
        drop(early);
        firmware_write(bytes);
        return;
    }
    let mut g = TEXT.lock();
    let Some(t) = g.as_mut() else { return };
    for &b in bytes {
        t.put(b);
    }
}

/// through the firmware's text output, a line end as it wants one
fn firmware_write(bytes: &[u8]) {
    let mut buf = [0u16; 128];
    let mut n = 0;
    let flush = |buf: &mut [u16; 128], n: &mut usize| {
        if *n > 0 {
            buf[*n] = 0;
            if let Ok(s) = uefi::CStr16::from_u16_with_nul(&buf[..=*n]) {
                uefi::system::with_stdout(|out| {
                    let _ = out.output_string(s);
                });
            }
            *n = 0;
        }
    };
    for &b in bytes {
        // what the firmware's console can show, and no escapes
        if b == 0x1b {
            continue;
        }
        if b == b'\n' {
            buf[n] = b'\r' as u16;
            n += 1;
        }
        if b == b'\n' || (0x20..0x7f).contains(&b) {
            buf[n] = b as u16;
            n += 1;
        }
        if n >= 120 {
            flush(&mut buf, &mut n);
        }
    }
    flush(&mut buf, &mut n);
}

fn fill(fb: &Framebuffer, x: usize, y: usize, w: usize, h: usize, c: u32) {
    for row in y..(y + h).min(fb.height) {
        let line = (fb.base + row * fb.stride * 4) as *mut u32;
        for col in x..(x + w).min(fb.width) {
            unsafe { line.add(col).write_volatile(c) };
        }
    }
}

fn clear(t: &Text) {
    fill(&t.fb, 0, 0, t.fb.width, t.fb.height, BG);
}

impl Text {
    /// the cell at x, y drawn as it holds
    fn draw(&self, x: usize, y: usize) {
        let c = self.cells[y * self.cols + x] as usize;
        let glyph = match c {
            0..=0x7f => font8x8::legacy::BASIC_LEGACY[c],
            0xa0..=0xff => font8x8::legacy::LATIN_LEGACY[c - 0xa0],
            _ => font8x8::legacy::BASIC_LEGACY[b'?' as usize],
        };
        let s = self.scale;
        let fb = &self.fb;
        for (gy, &bits) in glyph.iter().enumerate() {
            for dy in 0..s {
                let py = (y * 8 + gy) * s + dy;
                let line = (fb.base + py * fb.stride * 4) as *mut u32;
                for gx in 0..8 {
                    let px = if bits & (1 << gx) != 0 { FG } else { BG };
                    for dx in 0..s {
                        unsafe { line.add((x * 8 + gx) * s + dx).write_volatile(px) };
                    }
                }
            }
        }
    }

    fn set(&mut self, x: usize, y: usize, c: u8) {
        let i = y * self.cols + x;
        if self.cells[i] != c {
            self.cells[i] = c;
            self.draw(x, y);
        }
    }

    /// up a line, the screen drawn again where it changed
    fn scroll(&mut self) {
        let old = self.cells.clone();
        self.cells.copy_within(self.cols.., 0);
        let last = (self.rows - 1) * self.cols;
        self.cells[last..].fill(b' ');
        for y in 0..self.rows {
            for x in 0..self.cols {
                if self.cells[y * self.cols + x] != old[y * self.cols + x] {
                    self.draw(x, y);
                }
            }
        }
    }

    fn newline(&mut self) {
        self.x = 0;
        if self.y + 1 >= self.rows {
            self.scroll();
        } else {
            self.y += 1;
        }
    }

    fn clear_to_end_of_line(&mut self) {
        for x in self.x..self.cols {
            self.set(x, self.y, b' ');
        }
    }

    fn put(&mut self, b: u8) {
        // the rest of a UTF-8 character
        if self.utf8.1 > 0 {
            if b & 0xc0 == 0x80 {
                self.utf8 = (self.utf8.0 << 6 | (b & 0x3f) as u32, self.utf8.1 - 1);
                if self.utf8.1 == 0 {
                    let c = self.utf8.0;
                    self.glyph(if (0xa0..=0xff).contains(&c) { c as u8 } else { b'?' });
                }
                return;
            }
            // not the rest: the bytes so far were not a character
            self.utf8 = (0, 0);
            self.glyph(b'?');
        }
        match b {
            0xc0..=0xdf => return self.utf8 = ((b & 0x1f) as u32, 1),
            0xe0..=0xef => return self.utf8 = ((b & 0x0f) as u32, 2),
            0xf0..=0xf7 => return self.utf8 = ((b & 0x07) as u32, 3),
            0x80..=0xff => return self.glyph(b'?'),
            _ => {}
        }
        match core::mem::replace(&mut self.escape, Escape::None) {
            Escape::Esc => {
                if b == b'[' {
                    self.escape = Escape::Csi(Vec::new(), false);
                }
                return;
            }
            Escape::Csi(mut nums, mut digit) => {
                match b {
                    b'0'..=b'9' => {
                        if !digit {
                            nums.push(0);
                            digit = true;
                        }
                        let n = nums.last_mut().unwrap();
                        *n = n.saturating_mul(10).saturating_add((b - b'0') as u16);
                        self.escape = Escape::Csi(nums, digit);
                    }
                    b';' => {
                        if !digit {
                            nums.push(0);
                        }
                        self.escape = Escape::Csi(nums, false);
                    }
                    b'=' | b'?' | b' ' => self.escape = Escape::Csi(nums, digit),
                    _ => self.csi(b, &nums),
                }
                return;
            }
            Escape::None => {}
        }
        match b {
            0x1b => self.escape = Escape::Esc,
            b'\n' => self.newline(),
            b'\r' => self.x = 0,
            8 => self.x = self.x.saturating_sub(1),
            b'\t' => {
                let to = ((self.x / 8) + 1) * 8;
                while self.x < to.min(self.cols) {
                    self.set(self.x, self.y, b' ');
                    self.x += 1;
                }
            }
            0x20..=0x7e => self.glyph(b),
            _ => {}
        }
    }

    /// a character drawn at the cursor, which moves on
    fn glyph(&mut self, c: u8) {
        if self.x >= self.cols {
            self.newline();
        }
        self.set(self.x, self.y, c);
        self.x += 1;
    }

    /// a CSI sequence, its numbers and its last byte
    fn csi(&mut self, op: u8, nums: &[u16]) {
        let n = |i: usize, d: usize| nums.get(i).map(|&v| v as usize).filter(|&v| v != 0).unwrap_or(d);
        match op {
            b'H' | b'f' => {
                self.y = (n(0, 1) - 1).min(self.rows - 1);
                self.x = (n(1, 1) - 1).min(self.cols - 1);
            }
            b'A' => self.y = self.y.saturating_sub(n(0, 1)),
            b'B' => self.y = (self.y + n(0, 1)).min(self.rows - 1),
            b'C' => self.x = (self.x + n(0, 1)).min(self.cols - 1),
            b'D' => self.x = self.x.saturating_sub(n(0, 1)),
            b'G' => self.x = (n(0, 1) - 1).min(self.cols - 1),
            b'J' => {
                let from = match nums.first().copied().unwrap_or(0) {
                    0 => self.y * self.cols + self.x,
                    _ => 0,
                };
                for i in from..self.cells.len() {
                    self.set(i % self.cols, i / self.cols, b' ');
                }
                if nums.first() == Some(&2) {
                    self.x = 0;
                    self.y = 0;
                }
            }
            b'K' => match nums.first().copied().unwrap_or(0) {
                0 => self.clear_to_end_of_line(),
                1 => {
                    for x in 0..=self.x.min(self.cols - 1) {
                        self.set(x, self.y, b' ');
                    }
                }
                _ => {
                    for x in 0..self.cols {
                        self.set(x, self.y, b' ');
                    }
                }
            },
            _ => {}
        }
    }
}
