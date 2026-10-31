# Firecracker: what supporting it would take

A look at running machine's kernels (Imago's, and perhaps ChrysaLisp's)
as Firecracker microVMs, written on 31 October 2026. Nothing here is
decided or built: it is what was found, so that it is not lost.

## What Firecracker gives a guest

* **No firmware.** It loads a kernel straight into memory: on x86_64 an
  ELF, entered with a boot information structure (the PVH entry or
  Linux's boot protocol); on AArch64 a Linux-style image, entered with
  a device tree in x0. There is no UEFI and no BIOS.
* **Devices on virtio-mmio** by default. PCI came in v1.13, optional
  (`--enable-pci`), and with it every virtio device is on the PCI
  transport instead; v1.16 adds hotplug of block, pmem and net on PCI as
  a preview, and an `rng-seed` node in the AArch64 device tree.
* **Devices:** a serial port, virtio block, net, vsock, balloon, pmem
  and an entropy device. No screen, keyboard, tablet or sound.
* **Interrupts:** on x86_64 the local APIC and an I/O APIC, the MMIO
  devices on legacy lines (MSI-X with PCI); on AArch64 a GIC, the
  devices on SPIs.
* **Needs Linux with KVM** on the host.

## What machine would need

1. **A second boot path.** `boot` is built on the `uefi` crate: the
   heap, the boot disk's files, the time, the random seed and the screen
   all come from the firmware's services. Without firmware, machine takes
   the machine at the kernel's entry and makes those itself:
   * on x86_64, its own page tables and the switch to long mode (the PVH
     entry starts in 32-bit protected mode);
   * the memory map, from the boot information (x86_64) or the device
     tree (AArch64), for the heap;
   * the tar as an initrd, not a file on the boot disk: `ramfs` takes it
     as it is.
   The crate builds for `x86_64-unknown-none` and `aarch64-unknown-none`
   as well as the UEFI targets, chosen by a feature.
2. **A device tree reader** for AArch64: memory, the UART, the GIC, the
   timer and each virtio device are found there.
3. **virtio-mmio** beside virtio-pci. The queues and the block, net and
   rng drivers stay as they are; the transport (its registers, and
   finding the devices, from the device tree or, on x86_64, the kernel's
   command line) is new. Or Firecracker's PCI mode, which reuses machine's
   transport, if it gives the guest a way to find PCI's configuration
   space (an MCFG, or a device tree node): to check before choosing.
4. **Interrupts for MMIO devices.** On x86_64 these are legacy lines
   through an I/O APIC, which machine does not program today (it uses the
   local APIC and MSI-X only). On AArch64 they are SPIs, which `irq`
   already handles for QEMU's PCI line.
5. **The console.** On x86_64, COM1 as now. On AArch64 an 8250-style
   UART on MMIO, not a PL011: a small new driver.
6. **CPUs and the way out.** On x86_64 the other CPUs started from
   whatever table Firecracker gives (ACPI or an MP table: to check), and
   the way out a reset through the keyboard controller, not ACPI. On
   AArch64, PSCI for both, as now.
7. **Time and randomness.** The time from the network (Imago already has
   NTP and its peers' clocks); random numbers from RDRAND, virtio-rng or
   AArch64's `rng-seed`, keying the ChaCha20 stream as the firmware's
   seed does now.
8. **The UEFI-only modules optional**: `display`, `screen`,
   `virtio_input` and `virtio_snd`, and each kernel coping without them.

## Testing it

Firecracker needs Linux and KVM: not the Mac, nor the cloud workspace.
The Raspberry Pi 4, running Linux, has KVM (AArch64).

A stepping stone: QEMU's `microvm` machine on x86_64 is modelled on
Firecracker (a kernel loaded directly, virtio-mmio, no firmware), so
most of the new code, the boot path, virtio-mmio and the I/O APIC, can
be tried on the Mac's QEMU before Firecracker itself on the Pi.

## A possible order

A stack of six to eight pull requests:

1. a boot with no firmware, on QEMU's `microvm`;
2. virtio-mmio;
3. the I/O APIC;
4. AArch64's device tree and its 8250-style UART;
5. the UEFI-only modules made optional;
6. Firecracker itself, on the Pi; then its snapshots.

Each kernel then needs its own build and boot glue.

## Questions to settle first

* **The goal.** Fast-starting Imago nodes (a farm of microVMs, or nodes
  started from inside as `nodes -a` does) would favour x86_64 and
  snapshots; running on the Pi would favour AArch64.
* **Which kernels.** Imago's only, or ChrysaLisp's bare metal too.
* **PCI or MMIO** on Firecracker (point 3).

## Sources

* [Firecracker v1.13.0 release notes](https://newreleases.io/project/github/firecracker-microvm/firecracker/release/v1.13.0): PCI support, optional.
* [Firecracker v1.16.0 release notes](https://newreleases.io/project/github/firecracker-microvm/firecracker/release/v1.16.0): PCI hotplug preview, AArch64's `rng-seed`.
