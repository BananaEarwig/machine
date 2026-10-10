//! x86_64 interrupts, the GDT, the IDT, the local APIC and its timer.
//!
//! Interrupts are only ever taken in a `sti; hlt` window, when a CPU has
//! nothing to do till its timer goes off, or another CPU wakes it with an
//! IPI. The rest of the time they are off, so no handler ever runs in the
//! middle of ChrysaLisp's code, and the red zone it may use below its stack
//! pointer is never written over.
//!
//! But on a CPU that takes the tick (`cpu::start_tick`): there they are
//! on, the timer goes off every period, and its handler runs in the middle
//! of anything. Every interrupt is taken on a stack of each CPU's own, the
//! first IST stack of its TSS, never on the stack of the code it stops.

use crate::cpu;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

pub const VECTOR_TIMER: u8 = 0x20;
pub const VECTOR_WAKE: u8 = 0x21;
pub const VECTOR_TICK: u8 = 0x22;
pub const VECTOR_SPURIOUS: u8 = 0xff;

// the local APIC's registers, offsets from its base
const LAPIC_ID: usize = 0x20;
const LAPIC_TPR: usize = 0x80;
const LAPIC_EOI: usize = 0xb0;
const LAPIC_SVR: usize = 0xf0;
const LAPIC_ICR_LO: usize = 0x300;
const LAPIC_ICR_HI: usize = 0x310;
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_LVT_LINT0: usize = 0x350;
const LAPIC_LVT_LINT1: usize = 0x360;
const LAPIC_LVT_ERROR: usize = 0x370;
const LAPIC_TIMER_INIT: usize = 0x380;
const LAPIC_TIMER_CUR: usize = 0x390;
const LAPIC_TIMER_DIV: usize = 0x3e0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
/// the timer's ticks in a millisecond, divided by 16
static LAPIC_TICKS_PER_MS: AtomicU64 = AtomicU64::new(0);

/// where the interrupt stubs write to end an interrupt
#[no_mangle]
static mut lapic_eoi_addr: u64 = 0;

pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack));
    (hi as u64) << 32 | lo as u64
}

pub unsafe fn wrmsr(msr: u32, v: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nomem, nostack));
}

fn lapic_reg(off: usize) -> *mut u32 {
    (LAPIC_BASE.load(Ordering::Relaxed) as usize + off) as *mut u32
}

pub fn lapic_read(off: usize) -> u32 {
    unsafe { lapic_reg(off).read_volatile() }
}

pub fn lapic_write(off: usize, v: u32) {
    unsafe { lapic_reg(off).write_volatile(v) }
}

/// this CPU's local APIC id
pub fn apic_id() -> u32 {
    lapic_read(LAPIC_ID) >> 24
}

///////////////
// the GDT and the IDT
///////////////

/// the kernel's code segment, its selector, the IDT's gates use it
static CODE_SELECTOR: AtomicU64 = AtomicU64::new(0);

#[repr(C, packed)]
struct TableReg {
    limit: u16,
    base: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    off_lo: u16,
    selector: u16,
    ist: u8,
    attr: u8,
    off_mid: u16,
    off_hi: u32,
    zero: u32,
}

#[repr(C, align(16))]
struct Idt([Gate; 256]);

static mut IDT: Idt = Idt([Gate { off_lo: 0, selector: 0, ist: 0, attr: 0, off_mid: 0, off_hi: 0, zero: 0 }; 256]);

/// Load the GDT at `gdt`, its first four entries as `smp::GDT` has them, and
/// put the code and data segments in it. A CPU that comes up runs on the
/// same one, so the IDT's gates are good on all of them.
pub unsafe fn load_gdt(gdt: u64) {
    let reg = TableReg { limit: 31, base: gdt };
    asm!("lgdt [{}]", in(reg) &reg, options(nostack, readonly));
    asm!(
        "pushq $0x08",
        "leaq 2f(%rip), %rax",
        "pushq %rax",
        "lretq",
        "2:",
        "movw $0x10, %ax",
        "movw %ax, %ds",
        "movw %ax, %es",
        "movw %ax, %ss",
        out("rax") _,
        options(att_syntax)
    );
}

pub fn current_cs() -> u16 {
    let cs: u16;
    unsafe { asm!("mov {:x}, cs", out(reg) cs, options(nomem, nostack)) };
    cs
}

