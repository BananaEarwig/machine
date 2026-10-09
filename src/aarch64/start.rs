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
use core::sync::atomic::Ordering;

/// What a CPU starting reads, with its MMU off, so it is cleaned to memory
/// before each start. The offsets are the entry code's.
#[repr(C, align(64))]
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
}

static mut BOOT: BootBlock =
    BootBlock { mair: 0, tcr: 0, ttbr0: 0, ttbr1: 0, cpacr: 0, sctlr: 0, vbar: 0, stack: 0, entry: 0, arg: 0 };

global_asm!(
    ".balign 64",
    ".global secondary_entry",
    ".global secondary_entry_end",
    "secondary_entry:",
    // x0 is the boot block
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

/// Start every CPU of the machine, one at a time. Returns how many are
/// running, the boot one with them.
pub unsafe fn start_cpus(cpus: &[CpuInfo]) -> usize {
    let b = &mut *core::ptr::addr_of_mut!(BOOT);
    b.mair = mrs!("mair_el1");
    b.tcr = mrs!("tcr_el1");
    b.ttbr0 = mrs!("ttbr0_el1");
    b.ttbr1 = mrs!("ttbr1_el1");
    b.cpacr = mrs!("cpacr_el1");
    b.sctlr = mrs!("sctlr_el1");
    b.vbar = mrs!("vbar_el1");
    b.entry = secondary_main as *const () as usize as u64;
    let code = core::ptr::addr_of!(secondary_entry) as usize;
    clean_to_poc(code, core::ptr::addr_of!(secondary_entry_end) as usize - code);

    let mut n = 1;
    for c in cpus.iter().skip(1) {
        if n >= MAX_CPUS {
            break;
        }
        CPUS[n].hw_id.store(c.mpidr, Ordering::Relaxed);
        let stack = alloc::vec![0u8; STACK].leak();
        b.stack = stack.as_mut_ptr().add(STACK) as u64;
        b.arg = n as u64;
        clean_to_poc(b as *const BootBlock as usize, core::mem::size_of::<BootBlock>());
        let r = cpu_on(c.mpidr, code as u64, b as *const BootBlock as u64);
        let t0 = cpu::now_us();
        while r == 0 && !CPUS[n].online.load(Ordering::Acquire) && cpu::now_us() < t0 + 1_000_000 {
            core::hint::spin_loop();
        }
        if CPUS[n].online.load(Ordering::Acquire) {
            n += 1;
        } else {
            crate::kprintln!("[machine] CPU {:#x} did not start ({})", c.mpidr, r);
        }
    }
    smp::set_cpus(n);
    n
}

/// where a CPU other than the boot one comes, with its MMU on
extern "C" fn secondary_main(n: usize) -> ! {
    set_this_cpu(n);
    msr!("fpcr", 0);
    msr!("fpsr", 0);
    isb();
    unsafe { gic::init_cpu(n) };
    smp::cpu_main(n)
}

///////////////
// the exception vectors, any exception is a bug, it is reported and the
// machine stops
///////////////

macro_rules! vectors {
    ($($i:literal)*) => {
        global_asm!(
            ".balign 2048",
            ".global exception_vectors",
            "exception_vectors:",
            $(
                ".balign 128",
                concat!("mov x0, #", $i),
                "b vector_common",
            )*
            "vector_common:",
            "mrs x1, esr_el1",
            "mrs x2, elr_el1",
            "mrs x3, far_el1",
            "mov x4, sp",
            // the stack may be what went wrong, a stack of its own
            "adrp x9, EXCEPTION_STACK",
            "add x9, x9, :lo12:EXCEPTION_STACK",
            "add x9, x9, #4096",
            "add x9, x9, #4096",
            "mov sp, x9",
            "bl exception_report",
            "2:",
            "wfi",
            "b 2b",
        );
    };
}

vectors!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);

#[no_mangle]
static mut EXCEPTION_STACK: [u8; 8192] = [0; 8192];

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
        "\n[machine] cpu {} {} exception, ESR {:#x} (class {:#x})\n  pc {:#x} sp {:#x} far {:#x}",
        super::this_cpu(),
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
