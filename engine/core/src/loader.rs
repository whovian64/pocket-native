//! turning a ROM into a running process.

use zakuro_common::memory_map::*;
use zakuro_common::ConsoleModel;
use zakuro_fs::{MemoryType, Title};

use crate::memory::{self, MemoryState, Permission};
use crate::{Config, System};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Fs(#[from] zakuro_fs::FsError),
    #[error("the code image is too small for the segments the exheader describes")]
    ShortCodeImage,
}

/// the main thread's stack ends where the shared-memory region begins, which
/// is what the retail kernel does.
const STACK_TOP: u32 = SHARED_MEMORY_VADDR;

pub fn load(path: impl AsRef<std::path::Path>, mut config: Config) -> Result<System, LoadError> {
    let title = Title::load(path)?;
    log::info!("loaded {}", title.describe());

    let exheader = &title.exheader;
    let region = match exheader.memory_type {
        MemoryType::Application => memory::MemoryRegion::Application,
        MemoryType::System => memory::MemoryRegion::System,
        MemoryType::Base => memory::MemoryRegion::Base,
    };

    // a title marked New3DS-only has to run as one, and one with a New3DS
    // system mode gets that mode's memory, which only a New 3DS has; a New
    // 3DS is one all the way through
    let n3ds_memory = zakuro_fs::n3ds_application_memory(exheader.n3ds_system_mode);
    if title.ncch.platform() == 2 || n3ds_memory.is_some() {
        config.new3ds = true;
    }
    if config.new3ds {
        config.model = ConsoleModel::New3ds;
    }
    let (app_bytes, app_mem_type) = match n3ds_memory {
        Some(memory) if config.new3ds => memory,
        _ => (exheader.system_mode.application_memory(), exheader.system_mode as u32),
    };

    let mut system = System::new(config);
    system.memory = memory::Memory::new(system.config.new3ds, app_bytes);
    system.kernel = crate::kernel::Kernel::new(
        title.program_id(),
        region,
        linear_heap_base(system.config.new3ds),
    );

    map_special_pages(&mut system, app_bytes, app_mem_type);
    map_code(&mut system, &title)?;
    map_stack(&mut system, exheader.stack_size);

    system.kernel.heap_top = HEAP_VADDR;

    // the main thread starts at the beginning of .text.
    let entry = exheader.text.address;
    let priority = exheader.main_thread_priority as u32;
    let main = system.kernel.create_thread(
        "main",
        entry,
        STACK_TOP,
        0,
        priority,
        exheader.ideal_processor as i32,
    );
    system.map_tls_page(main);
    system.kernel.reschedule_pending = true;

    log::info!(
        "entry 0x{entry:08X}, stack top 0x{STACK_TOP:08X}, priority {priority}, {} MiB app memory",
        app_bytes / (1024 * 1024)
    );

    if let Some(linked) = &system.config.linked {
        if linked.program_id() != title.program_id() {
            log::warn!(
                "the code linked in is for title {:016X}, not {:016X}, interpreting everything",
                linked.program_id(),
                title.program_id()
            );
        } else {
            match crate::recompiled::Library::linked(linked) {
                Ok(library) => {
                    log::info!("running the recompiled code linked in, {}", library.describe());
                    system.recompiled = Some(library);
                }
                Err(error) => log::warn!("could not use the code linked in, {error}, interpreting everything"),
            }
        }
    } else if let Some(path) = system.config.recompiled.clone().or_else(|| installed(&system, &title)) {
        let path = if path.is_dir() { path.join(recomp_abi::library_name(title.program_id())) } else { path };
        let opened = crate::recompiled::Library::open(&path).and_then(|library| {
            if library.needs_fastmem() && system.memory.fastmem().is_null() {
                Err("it was built for fastmem, which this host has no view for".to_owned())
            } else {
                Ok(library)
            }
        });
        match opened {
            Ok(library) => {
                log::info!("running recompiled code from {}, {}", path.display(), library.describe());
                system.recompiled = Some(library);
                let text = exheader_text(&title);
                system.hints = Some(crate::hints::Hints::new(&path, text));
            }
            Err(error) => log::warn!("could not load {}, {error}, interpreting everything", path.display()),
        }
    }

    if system.config.hardware_renderer && system.config.software_draws {
        match system.gpu.enable_panel() {
            Ok(()) => log::info!("switch: drawing in software, the screens shown through OpenGL ES"),
            Err(error) => log::warn!("{error}, drawing in software with no panel"),
        }
    } else if system.config.hardware_renderer {
        match system.gpu.enable_hardware_renderer(system.config.resolution) {
            Ok(name) => log::info!("drawing on {name} through OpenGL ES"),
            Err(error) => log::warn!("{error}, drawing in software"),
        }
    }

    system.title = Some(title);
    Ok(system)
}

/// where the executable's code is.
fn exheader_text(title: &Title) -> std::ops::Range<u32> {
    let text = &title.exheader.text;
    text.address..text.address + text.num_pages * PAGE_SIZE
}

