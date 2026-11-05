//! Stacks with a guard page: a page below each, out of the map, so that a
//! stack that overflows faults there, and the fault is reported as an
//! overflow, rather than writing over whatever is below it in the heap.
//!
//! The guard is the machine's own page tables' (`plat::paging`), which
//! hold the heap in 4 KB pages from the take-over on. A stack made before
//! then, or on a machine whose tables cannot be had, has none.

use crate::plat;
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE: usize = 4096;
/// the guard pages there are, so a fault in one can be named
const MAX_GUARDS: usize = 512;
static GUARDS: [AtomicUsize; MAX_GUARDS] = [const { AtomicUsize::new(0) }; MAX_GUARDS];

/// a stack, its guard page below it
pub struct Stack {
    base: *mut u8,
    total: usize,
    guarded: bool,
}

unsafe impl Send for Stack {}

impl Stack {
    /// A stack of at least size bytes, zeroed, its top 4 KB aligned, with a
    /// guard page below it if the machine's tables can have one.
    pub fn new(size: usize) -> Stack {
        Stack::try_new(size).expect("no memory for a stack")
    }

    /// The same, or None if there is no memory for it, or the size makes no
    /// sense: what a kernel wants for a stack a program asked for.
    pub fn try_new(size: usize) -> Option<Stack> {
        let total = size.checked_next_multiple_of(PAGE)?.checked_add(PAGE)?;
        let layout = Layout::from_size_align(total, PAGE).ok()?;
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            return None;
        }
        let guarded = plat::paging::set_present(base as usize, false);
        if guarded {
            // the first free entry takes it; with none free, it is a guard
            // all the same, only not named as one in a report
            let _ = GUARDS.iter().find(|g| g.compare_exchange(0, base as usize, Ordering::AcqRel, Ordering::Relaxed).is_ok());
        }
        Some(Stack { base, total, guarded })
    }

    /// the top, where the stack starts, 4 KB aligned
    pub fn top(&self) -> *mut u8 {
        unsafe { self.base.add(self.total) }
    }

    /// the lowest address of the stack that may be used, above the guard
    pub fn low(&self) -> *mut u8 {
        unsafe { self.base.add(PAGE) }
    }

    /// kept for good, its top
    pub fn leak(self) -> *mut u8 {
        let top = self.top();
        core::mem::forget(self);
        top
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        if self.guarded {
            plat::paging::set_present(self.base as usize, true);
            if let Some(g) = GUARDS.iter().find(|g| g.load(Ordering::Acquire) == self.base as usize) {
                g.store(0, Ordering::Release);
            }
        }
        unsafe { dealloc(self.base, Layout::from_size_align(self.total, PAGE).unwrap()) };
    }
}

/// is addr in a guard page, so that a fault there is a stack overflowing?
pub fn is_guard(addr: usize) -> bool {
    GUARDS.iter().any(|g| {
        let b = g.load(Ordering::Acquire);
        b != 0 && addr >= b && addr < b + PAGE
    })
}

/// the heap in 4 KB pages on tables of the machine's own, at take-over
pub(crate) fn init() {
    let (lo, hi) = crate::boot::heap_range();
    if lo == hi || !unsafe { plat::paging::own_tables(lo as u64, hi as u64) } {
        crate::kprintln!("[machine] the firmware's page tables, so stacks have no guard pages");
        return;
    }
    // nothing lives at 0: reading it is a null pointer, and faults
    plat::paging::set_present(0, false);
}

/// What a fault on the first page calls, in place of reporting a bug: a kernel
/// that runs programs ends the program, as a host does.
static NULL_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Where a fault goes on from: pc, with these in the first two argument registers,
/// and sp lower by a word, pc at it, the address that faulted there.
#[derive(Clone, Copy)]
pub struct Resume {
    pub pc: u64,
    pub a0: u64,
    pub a1: u64,
}

/// `hook(address, pc, sp)` for a fault on a null pointer, or a jump to one, from
/// now on: where to go on, or None to report it as a bug. The hook may end the run.
pub fn on_null(hook: fn(u64, u64, u64) -> Option<Resume>) {
    NULL_HOOK.store(hook as usize, Ordering::Release);
}

/// A fault at addr, from pc with this sp: the hook's answer, if it is a null
/// pointer's and there is one.
pub(crate) fn null_check(addr: u64, pc: u64, sp: u64) -> Option<Resume> {
    let h = NULL_HOOK.load(Ordering::Acquire);
    if h == 0 || addr >= PAGE as u64 {
        return None;
    }
    let f: fn(u64, u64, u64) -> Option<Resume> = unsafe { core::mem::transmute(h) };
    f(addr, pc, sp)
}
