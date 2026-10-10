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
        let total = size.next_multiple_of(PAGE) + PAGE;
        let layout = Layout::from_size_align(total, PAGE).unwrap();
        let base = unsafe { alloc_zeroed(layout) };
        assert!(!base.is_null(), "no memory for a stack");
        let guarded = plat::paging::set_present(base as usize, false);
        if guarded {
            // the first free entry takes it; with none free, it is a guard
            // all the same, only not named as one in a report
            let _ = GUARDS.iter().find(|g| g.compare_exchange(0, base as usize, Ordering::AcqRel, Ordering::Relaxed).is_ok());
        }
        Stack { base, total, guarded }
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
    }
}
