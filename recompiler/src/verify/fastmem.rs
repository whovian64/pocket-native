//! Sparse, private guest address spaces for differential verification.
//! Both sides use the same zero-filled, writable unmapped-memory semantics
//! as the runtime's fastmem view. No signal handler or shared global state.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::ptr::null_mut;

const PAGE: usize = 4096;
// The extra page catches a native access crossing the final guest page.
const SPAN: usize = (1usize << 32) + PAGE;
const PAGEMAP_BATCH: usize = 4096;

/// /proc/pid/pagemap exposes present at bit 63 and swapped at bit 62.
/// PFNs and swap offsets are deliberately ignored, including masked PFNs.
fn page_flags(entry: u64) -> u8 {
    (entry >> 62) as u8
}

fn read_page_flags(pagemap: &File, first_page: u64, flags: &mut [u8]) -> io::Result<()> {
    let mut offset = first_page.checked_mul(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pagemap offset overflow"))?;
    let mut entries = [0u8; PAGEMAP_BATCH * 8];
    for batch in flags.chunks_mut(PAGEMAP_BATCH) {
        let bytes = batch.len() * 8;
        pagemap.read_exact_at(&mut entries[..bytes], offset)?;
        for (flag, entry) in batch.iter_mut().zip(entries[..bytes].chunks_exact(8)) {
            // The kernel copies its u64 entries directly, in host byte order.
            *flag = page_flags(u64::from_ne_bytes(entry.try_into().unwrap()));
        }
        offset += bytes as u64;
    }
    Ok(())
}

pub(super) struct View {
    base: *mut u8,
    pagemap: File,
}

impl View {
    pub(super) fn new() -> Result<Self, String> {
        // Pagemap entries describe host pages, while the verifier compares
        // guest pages. Reject a different granularity instead of omitting bytes.
        if unsafe { libc::sysconf(libc::_SC_PAGESIZE) } != PAGE as libc::c_long {
            return Err("verifier fastmem requires 4 KiB host pages".to_owned());
        }
        let pagemap = File::open("/proc/self/pagemap")
            .map_err(|error| format!("verifier fastmem requires readable /proc/self/pagemap: {error}"))?;
        // SAFETY: an anonymous mapping, checked before use, owned by View.
        let base = unsafe {
            libc::mmap(null_mut(), SPAN, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
        };
        if base == libc::MAP_FAILED {
            return Err(format!("cannot reserve verifier fastmem: {}", std::io::Error::last_os_error()));
        }
        // A single random guest write must not allocate a 2 MiB transparent
        // huge page. The verifier intentionally touches sparse 4 KiB pages.
        if unsafe { libc::madvise(base, SPAN, libc::MADV_NOHUGEPAGE) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::munmap(base, SPAN); }
            return Err(format!("cannot keep verifier fastmem sparse: {error}"));
        }
        let mut view = Self { base: base.cast(), pagemap };
        // Verify that this process can read materialization flags. A kernel
        // may mask PFNs without masking these flags; no PFN access is needed.
        // Discard the probe again so admission leaves the guest view sparse.
        view.initialize(0, &[1]);
        let mut probe = [0];
        read_page_flags(&view.pagemap, view.base as usize as u64 / PAGE as u64, &mut probe)
            .map_err(|error| format!("cannot inspect verifier fastmem pagemap: {error}"))?;
        if probe[0] == 0 {
            return Err("verifier fastmem requires present/swapped pagemap flags".to_owned());
        }
        view.reset();
        Ok(view)
    }

    pub(super) fn base(&self) -> *mut u8 { self.base }

    pub(super) fn reset(&mut self) {
        // SAFETY: this view owns the complete anonymous mapping. Discarding
        // it restores zero pages, including unknown guest writes, lazily.
        let result = unsafe { libc::madvise(self.base.cast(), SPAN, libc::MADV_DONTNEED) };
        assert_eq!(result, 0, "cannot reset verifier fastmem: {}", std::io::Error::last_os_error());
    }

    pub(super) fn initialize(&mut self, address: u32, bytes: &[u8]) {
        assert!((address as usize).checked_add(bytes.len()).is_some_and(|end| end <= SPAN));
        // SAFETY: the range was checked and Vec-backed source is disjoint.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(address as usize), bytes.len()); }
    }

    fn materialized_pages(&self) -> io::Result<Vec<u8>> {
        let mut flags = vec![0; SPAN / PAGE];
        let first_page = self.base as usize as u64 / PAGE as u64;
        read_page_flags(&self.pagemap, first_page, &mut flags)?;
        Ok(flags)
    }

    /// Compare every page either side touched, including addresses outside
    /// declared regions, even if both pages have been swapped out. Only
    /// pages with neither present nor swapped flags can be skipped as zero.
    /// The private views are not mutated concurrently while being compared.
    pub(super) fn first_difference(&self, other: &Self) -> Option<u64> {
        // A pagemap failure must abort verification, never turn incomplete
        // coverage into a pass or fall back to resident-only mincore.
        let ours = self.materialized_pages().expect("cannot inspect verifier fastmem pagemap");
        let theirs = other.materialized_pages().expect("cannot inspect verifier fastmem pagemap");
        for (page, (&a, &b)) in ours.iter().zip(&theirs).enumerate() {
            if (a | b) == 0 { continue; }
            let offset = page * PAGE;
            // SAFETY: a complete page within each live mapping.
            let a = unsafe { std::slice::from_raw_parts(self.base.add(offset), PAGE) };
            let b = unsafe { std::slice::from_raw_parts(other.base.add(offset), PAGE) };
            let mut i = 0;
            while i < PAGE {
                if a[i] == b[i] { i += 1; continue; }
                let word = i & !3;
                let double = i & !7;
                if super::nans(a, b, double, 8) { i = double + 8; }
                else if super::nans(a, b, word, 4) { i = word + 4; }
                else { return Some((offset + i) as u64); }
            }
        }
        None
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: this view owns the complete reservation.
        unsafe { libc::munmap(self.base.cast(), SPAN); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagemap_flags_include_present_swapped_and_ignore_other_bits() {
        let present = 1u64 << 63;
        let swapped = 1u64 << 62;
        let other = (1u64 << 62) - 1;
        for (entry, expected) in [
            (0, 0), (other, 0), (present, 2), (swapped, 1),
            (present | other, 2), (swapped | other, 1),
            (present | swapped | other, 3),
        ] {
            // Includes unprivileged PFN-zero present pages and swapped
            // entries with swap-offset bits: neither may be omitted.
            let encoded = entry.to_ne_bytes();
            assert_eq!(page_flags(u64::from_ne_bytes(encoded)), expected);
        }
        for (a, b, compare) in [(0, 0, false), (present, 0, true),
                               (0, swapped, true), (swapped, swapped, true),
                               (present, swapped, true), (other, other, false)] {
            assert_eq!((page_flags(a) | page_flags(b)) != 0, compare);
        }
    }

    #[test]
    fn short_pagemap_reads_are_errors() {
        let empty = File::open("/dev/null").unwrap();
        assert_eq!(read_page_flags(&empty, 0, &mut [0]).unwrap_err().kind(),
                   io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn reservations_are_sparse_and_discard_restores_zero_pages() {
        let mut view = View::new().unwrap();
        assert_eq!(view.materialized_pages().unwrap().iter().filter(|p| **p != 0).count(), 0);
        view.initialize(0x1234_5000, &[1, 2, 3, 4]);
        view.initialize(0xF000_0000, &[5, 6, 7, 8]);
        assert_eq!(view.materialized_pages().unwrap().iter().filter(|p| **p != 0).count(), 2);
        view.reset();
        assert_eq!(view.materialized_pages().unwrap().iter().filter(|p| **p != 0).count(), 0);
        assert_eq!(unsafe { *view.base().add(0x1234_5000) }, 0);
        assert_eq!(unsafe { *view.base().add(0xF000_0000) }, 0);
    }
}
