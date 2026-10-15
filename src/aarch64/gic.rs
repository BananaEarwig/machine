//! The GICv3, the generic timer, and sleeping.
//!
//! No interrupt is ever taken. A CPU with nothing to do waits in `wfi`,
//! with interrupts masked, which an interrupt pending at its GIC interface
//! still wakes it from: its virtual timer, set for the end of a sleep, or
//! the SGI another CPU sends to wake it. It then acknowledges and ends what
//! is pending, and carries on where it was. So no handler ever runs in the
//! middle of ChrysaLisp's code.
//!
//! A device's interrupt, its PCI line (`line_enable`), is an SPI routed to
//! one CPU, which it wakes as the timer does (`irq`).
//!
//! But on a CPU that takes the tick (`cpu::start_tick`): there IRQs are
//! unmasked, the virtual timer is the tick's, set again each period, and
//! `irq` handles it, and a wake, in the middle of anything.

use super::{isb, mrs, msr, CpuInfo};
use crate::cpu;
use crate::smp::MAX_CPUS;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// the virtual timer's interrupt, a PPI
const INTID_VTIMER: u32 = 27;
/// the SGI one CPU wakes another with
const INTID_WAKE: u32 = 1;

/// the distributor
static GICD: AtomicUsize = AtomicUsize::new(0);
/// each CPU's redistributor
static GICR: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// each ticking CPU's period, in the counter's ticks
static PERIOD: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

unsafe fn rd32(a: usize) -> u32 {
    (a as *const u32).read_volatile()
}

unsafe fn wr32(a: usize, v: u32) {
    (a as *mut u32).write_volatile(v)
}

unsafe fn rd64(a: usize) -> u64 {
    (a as *const u64).read_volatile()
}

/// MPIDR's affinity as GICR_TYPER has it, Aff3.Aff2.Aff1.Aff0
fn packed_affinity(mpidr: u64) -> u64 {
    (mpidr >> 32 & 0xff) << 24 | (mpidr & 0xff_ffff)
}

/// the redistributor of the CPU with this MPIDR, from its regions
unsafe fn find_redistributor(ranges: &[(u64, u64)], mpidr: u64) -> usize {
    let want = packed_affinity(mpidr);
    for &(base, len) in ranges {
        let mut rd = base as usize;
        while rd < (base + len) as usize {
            let typer = rd64(rd + 0x8);
            if typer >> 32 == want {
                return rd;
            }
            if typer & (1 << 4) != 0 {
                break; // the last in its region
            }
            // a GICv4 one has its VLPI frames as well
            rd += if typer & (1 << 1) != 0 { 0x4_0000 } else { 0x2_0000 };
        }
    }
    0
}

/// The distributor, and every CPU's redistributor found. Then the boot
/// CPU's interface. False if any CPU has no redistributor.
pub unsafe fn init(gicd: u64, ranges: &[(u64, u64)], cpus: &[CpuInfo]) -> bool {
    for (n, c) in cpus.iter().enumerate().take(MAX_CPUS) {
        let rd = if c.gicr != 0 { c.gicr as usize } else { find_redistributor(ranges, c.mpidr) };
        if rd == 0 {
            crate::kprintln!("[machine] no GIC redistributor for CPU {:#x}", c.mpidr);
            return false;
        }
        GICR[n].store(rd, Ordering::Relaxed);
    }
    // group 1 on in the distributor, the firmware has turned on affinity
    // routing
    let d = gicd as usize;
    GICD.store(d, Ordering::Relaxed);
    wr32(d, rd32(d) | (1 << 1));
    while rd32(d) & (1 << 31) != 0 {
        core::hint::spin_loop();
    }
    init_cpu(0);
    true
}

