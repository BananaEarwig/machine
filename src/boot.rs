//! Taking the machine from the firmware: what is done while it is still
//! there, the heap, the files, the time, and then leaving it.
//!
//! A kernel's `efi_main` calls, in order: `start`, `heap`, `read` for its
//! files, `prepare`, `display::init` if it wants the screen, and `take`.

use crate::{cpu, plat};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::{CString16, Status};

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);
static HEAP_BASE: AtomicUsize = AtomicUsize::new(0);
static RSDP: AtomicUsize = AtomicUsize::new(0);

/// what is free of the heap
pub fn heap_free() -> usize {
    HEAP.lock().free()
}

/// the heap's size
pub fn heap_size() -> usize {
    HEAP_SIZE.load(Ordering::Relaxed)
}

/// where the heap is, its first byte and the one after its last
pub(crate) fn heap_range() -> (usize, usize) {
    let b = HEAP_BASE.load(Ordering::Relaxed);
    (b, b + HEAP_SIZE.load(Ordering::Relaxed))
}

/// the firmware's ACPI 2 RSDP, 0 if it has none
pub fn rsdp() -> usize {
    RSDP.load(Ordering::Relaxed)
}

/// The console, the serial port the firmware's ACPI names, and a line to
/// say so. Gives the RSDP.
pub fn start(name: &str) -> usize {
    let rsdp = uefi::system::with_config_table(|t| {
        t.iter().find(|e| e.guid == uefi::table::cfg::ACPI2_GUID).map(|e| e.address as usize).unwrap_or(0)
    });
    RSDP.store(rsdp, Ordering::Relaxed);
    let serial = plat::console_init(rsdp);
    crate::screen::init(serial);
    kprintln!("[{}] booting, {}", name, plat::NAME);
    rsdp
}

/// the largest piece of free memory, as the firmware has it
fn largest_free() -> Option<(u64, u64)> {
    let map = boot::memory_map(MemoryType::LOADER_DATA).ok()?;
    map.entries()
        .filter(|d| d.ty == MemoryType::CONVENTIONAL)
        .map(|d| (d.phys_start, d.page_count))
        .max_by_key(|&(_, pages)| pages)
}

/// The heap: the largest piece of free memory less `margin` bytes, which
/// the firmware keeps to read the disk with. It is allocated as loader
/// code, which the firmware leaves executable, so code can be loaded or
/// written into any of it.
pub fn heap(margin: u64) -> Result<usize, Status> {
    let Some((_, pages)) = largest_free() else {
        kprintln!("[machine] no memory map");
        return Err(Status::LOAD_ERROR);
    };
    let heap_pages = (pages.saturating_sub(margin / 4096)) as usize;
    let heap = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_CODE, heap_pages).map_err(|e| {
        kprintln!("[machine] no heap: {:?}", e);
        Status::OUT_OF_RESOURCES
    })?;
    unsafe { HEAP.lock().init(heap.as_ptr(), heap_pages * 4096) };
    HEAP_SIZE.store(heap_pages * 4096, Ordering::Relaxed);
    HEAP_BASE.store(heap.as_ptr() as usize, Ordering::Relaxed);
    kprintln!("[machine] heap {} MB at {:p}", (heap_pages * 4096) >> 20, heap.as_ptr());
    Ok(heap_pages * 4096)
}

/// A file of the disk this was started from, by its path there, as
/// `\\chrysalisp.tar`. None if there is no such file.
pub fn read(path: &str) -> Option<Vec<u8>> {
    let sfs = boot::get_image_file_system(boot::image_handle()).ok()?;
    let mut disk = uefi::fs::FileSystem::new(sfs);
    let p = CString16::try_from(path).ok()?;
    disk.read(p.as_ref()).ok()
}

/// the time of day from the firmware's clock, in microseconds since 1970
fn firmware_time_us() -> u64 {
    match uefi::runtime::get_time() {
        Ok(t) => {
            let days = cpu::days_from_civil(t.year() as i64, t.month() as i64, t.day() as i64);
            let mut secs = days * 86400 + t.hour() as i64 * 3600 + t.minute() as i64 * 60 + t.second() as i64;
            // UEFI: local time = UTC - time_zone (minutes). With none said,
            // the clock is taken as UTC, as QEMU's and most servers' are; a
            // clock kept in local time is out by its zone till it is set
            // (imago's time-sync, or a linked node's time).
            match t.time_zone() {
                Some(tz) => {
                    secs += tz as i64 * 60;
                    kprintln!("[machine] the firmware's clock, {} minutes from UTC", -(tz as i64));
                }
                None => kprintln!("[machine] the firmware's clock, its zone not said: taken as UTC"),
            }
            (secs.max(0) as u64) * 1_000_000 + (t.nanosecond() / 1000) as u64
        }
        Err(_) => {
            kprintln!("[machine] no clock from the firmware: the time starts at 1970");
            0
        }
    }
}

/// a seed for the random numbers, from the firmware's generator if it has one
fn firmware_seed() -> u64 {
    use uefi::proto::rng::Rng;
    let mut buf = [0u8; 8];
    let ok = boot::get_handle_for_protocol::<Rng>()
        .and_then(|h| boot::open_protocol_exclusive::<Rng>(h))
        .and_then(|mut rng| rng.get_rng(None, &mut buf))
        .is_ok();
    if ok { u64::from_le_bytes(buf) } else { 0 }
}

/// The time of day, a seed, and what the platform wants of the firmware
/// while it is here: the clock's rate, the CPUs, the interrupt controller.
pub fn prepare() -> plat::Firmware {
    let unix_us = firmware_time_us();
    cpu::seed(firmware_seed());
    plat::firmware(rsdp(), unix_us)
}

/// Leave the firmware, for good, and take the machine: interrupts, timers,
/// and every CPU, each waiting for a job (`smp::spawn`). Gives how many
/// CPUs run.
pub unsafe fn take(fw: plat::Firmware) -> usize {
    kprintln!("[machine] leaving the firmware");
    crate::screen::leaving_firmware();
    let _map = boot::exit_boot_services(None);
    let ncpus = plat::take_machine(fw);
    kprintln!("[machine] {} CPU{}, {}", ncpus, if ncpus == 1 { "" } else { "s" }, plat::describe());
    ncpus
}
