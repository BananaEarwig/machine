//! Starting the other AArch64 CPUs, the exception vectors, and running a
//! node on a stack of its own.
//!
//! A CPU is started with PSCI CPU_ON, at `secondary_entry`, with its MMU
//! and caches off. It takes the boot CPU's translation, the same tables,
//! the same memory attributes, turns its MMU on, and goes on in Rust, on a
//! stack of its own, to `smp::cpu_main`, where it waits for a node.

use super::{clean_to_poc, cpu_on, gic, mrs, msr, isb, set_this_cpu, CpuInfo};
use crate::cpu;
use crate::smp::{self, CPUS, MAX_CPUS};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU8, Ordering};

/// What a CPU starting reads, with its MMU off, so it is cleaned to memory
/// before each start. The offsets are the entry code's.
#[repr(C, align(128))]
struct BootBlock {
    mair: u64,   // 0
    tcr: u64,    // 8
    ttbr0: u64,  // 16
    ttbr1: u64,  // 24
    cpacr: u64,  // 32
    sctlr: u64,  // 40
    vbar: u64,   // 48
    stack: u64,  // 56
    entry: u64,  // 64
    arg: u64,    // 72
    seen: u64,   // 80, how far it got with its MMU off, written by it
}

/// One for each CPU, so one that starts late reads its own, never the
/// next one's.
static mut BOOT: [BootBlock; MAX_CPUS] =
    [const { BootBlock { mair: 0, tcr: 0, ttbr0: 0, ttbr1: 0, cpacr: 0, sctlr: 0, vbar: 0, stack: 0, entry: 0, arg: 0, seen: 0 } }; MAX_CPUS];

global_asm!(
    ".balign 64",
    ".global secondary_entry",
    ".global secondary_entry_end",
    "secondary_entry:",
    // x0 is the boot block; it is told this CPU got here, to memory, as
    // its MMU is off
    "mov x3, #1",
    "str x3, [x0, #80]",
    "dsb sy",
    "ldr x1, [x0, #0]",
    "msr mair_el1, x1",
    "ldr x1, [x0, #8]",
    "msr tcr_el1, x1",
    "ldr x1, [x0, #16]",
    "msr ttbr0_el1, x1",
    "ldr x1, [x0, #24]",
    "msr ttbr1_el1, x1",
    "ldr x1, [x0, #32]",
    "msr cpacr_el1, x1",
    "ldr x1, [x0, #48]",
    "msr vbar_el1, x1",
    "isb",
    "tlbi vmalle1",
    "dsb nsh",
    "isb",
    "mov x3, #2",
    "str x3, [x0, #80]",
    "dsb sy",
    "ldr x1, [x0, #40]",
    "msr sctlr_el1, x1",
    "isb",
    "ldr x1, [x0, #56]",
    "mov sp, x1",
    "ldr x2, [x0, #64]",
    "ldr x0, [x0, #72]",
    "blr x2",
    "1:",
    "wfi",
    "b 1b",
    "secondary_entry_end:",
);

extern "C" {
    static secondary_entry: u8;
    static secondary_entry_end: u8;
}

const STACK: usize = 64 << 10;

/// How far each CPU has got, with its MMU on: 3 in Rust, 4 its GIC
/// interface up; then it is online.
static STAGE: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

/// how far CPU n got, for a report: its boot block's word is read from
/// memory, its cached copy dropped first
unsafe fn reached(n: usize) -> &'static str {
    let stage = STAGE[n].load(Ordering::Acquire);
    let b = &(*core::ptr::addr_of!(BOOT))[n];
    let seen = core::ptr::addr_of!(b.seen);
    clean_to_poc(seen as usize, 8);
    let seen = seen.read_volatile();
    match (seen, stage) {
        (_, 4) => "its GIC interface up, not yet online",
        (_, 3) => "in Rust, starting its GIC interface",
        (2, _) => "turning its MMU on",
        (1, _) => "at its entry, MMU off",
        _ => "not at its entry",
    }
}

/// how long a CPU has to come up, from its CPU_ON, before it is asked
/// again; and how many times it is asked
const START_US: u64 = 250_000;
const TRIES: usize = 4;

