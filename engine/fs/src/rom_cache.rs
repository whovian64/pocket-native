//! Opt-in retirement of old ROM chunks and successfully copied code. The LRU
//! bounds tracked chunks, not all file-cache residency: advice is best effort,
//! and kernel read-ahead or other readers may populate additional pages.

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::AsRawFd;

use memmap2::{Mmap, UncheckedAdvice};
use crate::RomCacheStats;

const CHUNK: usize = 1024 * 1024;

struct Chunk {
    offset: usize,
}

pub(super) struct RomCache {
    // This is the original File used to create the mmap, not a reopened fd.
    file: File,
    page: usize,
    chunks: VecDeque<Chunk>,
    stats: RomCacheStats,
}

impl RomCache {
    pub(super) fn new(file: File, mib: usize) -> Result<Self, &'static str> {
        if !(1..=1024).contains(&mib) { return Err("cache budget is outside 1..=1024 MiB"); }
        // SAFETY: sysconf has no pointer arguments or process-wide effects.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page < 4096 || page as usize > CHUNK || CHUNK % page as usize != 0 {
            return Err("unsupported host page geometry");
        }
        if !file.metadata().map_err(|_| "cannot inspect ROM file")?.is_file() {
            return Err("cache advice requires a regular ROM file");
        }
        log::info!("ROM cache enabled: budget_mib={mib} chunk_mib=1 host_page_bytes={page} eviction_scope=old_tracked_chunks code_copy_retirement=true best_effort=true");
        Ok(Self { file, page: page as usize, chunks: VecDeque::with_capacity(mib),
            stats: RomCacheStats { budget_mib: mib, ..RomCacheStats::default() } })
    }

    pub(super) fn stats(&self) -> RomCacheStats {
        RomCacheStats { tracked_chunks: self.chunks.len(), ..self.stats }
    }

    pub(super) fn finish_read(&mut self, map: &Mmap, offset: u64, len: usize) {
        if len == 0 { return; }
        let Ok(start) = usize::try_from(offset) else { return };
        let Some(end) = start.checked_add(len).filter(|&end| end <= map.len()) else { return };
        self.stats.completed_reads += 1;
        for chunk_offset in (start / CHUNK..=(end - 1) / CHUNK).map(|index| index * CHUNK) {
            let existing = self.chunks.iter().position(|chunk| chunk.offset == chunk_offset);
            // Even a partial completed read tracks its enclosing chunk. The
            // opt-in budget retires old chunks, including adjacent read-ahead.
            let chunk = match existing {
                Some(at) => self.chunks.remove(at).unwrap(),
                None => Chunk { offset: chunk_offset },
            };
            if self.chunks.len() == self.stats.budget_mib {
                let old = self.chunks.pop_front().unwrap();
                self.advise(map, old);
            }
            self.chunks.push_back(chunk);
        }
    }

    fn advise(&mut self, map: &Mmap, chunk: Chunk) {
        self.advise_range(map, chunk.offset, CHUNK.min(map.len() - chunk.offset));
    }

    /// Retire only the fully covered pages of a source that now has an owned
    /// copy. This bypasses the runtime chunk LRU without broadening the range.
    pub(super) fn retire_owned_copy(&mut self, map: &Mmap, offset: u64, len: usize) {
        let Ok(start) = usize::try_from(offset) else { return };
        if start.checked_add(len).is_none_or(|end| end > map.len()) { return; }
        self.advise_range(map, start, len);
    }

    fn advise_range(&mut self, map: &Mmap, start: usize, len: usize) {
        // Clamp both boundaries inward: never advise a partial final file page
        // or bytes outside the exact range supplied by an owned-copy caller.
        let offset = start.div_ceil(self.page) * self.page;
        let end = (start + len) / self.page * self.page;
        if offset >= end { return; }
        let len = end - offset;
        self.stats.eviction_requests += 1;
        self.stats.advised_bytes += len as u64;
        // SAFETY: the mapping is read-only and the ROM remains unchanged. These
        // aligned, in-bounds pages belong either to an old tracked chunk or a
        // successfully copied source. Borrowed slices remain valid: subsequent
        // reads fault identical bytes back from the original file.
        if unsafe { map.unchecked_advise_range(UncheckedAdvice::DontNeed, offset, len) }.is_err() {
            self.stats.mmap_advice_failures += 1;
            if self.stats.mmap_advice_failures == 1 { log::warn!("ROM cache mmap advice failed; reclamation remains best effort"); }
        }
        // Drop our PTEs before requesting physical file-cache reclamation.
        // Neither call changes ROM bytes or unrelated/global file caches.
        let result = unsafe { libc::posix_fadvise(self.file.as_raw_fd(), offset as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED) };
        if result != 0 {
            self.stats.file_advice_failures += 1;
            if self.stats.file_advice_failures == 1 { log::warn!("ROM cache file advice failed ({result}); reclamation remains best effort"); }
        }
    }

}

