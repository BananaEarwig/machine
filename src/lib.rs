//! machine: a small bare-metal base for UEFI kernels, on x86_64 and AArch64.
//!
//! It takes a machine from the firmware and leaves the kernel built on it
//! the work that is its own. While the firmware is still there it gives a
//! heap of nearly all the memory, the files of the disk it was started
//! from, the time of day, a random seed and the screen (`boot`, `display`).
//! Then it leaves the firmware for good and runs the machine itself:
//!
//! * the console, a serial port, or the screen if there is none, as on a
//!   laptop (`screen`), and the clock, the CPU's counter (`cpu`);
//! * every CPU, each idle till it is given a job (`smp`), on page tables
//!   of the machine's own, so that stacks have guard pages (`stack`);
//! * interrupts only to wake an idle CPU, never in the middle of other code:
//!   the local APIC on x86_64, the GICv3 on AArch64, and sleeps on their
//!   timers; unless a kernel asks for a tick on a CPU (`cpu::start_tick`),
//!   a periodic interrupt whose handler runs in the middle of anything;
//! * PCI, by the ACPI MCFG (`pci`), and virtio's keyboard, tablet, disks
//!   network card and sound (`virtio_input`, `virtio_blk`, `virtio_net`,
//!   `virtio_snd`); the disks and the network card with interrupts that
//!   wake a CPU, where there is a way (`irq`);
//! * a RAM disk unpacked from a tar (`ramfs`).
//!
//! What is the CPU's own is in `x86_64/` and `aarch64/`, as `plat`. It was
//! written for ChrysaLisp on bare metal (ChrysaLisp-host's `bare/`) and for
//! Imago's kernel, and serves both.

#![no_std]

extern crate alloc;

#[macro_use]
pub mod cpu;

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
pub mod plat;
#[cfg(target_arch = "aarch64")]
#[path = "aarch64/mod.rs"]
pub mod plat;

pub mod acpi;
pub mod boot;
pub mod display;
pub mod irq;
pub mod pci;
pub mod ramfs;
pub mod screen;
pub mod smp;
pub mod stack;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_input;
pub mod virtio_net;
pub mod virtio_snd;

/// Functions code of the guest's own calls, a boot image's or a translated
/// program's, with the C convention of the platform that code is built
/// for: System V on x86_64, though the UEFI target's own C calls are Win64,
/// and AAPCS64 on AArch64, which are its own. Items that are not functions
/// pass through as they are.
#[macro_export]
macro_rules! guest {
    () => {};
    ($(#[$m:meta])* $v:vis unsafe fn $n:ident ($($a:tt)*) $(-> $r:ty)? $b:block $($rest:tt)*) => {
        #[cfg(target_arch = "x86_64")]
        $(#[$m])* $v unsafe extern "sysv64" fn $n($($a)*) $(-> $r)? $b
        #[cfg(target_arch = "aarch64")]
        $(#[$m])* $v unsafe extern "C" fn $n($($a)*) $(-> $r)? $b
        $crate::guest! { $($rest)* }
    };
    ($(#[$m:meta])* $v:vis fn $n:ident ($($a:tt)*) $(-> $r:ty)? $b:block $($rest:tt)*) => {
        #[cfg(target_arch = "x86_64")]
        $(#[$m])* $v extern "sysv64" fn $n($($a)*) $(-> $r)? $b
        #[cfg(target_arch = "aarch64")]
        $(#[$m])* $v extern "C" fn $n($($a)*) $(-> $r)? $b
        $crate::guest! { $($rest)* }
    };
    ($i:item $($rest:tt)*) => {
        $i
        $crate::guest! { $($rest)* }
    };
}

// The C library functions the compiler can call that compiler_builtins does
// not have. LLVM turns a loop that counts UTF-16 units to a zero into a call
// to wcslen, on a target whose wchar_t is 16 bits, UEFI's, and the uefi crate
// has such a loop. The reads are volatile so this loop is not turned into a
// call to itself.
#[no_mangle]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while core::ptr::read_volatile(s.add(n)) != 0 {
        n += 1;
    }
    n
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("\n[machine] panic: {}", info);
    cpu::exit(101)
}
