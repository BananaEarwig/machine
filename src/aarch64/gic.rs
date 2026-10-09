//! The GICv3, the generic timer, and sleeping.
//!
//! No interrupt is ever taken. A CPU with nothing to do waits in `wfi`,
//! with interrupts masked, which an interrupt pending at its GIC interface
//! still wakes it from: its virtual timer, set for the end of a sleep, or
//! the SGI another CPU sends to wake it. It then acknowledges and ends what
//! is pending, and carries on where it was. So no handler ever runs in the
//! middle of ChrysaLisp's code.

use super::{isb, mrs, msr, CpuInfo};
use crate::cpu;
use crate::smp::MAX_CPUS;
use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

/// the virtual timer's interrupt, a PPI
const INTID_VTIMER: u32 = 27;
/// the SGI one CPU wakes another with
const INTID_WAKE: u32 = 1;

/// each CPU's redistributor
static GICR: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

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

/// wait for an interrupt, which is not taken, then let it go
fn halt() {
    unsafe { asm!("dsb sy", "wfi", options(nomem, nostack, preserves_flags)) };
}

/// sleep this long, in wfi, not spinning, the CPU is idle
pub fn sleep_us(us: u64) {
    let hz = cpu::counter_hz();
    let end = cpu::now_us() + us.max(1);
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

/// idle till woken
pub fn idle() {
    halt();
    ack_all();
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
