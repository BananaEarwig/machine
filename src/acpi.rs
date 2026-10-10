//! Finding a table of the firmware's ACPI, through the RSDP it gives.

/// read a little endian field of a table, at any alignment
pub unsafe fn rd<T: Copy>(p: *const u8, off: usize) -> T {
    (p.add(off) as *const T).read_unaligned()
}

/// a table's length, from its header
pub unsafe fn len(t: *const u8) -> usize {
    rd::<u32>(t, 4) as usize
}

static RSDP_OEM: spin::Once<[u8; 6]> = spin::Once::new();

/// is the firmware's ACPI from this OEM, "BOCHS " is QEMU's ?
pub fn oem_is(oem: &[u8; 6]) -> bool {
    RSDP_OEM.get() == Some(oem)
}

/// The table with this signature, through the XSDT of an ACPI 2 RSDP, or
/// None.
pub unsafe fn find(rsdp: usize, sig: &[u8; 4]) -> Option<*const u8> {
    let rsdp = rsdp as *const u8;
    if rsdp.is_null() || core::slice::from_raw_parts(rsdp, 8) != b"RSD PTR " || *rsdp.add(15) < 2 {
        return None;
    }
    RSDP_OEM.call_once(|| rd::<[u8; 6]>(rsdp, 9));
    let xsdt = rd::<u64>(rsdp, 24) as *const u8;
    if xsdt.is_null() || core::slice::from_raw_parts(xsdt, 4) != b"XSDT" {
        return None;
    }
    let n = (len(xsdt).saturating_sub(36)) / 8;
    (0..n)
        .map(|i| rd::<u64>(xsdt, 36 + i * 8) as *const u8)
        .find(|&t| !t.is_null() && core::slice::from_raw_parts(t, 4) == sig)
}

/// the entries of the MADT, each its type and its bytes
pub unsafe fn madt_entries(madt: *const u8) -> impl Iterator<Item = (u8, &'static [u8])> {
    let total = len(madt);
    let mut at = 44;
    core::iter::from_fn(move || {
        if at + 2 > total {
            return None;
        }
        let kind = *madt.add(at);
        let elen = *madt.add(at + 1) as usize;
        if elen < 2 || at + elen > total {
            return None;
        }
        let e = core::slice::from_raw_parts(madt.add(at), elen);
        at += elen;
        Some((kind, e))
    })
}

/// a field of an entry, 0 if the entry is too short to have it
pub fn field<T: Copy + Default>(e: &[u8], off: usize) -> T {
    if off + core::mem::size_of::<T>() <= e.len() {
        unsafe { rd::<T>(e.as_ptr(), off) }
    } else {
        T::default()
    }
}
