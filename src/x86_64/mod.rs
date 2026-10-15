//! The x86_64 platform: port I/O, the 16550 serial port as the console,
//! the time stamp counter as the clock, the local APIC and its timer
//! (`apic.rs`), and the CPUs started with INIT, SIPI, SIPI (`start.rs`).

mod apic;
pub mod paging;
mod ps2;
mod start;

use crate::cpu;
use alloc::vec::Vec;
use core::arch::asm;
use uefi::boot::{self, AllocateType, MemoryType};

pub use apic::{idle, nap_us, sleep_us, start_tick};
pub use start::{call_node, exit_node, this_cpu};

pub const NAME: &str = "x86_64";

pub unsafe fn outb(port: u16, v: u8) {
    asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags));
}

pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

pub unsafe fn outl(port: u16, v: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags));
}

pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

/// the clock's counter
pub fn counter() -> u64 {
    rdtsc()
}

pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

///////////////
// the console, COM1
///////////////

const COM1: u16 = 0x3f8;

/// COM1 set going; false if there is none, as on a laptop: its scratch
/// register does not keep what is written to it
pub fn console_init(_rsdp: usize) -> bool {
    unsafe {
        outb(COM1 + 7, 0x5a);
        if inb(COM1 + 7) != 0x5a {
            return false;
        }
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // divisor follows
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit
        outb(COM1 + 2, 0xc7); // FIFO on, cleared
        outb(COM1 + 4, 0x03); // DTR and RTS
    }
    true
}

pub fn serial_put(b: u8) {
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        outb(COM1, b);
    }
}

/// a byte from COM1, or from the PS/2 keyboard
/// the PS/2 keyboard's layout, by name, as `ps2::set_layout` has them
pub fn set_keyboard_layout(name: &str) -> bool {
    ps2::set_layout(name)
}

pub fn serial_get() -> Option<u8> {
    unsafe {
        let lsr = inb(COM1 + 5);
        if lsr != 0xff && lsr & 0x01 != 0 {
            return Some(inb(COM1));
        }
    }
    ps2::get()
}

///////////////
// random bytes
///////////////

fn rdrand() -> Option<u64> {
    let v: u64;
    let ok: u8;
    unsafe { asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)) };
    (ok != 0).then_some(v)
}

fn has_rdrand() -> bool {
    let ecx: u32;
    unsafe {
        asm!("mov {t}, rbx", "cpuid", "mov rbx, {t}", t = out(reg) _, inout("eax") 1u32 => _, out("ecx") ecx, out("edx") _, options(nomem, nostack));
    }
    ecx & (1 << 30) != 0
}

pub fn hw_random() -> Option<u64> {
    if has_rdrand() {
        (0..10).find_map(|_| rdrand())
    } else {
        None
    }
}

/// the caches of an x86 are coherent
pub fn flush_icache(_addr: usize, _len: usize) {}

///////////////
// the way out
///////////////

/// QEMU's isa-debug-exit device makes QEMU exit with (code << 1) | 1. It
/// is written to only on QEMU, its ACPI says; on a machine the CPU stops,
/// and says so.
pub fn exit(code: i32) -> ! {
    if crate::acpi::oem_is(b"BOCHS ") {
        unsafe { outl(0xf4, code as u32) };
    }
    crate::kprintln!("[machine] stopped, it can be switched off");
    halt_forever()
}

///////////////
// taking the machine
///////////////

/// what is found while the firmware is still there
pub struct Firmware {
    machine: Option<start::Machine>,
    low_page: Option<u64>,
}

/// With the firmware: the TSC's rate, against a wait the firmware times,
/// the CPUs and IO APICs, from ACPI, and a page below 1 MB for the
/// trampoline the other CPUs start in.
pub fn firmware(rsdp: usize, unix_us: u64) -> Firmware {
    let t0 = rdtsc();
    boot::stall(100_000);
    let t1 = rdtsc();
    let tsc_hz = (t1 - t0) * 10;
    cpu::clock_init(tsc_hz, t0, unix_us);
    crate::kprintln!("[machine] TSC {} MHz", tsc_hz / 1_000_000);
    let machine = unsafe { start::read_madt(rsdp) };
    let low_page = boot::allocate_pages(AllocateType::MaxAddress(0x9_f000), MemoryType::LOADER_CODE, 1)
        .ok()
        .map(|p| p.as_ptr() as u64);
    Firmware { machine, low_page }
}

/// With the firmware gone: interrupts of our own, the local APIC and its
/// timer, and the other CPUs. Returns how many CPUs run.
pub unsafe fn take_machine(fw: Firmware) -> usize {
    asm!("cli", options(nomem, nostack));
    crate::stack::init();
    if let Some(p) = fw.low_page {
        let gdt = start::install_trampoline(p);
        apic::load_gdt(gdt);
    }
    apic::build_idt();
    // the TSS first: the IDT's gates name its stacks
    apic::load_tss();
    apic::load_idt();
    apic::mask_pic();
    if let Some(m) = &fw.machine {
        for &io in &m.ioapics {
            apic::mask_ioapic(io);
        }
    }
    apic::lapic_init();
    apic::calibrate_timer();
    cpu::use_timer();
    match (&fw.machine, fw.low_page) {
        (Some(m), Some(p)) if m.apic_ids.len() > 1 => start::start_cpus(p, m),
        _ => start::start_cpus(0, &start::Machine { apic_ids: Vec::new(), ioapics: Vec::new() }),
    }
}

pub fn describe() -> alloc::string::String {
    alloc::format!("APIC timer {} MHz", apic::timer_mhz())
}

/// A pointer of this CPU's own, for code that is not the kernel's: GS's
/// base, so `gs:[n]` reaches what it points to. The kernel uses no GS.
pub fn set_cpu_local(p: u64) {
    unsafe { apic::wrmsr(0xc000_0101, p) };
}

/// wake a CPU that is halted
pub fn wake(hw_id: u64) {
    apic::wake(hw_id as u32)
}

///////////////
// interrupts from devices (`irq`): MSIs, to one local APIC each
///////////////

/// the address and data of an MSI that wakes the CPU with this APIC id:
/// fixed, edge, on the device's vector, which is ended and forgotten
pub fn msi_message(apic: u64) -> Option<(u64, u32)> {
    Some((0xfee0_0000 | (apic & 0xff) << 12, apic::VECTOR_DEVICE as u32))
}

/// an MSI-X table entry sent to this APIC from now on: its address's low
/// word, written whole, then read, so the write has reached the device
pub unsafe fn msi_route(entry: usize, apic: u64) {
    crate::virtio::w32(entry, 0xfee0_0000 | (apic as u32 & 0xff) << 12);
    crate::virtio::r32(entry);
}

/// a PC's PCI lines go through the IO APIC, which is left masked: MSIs
/// instead
pub fn pci_line(_f: &crate::pci::Function) -> Option<u32> {
    None
}

pub fn line_enable(_intid: u32, _apic: u64) {}

pub fn line_route(_intid: u32, _apic: u64) {}