extern "C" {
    fn irq_ack();
    fn irq_spurious();
    fn irq_tick();
    static isr_table: [u64; 32];
}

/// Build the IDT, on the boot CPU, with the selector of the code segment
/// the gates are in.
pub unsafe fn build_idt() {
    CODE_SELECTOR.store(current_cs() as u64, Ordering::Relaxed);
    let set = |v: usize, handler: u64| {
        let g = &mut (*core::ptr::addr_of_mut!(IDT)).0[v];
        *g = Gate {
            off_lo: handler as u16,
            selector: CODE_SELECTOR.load(Ordering::Relaxed) as u16,
            // an interrupt on the CPU's own stack, an exception on another,
            // so that one from a stack that has overflowed can be reported
            ist: if v >= 32 { 1 } else { 2 },
            attr: 0x8e, // present, ring 0, interrupt gate
            off_mid: (handler >> 16) as u16,
            off_hi: (handler >> 32) as u32,
            zero: 0,
        };
    };
    // anything that is not an exception is ended and forgotten
    for v in 32..256 {
        set(v, irq_ack as *const () as usize as u64);
    }
    for (v, &h) in (*core::ptr::addr_of!(isr_table)).iter().enumerate() {
        set(v, h);
    }
    set(VECTOR_SPURIOUS as usize, irq_spurious as *const () as usize as u64);
    set(VECTOR_TICK as usize, irq_tick as *const () as usize as u64);
}

/// the task state segment, for its interrupt stacks
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap: u16,
}

/// the stacks each CPU takes interrupts and exceptions on
const INTERRUPT_STACK: usize = 16 << 10;
const EXCEPTION_STACK: usize = 16 << 10;

/// This CPU's own GDT, a copy of the one it runs on with a TSS of its own
/// after it, whose first IST stack, a stack of this CPU's, every
/// interrupt is taken on. On each CPU, before it takes an interrupt.
pub unsafe fn load_tss() {
    let mut reg = [0u8; 10];
    asm!("sgdt [{}]", in(reg) reg.as_mut_ptr(), options(nostack));
    let limit = u16::from_le_bytes([reg[0], reg[1]]) as usize;
    let base = u64::from_le_bytes(reg[2..10].try_into().unwrap()) as *const u64;
    let n = (limit + 1) / 8;
    let gdt = alloc::vec![0u64; n + 2].leak();
    core::ptr::copy_nonoverlapping(base, gdt.as_mut_ptr(), n);
    let top = crate::stack::Stack::new(INTERRUPT_STACK).leak() as u64;
    let exception_top = crate::stack::Stack::new(EXCEPTION_STACK).leak() as u64;
    let size = core::mem::size_of::<Tss>() as u64;
    let tss = alloc::boxed::Box::leak(alloc::boxed::Box::new(Tss {
        reserved0: 0,
        rsp: [0; 3],
        reserved1: 0,
        ist: [top, exception_top, 0, 0, 0, 0, 0],
        reserved2: 0,
        reserved3: 0,
        iomap: size as u16,
    }));
    let b = tss as *const Tss as u64;
    let l = size - 1;
    // a 64-bit TSS descriptor, two entries: present, available
    gdt[n] = (l & 0xffff) | (b & 0xff_ffff) << 16 | 0x89 << 40 | (l >> 16 & 0xf) << 48 | (b >> 24 & 0xff) << 56;
    gdt[n + 1] = b >> 32;
    let new = TableReg { limit: ((n + 2) * 8 - 1) as u16, base: gdt.as_ptr() as u64 };
    asm!("lgdt [{}]", in(reg) &new, options(nostack, readonly));
    asm!("ltr {:x}", in(reg) (n * 8) as u16, options(nostack));
}

/// load the IDT, on each CPU
pub unsafe fn load_idt() {
    let reg = TableReg { limit: (core::mem::size_of::<Idt>() - 1) as u16, base: core::ptr::addr_of!(IDT) as u64 };
    asm!("lidt [{}]", in(reg) &reg, options(nostack, readonly));
}

