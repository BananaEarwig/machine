//! PCI, through the memory-mapped configuration space the ACPI MCFG gives,
//! on both platforms. The firmware has given every device its BARs; this
//! only finds them.

use crate::acpi;
use alloc::vec::Vec;

#[derive(Clone, Copy)]
pub struct Function {
    /// its configuration space
    pub cfg: usize,
    pub vendor: u16,
    pub device: u16,
}

impl Function {
    pub fn read8(&self, off: usize) -> u8 {
        unsafe { ((self.cfg + off) as *const u8).read_volatile() }
    }
    pub fn read16(&self, off: usize) -> u16 {
        unsafe { ((self.cfg + off) as *const u16).read_volatile() }
    }
    pub fn read32(&self, off: usize) -> u32 {
        unsafe { ((self.cfg + off) as *const u32).read_volatile() }
    }
    pub fn write16(&self, off: usize, v: u16) {
        unsafe { ((self.cfg + off) as *mut u16).write_volatile(v) }
    }

    /// the address a BAR has been given, 64 bit ones as well
    pub fn bar(&self, n: usize) -> usize {
        let lo = self.read32(0x10 + n * 4);
        if lo & 1 != 0 {
            return 0; // I/O space, no use here
        }
        let mut a = (lo & !0xf) as u64;
        if (lo >> 1) & 3 == 2 && n < 5 {
            a |= (self.read32(0x10 + (n + 1) * 4) as u64) << 32;
        }
        a as usize
    }

    /// memory decoding and bus mastering on, so it can be reached and can
    /// reach memory
    pub fn enable(&self) {
        let cmd = self.read16(0x04);
        self.write16(0x04, cmd | 0x6);
    }

    /// the capabilities, each its id and offset
    pub fn capabilities(&self) -> Vec<(u8, usize)> {
        let mut caps = Vec::new();
        if self.read16(0x06) & (1 << 4) == 0 {
            return caps;
        }
        let mut at = (self.read8(0x34) & !3) as usize;
        while at != 0 && caps.len() < 48 {
            caps.push((self.read8(at), at));
            at = (self.read8(at + 1) & !3) as usize;
        }
        caps
    }
}

/// every function on every bus the MCFG covers
pub fn scan(rsdp: usize) -> Vec<Function> {
    let mut found = Vec::new();
    let Some(mcfg) = (unsafe { acpi::find(rsdp, b"MCFG") }) else { return found };
    let len = unsafe { acpi::len(mcfg) };
    let mut at = 44;
    while at + 16 <= len {
        let (base, start, end): (u64, u8, u8) = unsafe { (acpi::rd(mcfg, at), acpi::rd(mcfg, at + 10), acpi::rd(mcfg, at + 11)) };
        for bus in start..=end {
            for dev in 0..32usize {
                for func in 0..8usize {
                    let cfg = base as usize + ((bus as usize) << 20 | dev << 15 | func << 12);
                    let f = Function { cfg, vendor: 0, device: 0 };
                    let vendor = f.read16(0);
                    if vendor == 0xffff {
                        if func == 0 {
                            break;
                        }
                        continue;
                    }
                    found.push(Function { cfg, vendor, device: f.read16(2) });
                    // not a multi-function device, no more to look at
                    if func == 0 && f.read8(0x0e) & 0x80 == 0 {
                        break;
                    }
                }
            }
        }
        at += 16;
    }
    found
}