/// This CPU's redistributor awake, the timer's PPI and the wake SGI on,
/// and its interface taking group 1 at any priority.
pub unsafe fn init_cpu(n: usize) {
    let rd = GICR[n].load(Ordering::Relaxed);
    // GICR_WAKER, ProcessorSleep off, and wait for ChildrenAsleep to go
    wr32(rd + 0x14, rd32(rd + 0x14) & !(1 << 1));
    while rd32(rd + 0x14) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }
    let sgi = rd + 0x1_0000;
    let mine = (1 << INTID_VTIMER) | (1 << INTID_WAKE);
    // group 1, a middling priority, enabled
    wr32(sgi + 0x80, rd32(sgi + 0x80) | mine);
    for i in [INTID_VTIMER, INTID_WAKE] {
        ((sgi + 0x400 + i as usize) as *mut u8).write_volatile(0x80);
    }
    wr32(sgi + 0x100, mine);
    // the interface: system registers on, every priority, ends that end
    // and deactivate in one, group 1 on
    msr!("s3_0_c12_c12_5", mrs!("s3_0_c12_c12_5") | 1); // ICC_SRE_EL1
    isb();
    msr!("s3_0_c4_c6_0", 0xff); // ICC_PMR_EL1
    msr!("s3_0_c12_c12_3", 0); // ICC_BPR1_EL1
    msr!("s3_0_c12_c12_4", mrs!("s3_0_c12_c12_4") & !(1 << 1)); // ICC_CTLR_EL1.EOImode
    msr!("s3_0_c12_c12_7", 1); // ICC_IGRPEN1_EL1
    isb();
    // the timer off till a sleep sets it
    msr!("cntv_ctl_el0", 0);
    isb();
}

/// GICD_IROUTER's affinity for the CPU with this MPIDR, Aff3 in bits 32-39
fn route_affinity(mpidr: u64) -> u64 {
    (mpidr >> 32 & 0xff) << 32 | (mpidr & 0xff_ffff)
}

/// A device's SPI, its line: group 1, a middling priority, taken on its
/// rising edge, routed to the CPU with this MPIDR, and on. A CPU it wakes
/// acknowledges and ends it, as it does a wake, so the device lowers the
/// line by its own means before it can raise it again (`irq`).
pub fn line_enable(intid: u32, mpidr: u64) {
    let d = GICD.load(Ordering::Relaxed);
    if d == 0 || !(32..1020).contains(&intid) {
        return;
    }
    let (word, bit) = ((intid / 32) as usize * 4, 1u32 << (intid % 32));
    unsafe {
        wr32(d + 0x180 + word, bit); // GICD_ICENABLER, off while it is set
        while rd32(d) & (1 << 31) != 0 {
            core::hint::spin_loop();
        }
        wr32(d + 0x080 + word, rd32(d + 0x080 + word) | bit); // GICD_IGROUPR
        ((d + 0x400 + intid as usize) as *mut u8).write_volatile(0x80); // GICD_IPRIORITYR
        let cfg = d + 0xc00 + (intid / 16) as usize * 4; // GICD_ICFGR, edge
        wr32(cfg, rd32(cfg) | 2 << (2 * (intid % 16)));
        ((d + 0x6000 + 8 * intid as usize) as *mut u64).write_volatile(route_affinity(mpidr)); // GICD_IROUTER
        wr32(d + 0x100 + word, bit); // GICD_ISENABLER
    }
}

