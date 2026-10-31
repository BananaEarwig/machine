# machine

A small bare-metal base for UEFI kernels, on x86_64 and AArch64, in Rust,
`no_std`. It takes a machine from the firmware and runs it, and leaves the
kernel built on it only the work that is its own.

It is shared by two kernels:

* **ChrysaLisp on bare metal**, `bare/` of
  [ChrysaLisp-host](https://github.com/BananaEarwig/ChrysaLisp-host): a
  ChrysaLisp node on each CPU, its desktop on the screen, its exFAT on a
  disk.
* **Imago's kernel**, `bare/` of
  [imago](https://github.com/BananaEarwig/imago): one Imago program as
  machine code, its threads on every CPU, its links over the network.

Both take it as a git submodule, and as a path dependency:

```toml
[dependencies]
machine = { path = "machine" }
```

## What it gives

| Module | |
| --- | --- |
| `boot` | the steps from the firmware to the machine: the heap, the boot disk's files, the time, a random seed, then leaving the firmware |
| `cpu` | the console, a serial port, and the screen too when there is none; the clock, the CPU's counter; sleeping; random numbers; the way out |
| `stack` | stacks with a guard page below, out of the map, on page tables of the machine's own, the heap in 4 KB pages; an overflow is reported as one |
| `smp` | every CPU, each idle till `spawn` gives it a job; `call_on` and `unwind`, to run code on a stack of its own and leave it from anywhere in it |
| `screen` | the console on the screen, for a machine with no serial port, a laptop: the firmware's text while it is there, then the framebuffer, in an 8x8 font, with the ANSI a TUI sends |
| `display` | the screen, the firmware's GOP framebuffer in the mode asked for, or QEMU's ramfb at any size on AArch64 |
| `pci` | PCI, by the ACPI MCFG |
| `virtio`, `virtio_input`, `virtio_blk`, `virtio_net` | virtio 1.0 on PCI: the keyboard and tablet, as Linux input events, polled; disks, by their serial, read and written at any offset, a request waited for asleep till the disk's interrupt; the network card, Ethernet frames in and out, with an interrupt for the kernel to route to a CPU that waits |
| `virtio_snd` | sound, QEMU's virtio-sound-pci: one 16-bit stereo stream at 44,100 frames a second, from a ring of buffers the kernel's mixer fills, polled |
| `irq` | interrupts from devices, that only wake a CPU: an MSI-X message on x86_64, the PCI line on QEMU's AArch64 virt machine, routed to one CPU at a time; a device with neither is polled |
| `ramfs` | a RAM disk unpacked from a ustar tar, with mtimes and modes; packed back whole, or just what changed since a point, as an overlay to put back over it |
| `chacha` | ChaCha20's block function, for `cpu`'s random bytes where the CPU has no instruction for them: a stream keyed from the firmware's seed |
| `acpi` | finding the firmware's tables |
| `plat` | what is the CPU's own: `x86_64/` (COM1, the PS/2 keyboard in any of pc-keyboard's layouts, the page tables, the TSC, the GDT and IDT, the local APIC and its timer, INIT-SIPI-SIPI) and `aarch64/` (the PL011, the page tables, the generic timer, the GICv3, PSCI, the exception vectors) |

`guest!` declares functions with the C convention of the code a kernel
runs, which is not UEFI's own on x86_64: System V there, AAPCS64 on
AArch64.

No interrupt handler runs in the middle of other code, unless the kernel
asks. On x86_64 a CPU takes interrupts only in a `sti; hlt` window; on
AArch64 it waits in `wfi` with them masked, which a pending interrupt
still wakes it from. Either way an idle CPU is idle, and the code it runs
need not know about interrupts at all, its red zone included.

A device's interrupt is no different (`irq`): it wakes the CPU it is
routed to, at once if that one sleeps, at its next sleep if not, and the
driver then looks at its queues. On x86_64 it is an MSI-X message, to the
local APIC of that CPU. On AArch64, where QEMU's virt machine has no MSIs
without an ITS, it is the device's PCI line, an SPI of the GIC routed to
that CPU and taken on its rising edge; the device holds the line up till
its ISR is read, which `Irq::rearm` does before the driver looks, so its
next news raises it again. The disk routes its interrupt to the CPU that
asks of it, and sleeps till the answer; the network card's is the
kernel's to route, to the CPU that waits for the network.

A kernel that wants code run in the middle of whatever a CPU runs, to
preempt it say, asks for the tick there: `cpu::start_tick(period_us,
handler)`. That CPU then has interrupts on, and its timer is the tick's;
it sleeps till the next tick or a wake. The handler must not allocate, nor
wait for a lock the code it stopped may hold. On x86_64 every interrupt is
taken on a stack of each CPU's own, its TSS's first IST stack, never on
the stack of the code it stops; on AArch64 on the stack it finds, with
every register, the SIMD ones too, saved.

## Using it

```rust
#[entry]
fn main() -> Status {
    let rsdp = machine::boot::start("mykernel");          // the console, "booting"
    machine::boot::heap(96 << 20).unwrap();                // nearly all the memory
    let files = machine::boot::read("\\files.tar");        // from the boot disk
    let fw = machine::boot::prepare();                     // the clock, the CPUs
    machine::display::init(None);                          // the screen, if wanted
    unsafe { machine::boot::take(fw) };                    // no more firmware
    let funcs = machine::pci::scan(rsdp);
    machine::virtio_blk::init(&funcs);
    machine::smp::spawn(Box::new(|| { /* on another CPU */ }));
    // ...
}
```

The kernel builds for `x86_64-unknown-uefi` or `aarch64-unknown-uefi`,
with `core` and `alloc` built for the target:

```
rustup component add rust-src
RUSTC_BOOTSTRAP=1 cargo build --release --target aarch64-unknown-uefi
```

## Running it

Under QEMU: q35 with OVMF for x86_64; `virt,gic-version=3` with AAVMF or
Homebrew's `edk2-aarch64-code.fd` for AArch64, under the Hypervisor
framework on an Apple Silicon Mac. ChrysaLisp-host's `bare/run-qemu.sh`
is a runner for both. It has not yet run on a real machine.

## Testing it

The modules with no hardware in them, `chacha` and `ramfs`, are tested
on the host, by a crate of their own that includes them as they are
(`host-tests/`): ChaCha20 against RFC 8439's vector, and the RAM disk's
files, seeks, tars and overlays.

```
cd host-tests && cargo test
```

The rest is tried by the kernels built on it, under QEMU.

## Licence

MIT, see `LICENSE`.