// The stubs. An interrupt ends itself and returns, it only woke a CPU from
// hlt. An exception is a bug, it is reported and the machine stops.
global_asm!(
    ".global irq_ack",
    "irq_ack:",
    "push rax",
    "mov rax, qword ptr [rip + lapic_eoi_addr]",
    "mov dword ptr [rax], 0",
    "pop rax",
    "iretq",
    ".global irq_spurious",
    "irq_spurious:",
    "iretq",
    // the tick: what the handler, System V code, may change, saved, on
    // the IST stack, which the CPU's push of five words leaves aligned
    // once these nine are pushed
    ".global irq_tick",
    "irq_tick:",
    "push rax",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "cld",
    "call machine_tick",
    "mov rax, qword ptr [rip + lapic_eoi_addr]",
    "mov dword ptr [rax], 0",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rax",
    "iretq",
);

#[no_mangle]
extern "sysv64" fn machine_tick() {
    cpu::tick();
}

macro_rules! isr_stubs {
    ($($v:literal $err:literal),*) => {
        global_asm!(
            $(
                concat!(".global isr", $v),
                concat!("isr", $v, ":"),
                // the CPU pushes an error code for some, a 0 for the rest
                concat!(".if ", $err, " == 0"),
                "push 0",
                ".endif",
                concat!("push ", $v),
                "jmp isr_common",
            )*
            "isr_common:",
            "mov rdi, rsp",
            "and rsp, -16",
            "call exception_report",
            "2: hlt",
            "jmp 2b",
            ".section .rdata",
            ".balign 8",
            ".global isr_table",
            "isr_table:",
            $(concat!(".quad isr", $v),)*
            ".text",
        );
    };
}

isr_stubs!(0 0, 1 0, 2 0, 3 0, 4 0, 5 0, 6 0, 7 0, 8 1, 9 0, 10 1, 11 1, 12 1, 13 1, 14 1, 15 0,
    16 0, 17 1, 18 0, 19 0, 20 0, 21 1, 22 0, 23 0, 24 0, 25 0, 26 0, 27 0, 28 0, 29 1, 30 1, 31 0);

const EXCEPTION_NAMES: [&str; 21] = [
    "divide error", "debug", "NMI", "breakpoint", "overflow", "bound range", "invalid opcode",
    "device not available", "double fault", "coprocessor overrun", "invalid TSS", "segment not present",
    "stack fault", "general protection", "page fault", "reserved", "x87 floating point", "alignment check",
    "machine check", "SIMD floating point", "virtualization",
];

/// what the stubs call, with the vector, the error code and what the CPU
/// pushed, rip, cs, rflags, rsp and ss
#[no_mangle]
extern "sysv64" fn exception_report(frame: *const u64) -> ! {
    let f = unsafe { core::slice::from_raw_parts(frame, 7) };
    let cr2: u64;
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)) };
    let name = EXCEPTION_NAMES.get(f[0] as usize).copied().unwrap_or("exception");
    crate::kprintln!(
        "\n[machine] cpu {} {} ({}), error {:#x}\n  rip {:#x} rsp {:#x} rflags {:#x} cr2 {:#x}",
        crate::smp::this_cpu(),
        name,
        f[0],
        f[1],
        f[2],
        f[5],
        f[4],
        cr2
    );
    // a page fault, or the double fault one became, in a guard page
    if (f[0] == 14 || f[0] == 8) && crate::stack::is_guard(cr2 as usize) {
        crate::kprintln!("  a stack has overflowed: {:#x} is the guard page below it", cr2);
    }
    cpu::exit(3)
}

///////////////
// the local APIC
///////////////

/// Turn on this CPU's local APIC: interrupts at any priority, the spurious
/// vector, and the timer, one shot, masked till a sleep sets it.
pub unsafe fn lapic_init() {
    let msr = rdmsr(0x1b);
    // enabled, and the base, which is the same on every CPU
    wrmsr(0x1b, msr | (1 << 11));
    LAPIC_BASE.store(msr & 0xf_ffff_f000, Ordering::Relaxed);
    lapic_eoi_addr = (msr & 0xf_ffff_f000) + LAPIC_EOI as u64;
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_SVR, 0x100 | VECTOR_SPURIOUS as u32);
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED | VECTOR_TIMER as u32);
    lapic_write(LAPIC_LVT_ERROR, LVT_MASKED);
    lapic_write(LAPIC_TIMER_DIV, 0x3); // divide by 16
    if crate::smp::this_cpu() != 0 {
        // only the boot CPU has the legacy interrupt lines
        lapic_write(LAPIC_LVT_LINT0, LVT_MASKED);
        lapic_write(LAPIC_LVT_LINT1, LVT_MASKED);
    }
}

