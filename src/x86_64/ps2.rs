//! The PS/2 keyboard, a laptop's own as most present it, through the 8042
//! controller at ports 0x60 and 0x64: polled, scan code set 1 as the
//! controller translates it by default, turned into the bytes a terminal
//! sends. Keys with letters and numbers, shift, control and caps lock;
//! the arrows, home, end and delete as ANSI sequences. The mouse's bytes
//! are let go.
//!
//! A machine with no controller reads 0xff from its status port, and so has
//! no keyboard here.

use super::inb;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;

struct Keys {
    shift: bool,
    ctrl: bool,
    caps: bool,
    /// an 0xe0, the next code is of the extended keys
    extended: bool,
    /// bytes made, not yet taken
    queue: [u8; 16],
    head: usize,
    len: usize,
}

static KEYS: spin::Mutex<Keys> =
    spin::Mutex::new(Keys { shift: false, ctrl: false, caps: false, extended: false, queue: [0; 16], head: 0, len: 0 });

// set 1, unshifted and shifted, for codes 0x00 to 0x39
const PLAIN: &[u8; 58] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8; 58] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

impl Keys {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len < self.queue.len() {
                let i = (self.head + self.len) % self.queue.len();
                self.queue[i] = b;
                self.len += 1;
            }
        }
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let b = self.queue[self.head];
        self.head = (self.head + 1) % self.queue.len();
        self.len -= 1;
        Some(b)
    }

    fn code(&mut self, c: u8) {
        if c == 0xe0 {
            self.extended = true;
            return;
        }
        let up = c & 0x80 != 0;
        let key = c & 0x7f;
        let extended = core::mem::replace(&mut self.extended, false);
        match key {
            0x2a | 0x36 => self.shift = !up,
            0x1d => self.ctrl = !up,
            0x3a if !up => self.caps = !self.caps,
            _ if up => {}
            _ if extended => match key {
                0x48 => self.push(b"\x1b[A"),
                0x50 => self.push(b"\x1b[B"),
                0x4d => self.push(b"\x1b[C"),
                0x4b => self.push(b"\x1b[D"),
                0x47 => self.push(b"\x1b[H"),
                0x4f => self.push(b"\x1b[F"),
                0x53 => self.push(b"\x1b[3~"),
                0x1c => self.push(b"\r"),
                0x35 => self.push(b"/"),
                _ => {}
            },
            // the keypad's own
            0x4a => self.push(b"-"),
            0x4e => self.push(b"+"),
            k if (k as usize) < PLAIN.len() => {
                let mut b = if self.shift { SHIFTED[k as usize] } else { PLAIN[k as usize] };
                if b == 0 {
                    return;
                }
                if self.caps && b.is_ascii_alphabetic() {
                    b ^= 0x20;
                }
                if self.ctrl && b.is_ascii_alphabetic() {
                    b &= 0x1f;
                }
                // a terminal sends a return for the key, and DEL for backspace
                if b == b'\n' {
                    b = b'\r';
                }
                if b == 0x08 {
                    b = 0x7f;
                }
                self.push(&[b]);
            }
            _ => {}
        }
    }
}

/// a byte typed, if there is one
pub fn get() -> Option<u8> {
    let mut k = KEYS.lock();
    if let Some(b) = k.pop() {
        return Some(b);
    }
    loop {
        let s = unsafe { inb(STATUS) };
        // no controller, or nothing come
        if s == 0xff || s & 1 == 0 {
            break;
        }
        let c = unsafe { inb(DATA) };
        // the mouse's, not the keyboard's
        if s & 0x20 != 0 {
            continue;
        }
        k.code(c);
    }
    k.pop()
}
