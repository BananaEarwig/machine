//! Starting the other x86_64 CPUs.
//!
//! The CPUs are found in the ACPI MADT. Each is started with INIT, SIPI,
//! SIPI, into a trampoline in a page below 1 MB that takes it from real
//! mode to long mode, on the boot CPU's page tables and GDT, and into
//! `ap_main`, and from there `smp::cpu_main`, where it waits for a node.

use super::apic;
use crate::acpi;
use crate::cpu;
use crate::smp::{self, CPUS, MAX_CPUS};
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// the CPU number of each APIC id, 0xff for none
static APIC_TO_CPU: [AtomicU8; 256] = [const { AtomicU8::new(0xff) }; 256];
static SMP_READY: AtomicBool = AtomicBool::new(false);

/// the number of the CPU this runs on, 0 for the boot one
pub fn this_cpu() -> usize {
    if !SMP_READY.load(Ordering::Relaxed) {
        return 0;
    }
    match APIC_TO_CPU[(apic::apic_id() & 0xff) as usize].load(Ordering::Relaxed) {
        0xff => 0,
        n => n as usize,
    }
}

///////////////
// ACPI, what CPUs there are
///////////////

/// What the firmware's ACPI tables say: the local APIC ids of the CPUs
/// that are enabled, the boot one first, and the IO APICs.
pub struct Machine {
    pub apic_ids: Vec<u32>,
    pub ioapics: Vec<u64>,
}

/// Read the MADT. None if there is no ACPI 2 RSDP or no MADT.
pub unsafe fn read_madt(rsdp: usize) -> Option<Machine> {
    let madt = acpi::find(rsdp, b"APIC")?;
    let mut m = Machine { apic_ids: Vec::new(), ioapics: Vec::new() };
    for (kind, e) in acpi::madt_entries(madt) {
        match kind {
            // a processor's local APIC, enabled
            0 if acpi::field::<u32>(e, 4) & 1 != 0 => m.apic_ids.push(e[3] as u32),
            // an IO APIC
            1 => m.ioapics.push(acpi::field::<u32>(e, 4) as u64),
            // an x2APIC, which a CPU numbered over 254 needs, and this does not do
            _ => {}
        }
    }
    Some(m)
}

///////////////
// the trampoline, real mode to long mode
///////////////

