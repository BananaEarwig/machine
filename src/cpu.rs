//! What every platform has, made of what its own `plat` module gives: the
//! console, the clock, random numbers and the way out.

use crate::plat;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

///////////////
// the console, the platform's serial port
///////////////

/// one CPU at a time on the serial port
static CONSOLE: spin::Mutex<()> = spin::Mutex::new(());

/// what a terminal is sent, a line end is a return as well, as a tty does
pub fn console_write(bytes: &[u8]) {
    let _g = CONSOLE.lock();
    for &b in bytes {
        if b == b'\n' {
            plat::serial_put(b'\r');
        }
        plat::serial_put(b);
    }
}

/// a byte typed, if there is one, never waits
pub fn serial_get() -> Option<u8> {
    plat::serial_get()
}

pub struct Console;

/// what writes under the lock already held
struct Locked;

impl fmt::Write for Locked {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                plat::serial_put(b'\r');
            }
            plat::serial_put(b);
        }
        Ok(())
    }
}

/// a whole message, its line end with it, with no other CPU's in it
pub fn console_print(args: fmt::Arguments) {
    use fmt::Write;
    let _g = CONSOLE.lock();
    let _ = Locked.write_fmt(args);
    let _ = Locked.write_str("\n");
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        console_write(s.as_bytes());
        Ok(())
    }
}

#[macro_export]
macro_rules! kprintln {
    ($($t:tt)*) => {{
        $crate::cpu::console_print(format_args!($($t)*));
    }};
}

///////////////
// the clock, the platform's counter, which runs at the same rate on every
// CPU, the TSC or the generic timer's
///////////////

static COUNTER_HZ: AtomicU64 = AtomicU64::new(1_000_000_000);
static COUNTER_AT_BOOT: AtomicU64 = AtomicU64::new(0);
static US_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// the counter's rate, and the time of day it starts from, in microseconds
/// since 1970
pub fn clock_init(hz: u64, at: u64, unix_us: u64) {
    COUNTER_HZ.store(hz.max(1), Ordering::Relaxed);
    COUNTER_AT_BOOT.store(at, Ordering::Relaxed);
    US_AT_BOOT.store(unix_us, Ordering::Relaxed);
}

pub fn counter_hz() -> u64 {
    COUNTER_HZ.load(Ordering::Relaxed)
}

/// microseconds since 1970
pub fn now_us() -> u64 {
    let ticks = plat::counter().wrapping_sub(COUNTER_AT_BOOT.load(Ordering::Relaxed));
    US_AT_BOOT.load(Ordering::Relaxed) + (ticks as u128 * 1_000_000 / counter_hz() as u128) as u64
}

static TIMER: AtomicBool = AtomicBool::new(false);

/// the timer interrupt is set up, a sleep can halt
pub fn use_timer() {
    TIMER.store(true, Ordering::Release);
}

/// wait, spinning, for the short waits of starting the CPUs
pub fn spin_us(us: u64) {
    let end = now_us() + us;
    while now_us() < end {
        core::hint::spin_loop();
    }
}

/// wait, halted till the timer goes off, the CPU is idle
pub fn sleep_us(us: u64) {
    if TIMER.load(Ordering::Acquire) {
        plat::sleep_us(us);
    } else {
        spin_us(us.max(1));
    }
}

/// Sleep at most this long, or till another CPU wakes this one: for a
/// wait that is to end at a wake as well as at a time. Without the timer,
/// a short spin.
pub fn nap_us(us: u64) {
    if TIMER.load(Ordering::Acquire) {
        plat::nap_us(us);
    } else {
        spin_us(us.clamp(1, 1000));
    }
}

///////////////
// the tick: a periodic interrupt on a CPU, for a kernel that wants code
// run in the middle of whatever that CPU runs
///////////////

static TICK: AtomicU64 = AtomicU64::new(0);

/// Take an interrupt every `period_us` on this CPU, from now on, and call
/// `handler` in it: interrupts are on from here, on this CPU only, and it
/// sleeps till the next tick or a wake, not on a timer of its own. The
/// handler runs in the middle of anything this CPU does, kernel and guest
/// code alike, on a stack of its own on x86_64 and on the one it finds on
/// AArch64: it must not allocate, nor wait for a lock the code it
/// interrupted may hold; `try_lock`, and skip a tick, instead. One handler
/// for every CPU that ticks.
pub fn start_tick(period_us: u64, handler: fn()) {
    TICK.store(handler as usize as u64, Ordering::Release);
    crate::smp::CPUS[crate::smp::this_cpu()].ticking.store(true, Ordering::Release);
    plat::start_tick(period_us.max(10));
}

/// does this CPU take the tick ?
pub fn ticking() -> bool {
    crate::smp::CPUS[crate::smp::this_cpu()].ticking.load(Ordering::Acquire)
}

/// the platform's interrupt, at a tick: the kernel's handler
pub(crate) fn tick() {
    let h = TICK.load(Ordering::Acquire);
    if h != 0 {
        let f: fn() = unsafe { core::mem::transmute(h as usize) };
        f();
    }
}

/// wake CPU n, if it naps or idles
pub fn wake(n: usize) {
    if n < crate::smp::cpus() {
        plat::wake(crate::smp::CPUS[n].hw_id.load(Ordering::Relaxed));
    }
}

/// days since 1970 of a date, Howard Hinnant's days_from_civil
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

///////////////
// random bytes
///////////////

static XORSHIFT: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

/// what the firmware gave to start the stirred numbers from, if anything
pub fn seed(s: u64) {
    XORSHIFT.fetch_xor(s, Ordering::Relaxed);
}

pub fn random_u64() -> u64 {
    if let Some(v) = plat::hw_random() {
        return v;
    }
    // no random number instruction, the seed and the counter stirred
    let mut x = XORSHIFT.load(Ordering::Relaxed) ^ plat::counter();
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    XORSHIFT.store(x, Ordering::Relaxed);
    x
}

///////////////
// the way out
///////////////

pub fn exit(code: i32) -> ! {
    plat::exit(code)
}
