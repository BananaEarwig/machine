//! The x86_64 platform: port I/O, the 16550 serial port as the console,
//! the time stamp counter as the clock, the local APIC and its timer
//! (`apic.rs`), and the CPUs started with INIT, SIPI, SIPI (`start.rs`).

mod apic;
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

pub fn console_init(_rsdp: usize) {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // divisor follows
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit
        outb(COM1 + 2, 0xc7); // FIFO on, cleared
        outb(COM1 + 4, 0x03); // DTR and RTS
    }
}

pub fn serial_put(b: u8) {
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        outb(COM1, b);
    }
}

pub fn serial_get() -> Option<u8> {
    unsafe {
        if inb(COM1 + 5) & 0x01 != 0 {
            Some(inb(COM1))
        } else {
            None
        }
    }
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

/// QEMU's isa-debug-exit device, if it is there, makes QEMU exit with
/// (code << 1) | 1. On a machine it is not, and the CPU stops.
pub fn exit(code: i32) -> ! {
    unsafe { outl(0xf4, code as u32) };
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
    if let Some(p) = fw.low_page {
        let gdt = start::install_trampoline(p);
        apic::load_gdt(gdt);
    }
    apic::build_idt();
    apic::load_idt();
    apic::load_tss();
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