impl Drop for RomCache {
    fn drop(&mut self) {
        log::info!("ROM cache advice: budget_mib={} completed_reads={} eviction_requests={} advised_bytes={} mmap_failures={} file_failures={}",
            self.stats.budget_mib, self.stats.completed_reads, self.stats.eviction_requests,
            self.stats.advised_bytes, self.stats.mmap_advice_failures, self.stats.file_advice_failures);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RomImage;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIXTURE: AtomicUsize = AtomicUsize::new(0);

    fn fixture(chunks: usize) -> PathBuf {
        // /tmp is often tmpfs, whose pages are not clean disk-file cache.
        // Use a tiny generated file on the workspace filesystem instead.
        let dir = std::env::var_os("ZAKURO_ROM_CACHE_TEST_DIR").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/rom-cache-tests"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("clean-rom-{}-{}.bin", std::process::id(), FIXTURE.fetch_add(1, Ordering::Relaxed)));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        let mut page = [0u8; 4096];
        for offset in (0..chunks * CHUNK).step_by(page.len()) {
            for (i, byte) in page.iter_mut().enumerate() { *byte = expected(offset + i); }
            file.write_all(&page).unwrap();
        }
        file.sync_all().unwrap();
        path
    }

    fn expected(offset: usize) -> u8 { ((offset / 4096 * 31 + offset % 4096 * 17) % 251) as u8 }

    fn resident(map: &Mmap, offset: usize, len: usize) -> usize {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut pages = vec![0u8; len.div_ceil(page)];
        let result = unsafe { libc::mincore(map.as_ptr().add(offset).cast_mut().cast(), len, pages.as_mut_ptr()) };
        assert_eq!(result, 0, "mincore failed: {}", std::io::Error::last_os_error());
        pages.iter().filter(|&&value| value & 1 != 0).count()
    }

    fn mapped_kib(path: &Path) -> (u64, u64) {
        let name = path.canonicalize().unwrap().display().to_string();
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let (mut selected, mut rss, mut pss) = (false, 0, 0);
        for line in smaps.lines() {
            if line.split_whitespace().next().is_some_and(|field| field.contains('-')) {
                selected = line.ends_with(&name);
            } else if selected {
                if line.starts_with("Rss:") { rss += line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap(); }
                if line.starts_with("Pss:") { pss += line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap(); }
            }
        }
        (rss, pss)
    }

    fn copy_read(image: &RomImage, offset: usize, guest: &mut [u8]) {
        guest.copy_from_slice(&image.data()[offset..offset + guest.len()]);
        for (i, &byte) in guest.iter().enumerate() { assert_eq!(byte, expected(offset + i)); }
        image.finish_read(offset as u64, guest.len());
    }

    #[test]
    fn completed_chunks_include_partial_reads_and_invalid_reads_are_ignored() {
        let path = fixture(3);
        let image = RomImage::open_with_cache(&path, Some(1)).unwrap();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut partial = vec![0; page * 2 + 2];
        copy_read(&image, 1, &mut partial);
        let mut whole = vec![0; CHUNK];
        copy_read(&image, CHUNK, &mut whole);
        let before = image.cache_stats().unwrap();
        assert_eq!(before.eviction_requests, 1);
        assert_eq!(before.advised_bytes, CHUNK as u64);
        assert_eq!(before.tracked_chunks, 1);
        image.finish_read(0, 0);
        image.finish_read(u64::MAX, 1);
        image.finish_read((3 * CHUNK) as u64, 1);
        let after = image.cache_stats().unwrap();
        assert_eq!(after.completed_reads, before.completed_reads);
        assert_eq!(after.advised_bytes, before.advised_bytes);
        copy_read(&image, 1, &mut partial);
        drop(image);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn one_byte_reads_track_chunks_and_final_partial_file_page_is_excluded() {
        let path = fixture(2);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((2 * CHUNK + 17) as u64).unwrap();
        file.sync_all().unwrap();
        let image = RomImage::open_with_cache(&path, Some(1)).unwrap();
        let mut byte = [0];
        copy_read(&image, 1, &mut byte);
        copy_read(&image, CHUNK + 1, &mut byte);
        assert_eq!(image.cache_stats().unwrap().advised_bytes, CHUNK as u64);
        image.finish_read((2 * CHUNK + 16) as u64, 1);
        assert_eq!(image.cache_stats().unwrap().advised_bytes, (2 * CHUNK) as u64);
        copy_read(&image, 1, &mut byte);
        // Retiring the last chunk must not advise the partial file page.
        assert_eq!(image.cache_stats().unwrap().advised_bytes, (2 * CHUNK) as u64);
        drop(image);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn owned_code_copy_retires_exact_full_pages_only_after_success() {
        use crate::{Title, ExHeader, NcchHeader, ExeFs};
        let path = fixture(3);
        let image = RomImage::open_with_cache(&path, Some(1)).unwrap();
        let mut header = vec![0u8; 0x400];
        header[0x100..0x104].copy_from_slice(b"NCCH");
        let mut exefs = ExeFs::parse(&[0u8; 0x200]).unwrap();
        exefs.entries.push(crate::exefs::ExeFsEntry {
            name: ".code".into(), offset: (CHUNK + 1 - 0x200) as u64,
            size: 3 * 4096 + 2, hash: [0; 32],
        });
        let mut title = Title { image, ncch_offset: 0,
            ncch: NcchHeader::parse(&header).unwrap(),
            exheader: ExHeader::parse(&[0u8; 0x400]).unwrap(),
            exefs, exefs_offset: 0, romfs: None };
        let owned = title.code().unwrap();
        for (i, &byte) in owned.iter().enumerate() { assert_eq!(byte, expected(CHUNK + 1 + i)); }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let expected_bytes = ((CHUNK + 1 + owned.len()) / page - (CHUNK + 1).div_ceil(page)) * page;
        let stats = title.image.cache_stats().unwrap();
        assert_eq!(stats.advised_bytes, expected_bytes as u64);
        assert_eq!(stats.tracked_chunks, 0);
        assert_eq!(title.exefs_file(".code").unwrap(), &owned);
        title.exheader.compress_code = true;
        title.exefs.entries[0].size = 7;
        assert!(title.code().is_err());
        assert_eq!(title.image.cache_stats().unwrap().advised_bytes, stats.advised_bytes);
        // Disabled policy remains inert even when the copy succeeds.
        title.image = RomImage::open_with_cache(&path, None).unwrap();
        title.exheader.compress_code = false;
        assert_eq!(title.code().unwrap().len(), 7);
        assert!(title.image.cache_stats().is_none());
        drop(title);
        std::fs::remove_file(path).unwrap();
    }

    /// This asserts actual clean file-cache reclamation, not just a lower
    /// mapping RSS. It may fail when a filesystem/kernel ignores advice, so
    /// run it explicitly on the intended disk-backed host and keep its result.
    #[test]
    #[ignore = "disk/filesystem-specific physical reclamation evidence"]
    fn completed_read_lru_reclaims_clean_disk_cache_and_rereads_exactly() {
        const BUDGET: usize = 16;
        let path = fixture(22);
        let file = File::open(&path).unwrap();
        let observer = unsafe { Mmap::map(&file).unwrap() };
        let mut guest = vec![0; CHUNK];
        let control = RomImage::open_with_cache(&path, None).unwrap();
        assert_eq!(control.data()[123], expected(123));
        for chunk in 1..=20 { copy_read(&control, chunk * CHUNK, &mut guest); }
        assert!(control.cache_stats().is_none());
        let control_pages = resident(&observer, 2 * CHUNK, 4 * CHUNK);
        let control_mapped = mapped_kib(&path);
        assert!(control_pages > 0, "control pages were not resident before advice");
        drop(control);

        let image = RomImage::open_with_cache(&path, Some(BUDGET)).unwrap();
        let initial = image.cache_stats().unwrap();
        assert_eq!(initial.eviction_requests, 0, "opening must not perform an initial bulk drop");
        assert_eq!(resident(&observer, 2 * CHUNK, 4 * CHUNK), control_pages);
        assert_eq!(image.data()[123], expected(123));
        let mut partial = [0u8];
        for chunk in 1..=BUDGET { copy_read(&image, chunk * CHUNK + 1, &mut partial); }
        // Refresh an existing chunk through a one-byte read. True LRU must
        // evict chunk 2 next, preserving the recently read chunk 1.
        let mut one = [0]; copy_read(&image, CHUNK + 5, &mut one);
        for chunk in 17..=20 { copy_read(&image, chunk * CHUNK + 1, &mut partial); }
        let old_pages = resident(&observer, 2 * CHUNK, 4 * CHUNK);
        let retained_pages = resident(&observer, CHUNK, CHUNK);
        let header_pages = resident(&observer, 0, CHUNK);
        let untouched_pages = resident(&observer, 21 * CHUNK, CHUNK);
        let capped_mapped = mapped_kib(&path);
        let stats = image.cache_stats().unwrap();
        let evidence = format!(r#"{{"budget_mib":16,"chunk_mib":1,"eviction_scope":"old_tracked_chunks","completed_guest_read_bytes_per_chunk":1,"host_page_bytes":{},"control_old_cache_pages":{},"after_advice_old_cache_pages":{},"retained_recent_chunk_pages":{},"untracked_header_pages":{},"untouched_tail_pages":{},"control_mapping_rss_kib":{},"control_mapping_pss_kib":{},"capped_mapping_rss_kib":{},"capped_mapping_pss_kib":{},"tracked_chunks":{},"completed_reads":{},"eviction_requests":{},"advised_bytes":{},"mmap_advice_failures":{},"file_advice_failures":{},"physical_reclamation_observed":{},"cgroup_charge_measured":false}}"#,
            unsafe { libc::sysconf(libc::_SC_PAGESIZE) }, control_pages, old_pages, retained_pages,
            header_pages, untouched_pages, control_mapped.0, control_mapped.1,
            capped_mapped.0, capped_mapped.1, stats.tracked_chunks, stats.completed_reads,
            stats.eviction_requests, stats.advised_bytes, stats.mmap_advice_failures,
            stats.file_advice_failures, old_pages == 0);
        println!("ROM_CACHE_PHYSICAL_EVIDENCE {evidence}");
        if let Some(out) = std::env::var_os("ZAKURO_ROM_CACHE_EVIDENCE") { std::fs::write(out, format!("{evidence}\n")).unwrap(); }
        assert_eq!(stats.eviction_requests, 4);
        assert_eq!(stats.advised_bytes, (4 * CHUNK) as u64);
        assert_eq!(stats.tracked_chunks, BUDGET);
        assert_eq!(stats.mmap_advice_failures, 0);
        assert_eq!(stats.file_advice_failures, 0);
        assert_eq!(old_pages, 0, "kernel/filesystem did not release all advised clean cache pages; do not claim a physical bound");
        assert!(retained_pages > 0 && header_pages > 0 && untouched_pages > 0);
        // Exact owned-copy retirement must preserve both boundary pages and
        // neighboring untracked bytes, while physically releasing inner pages.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        image.retire_owned_copy((page + 1) as u64, 3 * page);
        assert_eq!(resident(&observer, 2 * page, 2 * page), 0,
            "owned-copy advice did not release exact inner physical cache pages");
        assert!(resident(&observer, page, page) > 0);
        assert!(resident(&observer, 4 * page, page) > 0);
        for at in page + 1..4 * page + 1 { assert_eq!(image.data()[at], expected(at)); }
        // Fault old data back from disk, verify every byte, then repeat.
        copy_read(&image, 2 * CHUNK, &mut guest);
        copy_read(&image, 2 * CHUNK, &mut guest);
        assert_eq!(image.cache_stats().unwrap().eviction_requests, 6);
        assert_eq!(image.cache_stats().unwrap().tracked_chunks, BUDGET);
        drop(image); drop(observer); drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