/// Start every CPU of the machine, all at once, each with a boot block and
/// a stack of its own, as the n-th of the firmware's list: then wait till
/// they are up. One not up in a quarter of a second is asked again, with
/// CPU_ON, which says whether the first was lost (it starts now) or is
/// still on its way (ON_PENDING, ALREADY_ON); after four, a second, the
/// boot goes on without it. One that is slower still comes up when it
/// can, and waits for a job like the others. Returns how many are running,
/// the boot one with them.
pub unsafe fn start_cpus(cpus: &[CpuInfo]) -> usize {
    let code = core::ptr::addr_of!(secondary_entry) as usize;
    clean_to_poc(code, core::ptr::addr_of!(secondary_entry_end) as usize - code);
    let slots = cpus.len().clamp(1, MAX_CPUS);
    let mut started = [false; MAX_CPUS];
    for (n, c) in cpus.iter().enumerate().take(slots).skip(1) {
        let b = &mut (*core::ptr::addr_of_mut!(BOOT))[n];
        b.mair = mrs!("mair_el1");
        b.tcr = mrs!("tcr_el1");
        b.ttbr0 = mrs!("ttbr0_el1");
        b.ttbr1 = mrs!("ttbr1_el1");
        b.cpacr = mrs!("cpacr_el1");
        b.sctlr = mrs!("sctlr_el1");
        b.vbar = mrs!("vbar_el1");
        b.entry = secondary_main as *const () as usize as u64;
        CPUS[n].hw_id.store(c.mpidr, Ordering::Relaxed);
        let stack = alloc::vec![0u8; STACK].leak();
        // 16-byte aligned, as the hardware checks it (SCTLR_EL1.SA); a
        // vector of bytes need not be
        b.stack = stack.as_mut_ptr().add(STACK) as u64 & !15;
        b.arg = n as u64;
        clean_to_poc(b as *const BootBlock as usize, core::mem::size_of::<BootBlock>());
        let r = cpu_on(c.mpidr, code as u64, b as *const BootBlock as u64);
        started[n] = r == 0;
        if r != 0 {
            crate::kprintln!("[machine] CPU {:#x} would not start ({})", c.mpidr, r);
        }
    }
    smp::set_cpus(slots);
    let want = started.iter().filter(|&&s| s).count();
    let up = || (1..slots).filter(|&n| CPUS[n].online.load(Ordering::Acquire)).count();
    let t0 = cpu::now_us();
    for attempt in 1..=TRIES {
        let until = cpu::now_us() + START_US;
        while up() < want && cpu::now_us() < until {
            core::hint::spin_loop();
        }
        if up() == want {
            break;
        }
        for (n, c) in cpus.iter().enumerate().take(slots).skip(1) {
            if !started[n] || CPUS[n].online.load(Ordering::Acquire) {
                continue;
            }
            let ms = (cpu::now_us() - t0) / 1000;
            if attempt == TRIES {
                crate::kprintln!("[machine] CPU {:#x} not up after {} ms, {}, going on without it", c.mpidr, ms, reached(n));
                continue;
            }
            // asked again: 0 if the first was lost, -4 ALREADY_ON if it
            // is running but not here yet, -5 ON_PENDING if still starting
            let b = &(*core::ptr::addr_of!(BOOT))[n];
            let r = cpu_on(c.mpidr, code as u64, b as *const BootBlock as u64);
            crate::kprintln!("[machine] CPU {:#x} not up after {} ms, {}, CPU_ON again: {}", c.mpidr, ms, reached(n), r);
        }
    }
    1 + up()
}

/// where a CPU other than the boot one comes, with its MMU on
extern "C" fn secondary_main(n: usize) -> ! {
    STAGE[n].store(3, Ordering::Release);
    set_this_cpu(n);
    msr!("fpcr", 0);
    msr!("fpsr", 0);
    isb();
    unsafe { gic::init_cpu(n) };
    STAGE[n].store(4, Ordering::Release);
    smp::cpu_main(n)
}

///////////////
// the exception vectors: an IRQ is the tick's or a wake's, on a CPU that
// takes the tick (`cpu::start_tick`); any other exception is a bug, it is
// reported and the machine stops
///////////////