/// a device's SPI sent to the CPU with this MPIDR from now on
pub fn line_route(intid: u32, mpidr: u64) {
    let d = GICD.load(Ordering::Relaxed);
    if d == 0 || !(32..1020).contains(&intid) {
        return;
    }
    unsafe {
        ((d + 0x6000 + 8 * intid as usize) as *mut u64).write_volatile(route_affinity(mpidr));
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// acknowledge and end whatever is pending, it only woke this CPU
fn ack_all() {
    loop {
        let iar = mrs!("s3_0_c12_c12_0"); // ICC_IAR1_EL1
        if (1020..1024).contains(&(iar & 0xff_ffff)) {
            break;
        }
        msr!("s3_0_c12_c12_1", iar); // ICC_EOIR1_EL1
    }
    isb();
}

/// The tick, every `period_us`, on this CPU: the virtual timer is the
/// tick's from now on, and IRQs are unmasked.
pub fn start_tick(period_us: u64) {
    let n = super::this_cpu();
    let ticks = ((period_us as u128 * cpu::counter_hz() as u128 / 1_000_000) as u64).max(1);
    PERIOD[n].store(ticks, Ordering::Relaxed);
    msr!("cntv_cval_el0", super::counter() + ticks);
    msr!("cntv_ctl_el0", 1);
    isb();
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

/// An IRQ taken, on a CPU that ticks: the tick, its timer set for the next
/// period, and its handler called; or a wake, which only had to wake it.
pub fn irq() {
    let iar = mrs!("s3_0_c12_c12_0"); // ICC_IAR1_EL1
    let id = (iar & 0xff_ffff) as u32;
    if (1020..1024).contains(&id) {
        return;
    }
    if id == INTID_VTIMER {
        let n = super::this_cpu();
        let p = PERIOD[n].load(Ordering::Relaxed);
        if p == 0 {
            // a sleep's, not the tick's: off
            msr!("cntv_ctl_el0", 0);
        } else {
            // the next period, from now if it has fallen behind
            let next = mrs!("cntv_cval_el0") + p;
            let now = super::counter();
            msr!("cntv_cval_el0", if next > now { next } else { now + p });
        }
        isb();
        msr!("s3_0_c12_c12_1", iar); // ICC_EOIR1_EL1
        isb();
        if p != 0 {
            cpu::tick();
        }
        return;
    }
    msr!("s3_0_c12_c12_1", iar);
    isb();
}

/// wait for an interrupt, which is not taken, then let it go
fn halt() {
    unsafe { asm!("dsb sy", "wfi", options(nomem, nostack, preserves_flags)) };
}

/// sleep this long, in wfi, not spinning, the CPU is idle
pub fn sleep_us(us: u64) {
    let hz = cpu::counter_hz();
    let end = cpu::now_us() + us.max(1);
    if cpu::ticking() {
        // the tick wakes it, each period, and is taken
        while cpu::now_us() < end {
            halt();
        }
        return;
    }
    loop {
        let now = cpu::now_us();
        if now >= end {
            break;
        }
        let left = end - now;
        if left < 20 {
            core::hint::spin_loop();
            continue;
        }
        let ticks = (left as u128 * hz as u128 / 1_000_000) as u64;
        msr!("cntv_cval_el0", super::counter() + ticks.max(1));
        msr!("cntv_ctl_el0", 1); // enabled, not masked
        isb();
        halt();
        msr!("cntv_ctl_el0", 0); // off, which takes its interrupt away
        isb();
        ack_all();
    }
}

/// Sleep at most this long, or till anything wakes the CPU, an SGI from
/// another say: one wfi, for a caller that looks again when it ends.
pub fn nap_us(us: u64) {
    if cpu::ticking() {
        halt();
        return;
    }
    let ticks = (us.max(1) as u128 * cpu::counter_hz() as u128 / 1_000_000) as u64;
    msr!("cntv_cval_el0", super::counter() + ticks.max(1));
    msr!("cntv_ctl_el0", 1);
    isb();
    halt();
    msr!("cntv_ctl_el0", 0);
    isb();
    ack_all();
}

/// idle till woken
pub fn idle() {
    halt();
    if !cpu::ticking() {
        ack_all();
    }
}

/// wake a CPU that is idle, by its MPIDR, with an SGI
pub fn wake(mpidr: u64) {
    let aff0 = mpidr & 0xff;
    let v = (mpidr >> 32 & 0xff) << 48 // Aff3
        | (mpidr >> 16 & 0xff) << 32 // Aff2
        | (aff0 >> 4) << 44 // the range of Aff0
        | (mpidr >> 8 & 0xff) << 16 // Aff1
        | (INTID_WAKE as u64) << 24
        | 1 << (aff0 & 0xf);
    // what it is woken to see is seen before it is woken
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    msr!("s3_0_c12_c11_5", v); // ICC_SGI1R_EL1
    isb();
}
