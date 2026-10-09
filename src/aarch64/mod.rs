//! The AArch64 platform: the PL011 UART as the console, the generic
//! timer's counter as the clock, the GICv3 for the timer's interrupt and
//! for waking one CPU from another (`gic.rs`), PSCI to start the CPUs and
//! to turn the machine off, and the exception vectors (`start.rs`).
//!
//! It runs at EL1, as the firmware does on QEMU's virt machine, under TCG,
//! KVM or Apple's Hypervisor framework, on the firmware's page tables,
//! which map all memory flat, and the devices.

mod gic;
mod start;

use crate::acpi;
use crate::cpu;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub use gic::{idle, sleep_us, wake};
pub use start::{call_node, exit_node};

pub const NAME: &str = "aarch64";

macro_rules! mrs {
    ($r:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $r), out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }};
}
macro_rules! msr {
    ($r:literal, $v:expr) => {{
        let v: u64 = $v;
        unsafe { core::arch::asm!(concat!("msr ", $r, ", {}"), in(reg) v, options(nostack, preserves_flags)) };
    }};
}
pub(crate) use {mrs, msr};

pub fn isb() {
    unsafe { asm!("isb", options(nostack, preserves_flags)) };
}

/// this CPU's affinity, Aff3 to Aff0, as the MADT and the GIC have it
pub fn mpidr() -> u64 {
    mrs!("mpidr_el1") & 0xff_00ff_ffff
}

/// the number of the CPU this runs on, 0 for the boot one, kept in
/// TPIDR_EL1, which the boot image, at EL1 but a user program, leaves be
pub fn this_cpu() -> usize {
    mrs!("tpidr_el1") as usize
}

pub fn set_this_cpu(n: usize) {
    msr!("tpidr_el1", n as u64);
}

/// the clock's counter, the virtual count of the generic timer
pub fn counter() -> u64 {
    isb();
    mrs!("cntvct_el0")
}

///////////////
// the console, a PL011, where the SPCR says, or QEMU virt's
///////////////

static UART: AtomicUsize = AtomicUsize::new(0x0900_0000);
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

fn uart_reg(off: usize) -> *mut u32 {
    (UART.load(Ordering::Relaxed) + off) as *mut u32
}

/// the UART the firmware's SPCR names, a PL011 or an SBSA one, which are
/// the same for this; the firmware has set it going
pub fn console_init(rsdp: usize) {
    unsafe {
        if let Some(spcr) = acpi::find(rsdp, b"SPCR") {
            let kind: u8 = acpi::rd(spcr, 36);
            let space: u8 = acpi::rd(spcr, 40);
            let base: u64 = acpi::rd(spcr, 44);
            // 3 is a PL011, 0xd and 0xe SBSA UARTs, in memory space
            if matches!(kind, 0x03 | 0x0d | 0x0e) && space == 0 && base != 0 {
                UART.store(base as usize, Ordering::Relaxed);
            }
        }
    }
}

pub fn serial_put(b: u8) {
    unsafe {
        while uart_reg(UART_FR).read_volatile() & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        uart_reg(UART_DR).write_volatile(b as u32);
    }
}

pub fn serial_get() -> Option<u8> {
    unsafe {
        if uart_reg(UART_FR).read_volatile() & FR_RXFE == 0 {
            Some(uart_reg(UART_DR).read_volatile() as u8)
        } else {
            None
        }
    }
}

///////////////
// random numbers, RNDR if the CPU has it
///////////////

