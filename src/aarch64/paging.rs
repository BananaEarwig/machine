//! The page tables, the machine's own: at take-over a copy of the
//! firmware's, which map all of memory flat, with the heap in 4 KB pages,
//! so that a page of it can be taken out of the map, a stack's guard page.
//!
//! A block can not be split where it is, while the CPU may be using it,
//! without breaking it first, which would take out of the map the code
//! doing it. So the copy is made whole, the CPU switched to it, and its
//! TLB emptied. Taking a page out of the map, and putting it back, is
//! then a change to one valid entry or one invalid one, which needs no
//! break.
//!
//! The firmware's granule must be 4 KB, as QEMU's and most are; with
//! another, stacks have no guard pages.

use super::{isb, mrs};
use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

const VALID: u64 = 1;
/// a table, at levels 0 to 2; a page, at level 3
const TABLE: u64 = 2;
const ADDR: u64 = 0x0000_ffff_ffff_f000;

static ROOT: AtomicU64 = AtomicU64::new(0);
/// the level the walk starts at, and how many entries its table has
static START: AtomicU32 = AtomicU32::new(0);

fn new_table() -> *mut u64 {
    let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
    let t = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut u64;
    assert!(!t.is_null(), "no memory for a page table");
    t
}

/// how much an entry of a table at this level maps: 512 GB at 0, 4 KB at 3
fn span(level: u32) -> u64 {
    1 << (12 + 9 * (3 - level))
}

/// a block of this level split into a table of the next, its blocks split
/// again where they hold some of lo..hi
unsafe fn split(e: u64, level: u32, va: u64, lo: u64, hi: u64) -> u64 {
    let t = new_table();
    let size = span(level);
    let child = span(level + 1);
    let base = e & ADDR & !(size - 1);
    let attrs = e & !ADDR & !TABLE;
    for j in 0..512u64 {
        let phys = base + j * child;
        let cva = va + j * child;
        let c = if level == 1 {
            let c = phys | attrs;
            if cva < hi && cva + child > lo { split(c, 2, cva, lo, hi) } else { c }
        } else {
            phys | attrs | TABLE
        };
        t.add(j as usize).write(c);
    }
    t as u64 | TABLE | VALID
}

/// a copy of a table of this level, of n entries, mapping from va, and of
/// the tables under it, with what holds some of lo..hi in 4 KB pages
unsafe fn copy(table: u64, level: u32, n: u64, va: u64, lo: u64, hi: u64) -> u64 {
    let old = table as *const u64;
    let t = new_table();
    let size = span(level);
    for i in 0..n {
        let e = old.add(i as usize).read();
        let eva = va + i * size;
        let c = if e & VALID == 0 || level == 3 {
            e
        } else if e & TABLE == 0 {
            if (level == 1 || level == 2) && eva < hi && eva + size > lo { split(e, level, eva, lo, hi) } else { e }
        } else {
            copy(e & ADDR, level + 1, 512, eva, lo, hi) | (e & !ADDR)
        };
        t.add(i as usize).write(c);
    }
    t as u64
}

/// The machine's own tables, lo..hi in 4 KB pages, and this CPU on them;
/// the other CPUs take them when they start. False if they cannot be had.
pub unsafe fn own_tables(lo: u64, hi: u64) -> bool {
    let tcr = mrs!("tcr_el1");
    // the 4 KB granule, TG0 0
    if (tcr >> 14) & 3 != 0 {
        return false;
    }
    let va_bits = 64 - (tcr & 0x3f) as u32;
    let (level, n) = match va_bits {
        40..=48 => (0, 1u64 << (va_bits - 39)),
        31..=39 => (1, 1u64 << (va_bits - 30)),
        22..=30 => (2, 1u64 << (va_bits - 21)),
        _ => return false,
    };
    let ttbr0 = mrs!("ttbr0_el1");
    let root = copy(ttbr0 & ADDR, level, n, 0, lo, hi);
    START.store(level << 16 | n as u32, Ordering::Relaxed);
    // the tables written, then the CPU on them, and nothing of the old in
    // its TLB
    asm!(
        "dsb ishst",
        "msr ttbr0_el1, {t}",
        "isb",
        "tlbi vmalle1",
        "dsb nsh",
        "isb",
        t = in(reg) root | (ttbr0 & !ADDR),
        options(nostack)
    );
    ROOT.store(root, Ordering::Release);
    true
}

/// The 4 KB page at addr in the map or out of it, on the machine's own
/// tables; false if there are none, or it is not a 4 KB page of them. The
/// TLBs of all the CPUs are told.
pub fn set_present(addr: usize, present: bool) -> bool {
    let mut t = ROOT.load(Ordering::Acquire);
    if t == 0 {
        return false;
    }
    let start = START.load(Ordering::Relaxed);
    let va = addr as u64;
    unsafe {
        for level in (start >> 16)..3 {
            let i = (va >> (12 + 9 * (3 - level))) & 511;
            let e = (t as *const u64).add(i as usize).read();
            if e & VALID == 0 || e & TABLE == 0 {
                return false;
            }
            t = e & ADDR;
        }
        let p = (t as *mut u64).add(((va >> 12) & 511) as usize);
        let e = p.read();
        p.write(if present { e | VALID } else { e & !VALID });
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {v}",
            "dsb ish",
            v = in(reg) va >> 12,
            options(nostack)
        );
    }
    isb();
    true
}