/// the library 3dsrecomp build installed for the title, when the system is
/// to look for one.
fn installed(system: &System, title: &Title) -> Option<std::path::PathBuf> {
    if !system.config.find_recompiled {
        return None;
    }
    let found = recomp_abi::installed(title.program_id());
    if found.is_none() {
        log::info!(
            "no recompiled code for {:016X}, interpreting, 3dsrecomp build makes it",
            title.program_id()
        );
    }
    found
}

fn map_special_pages(system: &mut System, app_bytes: u32, app_mem_type: u32) {
    let model = system.config.model;
    let sys = system
        .memory
        .phys
        .region_size(memory::MemoryRegion::System);
    let base = system.memory.phys.region_size(memory::MemoryRegion::Base);

    memory::config::init_config_mem(
        system.memory.phys.config_mem_mut(),
        model,
        app_mem_type,
        app_bytes,
        sys,
        base,
    );
    let slider = system.config.slider_3d;
    let clock = system.boot_clock;
    memory::config::init_shared_page(system.memory.phys.shared_page_mut(), model, slider, clock);

    // both pages are read-only to the guest and live in AXI WRAM.
    system.memory.map(
        CONFIG_MEM_VADDR,
        AXI_WRAM_PADDR,
        CONFIG_MEM_SIZE,
        Permission::READ,
        MemoryState::Static,
    );
    system.memory.map(
        SHARED_PAGE_VADDR,
        AXI_WRAM_PADDR + (SHARED_PAGE_VADDR - CONFIG_MEM_VADDR),
        SHARED_PAGE_SIZE,
        Permission::READ,
        MemoryState::Static,
    );

    // VRAM is mapped into every process.
    system.memory.map(
        VRAM_VADDR,
        VRAM_PADDR,
        VRAM_SIZE,
        Permission::RW,
        MemoryState::Static,
    );
    // so is the DSP's memory.
    system.memory.map(
        DSP_RAM_VADDR,
        DSP_RAM_PADDR,
        DSP_RAM_SIZE,
        Permission::RW,
        MemoryState::Static,
    );
}

fn map_code(system: &mut System, title: &Title) -> Result<(), LoadError> {
    let code = title.code()?;
    let exheader = &title.exheader;

    let segments = [
        (".text", exheader.text, Permission::RX, 0u32),
        (
            ".rodata",
            exheader.rodata,
            Permission::READ,
            exheader.text.num_pages * PAGE_SIZE,
        ),
        (
            ".data",
            exheader.data,
            Permission::RW,
            (exheader.text.num_pages + exheader.rodata.num_pages) * PAGE_SIZE,
        ),
    ];

    for (name, info, permission, source_offset) in segments {
        if info.num_pages == 0 {
            continue;
        }
        let mapped_size = info.num_pages * PAGE_SIZE;
        let block = system
            .memory
            .phys
            .allocate(system.kernel.memory_region, mapped_size)
            .expect("FCRAM for a code segment");
        system.memory.map(
            info.address,
            block.addr,
            mapped_size,
            permission,
            MemoryState::Code,
        );

        // zero the whole mapped region first so the padding between the
        // segment's real size and its page-aligned size is defined.
        system.memory.zero_physical(block.addr, mapped_size);

        let start = source_offset as usize;
        let end = start + info.size as usize;
        let bytes = code.get(start..end).ok_or(LoadError::ShortCodeImage)?;
        // the write has to go to physical memory, .text and .rodata are about
        // to be visible to the guest without write permission.
        system.memory.write_physical(block.addr, bytes);

        log::debug!(
            "mapped {name} at 0x{:08X}, 0x{:X} bytes in 0x{mapped_size:X}",
            info.address,
            info.size
        );
    }

    // BSS starts right where .data's bytes end, in the rest of its last page,
    // and must start zeroed. what does not fit there gets pages of its own.
    let bss_start = exheader.data.address + exheader.data.num_pages * PAGE_SIZE;
    let bss_end = exheader.data.address + exheader.data.size + exheader.bss_size;
    if bss_end > bss_start {
        let bss_size = zakuro_common::bits::align_up(bss_end - bss_start, PAGE_SIZE);
        let block = system
            .memory
            .phys
            .allocate(system.kernel.memory_region, bss_size)
            .expect("FCRAM for BSS");
        system.memory.map(
            bss_start,
            block.addr,
            bss_size,
            Permission::RW,
            MemoryState::Private,
        );
        system.memory.zero_physical(block.addr, bss_size);
        log::debug!("mapped .bss at 0x{bss_start:08X}, 0x{bss_size:X} bytes");
    }

    Ok(())
}

fn map_stack(system: &mut System, stack_size: u32) {
    let size = zakuro_common::bits::align_up(stack_size.max(0x4000), PAGE_SIZE);
    let base = STACK_TOP - size;
    let block = system
        .memory
        .phys
        .allocate(system.kernel.memory_region, size)
        .expect("FCRAM for the main stack");
    system.memory.map(
        base,
        block.addr,
        size,
        Permission::RW,
        MemoryState::Locked,
    );
    system.memory.zero_physical(block.addr, size);
    log::debug!("mapped the main stack at 0x{base:08X}, 0x{size:X} bytes");
}
