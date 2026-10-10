//! More than one CPU: work for each.
//!
//! The platform finds the CPUs and starts them, each into `cpu_main`, where
//! it waits, idle, for a job. `spawn` gives a free CPU a job, a closure,
//! and wakes it; when the job returns the CPU waits again. A job that runs
//! code of its own on a stack of its own can be left from anywhere in that
//! code with `unwind`, back to where `call_on` was called.

use crate::plat;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 64;

pub type Job = Box<dyn FnOnce() + Send>;

pub struct Cpu {
    /// the platform's name for it, its local APIC id or its MPIDR
    pub hw_id: AtomicU64,
    pub online: AtomicBool,
    /// a job is running, or about to
    pub running: AtomicBool,
    job: spin::Mutex<Option<Job>>,
    /// where unwind goes back to, the stack call_on left
    pub unwind_sp: AtomicU64,
    /// it takes the tick, `cpu::start_tick`, with interrupts on
    pub ticking: AtomicBool,
}

impl Cpu {
    const fn new() -> Self {
        Cpu {
            hw_id: AtomicU64::new(0),
            online: AtomicBool::new(false),
            running: AtomicBool::new(false),
            job: spin::Mutex::new(None),
            unwind_sp: AtomicU64::new(0),
            ticking: AtomicBool::new(false),
        }
    }
}

pub static CPUS: [Cpu; MAX_CPUS] = [const { Cpu::new() }; MAX_CPUS];
static NCPUS: AtomicUsize = AtomicUsize::new(1);

/// how many CPUs there are places for, numbered from 0; each that is
/// running, jobs or waiting for one, is `online`
pub fn cpus() -> usize {
    NCPUS.load(Ordering::Relaxed)
}

/// how many CPUs are running, jobs or waiting for one
pub fn online() -> usize {
    (0..cpus()).filter(|&n| CPUS[n].online.load(Ordering::Acquire)).count()
}

pub fn set_cpus(n: usize) {
    NCPUS.store(n, Ordering::Release);
}

/// the number of the CPU this runs on, 0 for the boot one
pub fn this_cpu() -> usize {
    plat::this_cpu()
}

/// the boot CPU, running the kernel's own work
pub fn boot_cpu(hw_id: u64) {
    CPUS[0].hw_id.store(hw_id, Ordering::Relaxed);
    CPUS[0].online.store(true, Ordering::Relaxed);
    CPUS[0].running.store(true, Ordering::Relaxed);
}

/// where a CPU other than the boot one goes once the platform has it
/// going: it waits for a job, runs it, and waits again
pub fn cpu_main(n: usize) -> ! {
    CPUS[n].online.store(true, Ordering::Release);
    loop {
        let job = CPUS[n].job.lock().take();
        match job {
            Some(job) => {
                job();
                CPUS[n].running.store(false, Ordering::Release);
            }
            None => plat::idle(),
        }
    }
}

static SPAWN: spin::Mutex<()> = spin::Mutex::new(());

/// Run a job on a CPU that has none. The CPU's number, or None if every
/// CPU is busy.
pub fn spawn(job: Job) -> Option<usize> {
    let _g = SPAWN.lock();
    for n in 1..cpus() {
        let c = &CPUS[n];
        if c.online.load(Ordering::Acquire) && !c.running.load(Ordering::Acquire) {
            c.running.store(true, Ordering::Release);
            *c.job.lock() = Some(job);
            plat::wake(c.hw_id.load(Ordering::Relaxed));
            return Some(n);
        }
    }
    None
}

/// is CPU n running a job ?
pub fn busy(n: usize) -> bool {
    n < cpus() && CPUS[n].running.load(Ordering::Acquire)
}

/// Call `f(a0, a1)`, a function with the guest's convention (see `guest!`),
/// on the stack whose top is `stack_top`, from this CPU. It returns what f
/// returns, or what `unwind` is given.
pub unsafe fn call_on(stack_top: *mut u8, f: usize, a0: usize, a1: usize) -> i64 {
    plat::call_node(stack_top, f, a0, a1, CPUS[this_cpu()].unwind_sp.as_ptr())
}

/// Leave what `call_on` called on this CPU, from anywhere in it, as if it
/// had returned `code`.
pub fn unwind(code: i64) -> ! {
    let saved = CPUS[this_cpu()].unwind_sp.load(Ordering::Acquire);
    unsafe { plat::exit_node(saved, code) }
}
