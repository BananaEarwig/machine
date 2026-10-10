//! The PS/2 keyboard, a laptop's own as most present it, through the 8042
//! controller at ports 0x60 and 0x64: polled, scan code set 1 as the
//! controller translates it by default, decoded by the `pc-keyboard`
//! crate in the layout asked for (`set_layout`), US unless told, and
//! turned into the bytes a terminal sends: UTF-8, a return for Enter, DEL
//! for backspace, the control letters, and ANSI sequences for the arrows,
//! home, end and delete. The mouse's bytes are let go.
//!
//! A machine with no controller reads 0xff from its status port, and so has
//! no keyboard here.

use super::inb;
use pc_keyboard::layouts::{self, AnyLayout};
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, PS2Keyboard, ScancodeSet1};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;

struct Keys {
    kb: PS2Keyboard<AnyLayout, ScancodeSet1>,
    /// bytes made, not yet taken
    queue: [u8; 32],
    head: usize,
    len: usize,
}

static KEYS: spin::Mutex<Keys> = spin::Mutex::new(Keys {
    kb: PS2Keyboard::new(ScancodeSet1::new(), AnyLayout::Us104Key(layouts::Us104Key), HandleControl::MapLettersToUnicode),
    queue: [0; 32],
    head: 0,
    len: 0,
});

/// The keyboard's layout, by name: us, uk, de, fr, no, fi or se, jp,
/// dvorak, colemak. False, and the layout as it was, for any other.
pub fn set_layout(name: &str) -> bool {
    let layout = match name.trim().to_ascii_lowercase().as_str() {
        "us" => AnyLayout::Us104Key(layouts::Us104Key),
        "uk" | "gb" => AnyLayout::Uk105Key(layouts::Uk105Key),
        "de" => AnyLayout::De105Key(layouts::De105Key),
        "fr" | "azerty" => AnyLayout::Azerty(layouts::Azerty),
        "no" => AnyLayout::No105Key(layouts::No105Key),
        "fi" | "se" => AnyLayout::FiSe105Key(layouts::FiSe105Key),
        "jp" => AnyLayout::Jis109Key(layouts::Jis109Key),
        "dvorak" => AnyLayout::Dvorak104Key(layouts::Dvorak104Key),
        "colemak" => AnyLayout::Colemak(layouts::Colemak),
        _ => return false,
    };
    KEYS.lock().kb = PS2Keyboard::new(ScancodeSet1::new(), layout, HandleControl::MapLettersToUnicode);
    true
}

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
        let Ok(Some(ev)) = self.kb.add_byte(c) else { return };
        match self.kb.process_keyevent(ev) {
            Some(DecodedKey::Unicode(ch)) => match ch {
                '\n' | '\r' => self.push(b"\r"),
                '\u{8}' => self.push(&[0x7f]),
                '\u{7f}' => self.push(b"\x1b[3~"),
                _ => {
                    let mut buf = [0u8; 4];
                    self.push(ch.encode_utf8(&mut buf).as_bytes());
                }
            },
            Some(DecodedKey::RawKey(k)) => match k {
                KeyCode::ArrowUp => self.push(b"\x1b[A"),
                KeyCode::ArrowDown => self.push(b"\x1b[B"),
                KeyCode::ArrowRight => self.push(b"\x1b[C"),
                KeyCode::ArrowLeft => self.push(b"\x1b[D"),
                KeyCode::Home => self.push(b"\x1b[H"),
                KeyCode::End => self.push(b"\x1b[F"),
                KeyCode::Delete => self.push(b"\x1b[3~"),
                _ => {}
            },
            None => {}
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
