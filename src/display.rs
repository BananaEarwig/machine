//! The screen: the framebuffer the firmware's Graphics Output Protocol
//! gives, in a mode chosen while the firmware is still there. It stays
//! where it is when the firmware has gone, memory the display reads.

use uefi::boot;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub base: usize,
    pub width: usize,
    pub height: usize,
    /// pixels to a line
    pub stride: usize,
    /// red in the low byte, so a pixel is swapped on its way out; else the
    /// low byte is blue, as a pixel of ChrysaLisp's is
    pub rgb: bool,
}

static FB: spin::Once<Framebuffer> = spin::Once::new();

pub fn framebuffer() -> Option<Framebuffer> {
    FB.get().copied()
}

/// the size asked for in screen.txt, "1280x800", or that
fn wanted(text: Option<&[u8]>) -> (usize, usize) {
    let parse = |t: &[u8]| -> Option<(usize, usize)> {
        let s = core::str::from_utf8(t).ok()?.trim();
        let (w, h) = s.split_once(['x', 'X'])?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };
    text.and_then(parse).unwrap_or((1280, 800))
}

/// Set the screen to the mode asked for, or the nearest that is no bigger,
/// with pixels in memory. Nothing if the machine has no such screen.
pub fn init(screen_txt: Option<&[u8]>) {
    let (ww, wh) = wanted(screen_txt);
    let Ok(h) = boot::get_handle_for_protocol::<GraphicsOutput>() else {
        crate::kprintln!("[machine] no screen");
        return;
    };
    let Ok(mut gop) = boot::open_protocol_exclusive::<GraphicsOutput>(h) else {
        crate::kprintln!("[machine] the screen is not to be had");
        return;
    };
    let usable = |f: PixelFormat| matches!(f, PixelFormat::Rgb | PixelFormat::Bgr);
    // exactly what is asked, or the biggest that fits in it
    let best = gop
        .modes()
        .filter(|m| usable(m.info().pixel_format()))
        .filter(|m| {
            let (w, h) = m.info().resolution();
            w <= ww && h <= wh
        })
        .max_by_key(|m| {
            let (w, h) = m.info().resolution();
            ((w, h) == (ww, wh), w * h)
        });
    if best.as_ref().map(|m| m.info().resolution()) != Some((ww, wh)) {
        let mut list = alloc::string::String::new();
        for m in gop.modes().filter(|m| usable(m.info().pixel_format())) {
            let (w, h) = m.info().resolution();
            list += &alloc::format!(" {}x{}", w, h);
        }
        crate::kprintln!("[machine] no {}x{} screen, there are{}", ww, wh, list);
    }
    if let Some(m) = best {
        if m.info().resolution() != gop.current_mode_info().resolution() {
            let _ = gop.set_mode(&m);
        }
    }
    let info = gop.current_mode_info();
    if !usable(info.pixel_format()) {
        crate::kprintln!("[machine] the screen has no framebuffer");
        return;
    }
    let (width, height) = info.resolution();
    #[cfg(target_arch = "aarch64")]
    if (width, height) != (ww, wh) {
        // QEMU's ramfb takes any size, its firmware driver offers three
        if let Some(fb) = unsafe { ramfb::set(ww, wh) } {
            crate::kprintln!("[machine] screen {}x{} at {:#x}, ramfb", ww, wh, fb.base);
            FB.call_once(|| fb);
            return;
        }
    }
    let fb = Framebuffer {
        base: gop.frame_buffer().as_mut_ptr() as usize,
        width,
        height,
        stride: info.stride(),
        rgb: info.pixel_format() == PixelFormat::Rgb,
    };
    crate::kprintln!("[machine] screen {}x{} at {:#x}", width, height, fb.base);
    FB.call_once(|| fb);
}

/// The screen as the firmware left it, its mode not changed: for the
/// console on the screen, when no kernel has set it.
pub fn current() {
    let Ok(h) = boot::get_handle_for_protocol::<GraphicsOutput>() else { return };
    let Ok(mut gop) = boot::open_protocol_exclusive::<GraphicsOutput>(h) else { return };
    let info = gop.current_mode_info();
    if !matches!(info.pixel_format(), PixelFormat::Rgb | PixelFormat::Bgr) {
        return;
    }
    let (width, height) = info.resolution();
    let fb = Framebuffer {
        base: gop.frame_buffer().as_mut_ptr() as usize,
        width,
        height,
        stride: info.stride(),
        rgb: info.pixel_format() == PixelFormat::Rgb,
    };
    FB.call_once(|| fb);
}

/// QEMU's ramfb, on the AArch64 virt machine, set to a size of our own
/// through fw_cfg, as its firmware driver does with a size of its own.
#[cfg(target_arch = "aarch64")]
mod ramfb {
    use super::Framebuffer;
    use uefi::boot::{self, AllocateType, MemoryType};

    /// the virt machine's fw_cfg, data, selector and DMA, big endian
    const FW_CFG: usize = 0x0902_0000;

    unsafe fn select(key: u16) {
        ((FW_CFG + 8) as *mut u16).write_volatile(key.to_be());
    }

    unsafe fn read(buf: &mut [u8]) {
        for b in buf {
            *b = (FW_CFG as *const u8).read_volatile();
        }
    }

    #[repr(C, align(16))]
    struct Dma {
        control: u32,
        length: u32,
        address: u64,
    }

    pub unsafe fn set(w: usize, h: usize) -> Option<Framebuffer> {
        // only on QEMU, its ACPI says
        if !crate::acpi::oem_is(b"BOCHS ") {
            return None;
        }
        let mut sig = [0u8; 4];
        select(0);
        read(&mut sig);
        if &sig != b"QEMU" {
            return None;
        }
        // the file etc/ramfb, in the directory
        select(0x19);
        let mut n = [0u8; 4];
        read(&mut n);
        let mut key = None;
        for _ in 0..u32::from_be_bytes(n).min(4096) {
            let mut e = [0u8; 64];
            read(&mut e);
            if e[8..].starts_with(b"etc/ramfb\0") {
                key = Some(u16::from_be_bytes([e[4], e[5]]));
            }
        }
        let key = key?;
        let pages = (w * h * 4).div_ceil(4096);
        let mem = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).ok()?;
        core::ptr::write_bytes(mem.as_ptr(), 0, pages * 4096);
        let base = mem.as_ptr() as u64;
        // address, XRGB8888, flags, width, height, stride
        let mut cfg = [0u8; 28];
        cfg[0..8].copy_from_slice(&base.to_be_bytes());
        cfg[8..12].copy_from_slice(&0x3432_5258u32.to_be_bytes());
        cfg[16..20].copy_from_slice(&(w as u32).to_be_bytes());
        cfg[20..24].copy_from_slice(&(h as u32).to_be_bytes());
        cfg[24..28].copy_from_slice(&(w as u32 * 4).to_be_bytes());
        let dma = Dma {
            control: ((key as u32) << 16 | 0x08 | 0x10).to_be(),
            length: (cfg.len() as u32).to_be(),
            address: (cfg.as_ptr() as u64).to_be(),
        };
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        ((FW_CFG + 16) as *mut u64).write_volatile((&dma as *const Dma as u64).to_be());
        let control = core::ptr::addr_of!(dma.control);
        for _ in 0..1_000_000 {
            let c = u32::from_be(control.read_volatile());
            if c == 0 {
                return Some(Framebuffer { base: base as usize, width: w, height: h, stride: w, rgb: false });
            }
            if c & 1 != 0 {
                return None;
            }
            core::hint::spin_loop();
        }
        None
    }
}
