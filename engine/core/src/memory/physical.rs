//! physical memory and the FCRAM page allocator.

use zakuro_common::memory_map::*;

/// which of the kernel's three FCRAM regions an allocation comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRegion {
    Application,
    System,
    Base,
}

/// a contiguous run of physical pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalBlock {
    pub addr: u32,
    pub size: u32,
}

/// one of the console's memories, in the shared file when there is one.
pub struct Store {
    ptr: *mut u8,
    len: usize,
    /// where it starts in the file, none for memory of its own.
    offset: Option<u64>,
}

impl std::ops::Deref for Store {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: ptr and len came from one allocation or mapping that lives
        // as long as the PhysicalMemory owning this
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::ops::DerefMut for Store {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self is the only way in
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// the memories in one shared memory file, so a page can be mapped at more
/// than one place, the way the console's memory map has it, for fastmem.
struct Shared {
    fd: i32,
    base: *mut u8,
    len: usize,
}

impl Shared {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn new(len: usize) -> Option<Shared> {
        // SAFETY: plain system calls, every result checked
        unsafe {
            let fd = libc::memfd_create(c"zakuro-physical".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return None;
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                libc::close(fd);
                return None;
            }
            let base = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if base == libc::MAP_FAILED {
                libc::close(fd);
                return None;
            }
            Some(Shared { fd, base: base as *mut u8, len })
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn new(_len: usize) -> Option<Shared> {
        None
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: the mapping and descriptor Shared::new made
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

/// backing store for every physical memory the CPU can reach.
pub struct PhysicalMemory {
    pub fcram: Store,
    pub vram: Store,
    pub dsp_ram: Store,
    /// AXI WRAM.
    pub axi_wram: Store,
    shared: Option<Shared>,
    /// the memories that are not in the shared file.
    _own: Vec<Box<[u8]>>,

    /// one bit per FCRAM page, set when allocated.
    allocated: Vec<bool>,
    /// region boundaries as page indices into allocated.
    regions: [(u32, u32); 3],
}

impl PhysicalMemory {
    /// app_bytes comes from the exheader's system mode, the rest of FCRAM is
    /// split between the SYSTEM and BASE regions the way the retail kernel
    /// does it.
    pub fn new(new3ds: bool, app_bytes: u32) -> PhysicalMemory {
        let total = fcram_size(new3ds);
        let app = app_bytes.min(total);
        // retail Old3DS gives SYSTEM 32 MiB and BASE whatever is left, the
        // New3DS extra 128 MiB all goes to the application region when the
        // title asks for it.
        let system = (total - app).min(32 * 1024 * 1024);
        let base = total - app - system;

        let pages = (total / PAGE_SIZE) as usize;
        let app_pages = app / PAGE_SIZE;
        let system_pages = system / PAGE_SIZE;
        let base_pages = base / PAGE_SIZE;

        let sizes = [total as usize, VRAM_SIZE as usize, DSP_RAM_SIZE as usize, AXI_WRAM_SIZE as usize];
        let shared = Shared::new(sizes.iter().sum());
        let mut own = Vec::new();
        let mut offset = 0;
        let [fcram, vram, dsp_ram, axi_wram] = sizes.map(|len| {
            let store = match &shared {
                // SAFETY: the sizes add up to the file's length
                Some(shared) => Store { ptr: unsafe { shared.base.add(offset) }, len, offset: Some(offset as u64) },
                None => {
                    let mut memory = vec![0u8; len].into_boxed_slice();
                    let ptr = memory.as_mut_ptr();
                    own.push(memory);
                    Store { ptr, len, offset: None }
                }
            };
            offset += len;
            store
        });

        PhysicalMemory {
            fcram,
            vram,
            dsp_ram,
            axi_wram,
            shared,
            _own: own,
            allocated: vec![false; pages],
            regions: [
                (0, app_pages),
                (app_pages, system_pages),
                (app_pages + system_pages, base_pages),
            ],
        }
    }

    /// the kernel's read-only configuration page, at the start of AXI WRAM.
    pub fn config_mem_mut(&mut self) -> &mut [u8] {
        let offset = (CONFIG_MEM_VADDR - AXI_WRAM_PADDR) as usize;
        &mut self.axi_wram[offset..offset + CONFIG_MEM_SIZE as usize]
    }

    /// the page the kernel keeps updating with time, battery and wifi state.
    pub fn shared_page_mut(&mut self) -> &mut [u8] {
        let offset = (SHARED_PAGE_VADDR - AXI_WRAM_PADDR) as usize;
        &mut self.axi_wram[offset..offset + SHARED_PAGE_SIZE as usize]
    }

    fn region_pages(&self, region: MemoryRegion) -> (u32, u32) {
        self.regions[match region {
            MemoryRegion::Application => 0,
            MemoryRegion::System => 1,
            MemoryRegion::Base => 2,
        }]
    }

    pub fn region_size(&self, region: MemoryRegion) -> u32 {
        self.region_pages(region).1 * PAGE_SIZE
    }

    pub fn region_used(&self, region: MemoryRegion) -> u32 {
        let (start, count) = self.region_pages(region);
        let used = self.allocated[start as usize..(start + count) as usize]
            .iter()
            .filter(|&&a| a)
            .count();
        used as u32 * PAGE_SIZE
    }

    pub fn region_free(&self, region: MemoryRegion) -> u32 {
        self.region_size(region) - self.region_used(region)
    }

    /// allocates size bytes of physically contiguous FCRAM.
    pub fn allocate(&mut self, region: MemoryRegion, size: u32) -> Option<PhysicalBlock> {
        let needed = (align_up(size, PAGE_SIZE) / PAGE_SIZE) as usize;
        if needed == 0 {
            return Some(PhysicalBlock {
                addr: FCRAM_PADDR,
                size: 0,
            });
        }

        let (start, count) = self.region_pages(region);
        let (start, count) = (start as usize, count as usize);

        let mut run = 0usize;
        for i in start..start + count {
            if self.allocated[i] {
                run = 0;
                continue;
            }
            run += 1;
            if run == needed {
                let first = i + 1 - needed;
                for page in &mut self.allocated[first..first + needed] {
                    *page = true;
                }
                return Some(PhysicalBlock {
                    addr: FCRAM_PADDR + (first as u32 * PAGE_SIZE),
                    size: needed as u32 * PAGE_SIZE,
                });
            }
        }
        None
    }

    pub fn free(&mut self, block: PhysicalBlock) {
        let first = ((block.addr - FCRAM_PADDR) / PAGE_SIZE) as usize;
        let count = (block.size / PAGE_SIZE) as usize;
        let end = (first + count).min(self.allocated.len());
        for page in &mut self.allocated[first..end] {
            *page = false;
        }
    }

    /// Zeroes exactly the backed range, including any aliases of its pages.
    pub fn zero(&mut self, paddr: u32, size: u32) -> bool {
        // A shared mapping cannot use MADV_DONTNEED to zero its backing:
        // that merely drops this mapping's PTEs. Punching a hole in the memfd
        // zeroes all aliases and releases old pages without faulting fresh
        // untouched pages in. The kernel also zeroes partial boundary pages.
        // Keep small clears in userspace, and fall back on kernels/filesystems
        // that do not support hole punching (including non-Linux hosts).
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if size >= 64 * 1024 {
            if let Some((fd, offset)) = self.backing(paddr, size) {
                // SAFETY: backing checked that this exact range is inside
                // the live shared file. KEEP_SIZE preserves every mapping.
                if unsafe {
                    libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        offset as libc::off_t, size as libc::off_t)
                } == 0 {
                    return true;
                }
            }
        }
        match self.host_slice_mut(paddr, size) {
            Some(slice) => {
                slice.fill(0);
                true
            }
            None => false,
        }
    }

    /// host slice backing a physical address range, if it is real memory.
    pub fn host_slice_mut(&mut self, paddr: u32, size: u32) -> Option<&mut [u8]> {
        let (base, store): (u32, &mut Store) = match paddr {
            p if (VRAM_PADDR..VRAM_PADDR + VRAM_SIZE).contains(&p) => (VRAM_PADDR, &mut self.vram),
            p if (DSP_RAM_PADDR..DSP_RAM_PADDR + DSP_RAM_SIZE).contains(&p) => {
                (DSP_RAM_PADDR, &mut self.dsp_ram)
            }
            p if (AXI_WRAM_PADDR..AXI_WRAM_PADDR + AXI_WRAM_SIZE).contains(&p) => {
                (AXI_WRAM_PADDR, &mut self.axi_wram)
            }
            p if p >= FCRAM_PADDR => (FCRAM_PADDR, &mut self.fcram),
            _ => return None,
        };
        let offset = (paddr - base) as usize;
        let end = offset.checked_add(size as usize)?;
        store.get_mut(offset..end)
    }
    /// the shared file and where in it size bytes from paddr are, when the
    /// memories are in one and the range is inside one of them.
    pub fn backing(&self, paddr: u32, size: u32) -> Option<(i32, u64)> {
        let shared = self.shared.as_ref()?;
        let (base, store) = match paddr {
            p if (VRAM_PADDR..VRAM_PADDR + VRAM_SIZE).contains(&p) => (VRAM_PADDR, &self.vram),
            p if (DSP_RAM_PADDR..DSP_RAM_PADDR + DSP_RAM_SIZE).contains(&p) => (DSP_RAM_PADDR, &self.dsp_ram),
            p if (AXI_WRAM_PADDR..AXI_WRAM_PADDR + AXI_WRAM_SIZE).contains(&p) => (AXI_WRAM_PADDR, &self.axi_wram),
            p if p >= FCRAM_PADDR => (FCRAM_PADDR, &self.fcram),
            _ => return None,
        };
        let offset = (paddr - base) as u64;
        let start = store.offset?;
        (offset + size as u64 <= store.len as u64).then_some((shared.fd, start + offset))
    }

}

#[inline]
const fn align_up(value: u32, align: u32) -> u32 {
    (value + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_contiguously_and_frees() {
        let mut phys = PhysicalMemory::new(false, 64 * 1024 * 1024);
        let a = phys.allocate(MemoryRegion::Application, 0x4000).unwrap();
        let b = phys.allocate(MemoryRegion::Application, 0x1000).unwrap();
        assert_eq!(a.addr, FCRAM_PADDR);
        assert_eq!(a.size, 0x4000);
        assert_eq!(b.addr, FCRAM_PADDR + 0x4000);

        assert_eq!(phys.region_used(MemoryRegion::Application), 0x5000);
        phys.free(a);
        assert_eq!(phys.region_used(MemoryRegion::Application), 0x1000);

        // the freed hole is reused.
        let c = phys.allocate(MemoryRegion::Application, 0x2000).unwrap();
        assert_eq!(c.addr, FCRAM_PADDR);
    }

    #[test]
    fn refuses_to_overcommit() {
        let mut phys = PhysicalMemory::new(false, 0x4000);
        assert!(phys.allocate(MemoryRegion::Application, 0x4000).is_some());
        assert!(phys.allocate(MemoryRegion::Application, 0x1000).is_none());
    }

    #[test]
    fn regions_do_not_overlap() {
        let phys = PhysicalMemory::new(false, 64 * 1024 * 1024);
        assert_eq!(phys.region_size(MemoryRegion::Application), 64 << 20);
        assert_eq!(phys.region_size(MemoryRegion::System), 32 << 20);
        assert_eq!(phys.region_size(MemoryRegion::Base), 32 << 20);
    }

    #[test]
    fn zero_preserves_bytes_outside_exact_range() {
        let mut phys = PhysicalMemory::new(false, 64 << 20);
        for (base, region_size) in [(FCRAM_PADDR, FCRAM_SIZE_OLD3DS), (VRAM_PADDR, VRAM_SIZE),
                                    (DSP_RAM_PADDR, DSP_RAM_SIZE), (AXI_WRAM_PADDR, AXI_WRAM_SIZE)] {
            let total = region_size.min(512 * 1024);
            for (offset, size) in [(0, 0), (3, 17), (4093, 65535), (4093, 65536),
                                   (0, 65536), (123, 200003), (total - 65537, 65537)] {
                phys.host_slice_mut(base, total).unwrap().fill(0xB7);
                assert!(phys.zero(base + offset, size));
                let bytes = phys.host_slice_mut(base, total).unwrap();
                assert!(bytes[..offset as usize].iter().all(|&byte| byte == 0xB7));
                assert!(bytes[offset as usize..(offset + size) as usize].iter().all(|&byte| byte == 0));
                assert!(bytes[(offset + size) as usize..].iter().all(|&byte| byte == 0xB7));
            }
            assert!(!phys.zero(base + region_size - 4096, 65536));
        }
        assert!(!phys.zero(FCRAM_PADDR - 1, 65536));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn large_zero_releases_backing_and_preserves_aliases() {
        let mut phys = PhysicalMemory::new(false, 64 << 20);
        let size = 4 << 20;
        let (fd, offset) = phys.backing(FCRAM_PADDR, size).expect("shared backing");
        let alias = super::super::fastmem::View::new().expect("alias reservation");
        assert!(alias.map(0x1000, size, fd, offset));
        phys.host_slice_mut(FCRAM_PADDR, size).unwrap().fill(0xA5);
        // SAFETY: the alias maps exactly size bytes of the live shared file.
        // Each temporary read ends before either mapping is modified again.
        let bytes = || unsafe { std::slice::from_raw_parts(alias.base().add(0x1000), size as usize) };
        assert!(bytes().iter().all(|&byte| byte == 0xA5));
        assert!(phys.zero(FCRAM_PADDR, size));
        // Observe allocation before reading the zeroes back into the page cache.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
        assert!(stat.st_blocks * 512 < size as i64 / 16, "zero range remains resident in backing: {} blocks", stat.st_blocks);
        assert!(bytes().iter().all(|&byte| byte == 0));
        phys.host_slice_mut(FCRAM_PADDR, 1).unwrap()[0] = 0x63;
        assert_eq!(bytes()[0], 0x63);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn zero_falls_back_when_hole_punching_fails() {
        let mut phys = PhysicalMemory::new(false, 64 << 20);
        let size = 128 * 1024;
        phys.host_slice_mut(FCRAM_PADDR, size).unwrap().fill(0x91);
        // The mapping stays valid after losing a usable descriptor. Simulate
        // an unsupported backing operation without depending on this kernel.
        let fd = phys.shared.as_ref().expect("shared backing").fd;
        phys.shared.as_mut().unwrap().fd = -1;
        let cleared = phys.zero(FCRAM_PADDR + 3, size - 6);
        phys.shared.as_mut().unwrap().fd = fd;
        assert!(cleared);
        let bytes = phys.host_slice_mut(FCRAM_PADDR, size).unwrap();
        assert_eq!(&bytes[..3], &[0x91; 3]);
        assert!(bytes[3..size as usize - 3].iter().all(|&byte| byte == 0));
        assert_eq!(&bytes[size as usize - 3..], &[0x91; 3]);
    }

    #[test]
    fn zero_works_without_shared_backing() {
        let mut memory = vec![0xA7; 128 * 1024].into_boxed_slice();
        let empty = || Store { ptr: std::ptr::NonNull::dangling().as_ptr(), len: 0, offset: None };
        let mut phys = PhysicalMemory {
            fcram: Store { ptr: memory.as_mut_ptr(), len: memory.len(), offset: None },
            vram: empty(), dsp_ram: empty(), axi_wram: empty(),
            shared: None, _own: vec![memory], allocated: Vec::new(), regions: [(0, 0); 3],
        };
        assert!(phys.zero(FCRAM_PADDR + 3, 128 * 1024 - 6));
        let bytes = &*phys.fcram;
        assert_eq!(&bytes[..3], &[0xA7; 3]);
        assert!(bytes[3..bytes.len() - 3].iter().all(|&byte| byte == 0));
        assert_eq!(&bytes[bytes.len() - 3..], &[0xA7; 3]);
        assert!(!phys.zero(FCRAM_PADDR + 128 * 1024 - 1, 2));
    }
}