// It is copied to a page below 1 MB, and the CPU starts at its first byte,
// in real mode, CS the page's number times 256. Everything it reads is
// found from ebx, the page's address. The fields after the code are
// filled in by the boot CPU before each start.
global_asm!(
    ".balign 16",
    ".global ap_tramp_start",
    ".global ap_tramp_end",
    ".global tr_gdt",
    ".global tr_gdtr",
    ".global tr_jmp32",
    ".global tr_jmp64",
    ".global tr_cr3",
    ".global tr_cr4",
    ".global tr_cr0",
    ".global tr_efer",
    ".global tr_stack",
    ".global tr_entry",
    ".global tr_arg",
    ".global tr_pm",
    ".global tr_lm",
    ".code16",
    "ap_tramp_start:",
    "cli",
    "cld",
    "movw %cs, %ax",
    "movw %ax, %ds",
    "xorl %ebx, %ebx",
    "movw %ax, %bx",
    "shll $4, %ebx",
    "lgdtl (tr_gdtr - ap_tramp_start)",
    "movl %cr0, %eax",
    "orl $1, %eax",
    "movl %eax, %cr0",
    "ljmpl *(tr_jmp32 - ap_tramp_start)",
    ".code32",
    "tr_pm:",
    "movw $0x10, %ax",
    "movw %ax, %ds",
    "movw %ax, %es",
    "movw %ax, %ss",
    "movl (tr_cr4 - ap_tramp_start)(%ebx), %eax",
    "movl %eax, %cr4",
    "movl (tr_cr3 - ap_tramp_start)(%ebx), %eax",
    "movl %eax, %cr3",
    "movl $0xC0000080, %ecx",
    "rdmsr",
    "orl (tr_efer - ap_tramp_start)(%ebx), %eax",
    "wrmsr",
    "movl (tr_cr0 - ap_tramp_start)(%ebx), %eax",
    "movl %eax, %cr0",
    "ljmpl *(tr_jmp64 - ap_tramp_start)(%ebx)",
    ".code64",
    "tr_lm:",
    "movw $0x10, %ax",
    "movw %ax, %ds",
    "movw %ax, %es",
    "movw %ax, %ss",
    "movw %ax, %fs",
    "movw %ax, %gs",
    "movl %ebx, %ebx",
    "movq (tr_stack - ap_tramp_start)(%rbx), %rsp",
    "movq (tr_arg - ap_tramp_start)(%rbx), %rdi",
    "movq (tr_entry - ap_tramp_start)(%rbx), %rax",
    "callq *%rax",
    "1:",
    "hlt",
    "jmp 1b",
    ".balign 8",
    "tr_gdt:",
    ".quad 0",
    ".quad 0x00AF9A000000FFFF", // 0x08, 64 bit code
    ".quad 0x00CF92000000FFFF", // 0x10, data
    ".quad 0x00CF9A000000FFFF", // 0x18, 32 bit code
    "tr_gdtr:",
    ".word 31",
    ".long 0",
    "tr_jmp32:",
    ".long 0",
    ".word 0x18",
    "tr_jmp64:",
    ".long 0",
    ".word 0x08",
    ".balign 8",
    "tr_cr3: .quad 0",
    "tr_cr4: .quad 0",
    "tr_cr0: .quad 0",
    "tr_efer: .quad 0",
    "tr_stack: .quad 0",
    "tr_entry: .quad 0",
    "tr_arg: .quad 0",
    "ap_tramp_end:",
    options(att_syntax)
);

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
    static tr_gdt: u8;
    static tr_gdtr: u8;
    static tr_jmp32: u8;
    static tr_jmp64: u8;
    static tr_cr3: u8;
    static tr_cr4: u8;
    static tr_cr0: u8;
    static tr_efer: u8;
    static tr_stack: u8;
    static tr_entry: u8;
    static tr_arg: u8;
    static tr_pm: u8;
    static tr_lm: u8;
}

/// where a symbol of the trampoline is, in the copy at `page`
unsafe fn tr_at(page: u64, sym: *const u8) -> u64 {
    page + (sym as u64 - core::ptr::addr_of!(ap_tramp_start) as u64)
}

unsafe fn tr_put<T>(page: u64, sym: *const u8, v: T) {
    (tr_at(page, sym) as *mut T).write_unaligned(v);
}

/// Copy the trampoline to its page, below 1 MB, and fill in what does not
/// change, the GDT's address and the far jumps. Returns the GDT's address,
/// which the boot CPU loads as well.
pub unsafe fn install_trampoline(page: u64) -> u64 {
    let start = core::ptr::addr_of!(ap_tramp_start);
    let len = core::ptr::addr_of!(ap_tramp_end) as usize - start as usize;
    core::ptr::copy_nonoverlapping(start, page as *mut u8, len);
    tr_put(page, core::ptr::addr_of!(tr_gdtr).add(2), tr_at(page, core::ptr::addr_of!(tr_gdt)) as u32);
    tr_put(page, core::ptr::addr_of!(tr_jmp32), tr_at(page, core::ptr::addr_of!(tr_pm)) as u32);
    tr_put(page, core::ptr::addr_of!(tr_jmp64), tr_at(page, core::ptr::addr_of!(tr_lm)) as u32);
    tr_at(page, core::ptr::addr_of!(tr_gdt))
}

fn read_cr(n: u8) -> u64 {
    let v: u64;
    unsafe {
        match n {
            0 => core::arch::asm!("mov {}, cr0", out(reg) v, options(nomem, nostack)),
            3 => core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)),
            _ => core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack)),
        }
    }
    v
}

///////////////
// starting the CPUs
///////////////

const AP_STACK: usize = 64 << 10;