/// the legacy PIC, masked, nothing is wanted from it
pub unsafe fn mask_pic() {
    super::outb(0x21, 0xff);
    super::outb(0xa1, 0xff);
}

/// every input of an IO APIC, masked, at its MMIO base
pub unsafe fn mask_ioapic(base: u64) {
    let sel = base as *mut u32;
    let win = (base + 0x10) as *mut u32;
    sel.write_volatile(1);
    let count = ((win.read_volatile() >> 16) & 0xff) + 1;
    for i in 0..count {
        sel.write_volatile(0x10 + 2 * i);
        let lo = win.read_volatile();
        sel.write_volatile(0x10 + 2 * i);
        win.write_volatile(lo | LVT_MASKED);
    }
}

/// how fast the timer counts, against the TSC
pub fn calibrate_timer() {
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED | VECTOR_TIMER as u32);
    lapic_write(LAPIC_TIMER_INIT, u32::MAX);
    let start = cpu::now_us();
    while cpu::now_us() < start + 10_000 {
        core::hint::spin_loop();
    }
    let left = lapic_read(LAPIC_TIMER_CUR);
    lapic_write(LAPIC_TIMER_INIT, 0);
    LAPIC_TICKS_PER_MS.store(((u32::MAX - left) / 10) as u64, Ordering::Relaxed);
}

pub fn timer_mhz() -> u64 {
    LAPIC_TICKS_PER_MS.load(Ordering::Relaxed) * 16 / 1000
}

/// sleep till the timer goes off, or another CPU wakes this one; on a CPU
/// that takes the tick, interrupts stay on
fn halt() {
    if cpu::ticking() {
        unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
    } else {
        unsafe { asm!("sti", "hlt", "cli", options(nomem, nostack)) };
    }
}

/// The tick, every `period_us`, on this CPU, and interrupts on: the
/// timer, periodic, is the tick's alone from now on.
pub fn start_tick(period_us: u64) {
    let per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Relaxed).max(1);
    let ticks = (period_us * per_ms / 1000).clamp(1, u32::MAX as u64) as u32;
    lapic_write(LAPIC_LVT_TIMER, LVT_PERIODIC | VECTOR_TICK as u32);
    lapic_write(LAPIC_TIMER_INIT, ticks);
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// sleep this long, halted, not spinning, the CPU is idle
pub fn sleep_us(us: u64) {
    let per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Relaxed);
    let end = cpu::now_us() + us.max(1);
    if cpu::ticking() {
        // the tick wakes it, each period
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
        if per_ms == 0 || left < 20 {
            // too short to be worth a halt, or no timer
            core::hint::spin_loop();
            continue;
        }
        let ticks = (left * per_ms / 1000).clamp(1, u32::MAX as u64) as u32;
        lapic_write(LAPIC_LVT_TIMER, VECTOR_TIMER as u32);
        lapic_write(LAPIC_TIMER_INIT, ticks);
        halt();
        lapic_write(LAPIC_TIMER_INIT, 0);
    }
}

/// Sleep at most this long, or till anything wakes the CPU, an IPI from
/// another say: one halt, for a caller that looks again when it ends.
pub fn nap_us(us: u64) {
    let per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Relaxed);
    if cpu::ticking() {
        halt();
        return;
    }
    if per_ms == 0 {
        cpu::spin_us(us.min(1000));
        return;
    }
    let ticks = (us.max(1) * per_ms / 1000).clamp(1, u32::MAX as u64) as u32;
    lapic_write(LAPIC_LVT_TIMER, VECTOR_TIMER as u32);
    lapic_write(LAPIC_TIMER_INIT, ticks);
    halt();
    lapic_write(LAPIC_TIMER_INIT, 0);
}

/// idle till woken, an IPI or the next timer
pub fn idle() {
    halt();
}

///////////////
// inter-processor interrupts
///////////////

fn icr_wait() {
    while lapic_read(LAPIC_ICR_LO) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
}

pub fn send_ipi(apic: u32, low: u32) {
    icr_wait();
    lapic_write(LAPIC_ICR_HI, apic << 24);
    lapic_write(LAPIC_ICR_LO, low);
    icr_wait();
}

/// wake a CPU that is halted
pub fn wake(apic: u32) {
    send_ipi(apic, VECTOR_WAKE as u32);
}
