//! The page tables, the machine's own: at take-over a copy of the
//! firmware's, which map all of memory flat, with the heap in 4 KB pages,
//! so that a page of it can be taken out of the map, a stack's guard page.
//! The firmware's own tables are left as they are; they may be read-only.
//!
//! Four levels only: a machine with five, LA57 on, keeps the firmware's,
//! and stacks have no guard pages.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

const PRESENT: u64 = 1;
/// a 1 GB or 2 MB page, in a level 3 or 2 entry
const LARGE: u64 = 1 << 7;
/// a large page's PAT bit; a 4 KB page's is bit 7
const LARGE_PAT: u64 = 1 << 12;
const ADDR: u64 = 0x000f_ffff_ffff_f000;
/// what a table entry gives its table: present, writable, user; the
/// leaves say what may be done
const TABLE: u64 = 0x7;

/// the root of the machine's own tables, 0 till there are some
static ROOT: AtomicU64 = AtomicU64::new(0);

fn cr(n: u8) -> u64 {
    let v: u64;
    unsafe {
        match n {
            3 => asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)),
            _ => asm!("mov {}, cr4", out(reg) v, options(nomem, nostack)),
        }
    }
    v
}

fn new_table() -> *mut u64 {
    let layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
    let t = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut u64;
    assert!(!t.is_null(), "no memory for a page table");
    t
}

/// how much an entry of a table at this level maps: 4 KB at 1, 512 GB at 4
fn span(level: u32) -> u64 {
    1 << (12 + 9 * (level - 1))
}

/// a large page of this level split into a table of the next, its pages
/// split again where they hold some of lo..hi
unsafe fn split(e: u64, level: u32, va: u64, lo: u64, hi: u64) -> u64 {
    let t = new_table();
    let size = span(level);
    let child = span(level - 1);
    let base = e & ADDR & !(size - 1);
    let pat = e & LARGE_PAT != 0;
    let flags = e & !ADDR & !LARGE_PAT;
    for j in 0..512u64 {
        let phys = base + j * child;
        let cva = va + j * child;
        let c = if level == 3 {
            // 2 MB pages, the PAT bit where it is in a large page
            let c = phys | flags | if pat { LARGE_PAT } else { 0 };
            if cva < hi && cva + child > lo { split(c, 2, cva, lo, hi) } else { c }
        } else {
            // 4 KB pages, with no large bit, the PAT bit at 7
            phys | (flags & !LARGE) | if pat { 1 << 7 } else { 0 }
        };
        t.add(j as usize).write(c);
    }
    t as u64 | TABLE
}

/// a copy of a table of this level, mapping from va, and of the tables
/// under it, with what holds some of lo..hi in 4 KB pages
unsafe fn copy(table: u64, level: u32, va: u64, lo: u64, hi: u64) -> u64 {
    let old = table as *const u64;
    let t = new_table();
    let size = span(level);
    for i in 0..512u64 {
        let e = old.add(i as usize).read();
        let eva = va + i * size;
        let n = if e & PRESENT == 0 || level == 1 {
            e
        } else if level <= 3 && e & LARGE != 0 {
            if eva < hi && eva.wrapping_add(size) > lo { split(e, level, eva, lo, hi) } else { e }
        } else {
            copy(e & ADDR, level - 1, eva, lo, hi) | (e & !ADDR)
        };
        t.add(i as usize).write(n);
    }
    t as u64
}

/// The machine's own tables, lo..hi in 4 KB pages, and the CPU on them.
/// False if they cannot be had.
pub unsafe fn own_tables(lo: u64, hi: u64) -> bool {
    let cr4 = cr(4);
    if cr4 & (1 << 12) != 0 {
        return false;
    }
    let cr3 = cr(3);
    let root = copy(cr3 & ADDR, 4, 0, lo, hi);
    asm!("mov cr3, {}", in(reg) root | (cr3 & !ADDR), options(nostack));
    // global pages too, as the firmware may have made them
    if cr4 & (1 << 7) != 0 {
        asm!("mov cr4, {}", "mov cr4, {}", in(reg) cr4 & !(1 << 7), in(reg) cr4, options(nostack));
    }
    ROOT.store(root, Ordering::Release);
    true
}

/// The 4 KB page at addr in the map or out of it, on the machine's own
/// tables; false if there are none, or it is not a 4 KB page of them.
/// Only this CPU's TLB is flushed: another that has the page in its own
/// sees the change when it next loses it.
pub fn set_present(addr: usize, present: bool) -> bool {
    let mut t = ROOT.load(Ordering::Acquire);
    if t == 0 {
        return false;
    }
    let va = addr as u64;
    unsafe {
        for level in (2..=4).rev() {
            let e = (t as *const u64).add(((va >> (12 + 9 * (level - 1))) & 511) as usize).read();
            if e & PRESENT == 0 || e & LARGE != 0 {
                return false;
            }
            t = e & ADDR;
        }
        let p = (t as *mut u64).add(((va >> 12) & 511) as usize);
        let e = p.read();
        p.write(if present { e | PRESENT } else { e & !PRESENT });
        asm!("invlpg [{}]", in(reg) addr, options(nostack));
    }
    true
}