macro_rules! vectors {
    ($($i:literal)*) => {
        global_asm!(
            ".balign 2048",
            ".global exception_vectors",
            "exception_vectors:",
            $(
                ".balign 128",
                // an IRQ from this EL, on SP_EL0 or SP_ELx
                concat!(".if ", $i, " == 1 || ", $i, " == 5"),
                "b irq_entry",
                ".else",
                concat!("mov x0, #", $i),
                "b vector_common",
                ".endif",
            )*
            "vector_common:",
            "mrs x1, esr_el1",
            "mrs x2, elr_el1",
            "mrs x3, far_el1",
            "mov x4, sp",
            // the stack may be what went wrong, a stack of its own
            // one for each CPU, by the low bits of its affinity, so that
            // CPUs failing together do not share one
            "adrp x9, EXCEPTION_STACK",
            "add x9, x9, :lo12:EXCEPTION_STACK",
            "mrs x10, mpidr_el1",
            "and x10, x10, #15", // EXCEPTION_CPUS - 1
            "add x10, x10, #1",
            "lsl x10, x10, #13",
            "add x9, x9, x10",
            "mov sp, x9",
            "bl exception_report",
            "2:",
            "wfi",
            "b 2b",
        );
    };
}

vectors!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);

// An IRQ, on the stack it finds: everything the handler, Rust code with
// the C convention, may change, saved, the floating point and SIMD
// registers with them, and the return state.
global_asm!(
    "irq_entry:",
    "sub sp, sp, #720",
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x5, [sp, #32]",
    "stp x6, x7, [sp, #48]",
    "stp x8, x9, [sp, #64]",
    "stp x10, x11, [sp, #80]",
    "stp x12, x13, [sp, #96]",
    "stp x14, x15, [sp, #112]",
    "stp x16, x17, [sp, #128]",
    "stp x18, x29, [sp, #144]",
    "mrs x0, elr_el1",
    "stp x30, x0, [sp, #160]",
    "mrs x0, spsr_el1",
    "mrs x1, fpsr",
    "stp x0, x1, [sp, #176]",
    "mrs x0, fpcr",
    "str x0, [sp, #192]",
    "stp q0, q1, [sp, #208]",
    "stp q2, q3, [sp, #240]",
    "stp q4, q5, [sp, #272]",
    "stp q6, q7, [sp, #304]",
    "stp q8, q9, [sp, #336]",
    "stp q10, q11, [sp, #368]",
    "stp q12, q13, [sp, #400]",
    "stp q14, q15, [sp, #432]",
    "stp q16, q17, [sp, #464]",
    "stp q18, q19, [sp, #496]",
    "stp q20, q21, [sp, #528]",
    "stp q22, q23, [sp, #560]",
    "stp q24, q25, [sp, #592]",
    "stp q26, q27, [sp, #624]",
    "stp q28, q29, [sp, #656]",
    "stp q30, q31, [sp, #688]",
    "bl machine_irq",
    "ldp q0, q1, [sp, #208]",
    "ldp q2, q3, [sp, #240]",
    "ldp q4, q5, [sp, #272]",
    "ldp q6, q7, [sp, #304]",
    "ldp q8, q9, [sp, #336]",
    "ldp q10, q11, [sp, #368]",
    "ldp q12, q13, [sp, #400]",
    "ldp q14, q15, [sp, #432]",
    "ldp q16, q17, [sp, #464]",
    "ldp q18, q19, [sp, #496]",
    "ldp q20, q21, [sp, #528]",
    "ldp q22, q23, [sp, #560]",
    "ldp q24, q25, [sp, #592]",
    "ldp q26, q27, [sp, #624]",
    "ldp q28, q29, [sp, #656]",
    "ldp q30, q31, [sp, #688]",
    "ldr x0, [sp, #192]",
    "msr fpcr, x0",
    "ldp x0, x1, [sp, #176]",
    "msr spsr_el1, x0",
    "msr fpsr, x1",
    "ldp x30, x0, [sp, #160]",
    "msr elr_el1, x0",
    "ldp x18, x29, [sp, #144]",
    "ldp x16, x17, [sp, #128]",
    "ldp x14, x15, [sp, #112]",
    "ldp x12, x13, [sp, #96]",
    "ldp x10, x11, [sp, #80]",
    "ldp x8, x9, [sp, #64]",
    "ldp x6, x7, [sp, #48]",
    "ldp x4, x5, [sp, #32]",
    "ldp x2, x3, [sp, #16]",
    "ldp x0, x1, [sp, #0]",
    "add sp, sp, #720",
    "eret",
);

#[no_mangle]
extern "C" fn machine_irq() {
    super::gic::irq();
}

/// an exception stack for each CPU, 8 KB, by its affinity's low four bits
const EXCEPTION_CPUS: usize = 16;
#[repr(C, align(16))]
struct ExceptionStacks([[u8; 8192]; EXCEPTION_CPUS]);
#[no_mangle]
static mut EXCEPTION_STACK: ExceptionStacks = ExceptionStacks([[0; 8192]; EXCEPTION_CPUS]);

extern "C" {
    static exception_vectors: u8;
}

pub unsafe fn install_vectors() {
    msr!("vbar_el1", core::ptr::addr_of!(exception_vectors) as u64);
    isb();
}

const VECTOR_KINDS: [&str; 4] = ["synchronous", "IRQ", "FIQ", "SError"];

#[no_mangle]
extern "C" fn exception_report(vector: u64, esr: u64, elr: u64, far: u64, sp: u64) -> ! {
    crate::kprintln!(
        "\n[machine] cpu {} (MPIDR {:#x}) {} exception, ESR {:#x} (class {:#x})\n  pc {:#x} sp {:#x} far {:#x}",
        super::this_cpu(),
        mrs!("mpidr_el1") & 0xff_00ff_ffff,
        VECTOR_KINDS[(vector & 3) as usize],
        esr,
        esr >> 26,
        elr,
        sp,
        far
    );
    cpu::exit(3)
}

///////////////
// running a node, and unwinding it when it exits
///////////////

// call_node(stack_top, f, a0, a1, save) calls f(a0, a1) on stack_top, and
// keeps where to come back to in *save, and at the top of the node's
// stack. exit_node(saved, code) comes back there from anywhere in the
// node, as if f had returned code.
global_asm!(
    ".global call_node",
    "call_node:",
    "stp x29, x30, [sp, #-176]!",
    "stp x19, x20, [sp, #16]",
    "stp x21, x22, [sp, #32]",
    "stp x23, x24, [sp, #48]",
    "stp x25, x26, [sp, #64]",
    "stp x27, x28, [sp, #80]",
    "stp d8, d9, [sp, #96]",
    "stp d10, d11, [sp, #112]",
    "stp d12, d13, [sp, #128]",
    "stp d14, d15, [sp, #144]",
    "mov x9, sp",
    "str x9, [x4]",
    // the stack given need not be aligned, the hardware wants 16
    "and x0, x0, #~15",
    "sub x0, x0, #16",
    "str x9, [x0]",
    "mov sp, x0",
    "mov x16, x1",
    "mov x0, x2",
    "mov x1, x3",
    "blr x16",
    "ldr x9, [sp]",
    "mov sp, x9",
    "call_node_return:",
    "ldp x19, x20, [sp, #16]",
    "ldp x21, x22, [sp, #32]",
    "ldp x23, x24, [sp, #48]",
    "ldp x25, x26, [sp, #64]",
    "ldp x27, x28, [sp, #80]",
    "ldp d8, d9, [sp, #96]",
    "ldp d10, d11, [sp, #112]",
    "ldp d12, d13, [sp, #128]",
    "ldp d14, d15, [sp, #144]",
    "ldp x29, x30, [sp], #176",
    "ret",
    ".global exit_node",
    "exit_node:",
    "mov sp, x0",
    "mov x0, x1",
    "b call_node_return",
);

extern "C" {
    pub fn call_node(stack_top: *mut u8, f: usize, a0: usize, a1: usize, save: *mut u64) -> i64;
    pub fn exit_node(saved: u64, code: i64) -> !;
}