/// Start every CPU of the machine, one at a time, through the trampoline at
/// `page`. Returns how many are running, the boot one with them.
pub unsafe fn start_cpus(page: u64, machine: &Machine) -> usize {
    let bsp = apic::apic_id();
    smp::boot_cpu(bsp as u64);
    APIC_TO_CPU[(bsp & 0xff) as usize].store(0, Ordering::Relaxed);
    SMP_READY.store(true, Ordering::Release);
    smp::set_cpus(1);
    if page == 0 {
        return 1;
    }

    let cr3 = read_cr(3);
    if cr3 >= 1 << 32 {
        crate::kprintln!("[machine] page tables above 4 GB, one CPU");
        return 1;
    }
    // the boot CPU's paging, less what only long mode can have turned on,
    // PCIDE and CET
    tr_put(page, core::ptr::addr_of!(tr_cr3), cr3);
    tr_put(page, core::ptr::addr_of!(tr_cr4), read_cr(4) & !((1 << 17) | (1 << 23)));
    tr_put(page, core::ptr::addr_of!(tr_cr0), read_cr(0));
    tr_put(page, core::ptr::addr_of!(tr_efer), apic::rdmsr(0xc000_0080) & 0xffff_ffff & !(1 << 10));
    tr_put(page, core::ptr::addr_of!(tr_entry), ap_main as *const () as usize as u64);

    let mut n = 1;
    for &id in machine.apic_ids.iter().filter(|&&id| id != bsp) {
        if n >= MAX_CPUS || id > 0xfe {
            break;
        }
        CPUS[n].hw_id.store(id as u64, Ordering::Relaxed);
        APIC_TO_CPU[id as usize].store(n as u8, Ordering::Relaxed);
        let top = crate::stack::Stack::new(AP_STACK).leak();
        tr_put(page, core::ptr::addr_of!(tr_stack), top as u64);
        tr_put(page, core::ptr::addr_of!(tr_arg), n as u64);
        // INIT, then two SIPIs at the trampoline's page
        apic::send_ipi(id, 0x4500);
        cpu::spin_us(10_000);
        for _ in 0..2 {
            apic::send_ipi(id, 0x4600 | (page >> 12) as u32);
            cpu::spin_us(200);
        }
        let t0 = cpu::now_us();
        while !CPUS[n].online.load(Ordering::Acquire) && cpu::now_us() < t0 + 200_000 {
            core::hint::spin_loop();
        }
        if CPUS[n].online.load(Ordering::Acquire) {
            n += 1;
        } else {
            crate::kprintln!("[machine] CPU with APIC id {} did not start", id);
            APIC_TO_CPU[id as usize].store(0xff, Ordering::Relaxed);
        }
    }
    smp::set_cpus(n);
    n
}

/// where a CPU other than the boot one starts, from the trampoline
extern "sysv64" fn ap_main(n: usize) -> ! {
    unsafe {
        // the x87 and SSE state as a program starts with it
        core::arch::asm!("fninit", "push 0x1f80", "ldmxcsr [rsp]", "add rsp, 8");
        apic::load_tss();
        apic::load_idt();
        apic::lapic_init();
    }
    smp::cpu_main(n)
}

///////////////
// running a node, and unwinding it when it exits
///////////////

// call_node(stack_top, f, a0, a1, save) calls f(a0, a1) on stack_top, and
// keeps where to come back to in *save. exit_node(saved, code) comes back
// there from anywhere in the node, as if f had returned code.
global_asm!(
    ".global call_node",
    "call_node:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov qword ptr [r8], rsp",
    "mov r15, r8",
    // the stack given need not be aligned; the call below leaves it as
    // the ABI has it at a function's entry
    "and rdi, -16",
    "mov rsp, rdi",
    "mov rax, rsi",
    "mov rdi, rdx",
    "mov rsi, rcx",
    "call rax",
    "mov rsp, qword ptr [r15]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".global exit_node",
    "exit_node:",
    "mov rsp, rdi",
    "mov rax, rsi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

extern "sysv64" {
    pub fn call_node(stack_top: *mut u8, f: usize, a0: usize, a1: usize, save: *mut u64) -> i64;
    pub fn exit_node(saved: u64, code: i64) -> !;
}