pub fn hw_random() -> Option<u64> {
    if (mrs!("id_aa64isar0_el1") >> 60) & 0xf == 0 {
        return None;
    }
    for _ in 0..10 {
        let v: u64;
        let ok: u64;
        // RNDR, which sets Z when it has no number
        unsafe { asm!("mrs {v}, s3_3_c2_c4_0", "cset {ok}, ne", v = out(reg) v, ok = out(reg) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

///////////////
// the caches: code written as data is cleaned to where the instruction
// fetches see it, and the stale instructions let go of
///////////////

pub fn flush_icache(addr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let ctr = mrs!("ctr_el0");
    let dline = 4usize << ((ctr >> 16) & 0xf);
    let iline = 4usize << (ctr & 0xf);
    let end = addr + len;
    unsafe {
        // IDC, no cleaning needed for the data to be seen
        if ctr & (1 << 28) == 0 {
            let mut a = addr & !(dline - 1);
            while a < end {
                asm!("dc cvau, {}", in(reg) a, options(nostack, preserves_flags));
                a += dline;
            }
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        // DIC, no invalidating needed for the instructions
        if ctr & (1 << 29) == 0 {
            let mut a = addr & !(iline - 1);
            while a < end {
                asm!("ic ivau, {}", in(reg) a, options(nostack, preserves_flags));
                a += iline;
            }
            asm!("dsb ish", options(nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// clean to the point of coherency, so a CPU with its MMU and caches still
/// off reads what was written
pub fn clean_to_poc(addr: usize, len: usize) {
    let dline = 4usize << ((mrs!("ctr_el0") >> 16) & 0xf);
    let mut a = addr & !(dline - 1);
    unsafe {
        while a < addr + len {
            asm!("dc civac, {}", in(reg) a, options(nostack, preserves_flags));
            a += dline;
        }
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

///////////////
// PSCI, the firmware's calls that stay after it has gone
///////////////

const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_CPU_ON: u64 = 0xc400_0003;

static PSCI_HVC: AtomicBool = AtomicBool::new(true);
static PSCI_OK: AtomicBool = AtomicBool::new(false);

fn psci(f: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let mut r = f;
    unsafe {
        if PSCI_HVC.load(Ordering::Relaxed) {
            asm!("hvc #0", inout("x0") r, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _, clobber_abi("C"));
        } else {
            asm!("smc #0", inout("x0") r, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _, clobber_abi("C"));
        }
    }
    r as i64
}

pub fn cpu_on(mpidr: u64, entry: u64, context: u64) -> i64 {
    psci(PSCI_CPU_ON, mpidr, entry, context)
}

///////////////
// the way out
///////////////

/// PSCI SYSTEM_OFF, which QEMU takes as the end, its own exit status is 0,
/// the code is on the console. With no PSCI, the CPU stops.
pub fn exit(code: i32) -> ! {
    let _ = code;
    if PSCI_OK.load(Ordering::Relaxed) {
        psci(PSCI_SYSTEM_OFF, 0, 0, 0);
    }
    halt_forever()
}

pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("msr daifset, #0xf", "wfi", options(nomem, nostack)) };
    }
}

///////////////
// taking the machine
///////////////

pub struct CpuInfo {
    pub mpidr: u64,
    /// its redistributor, if the MADT says
    pub gicr: u64,
}

/// what is found while the firmware is still there
pub struct Firmware {
    /// the CPUs that are enabled, the boot one first
    cpus: Vec<CpuInfo>,
    gicd: u64,
    gic_version: u8,
    /// the redistributors' regions, base and length
    gicr_ranges: Vec<(u64, u64)>,
}

/// With the firmware: the counter's rate, the CPUs and the GIC from the
/// MADT, and how PSCI is called from the FADT.
pub fn firmware(rsdp: usize, unix_us: u64) -> Firmware {
    cpu::clock_init(mrs!("cntfrq_el0"), counter(), unix_us);
    let el = (mrs!("CurrentEL") >> 2) & 3;
    if el != 1 {
        crate::kprintln!("[machine] at EL{}, this kernel is for EL1", el);
    }
    let mut fw = Firmware { cpus: Vec::new(), gicd: 0, gic_version: 0, gicr_ranges: Vec::new() };
    unsafe {
        if let Some(fadt) = acpi::find(rsdp, b"FACP") {
            if acpi::len(fadt) >= 131 {
                let arch: u16 = acpi::rd(fadt, 129);
                PSCI_OK.store(arch & 1 != 0, Ordering::Relaxed);
                PSCI_HVC.store(arch & 2 != 0, Ordering::Relaxed);
            }
        }
        if let Some(madt) = acpi::find(rsdp, b"APIC") {
            let me = mpidr();
            for (kind, e) in acpi::madt_entries(madt) {
                match kind {
                    // a CPU's GIC interface, enabled
                    0x0b if acpi::field::<u32>(e, 12) & 1 != 0 => {
                        let c = CpuInfo { mpidr: acpi::field::<u64>(e, 68) & 0xff_00ff_ffff, gicr: acpi::field(e, 60) };
                        if c.mpidr == me {
                            fw.cpus.insert(0, c);
                        } else {
                            fw.cpus.push(c);
                        }
                    }
                    // the distributor
                    0x0c => {
                        fw.gicd = acpi::field(e, 8);
                        fw.gic_version = acpi::field(e, 20);
                    }
                    // a region of redistributors
                    0x0e => fw.gicr_ranges.push((acpi::field(e, 4), acpi::field::<u32>(e, 12) as u64)),
                    _ => {}
                }
            }
        }
    }
    if fw.cpus.first().map(|c| c.mpidr) != Some(mpidr()) {
        fw.cpus.insert(0, CpuInfo { mpidr: mpidr(), gicr: 0 });
    }
    fw
}

/// With the firmware gone: the exception vectors, the GIC and the timer,
/// and the other CPUs. Returns how many CPUs run.
pub unsafe fn take_machine(fw: Firmware) -> usize {
    asm!("msr daifset, #0xf", options(nomem, nostack));
    set_this_cpu(0);
    start::install_vectors();
    // ChrysaLisp writes the code it runs, so writable memory must be
    // executable, as the firmware's loader code is, unless it has WXN on
    let sctlr = mrs!("sctlr_el1");
    if sctlr & (1 << 19) != 0 {
        msr!("sctlr_el1", sctlr & !(1 << 19));
        asm!("isb", "tlbi vmalle1", "dsb nsh", "isb", options(nostack));
    }
    crate::smp::boot_cpu(mpidr());
    let gic = fw.gicd != 0 && (fw.gic_version == 0 || fw.gic_version >= 3) && gic::init(fw.gicd, &fw.gicr_ranges, &fw.cpus);
    if gic {
        cpu::use_timer();
    } else {
        crate::kprintln!("[machine] no GICv3, one CPU, and sleeps spin");
    }
    if gic && PSCI_OK.load(Ordering::Relaxed) {
        start::start_cpus(&fw.cpus)
    } else {
        crate::smp::set_cpus(1);
        1
    }
}

pub fn describe() -> String {
    alloc::format!(
        "GICv3, timer {} MHz, PSCI by {}",
        cpu::counter_hz() / 1_000_000,
        if PSCI_HVC.load(Ordering::Relaxed) { "HVC" } else { "SMC" }
    )
}
