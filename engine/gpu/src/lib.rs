//! PICA200 GPU emulation.

mod overlay;
pub mod blend;
pub mod proctex;
pub mod format;
pub mod lighting;
pub mod raster;
pub mod registers;
pub mod renderer;
pub mod shader;
pub mod tev;
pub mod texture;

use format::ColorFormat;
use registers::*;
pub use overlay::{Overlay, OverlayMesh, OverlayTexture, OverlayVertex};
pub use renderer::{DrawCall, Renderer, SoftwareRenderer};
#[cfg(feature = "gles")]
pub use raster::gles::{layout_size, panel_timing, second_window, set_drops, set_second_shows_top, set_second_stretched, set_second_window, set_stacked, set_window, stacked};

/// how the GPU reaches guest memory.
pub trait GpuMemory {
    fn read(&mut self, addr: u32, out: &mut [u8]);
    fn write(&mut self, addr: u32, data: &[u8]);

    /// the bytes read would find at addr, straight from memory when they
    /// sit together there, to look at without copying them.
    fn slice(&mut self, _addr: u32, _len: usize) -> Option<&[u8]> {
        None
    }

    fn slice_mut(&mut self, _addr: u32, _len: usize) -> Option<&mut [u8]> {
        None
    }

    fn read_u32(&mut self, addr: u32) -> u32 {
        let mut buf = [0u8; 4];
        self.read(addr, &mut buf);
        u32::from_le_bytes(buf)
    }

    /// translates a GPU physical address to whatever address read/write
    /// expect. GX commands and registers all carry physical addresses.
    fn translate(&self, paddr: u32) -> u32 {
        paddr
    }
}

/// what one LCD controller holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferConfig {
    pub address_a_left: u32,
    pub address_a_right: u32,
    pub address_b_left: u32,
    pub address_b_right: u32,
    pub stride: u32,
    pub format: u32,
    /// 0 selects the first pair of addresses, 1 the second.
    pub active: u32,
}

impl FramebufferConfig {
    /// address of the buffer currently being scanned out.
    pub fn address_left(&self) -> u32 {
        if self.active == 0 {
            self.address_a_left
        } else {
            self.address_b_left
        }
    }

    pub fn address_right(&self) -> u32 {
        if self.active == 0 {
            self.address_a_right
        } else {
            self.address_b_right
        }
    }

    /// the low three bits of the format register pick the color format, the
    /// rest configures the controller.
    pub fn color_format(&self) -> ColorFormat {
        ColorFormat::from_raw(self.format & 7)
    }

    /// true when the framebuffer is interleaved for stereoscopic output.
    pub fn is_stereo(&self) -> bool {
        let right = self.address_right();
        right != 0 && right != self.address_left()
    }
}

/// vertices a title sends one attribute at a time, by selecting fixed attribute
/// 15 and writing to the fixed attribute data registers.
#[derive(Default)]
struct Immediate {
    /// the vertex being assembled.
    attributes: [shader::Vec4; 16],
    next_attribute: usize,
    /// finished vertices, drawn together before the next change of state.
    vertices: Vec<[shader::Vec4; 16]>,
}

/// a screen's picture, upright RGBA, and the scale it is at.
pub type Picture = (std::sync::Arc<Vec<u8>>, u32);

/// a screen's picture for the panel the GPU shows the screens on itself,
/// the buffer the GPU drew or the screen's pixels upright as RGBA.
pub enum PanelImage {
    Gpu(ScreenRef),
    Pixels(Vec<u8>, u32, u32),
}

/// the picture a display transfer left for a screen, drawn scaled by the
/// host's GPU, to be shown once the GPU finishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRef {
    /// the buffer the transfer drew, its address and size.
    pub(crate) addr: u32,
    pub(crate) size: (u32, u32),
    pub(crate) format: ColorFormat,
    pub(crate) batch: u64,
    /// the rows of it the screen shows, the first and how many, and how
    /// many pixels of each row, a buffer can have longer rows than the
    /// screen and start the screen a few rows in.
    pub(crate) rows: (u32, u32),
    pub(crate) columns: u32,
}

/// whether ZAKURO_FAST asks for speed over exactness, the GPU's own copy of
/// what it drew trusted over checking guest memory for every change.
pub(crate) fn fast() -> bool {
    static FAST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FAST.get_or_init(|| {
        let fast = std::env::var_os("ZAKURO_FAST").is_some();
        if !fast {
            note("fast mode off");
        }
        fast
    })
}

/// whether a switch is set in the environment, noted in the log the first
/// time it is read set.
pub(crate) fn switch(variable: &str, what: &str) -> bool {
    let set = std::env::var_os(variable).is_some();
    if set {
        note(&format!("{what}, {variable}"));
    }
    set
}

fn note(what: &str) {
    log::info!("switch: {what}");
}

/// fast mode's speed-ups one by one, which ZAKURO_FAST_OFF=name,name turns
/// off while leaving the rest.
pub(crate) mod speedup {
    /// a surface checked against memory from a few spots.
    pub const SPOTS: u32 = 1;
    /// fragment colors at half precision.
    pub const HALF: u32 = 2;
    /// vertex programs multiplying as the host does.
    pub const PLAIN_MATH: u32 = 4;
    /// depth tiles one surface drew over another's taken over on the GPU.
    pub const TILES: u32 = 8;
    /// fills of a whole surface cleared on the GPU alone, and the depth a
    /// fill runs over in part left out of memory.
    pub const CLEARS: u32 = 16;
    /// vertex arrays kept in buffers on the GPU.
    pub const ARRAYS: u32 = 32;
    /// index buffers and vertex arrays trusted for a few command lists.
    pub const MESHES: u32 = 64;
    /// decoded textures trusted for a few command lists.
    pub const TEXTURES: u32 = 128;
    /// a quarter of the decoded texture cache, for a handheld's memory.
    pub const SMALL_CACHE: u32 = 256;
    pub const NAMES: [(&str, u32); 9] = [
        ("spots", SPOTS),
        ("half", HALF),
        ("plain-math", PLAIN_MATH),
        ("tiles", TILES),
        ("clears", CLEARS),
        ("arrays", ARRAYS),
        ("meshes", MESHES),
        ("textures", TEXTURES),
        ("small-cache", SMALL_CACHE),
    ];
}

/// whether fast mode is on with this speed-up.
pub(crate) fn fast_with(speedup: u32) -> bool {
    static OFF: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let off = *OFF.get_or_init(|| {
        let names = std::env::var("ZAKURO_FAST_OFF").unwrap_or_default();
        let off = speedup::NAMES.iter().filter(|(name, _)| names.split(',').any(|named| named.trim() == *name)).fold(0, |off, (_, bit)| off | bit);
        if off != 0 {
            note(&format!("fast mode without {names}"));
        }
        off
    });
    fast() && off & speedup == 0
}

pub struct Gpu {
    /// external registers, 0x1EF00000 upwards, indexed by word.
    pub external: Box<[u32]>,
    /// PICA internal registers, written by command lists.
    pub internal: Box<[u32]>,
    /// the vertex shader unit, its program, uniforms and upload cursors.
    pub vertex_shader: shader::ShaderUnit,
    /// the unit that runs geometry shaders, when the pipeline uses one.
    pub geometry_shader: shader::ShaderUnit,
    /// values for attributes no array feeds, set through the fixed
    /// attribute registers.
    fixed_attributes: [shader::Vec4; 16],
    fixed_attribute_staging: [u32; 3],
    fixed_attribute_words: usize,
    /// vertices sent through the same registers in immediate mode.
    immediate: Immediate,
    /// where a command list asked execution to continue, as (physical
    /// address, size in bytes), taken once the current command finishes.
    pending_jump: Option<(u32, u32)>,
    list: Vec<u32>,

    /// one config per screen, index 0 is the top screen, 1 the bottom.
    pub framebuffers: [FramebufferConfig; 2],
    /// the last few buffers the screens were given, oldest first, which a
    /// title drawing into three in turn, for each eye, hands the next
    /// picture to before it is shown. a buffer belongs to the screen given
    /// it last, the boot's own buffers often becoming the other screen's.
    screen_buffers: Vec<(usize, u32)>,
    /// each screen's buffers whose last picture was skipped, and the buffer
    /// its last drawn picture went to, shown while the screen is on one of
    /// those instead.
    stale: [Vec<u32>; 2],
    fresh: [Option<u32>; 2],
    /// display transfers into the top screen's buffers the frameskip let
    /// through, the pictures of the title's that are shown.
    pub top_pictures: u64,

    /// counters for the diagnostics overlay.
    pub command_lists: u64,
    pub draw_calls: u64,
    pub fills: u64,
    pub transfers: u64,
    /// the transfers and copies that may have changed a screen, skipped
    /// pictures left out.
    pub pictures: u64,
    /// the screen transfer count and the buffers shown when the panel last
    /// showed the screens.
    pub presented: Option<(u64, u32, u32)>,
    /// the last present showed something new, which an interpolated
    /// picture shows once more.
    pub presented_new: bool,
    /// how many times the panel showed the screens.
    pub shown: u64,
    /// vertices actually rasterized, as opposed to draw calls issued, a
    /// draw call with a degenerate vertex count still counts as a call.
    pub vertices_drawn: u64,
    /// host time spent running command lists and display transfers.
    pub busy: std::time::Duration,
    /// skips the next frames the title draws while set, their draws and the
    /// display transfers that would show them, for a frontend that cannot
    /// show them in time, a frame at a time from its first draw.
    pub skip_draws: bool,
    /// the frame skips only because the panel will not show it, which
    /// leaves the bottom screen drawn: some titles draw it only in the
    /// frames between their top pictures.
    keep_bottom: bool,
    /// whether the title sent a picture to the top screen this frame, and
    /// in the two frames before it.
    top_sent: bool,
    top_sent_before: [bool; 2],
    skipping: bool,
    drawing: bool,
    /// in fast mode, what the command lists since the last display transfer
    /// hash to, and the last two bottom pictures', which tell a bottom
    /// screen standing still, and how many were skipped since one was drawn.
    picture_hash: u64,
    bottom_hashes: [u64; 2],
    /// the hash at the last bottom picture's first draw, its first command
    /// list read whole by then.
    bottom_first: u64,
    bottom_skipped: u32,
    /// the screens the draws since the last transfer were sized for, 1 the
    /// bottom one and 2 the top one, and whether the frontend skipped them.
    screens_drawn: u8,
    skipping_frame: bool,
    /// the color buffers draws of a skipped picture were left out of, which
    /// a transfer out of copies nothing worth showing.
    skipped_targets: Vec<(u32, u32)>,
    /// a title drawing both screens between two transfers, whose bottom
    /// screen is never skipped, as that would take the top one with it.
    both_screens: bool,
    /// when a skipped bottom picture went to the top
    both_at: Option<u64>,
    /// bottom screen skipping back on a while after that, for titles doing
    /// it only around their title screen.
    pub bottom_relearn: bool,
    /// the bottom screen drawn with every picture while it changes, not
    /// every other one.
    pub bottom_full_rate: bool,
    /// whether fast mode may skip the bottom screen's pictures at all.
    pub skip_bottom: bool,
    /// in fast mode, the right eye's pictures of a title drawing both left
    /// out, the panel showing the left eye alone. a right eye's picture is a
    /// top one drawn into the buffer the left eye's just went out of, of top
    /// screen draws alone, going to one of the top screen's right buffers.
    /// four of them in turn and the next ones are skipped. a picture that
    /// looked like one and was not pauses it until four more, and the third
    /// time that comes soon after it took up again turns it off for the title.
    pub skip_right_eye: bool,
    right_buffers: Vec<u32>,
    stereo: bool,
    left_eye: Option<u32>,
    right_eye_candidate: bool,
    right_eye_seen: u8,
    right_eye_short: u8,
    /// the color buffer the draws since the last transfer went to, the
    /// first top screen sized one's after passes of other sizes, and
    /// whether they were skipped as the bottom screen's, which a transfer
    /// to the top screen shows was wrong, turning that skipping off for the
    /// title.
    period_target: u32,
    skipped_bottom: bool,
    right_eye_wrong: bool,
    /// the host's GPU showing the screens on the panel while the software
    /// rasterizer draws them.
    #[cfg(feature = "gles")]
    panel: Option<raster::HostRenderer>,
    /// the transfer count when the frontend last skipped a picture, behind
    /// real time.
    skipped_at: u64,
    /// the bottom buffer drawn last, which the panel shows while the
    /// pictures after it are skipped.
    bottom_shown: Option<u32>,
    /// bottom pictures drawn.
    pub bottom_pictures: u64,
    /// decoded textures and the lighting tables, kept across draws.
    resources: raster::Resources,
}

/// number of external register words we track (0x1EF00000..0x1EF04000).
const EXTERNAL_REGISTER_WORDS: usize = 0x1000;
/// PICA internal register file size.
const INTERNAL_REGISTER_WORDS: usize = 0x300;

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpu {
    pub fn new() -> Gpu {
        Gpu {
            external: vec![0; EXTERNAL_REGISTER_WORDS].into_boxed_slice(),
            internal: vec![0; INTERNAL_REGISTER_WORDS].into_boxed_slice(),
            vertex_shader: shader::ShaderUnit::new(),
            geometry_shader: shader::ShaderUnit::new(),
            fixed_attributes: [[0.0, 0.0, 0.0, 1.0]; 16],
            fixed_attribute_staging: [0; 3],
            fixed_attribute_words: 0,
            immediate: Immediate::default(),
            pending_jump: None,
            list: Vec::new(),
            framebuffers: [FramebufferConfig::default(); 2],
            screen_buffers: Vec::new(),
            stale: [Vec::new(), Vec::new()],
            fresh: [None, None],
            top_pictures: 0,
            command_lists: 0,
            draw_calls: 0,
            fills: 0,
            transfers: 0,
            pictures: 0,
            presented: None,
            presented_new: false,
            shown: 0,
            vertices_drawn: 0,
            skip_draws: false,
            keep_bottom: false,
            top_sent: false,
            top_sent_before: [false; 2],
            skipping: false,
            drawing: false,
            picture_hash: 0,
            bottom_hashes: [0, 1],
            bottom_first: 0,
            bottom_skipped: 0,
            screens_drawn: 0,
            skipping_frame: false,
            skipped_targets: Vec::new(),
            both_screens: false,
            both_at: None,
            bottom_relearn: false,
            bottom_full_rate: false,
            #[cfg(feature = "gles")]
            panel: None,
            skip_bottom: true,
            skip_right_eye: true,
            right_buffers: Vec::new(),
            stereo: false,
            left_eye: None,
            right_eye_candidate: false,
            right_eye_seen: 0,
            right_eye_short: 0,
            period_target: 0,
            skipped_bottom: false,
            right_eye_wrong: false,
            skipped_at: 0,
            bottom_shown: None,
            bottom_pictures: 0,
            busy: std::time::Duration::ZERO,
            resources: raster::Resources::default(),
        }
    }

    // -- external registers -------------------------------------------------

    /// offset is the byte offset GSP uses, where zero means 0x1EB00000.
    pub fn write_external(&mut self, gsp_offset: u32, value: u32) {
        let Some(index) = external_index(gsp_offset) else {
            return;
        };
        if index >= self.external.len() {
            return;
        }
        self.external[index] = value;
        let byte_offset = index * 4;
        if log::log_enabled!(log::Level::Trace)
            && (LCD_TOP_BASE..LCD_BOTTOM_BASE + 0x100).contains(&byte_offset)
        {
            log::trace!("GPU reg 0x1EF00{byte_offset:03X} = 0x{value:08X}");
        }
        self.on_external_write(byte_offset, value);
    }

    pub fn read_external(&self, gsp_offset: u32) -> u32 {
        external_index(gsp_offset)
            .and_then(|index| self.external.get(index).copied())
            .unwrap_or(0)
    }

    /// mirrors the LCD registers into our framebuffer state.
    fn on_external_write(&mut self, byte_offset: usize, value: u32) {
        for (screen, base) in [(0usize, LCD_TOP_BASE), (1, LCD_BOTTOM_BASE)] {
            let relative = byte_offset.wrapping_sub(base);
            if matches!(relative, LCD_FB_A_LEFT | LCD_FB_A_RIGHT | LCD_FB_B_LEFT | LCD_FB_B_RIGHT) && value != 0 && self.screen_buffers.last() != Some(&(screen, value)) {
                let buffers = &mut self.screen_buffers;
                buffers.retain(|&(_, buffer)| buffer != value);
                if buffers.len() == 16 {
                    buffers.remove(0);
                }
                buffers.push((screen, value));
                log::debug!("screen {screen} given buffer 0x{value:08X}");
            }
            if screen == 0 && matches!(relative, LCD_FB_A_RIGHT | LCD_FB_B_RIGHT) && value != 0 && !self.right_buffers.contains(&value) {
                if self.right_buffers.len() == 4 {
                    self.right_buffers.remove(0);
                }
                self.right_buffers.push(value);
            }
            let config = &mut self.framebuffers[screen];
            match relative {
                LCD_FB_A_LEFT => config.address_a_left = value,
                LCD_FB_A_RIGHT => config.address_a_right = value,
                LCD_FB_B_LEFT => config.address_b_left = value,
                LCD_FB_B_RIGHT => config.address_b_right = value,
                LCD_FB_FORMAT => config.format = value,
                LCD_FB_STRIDE => config.stride = value,
                LCD_FB_SELECT => config.active = value & 1,
                _ => {}
            }
        }
    }

    /// gsp::SetBufferSwap.
    pub fn set_framebuffer(
        &mut self,
        screen: u32,
        active: u32,
        left: u32,
        right: u32,
        stride: u32,
        format: u32,
    ) {
        let base = match screen {
            0 => LCD_TOP_BASE,
            1 => LCD_BOTTOM_BASE,
            _ => return,
        };
        let (left_offset, right_offset) = if active == 0 {
            (LCD_FB_A_LEFT, LCD_FB_A_RIGHT)
        } else {
            (LCD_FB_B_LEFT, LCD_FB_B_RIGHT)
        };
        for (offset, value) in [
            (left_offset, left),
            (right_offset, right),
            (LCD_FB_STRIDE, stride),
            (LCD_FB_FORMAT, format),
            (LCD_FB_SELECT, active),
        ] {
            let index = (base + offset) / 4;
            if index < self.external.len() {
                self.external[index] = value;
            }
            self.on_external_write(base + offset, value);
        }
    }

    // -- fill and transfer engines -----------------------------------------

    /// MemoryFill, writes a repeating pattern over a physical range.
    pub fn memory_fill<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        start: u32,
        end: u32,
        value: u32,
        width: u32,
    ) {
        if end <= start {
            return;
        }
        // a skipped frame leaves the screens as they are, where a fill over
        // one would clear it for draws that never come
        if self.skip_draws && self.on_screen(memory, start, end - start).is_some_and(|(screen, _)| screen == 0 || !self.keep_bottom) {
            self.fills += 1;
            return;
        }
        let length = (end - start) as usize;
        let address = memory.translate(start);
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            if let Err(error) = hardware.before_fill(memory, address, length as u32, if matches!(width, 2 | 3) { width } else { 4 }) {
                log::error!("the GPU could not write back a buffer, {error}");
            }
        }

        let pattern: Vec<u8> = match width {
            2 => value.to_le_bytes()[..2].to_vec(),
            3 => value.to_le_bytes()[..3].to_vec(),
            _ => value.to_le_bytes().to_vec(),
        };
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            match hardware.fill_only(address, length as u32, &pattern) {
                Ok(true) => {
                    self.fills += 1;
                    return;
                }
                Ok(false) => {}
                Err(error) => log::error!("the GPU could not clear a buffer, {error}"),
            }
        }

        if let Some(target) = memory.slice_mut(address, length) {
            let first = pattern.len().min(target.len());
            target[..first].copy_from_slice(&pattern[..first]);
            let mut done = first;
            while done < target.len() {
                let more = done.min(target.len() - done);
                target.copy_within(..more, done);
                done += more;
            }
            self.fills += 1;
            #[cfg(feature = "gles")]
            if let (Some(hardware), Some(bytes)) = (self.resources.hardware.as_mut(), memory.slice(address, length)) {
                if let Err(error) = hardware.filled(address, bytes) {
                    log::error!("the GPU could not clear a buffer, {error}");
                }
            }
            return;
        }
        thread_local! {
            static FILL: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let mut buffer = FILL.with(|fill| std::mem::take(&mut *fill.borrow_mut()));
        buffer.clear();
        buffer.extend_from_slice(&pattern);
        while buffer.len() < length {
            let more = buffer.len().min(length - buffer.len());
            buffer.extend_from_within(..more);
        }
        buffer.truncate(length);
        log::debug!(
            "memory fill: 0x{start:08X}..0x{end:08X} with 0x{value:08X} ({width}-byte pattern)"
        );
        memory.write(address, &buffer);
        self.fills += 1;
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            if let Err(error) = hardware.filled(address, &buffer) {
                log::error!("the GPU could not clear a buffer, {error}");
            }
        }
        FILL.with(|fill| *fill.borrow_mut() = buffer);
    }

    /// DisplayTransfer, copies a rectangle between buffers, converting format
    /// and tiling on the way.
    pub fn display_transfer<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        input_paddr: u32,
        output_paddr: u32,
        input_dimensions: u32,
        output_dimensions: u32,
        flags: u32,
    ) {
        self.drawing = false;
        // a skipped picture never reaches the screens, transfers anywhere
        // else go on, the title may be waiting for what they bring. a
        // transfer reaches a screen when it overlaps a buffer the screen was
        // given, some titles starting it a few rows before the buffer
        let output_bytes = match (flags >> 12) & 7 {
            0 => 4,
            1 => 3,
            _ => 2,
        };
        let output_end = output_paddr as u64 + (output_dimensions & 0xFFFF) as u64 * (output_dimensions >> 16) as u64 * output_bytes;
        let reached = self.on_screen(memory, output_paddr, (output_end - output_paddr as u64) as u32);
        if let Some((screen, buffer)) = reached {
            self.stale[screen].retain(|&stale| stale != buffer);
            if self.skipping {
                self.stale[screen].push(buffer);
            } else {
                self.fresh[screen] = Some(buffer);
            }
        }
        let shown_on = reached.map(|(screen, _)| screen);
        let screen = shown_on.is_some();
        // the draws name their color buffers by physical address, which
        // translate takes where transfers name them
        let input = memory.translate(input_paddr);
        let input_end = input as u64 + (input_dimensions & 0xFFFF) as u64 * (input_dimensions >> 16) as u64 * 4;
        let from_skipped = self.skipped_targets.iter().any(|&(start, end)| {
            let start_at = memory.translate(start) as u64;
            (input as u64) < start_at + (end - start) as u64 && start_at < input_end
        });
        if self.skipped_bottom && from_skipped && shown_on == Some(0) && !self.both_screens {
            log::info!("switch: bottom screen skipping off, a skipped picture went to the top screen");
            self.both_screens = true;
            self.both_at = Some(self.transfers);
        }
        if let Some((0, buffer)) = reached {
            let right = self.right_buffers.contains(&buffer);
            let drawn_here = memory.translate(self.period_target) == input;
            if self.right_eye_candidate && drawn_here {
                if !right {
                    self.right_eye_off("a picture taken for the right eye's went to the left");
                } else if self.screens_drawn == 2 {
                    self.right_eye_seen = self.right_eye_seen.saturating_add(1);
                }
            }
            if right {
                self.stereo = true;
            } else if drawn_here {
                self.left_eye = Some(self.period_target);
            }
        }
        if shown_on == Some(0) {
            self.top_sent = true;
            if !self.skipping {
                self.top_pictures += 1;
            }
        }
        if fast() && shown_on == Some(1) {
            self.bottom_hashes = [self.picture_hash, self.bottom_hashes[0]];
            if self.skipping {
                self.bottom_skipped += 1;
            } else {
                (self.bottom_skipped, self.bottom_shown) = (0, Some(output_paddr));
                self.bottom_pictures += 1;
            }
        }
        self.picture_hash = 0;
        if self.skipping && (screen || from_skipped) {
            self.transfers += 1;
            return;
        }
        self.pictures += 1;
        let input_width = input_dimensions & 0xFFFF;
        let input_height = input_dimensions >> 16;
        let (input_base, output_base) = (memory.translate(input_paddr), memory.translate(output_paddr));

        // flag layout, from the transfer engine's register,
        //   bit 0      flip the input vertically
        //   bit 1      linear to tiled, tiled to linear without it
        //   bit 3      raw copy, no format conversion
        //   bit 5      keep the layout, tiled or linear on both sides
        //   bits 8-10  input color format
        //   bits 12-14 output color format
        //   bits 24-25 downscale
        let flip_vertically = flags & 1 != 0;
        let input_linear = flags & (1 << 1) != 0;
        let output_tiled = input_linear != (flags & (1 << 5) != 0);
        let input_format = ColorFormat::from_raw((flags >> 8) & 7);
        let output_format = ColorFormat::from_raw((flags >> 12) & 7);
        let downscale = (flags >> 24) & 3;

        let input_bpp = input_format.bytes_per_pixel();
        let output_bpp = output_format.bytes_per_pixel();

        let (scale_x, scale_y) = match downscale {
            1 => (2u32, 1u32),
            2 => (2, 2),
            _ => (1, 1),
        };
        // the output's size counts input pixels, a downscale shrinks it
        let output_width = (output_dimensions & 0xFFFF) / scale_x;
        let output_height = (output_dimensions >> 16) / scale_y;

        let copy_width = output_width.min(input_width / scale_x);
        let copy_height = output_height.min(input_height / scale_y);

        log::debug!(
            "display transfer: 0x{input_paddr:08X} {input_width}x{input_height} \
             {input_format:?} {} -> 0x{output_paddr:08X} {output_width}x{output_height} \
             {output_format:?} {} (flags 0x{flags:08X})",
            if input_linear { "linear" } else { "tiled" },
            if output_tiled { "tiled" } else { "linear" },
        );

        // what the host GPU drew it transfers itself, keeping the output
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            let transfer = raster::host::Transfer {
                input: input_base,
                output: output_base,
                input_width,
                input_height,
                output_width,
                output_height,
                copy: (copy_width, copy_height),
                scale: (scale_x, scale_y),
                flip: flip_vertically,
                input_linear,
                output_tiled,
                input_format,
                output_format,
                column: 0,
            };
            match hardware.display_transfer(memory, &transfer) {
                Ok(true) => {
                    self.transfers += 1;
                    return;
                }
                Ok(false) => {}
                Err(error) => log::error!("the GPU could not transfer a buffer, {error}"),
            }
        }
        // four bytes a pixel covers every format either side uses
        self.sync_memory_for(memory, input_base, input_width * input_height * 4, "sync for a CPU transfer from");
        self.sync_memory_for(memory, output_base, output_width * output_height * 4, "sync for a CPU transfer into");
        if copy_width == 0 || copy_height == 0 {
            return;
        }

        let mut input = vec![0u8; (input_width * input_height) as usize * input_bpp];
        memory.read(input_base, &mut input);
        let mut output = vec![0u8; (output_width * output_height) as usize * output_bpp];
        // preserve whatever was already there outside the copied rectangle.
        memory.read(output_base, &mut output);

        let trace_pixels = std::env::var("ZAKURO_TRACE_PIXELS").is_ok();
        let mut distinct = std::collections::HashSet::new();

        for y in 0..copy_height {
            for x in 0..copy_width {
                let dst = if output_tiled {
                    morton(x, y, output_width, output_bpp)
                } else {
                    (y * output_width + x) as usize * output_bpp
                };
                if dst + output_bpp > output.len() {
                    continue;
                }

                // a downscale averages each 2x1 or 2x2 block
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                for dy in 0..scale_y {
                    for dx in 0..scale_x {
                        let src_x = x * scale_x + dx;
                        let mut src_y = y * scale_y + dy;
                        if flip_vertically {
                            src_y = input_height.saturating_sub(1 + src_y);
                        }
                        let src = if input_linear {
                            (src_y * input_width + src_x) as usize * input_bpp
                        } else {
                            morton(src_x, src_y, input_width, input_bpp)
                        };
                        if src + input_bpp > input.len() {
                            continue;
                        }
                        let sample = input_format.decode(&input[src..src + input_bpp]);
                        for (total, value) in sum.iter_mut().zip(sample) {
                            *total += value as u32;
                        }
                        count += 1;
                    }
                }
                if count == 0 {
                    continue;
                }
                let pixel = sum.map(|total| ((total + count / 2) / count) as u8);
                if trace_pixels {
                    distinct.insert(pixel);
                }
                output_format.encode(pixel, &mut output[dst..dst + output_bpp]);
            }
        }
        if trace_pixels {
            log::debug!(
                "display transfer input 0x{input_paddr:08X}: {} distinct pixel(s), sample {:?}",
                distinct.len(),
                distinct.iter().take(5).collect::<Vec<_>>()
            );
        }

        memory.write(output_base, &output);
        self.transfers += 1;
    }

    /// TextureCopy, a raw byte copy with a stride, no format conversion.
    pub fn texture_copy<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        input_paddr: u32,
        output_paddr: u32,
        size: u32,
        input_gap: u32,
        output_gap: u32,
    ) {
        let input_width = (input_gap & 0xFFFF) * 16;
        let input_skip = (input_gap >> 16) * 16;
        // every line and the gaps between them
        let span = |width: u32, skip: u32| if width == 0 { size } else { size + size.div_ceil(width) * skip };
        let (input, output) = (memory.translate(input_paddr), memory.translate(output_paddr));
        let output_width = (output_gap & 0xFFFF) * 16;
        let output_skip = (output_gap >> 16) * 16;
        // a copy of what the host GPU drew it does itself, without reading
        // the picture back
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            match hardware.texture_copy(memory, input, output, size, (input_width, input_skip), (output_width, output_skip)) {
                Ok(true) => {
                    self.transfers += 1;
                    return;
                }
                Ok(false) => {}
                Err(error) => log::error!("the GPU could not copy a texture, {error}"),
            }
        }
        self.sync_memory_for(memory, input, span(input_width, input_skip), "sync for a copy from");
        self.sync_memory_for(memory, output, span(output_width, output_skip), "sync for a copy into");

        log::debug!(
            "texture copy: 0x{input_paddr:08X} -> 0x{output_paddr:08X} size 0x{size:X}"
        );

        let (mut src, mut dst) = (input, output);

        // a zero width means one contiguous run.
        if input_width == 0 || output_width == 0 {
            let mut buffer = vec![0u8; size as usize];
            memory.read(src, &mut buffer);
            memory.write(dst, &buffer);
            self.transfers += 1;
            return;
        }

        let mut remaining = size;
        let chunk = input_width.min(output_width);
        let mut buffer = vec![0u8; chunk as usize];
        while remaining >= chunk && chunk > 0 {
            memory.read(src, &mut buffer);
            memory.write(dst, &buffer);
            src += input_width + input_skip;
            dst += output_width + output_skip;
            remaining -= chunk;
        }
        self.transfers += 1;
    }

    // -- command lists ------------------------------------------------------

    /// walks a command list, applying its register writes and dispatching any
    /// draw it triggers to the software rasterizer.
    pub fn process_command_list<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        renderer: &mut dyn Renderer,
        paddr: u32,
        size: u32,
    ) {
        let start = std::time::Instant::now();
        self.resources.textures.begin_list();
        self.run_command_list(memory, renderer, paddr, size);
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_ref() {
            hardware.flush_draws();
        }
        self.busy += start.elapsed();
    }

    /// draws on the host's GPU from now on, rather than in software, and
    /// says which GPU that is.
    /// scale is how many times the console's resolution it draws at, for
    /// sharper pictures. on the device of a Vulkan presenter, when it gives
    /// one, the screens are shown straight from the images drawn.
    #[cfg(feature = "gles")]
    pub fn enable_hardware_renderer(&mut self, scale: u32) -> Result<String, String> {
        let mut hardware = raster::gles::Gles::new()?;
        let scale = hardware.set_scale(scale);
        let shaded = if hardware.shades() { "" } else { ", vertices shaded on the CPU," };
        let name = format!("{} at {scale}x{shaded}", hardware.name());
        self.resources.hardware = Some(hardware);
        Ok(name)
    }

    /// the host's GPU for the panel alone, every draw left to the software
    /// rasterizer.
    #[cfg(feature = "gles")]
    pub fn enable_panel(&mut self) -> Result<(), String> {
        self.panel = Some(raster::HostRenderer::new()?);
        Ok(())
    }

    /// shows the screens on the panel the GPU drives itself, each in its
    /// area, left, top, width and height in the panel's pixels, the overlay
    /// over them, the top screen filtered when smooth and a picture halfway
    /// between its last two shown first when interpolating.
    pub fn present_panel(&mut self, shown: [Option<(PanelImage, [i32; 4])>; 2], cursor: Option<(i32, i32)>, overlay: Option<std::sync::Arc<Overlay>>, smooth: bool, interpolate: bool) {
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut().or(self.panel.as_mut()) {
            hardware.present(shown, cursor, overlay, smooth, interpolate);
        }
        #[cfg(not(feature = "gles"))]
        let _ = (shown, cursor, overlay, smooth, interpolate);
    }

    /// how many times the console's resolution the host's GPU draws at, 1
    /// when drawing in software.
    pub fn scale(&self) -> u32 {
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_ref() {
            return hardware.scale();
        }
        1
    }

    /// the newest picture the host's GPU drew for a screen's buffer, at the
    /// scale it draws at, while guest memory still holds the same picture.
    /// size is the pixels of a row the screen shows and its rows, stride the
    /// pixels a row takes in memory.
    pub fn scaled_screen(&mut self, addr: u32, size: (u32, u32), stride: u32, format: ColorFormat, guest: &[u8]) -> Option<ScreenRef> {
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            return hardware.screen(addr, size, stride, format, guest);
        }
        #[cfg(not(feature = "gles"))]
        let _ = (addr, size, stride, format, guest);
        None
    }

    /// a screen's picture upright, RGBA the way the screen shows it, and the
    /// scale it is at, once the GPU finished it.
    pub fn scaled_picture(&mut self, screen: ScreenRef) -> Option<Picture> {
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            match hardware.picture(screen) {
                Ok(picture) => return picture,
                Err(error) => log::error!("the GPU could not show a screen, {error}"),
            }
        }
        #[cfg(not(feature = "gles"))]
        let _ = screen;
        None
    }

    /// makes guest memory right over a range something other than a draw
    /// is about to read or write, the host GPU keeps what it draws until then.
    pub fn sync_memory<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) {
        self.sync_memory_for(memory, addr, len, "sync");
    }

    /// sync_memory, saying what for, which the GL waits in the log name.
    pub fn sync_memory_for<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32, why: &'static str) {
        #[cfg(feature = "gles")]
        raster::gles::sync_why(why);
        #[cfg(not(feature = "gles"))]
        let _ = why;
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            if let Err(error) = hardware.sync(memory, addr, len) {
                log::error!("the GPU could not write back a buffer, {error}");
            }
        }
        #[cfg(not(feature = "gles"))]
        let _ = (memory, addr, len);
    }

    pub fn before_overwrite<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) {
        #[cfg(feature = "gles")]
        if let Some(hardware) = self.resources.hardware.as_mut() {
            if let Err(error) = hardware.before_overwrite(memory, addr, len) {
                log::error!("the GPU could not write back a buffer, {error}");
            }
        }
        #[cfg(not(feature = "gles"))]
        let _ = (memory, addr, len);
    }

    fn run_command_list<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        renderer: &mut dyn Renderer,
        paddr: u32,
        size: u32,
    ) {
        let mut words = std::mem::take(&mut self.list);
        read_command_buffer(memory, paddr, size, &mut words);
        self.hash_words(&words);
        // buffers jump into each other, and a buffer jumping to itself
        // would never end, real lists stay far below this.
        let mut jumps = 0u32;
        self.pending_jump = None;

        let mut index = 0usize;
        while index + 1 < words.len() {
            let data = words[index];
            let header = words[index + 1];
            index += 2;

            let register = (header & 0xFFFF) as usize;
            let mask = (header >> 16) & 0xF;
            let extra = ((header >> 20) & 0xFF) as usize;
            let consecutive = header & 0x8000_0000 != 0;

            // no real PICA register lives anywhere near this index (the whole
            // internal file is 0x300 words), seeing one would mean a desync in
            // the parsing above.
            if register >= self.internal.len() {
                log::debug!(
                    "command list stops at word {}/{}: register 0x{register:04X} is out of range",
                    index - 2,
                    words.len()
                );
                break;
            }

            self.write_internal(memory, renderer, register, data, mask);

            // a burst into one register, uniforms, program words and light
            // tables the most of what a list carries, goes to the units
            // directly
            let offset = register.wrapping_sub(REG_VS_BLOCK);
            let uniforms = matches!(offset, SHADER_UNIFORM_DATA..=SHADER_UNIFORM_DATA_END);
            let program = matches!(offset, SHADER_PROGRAM_DATA..=SHADER_PROGRAM_DATA_END);
            let table = (lighting::REG_TABLE_DATA..=lighting::REG_TABLE_DATA_END).contains(&register);
            if extra > 0 && !consecutive && mask == 0xF && (uniforms || program || table || !ACTS[register]) && self.immediate.vertices.is_empty() {
                let end = (index + extra).min(words.len());
                let burst = &words[index..end];
                if uniforms {
                    for &value in burst {
                        self.vertex_shader.upload_float_uniform(value);
                    }
                } else if program {
                    let mirror = self.internal[REG_VS_COM_MODE] & 1 == 0;
                    for &value in burst {
                        self.vertex_shader.upload_program(value);
                        if mirror {
                            self.geometry_shader.upload_program(value);
                        }
                        renderer.upload_shader_code(value);
                    }
                } else if table {
                    self.resources.light_tables.write(&mut self.internal, burst);
                }
                if let Some(&last) = burst.last() {
                    self.internal[register] = last;
                }
                index = end;
                if !extra.is_multiple_of(2) {
                    index += 1;
                }
                index = index.min(words.len());
                continue;
            }

            for i in 0..extra {
                if index >= words.len() {
                    break;
                }
                let value = words[index];
                index += 1;
                let target = if consecutive { register + i + 1 } else { register };
                self.write_internal(memory, renderer, target, value, mask);
            }

            // each command is padded so its total length (the base pair plus
            // extra words) is a multiple of two, an odd extra needs one
            // more word to reach that, an even one is already there.
            if !extra.is_multiple_of(2) {
                index += 1;
            }
            index = index.min(words.len());

            // a jump replaces the list being executed.
            if let Some((address, size)) = self.pending_jump.take() {
                jumps += 1;
                if jumps > 0x10000 {
                    log::warn!("command list jumps more than {jumps} times; stopping");
                    break;
                }
                read_command_buffer(memory, address, size, &mut words);
                self.hash_words(&words);
                index = 0;
            }
        }

        self.list = words;
        self.flush_immediate(memory);
        self.command_lists += 1;
    }

    /// decodes the programs a draw runs. the geometry unit gets the vertex
    /// shader's uploads too, and decoding hashes a whole program, so it is
    /// left alone while the geometry stage is off.
    fn prepare_shaders(&mut self) {
        self.vertex_shader.prepare();
        if self.internal[REG_GEOSTAGE_CONFIG] & 0x3 == 2 {
            self.geometry_shader.prepare();
        }
    }

    /// folds a command list into the picture's hash, in fast mode, while
    /// bottom screen skipping can still use it. four lanes at once.
    fn hash_words(&mut self, words: &[u32]) {
        if !fast() || !self.skip_bottom || self.both_screens {
            return;
        }
        let step = |hash: u64, word: u32| (hash.rotate_left(5) ^ word as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        let (blocks, rest) = words.as_chunks::<4>();
        let mut lanes = [self.picture_hash, 1, 2, 3];
        for block in blocks {
            for (lane, &word) in lanes.iter_mut().zip(block) {
                *lane = step(*lane, word);
            }
        }
        let hash = lanes.into_iter().fold(0, |hash, lane| step(hash, lane as u32) ^ lane);
        self.picture_hash = rest.iter().fold(hash, |hash, &word| step(hash, word));
    }

    /// the screen and the buffer of it a range of memory overlaps most, of
    /// the last few the screens were given, the one given last of those
    /// that tie.
    fn on_screen<M: GpuMemory>(&self, memory: &M, start: u32, length: u32) -> Option<(usize, u32)> {
        let start = memory.translate(start) as u64;
        let end = start + length as u64;
        let overlaps = self.screen_buffers.iter().enumerate().filter_map(|(given, &(screen, buffer))| {
            let at = memory.translate(buffer) as u64;
            let size = self.framebuffers[screen].stride as u64 * if screen == 0 { 400 } else { 320 };
            let overlap = end.min(at + size).saturating_sub(start.max(at));
            (overlap > 0).then_some(((overlap, given), (screen, buffer)))
        });
        overlaps.max_by_key(|&(key, _)| key).map(|(_, found)| found)
    }

    /// a frame of the title's begins, skipped when the frontend is behind,
    /// or when the panel will not show it and the title sent a top picture
    /// in each of the two frames before: drawing every other frame of a title
    /// that makes one a frame loses nothing shown, one that makes one every
    /// other frame never sends two in a row.
    pub fn next_frame(&mut self, behind: bool, unshown: bool) {
        self.top_sent_before = [self.top_sent_before[1], self.top_sent];
        self.top_sent = false;
        self.skip_draws = behind || (unshown && self.top_sent_before == [true, true]);
        self.keep_bottom = !behind;
    }

    fn right_eye_off(&mut self, why: &str) {
        if self.right_eye_wrong {
            return;
        }
        if self.right_eye_seen < 60 {
            self.right_eye_short += 1;
        }
        if self.right_eye_short < 3 {
            log::info!("right eye skipping paused after {} pictures, {why}", self.right_eye_seen);
            (self.right_eye_seen, self.right_eye_candidate) = (0, false);
        } else {
            log::info!("switch: right eye skipping off, {why}");
            (self.right_eye_wrong, self.right_eye_candidate) = (true, false);
        }
    }

    /// whether the frame being drawn is skipped, decided at its first draw.
    fn skips(&mut self) -> bool {
        let height = (raster::read_float24(&self.internal, registers::REG_VIEWPORT_HEIGHT) * 2.0).round();
        let screen = if height == 320.0 { 1 } else if height == 400.0 { 2 } else { 0 };
        if !self.drawing {
            self.drawing = true;
            let skip_draws = self.skip_draws && !(screen == 1 && self.keep_bottom);
            if skip_draws {
                self.skipped_at = self.transfers;
            }
            (self.screens_drawn, self.skipping_frame) = (screen, skip_draws);
            let target = self.internal[REG_COLOR_BUFFER_ADDRESS] << 3;
            self.right_eye_candidate = screen == 2 && self.stereo && !self.right_eye_wrong && self.left_eye == Some(target);
            let right_eye = self.right_eye_candidate && fast() && self.skip_right_eye && self.right_eye_seen >= 4;
            if screen == 2 {
                self.left_eye = None;
            }
            self.period_target = target;
            self.skipped_bottom = screen == 1 && self.skips_bottom();
            self.skipping = skip_draws || right_eye || self.skipped_bottom;
            self.skipped_targets.clear();
        } else if self.screens_drawn | screen != self.screens_drawn {
            if self.screens_drawn == 0 && screen == 2 {
                self.period_target = self.internal[REG_COLOR_BUFFER_ADDRESS] << 3;
            }
            self.screens_drawn |= screen;
            if self.screens_drawn == 3 {
                if self.right_eye_candidate {
                    self.right_eye_off("a picture taken for the right eye's drew the bottom screen too");
                }
                if !self.both_screens {
                    log::info!("switch: bottom screen skipping off, the title drew both screens between two transfers");
                }
                self.both_screens = true;
                self.skipped_bottom = false;
                if self.skipping && !self.skipping_frame {
                    self.skipping = false;
                    self.skipped_targets.clear();
                }
            }
        }
        if self.skipping {
            let dimensions = self.internal[REG_FRAMEBUFFER_DIMENSIONS];
            let bytes = match ColorFormat::from_color_buffer((self.internal[REG_COLOR_BUFFER_FORMAT] >> 16) & 7) {
                ColorFormat::Rgba8 => 4,
                ColorFormat::Rgb8 => 3,
                _ => 2,
            };
            let start = self.internal[REG_COLOR_BUFFER_ADDRESS] << 3;
            let target = (start, start + (dimensions & 0x7FF) * (((dimensions >> 12) & 0x3FF) + 1) * bytes);
            if !self.skipped_targets.contains(&target) {
                self.skipped_targets.push(target);
            }
        }
        self.skipping
    }

    /// in fast mode the bottom screen, which mostly stands still, is drawn
    /// every other picture while its pictures change, every third while the
    /// frontend has been skipping pictures to keep up, and one in six while
    /// they stay the same, the panel showing the last one drawn meanwhile. a
    /// change in its first command list after a still one shows at once, one
    /// further in a picture after. none is skipped before the title hands the
    /// bottom screen a picture by display transfer, which some never do, nor
    /// for a title drawing both screens between transfers.
    fn skips_bottom(&mut self) -> bool {
        if self.bottom_relearn && self.both_at.is_some_and(|at| self.transfers - at >= 300) {
            log::info!("switch: bottom screen skipping back on");
            (self.both_screens, self.both_at) = (false, None);
        }
        if !fast() || !self.skip_bottom || self.bottom_shown.is_none() || self.both_screens {
            return false;
        }
        let first = std::mem::replace(&mut self.bottom_first, self.picture_hash) == self.picture_hash;
        if first && self.bottom_hashes[0] == self.bottom_hashes[1] {
            self.bottom_skipped < 5
        } else {
            let behind = self.transfers.saturating_sub(self.skipped_at) < 90;
            self.bottom_skipped < if self.bottom_full_rate { 0 } else if behind { 2 } else { 1 }
        }
    }

    /// the address of the buffer a screen shows, for the bottom one in fast
    /// mode the one drawn last.
    pub fn shown_address(&self, screen: usize) -> u32 {
        let config = &self.framebuffers[screen];
        let shown = match self.bottom_shown {
            Some(shown) if screen == 1 && [config.address_a_left, config.address_b_left].contains(&shown) => shown,
            _ => config.address_left(),
        };
        match self.fresh[screen] {
            Some(fresh) if self.stale[screen].contains(&shown) => fresh,
            _ => shown,
        }
    }

    /// a draw the command list asked for, kept out of the register write
    /// path that runs for every word.
    #[inline(never)]
    fn draw<M: GpuMemory>(&mut self, memory: &mut M, renderer: &mut dyn Renderer, indexed: bool) {
        self.draw_calls += 1;
        renderer.draw(DrawCall {
            indexed,
            registers: &self.internal,
        });
        self.prepare_shaders();
        let vertices = raster::draw(
            &self.internal,
            &self.vertex_shader,
            &self.geometry_shader,
            &self.fixed_attributes,
            memory,
            &mut self.resources,
            indexed,
        );
        self.vertices_drawn += vertices as u64;
    }

    /// draws any immediate-mode vertices still waiting.
    #[inline(never)]
    fn flush_immediate<M: GpuMemory>(&mut self, memory: &mut M) {
        if self.immediate.vertices.is_empty() {
            return;
        }
        let vertices = std::mem::take(&mut self.immediate.vertices);
        self.draw_calls += 1;
        if self.skips() {
            return;
        }
        self.vertices_drawn += vertices.len() as u64;
        self.prepare_shaders();
        raster::draw_immediate(
            &self.internal,
            &self.vertex_shader,
            &self.geometry_shader,
            memory,
            &mut self.resources,
            &vertices,
        );
    }

    #[inline(always)]
    fn write_internal<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        renderer: &mut dyn Renderer,
        register: usize,
        value: u32,
        mask: u32,
    ) {
        if register >= self.internal.len() {
            return;
        }

        // immediate-mode vertices are drawn with the state they were sent
        // under, so anything else touching a register draws them first.
        if !self.immediate.vertices.is_empty() && !(REG_FIXED_ATTRIBUTE_DATA..=REG_FIXED_ATTRIBUTE_DATA_END).contains(&register) {
            self.flush_immediate(memory);
        }

        // the mask selects which bytes of the register the write touches, a set
        // bit writes that byte.
        const BYTES: [u32; 16] = {
            let mut masks = [0u32; 16];
            let mut mask = 0;
            while mask < 16 {
                let mut byte = 0;
                while byte < 4 {
                    if mask & (1 << byte) != 0 {
                        masks[mask] |= 0xFF << (byte * 8);
                    }
                    byte += 1;
                }
                mask += 1;
            }
            masks
        };
        let write_mask = BYTES[(mask & 0xF) as usize];
        let old = self.internal[register];
        let new = (old & !write_mask) | (value & write_mask);
        self.internal[register] = new;
        if ACTS[register] {
            self.act(memory, renderer, register, new);
        }
    }

    #[inline(never)]
    fn act<M: GpuMemory>(&mut self, memory: &mut M, renderer: &mut dyn Renderer, register: usize, new: u32) {
        match register {
            REG_DRAW_ARRAYS | REG_DRAW_ELEMENTS if self.skips() => {
                self.draw_calls += 1;
            }
            REG_DRAW_ARRAYS | REG_DRAW_ELEMENTS => self.draw(memory, renderer, register == REG_DRAW_ELEMENTS),

            REG_CMDBUF_JUMP0 | REG_CMDBUF_JUMP1 => {
                // every 3D model lives behind one of these jumps. spent way too long
                // wondering why the professor wasn't there, fml
                let channel = register - REG_CMDBUF_JUMP0;
                let size = (self.internal[REG_CMDBUF_SIZE0 + channel] & 0x1F_FFFF) * 8;
                let address = (self.internal[REG_CMDBUF_ADDR0 + channel] & 0x1FFF_FFFF) * 8;
                self.pending_jump = Some((address, size));
            }

            REG_FIXED_ATTRIBUTE_INDEX => {
                self.fixed_attribute_words = 0;
                self.immediate.next_attribute = 0;
            }
            REG_FIXED_ATTRIBUTE_DATA..=REG_FIXED_ATTRIBUTE_DATA_END => {
                self.fixed_attribute_staging[self.fixed_attribute_words] = new;
                self.fixed_attribute_words += 1;
                if self.fixed_attribute_words == 3 {
                    self.fixed_attribute_words = 0;
                    // packed like float uniforms, four 24-bit floats in
                    // three words, w first.
                    let [a, b, c] = self.fixed_attribute_staging;
                    let attribute = [
                        shader::isa::decode_float24(c & 0x00FF_FFFF),
                        shader::isa::decode_float24(((b & 0xFFFF) << 8) | (c >> 24)),
                        shader::isa::decode_float24(((a & 0xFF) << 16) | (b >> 16)),
                        shader::isa::decode_float24(a >> 8),
                    ];
                    let index = (self.internal[REG_FIXED_ATTRIBUTE_INDEX] & 0xF) as usize;
                    if index < 15 {
                        self.fixed_attributes[index] = attribute;
                        // the index moves on so consecutive attributes can
                        // be set in one burst.
                        self.internal[REG_FIXED_ATTRIBUTE_INDEX] =
                            (self.internal[REG_FIXED_ATTRIBUTE_INDEX] & !0xF) | (index as u32 + 1);
                    } else {
                        // immediate mode, attributes arrive in order, and
                        // the last one completes a vertex.
                        let immediate = &mut self.immediate;
                        immediate.attributes[immediate.next_attribute] = attribute;
                        let last = (self.internal[REG_VS_ATTRIBUTE_COUNT] & 0xF) as usize;
                        if immediate.next_attribute < last {
                            immediate.next_attribute += 1;
                        } else {
                            immediate.next_attribute = 0;
                            immediate.vertices.push(immediate.attributes);
                        }
                    }
                }
            }

            lighting::REG_TABLE_DATA..=lighting::REG_TABLE_DATA_END => {
                self.resources.light_tables.write(&mut self.internal, &[new]);
            }
            proctex::REG_TABLE_DATA..=proctex::REG_TABLE_DATA_END => {
                self.resources.proctex_tables.write(&mut self.internal, new);
            }
            REG_GS_BLOCK..=REG_GS_BLOCK_END => {
                configure_shader(&mut self.geometry_shader, register - REG_GS_BLOCK, new);
            }
            REG_VS_BLOCK..=REG_VS_BLOCK_END => {
                let offset = register - REG_VS_BLOCK;
                configure_shader(&mut self.vertex_shader, offset, new);
                // the geometry shader unit takes the vertex shader's program
                // and descriptors as well, unless the title configures it on
                // its own.
                let program = matches!(
                    offset,
                    SHADER_PROGRAM_INDEX..=SHADER_PROGRAM_DATA_END
                        | SHADER_DESCRIPTOR_INDEX..=SHADER_DESCRIPTOR_DATA_END
                );
                if program && self.internal[REG_VS_COM_MODE] & 1 == 0 {
                    configure_shader(&mut self.geometry_shader, offset, new);
                }
                match offset {
                    SHADER_PROGRAM_DATA..=SHADER_PROGRAM_DATA_END => renderer.upload_shader_code(new),
                    SHADER_DESCRIPTOR_DATA..=SHADER_DESCRIPTOR_DATA_END => {
                        renderer.upload_shader_operand_descriptor(new)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// reads a command buffer as words.
fn read_command_buffer<M: GpuMemory>(memory: &mut M, paddr: u32, size: u32, words: &mut Vec<u32>) {
    let addr = memory.translate(paddr);
    words.clear();
    words.resize(size as usize / 4, 0);
    // SAFETY: the words' own bytes
    let bytes = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 4) };
    match memory.slice(addr, bytes.len()) {
        Some(found) => bytes.copy_from_slice(found),
        None => memory.read(addr, bytes),
    }
    for word in words.iter_mut() {
        *word = u32::from_le(*word);
    }
}

const REG_GS_BLOCK_END: usize = REG_GS_BLOCK + SHADER_BLOCK_SIZE - 1;
const REG_VS_BLOCK_END: usize = REG_VS_BLOCK + SHADER_BLOCK_SIZE - 1;

/// the registers a write does more to than store, as act matches them.
const ACTS: [bool; INTERNAL_REGISTER_WORDS] = {
    let mut acts = [false; INTERNAL_REGISTER_WORDS];
    let ranges = [
        (REG_DRAW_ARRAYS, REG_DRAW_ELEMENTS),
        (REG_CMDBUF_JUMP0, REG_CMDBUF_JUMP1),
        (REG_FIXED_ATTRIBUTE_INDEX, REG_FIXED_ATTRIBUTE_DATA_END),
        (lighting::REG_TABLE_DATA, lighting::REG_TABLE_DATA_END),
        (proctex::REG_TABLE_DATA, proctex::REG_TABLE_DATA_END),
        (REG_GS_BLOCK, REG_GS_BLOCK_END),
        (REG_VS_BLOCK, REG_VS_BLOCK_END),
    ];
    let mut at = 0;
    while at < ranges.len() {
        let mut register = ranges[at].0;
        while register <= ranges[at].1 {
            acts[register] = true;
            register += 1;
        }
        at += 1;
    }
    acts
};

/// applies a write to one of a shader unit's configuration registers.
fn configure_shader(unit: &mut shader::ShaderUnit, offset: usize, value: u32) {
    match offset {
        SHADER_BOOL_UNIFORMS => unit.bool_uniforms = (value & 0xFFFF) as u16,
        SHADER_INT_UNIFORMS..=SHADER_INT_UNIFORMS_END => {
            unit.int_uniforms[offset - SHADER_INT_UNIFORMS] = value.to_le_bytes();
        }
        SHADER_ENTRY_POINT => unit.entry_point = value & (shader::PROGRAM_SIZE as u32 - 1),
        SHADER_UNIFORM_INDEX => unit.set_float_uniform_index(value),
        SHADER_UNIFORM_DATA..=SHADER_UNIFORM_DATA_END => unit.upload_float_uniform(value),
        SHADER_PROGRAM_INDEX => {
            unit.program_write_offset = value as usize & (shader::PROGRAM_SIZE - 1);
        }
        SHADER_PROGRAM_DATA..=SHADER_PROGRAM_DATA_END => unit.upload_program(value),
        SHADER_DESCRIPTOR_INDEX => {
            unit.descriptor_write_offset = value as usize & (shader::DESCRIPTOR_SIZE - 1);
        }
        SHADER_DESCRIPTOR_DATA..=SHADER_DESCRIPTOR_DATA_END => unit.upload_descriptor(value),
        _ => {}
    }
}

#[inline]
fn morton(x: u32, y: u32, width: u32, bytes_per_pixel: usize) -> usize {
    format::morton_offset(x, y, width, bytes_per_pixel as u32) as usize
}

/// converts a GSP register offset to an index into our external register file.
fn external_index(gsp_offset: u32) -> Option<usize> {
    // GSP offsets are relative to 0x1EB00000, but the GPU's registers start at
    // 0x1EF00000, so the first 0x400000 bytes are other hardware.
    let byte_offset = gsp_offset.checked_sub(0x0040_0000)?;
    Some(byte_offset as usize / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// guest memory as one flat block starting at address zero.
    struct FlatMemory(Vec<u8>);

    impl GpuMemory for FlatMemory {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            let start = addr as usize;
            out.copy_from_slice(&self.0[start..start + out.len()]);
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            let start = addr as usize;
            self.0[start..start + data.len()].copy_from_slice(data);
        }
    }

    /// the inverse of [shader::isa::decode_float24], for normal values.
    fn float24(value: f32) -> u32 {
        if value == 0.0 {
            return 0;
        }
        let bits = value.to_bits();
        let sign = bits >> 31;
        let exponent = ((bits >> 23) & 0xFF) + 63 - 127;
        (sign << 23) | (exponent << 16) | ((bits >> 7) & 0xFFFF)
    }

    /// packs a vector the way the fixed attribute registers take it.
    fn pack(v: [f32; 4]) -> [u32; 3] {
        let [x, y, z, w] = v.map(float24);
        [(w << 8) | (z >> 16), ((z & 0xFFFF) << 16) | (y >> 8), ((y & 0xFF) << 24) | x]
    }

    const COLOR_BUFFER: u32 = 0x1000;

    /// A GPU set up to draw an 8x8 RGBA8 target with a shader that passes
    /// v0 through as the position and v1 as the color.
    fn gpu_for_immediate_draws(memory: &mut FlatMemory, renderer: &mut SoftwareRenderer) -> Gpu {
        let mut gpu = Gpu::new();
        let mov = 0x13 << 26;
        gpu.vertex_shader.program[..3]
            .copy_from_slice(&[mov, mov | (1 << 21) | (1 << 12), 0x22 << 26]);
        gpu.vertex_shader.descriptors[0] = 0xF | (0b00_01_10_11 << 5) | (0b00_01_10_11 << 14);

        let mut write = |register: usize, value: u32| {
            gpu.write_internal(memory, renderer, register, value, 0xF)
        };
        write(REG_SHADER_OUTPUT_TOTAL, 2);
        write(REG_SHADER_OUTPUT_MAP, 0x0302_0100); // o0 = position
        write(REG_SHADER_OUTPUT_MAP + 1, 0x0B0A_0908); // o1 = color
        write(REG_VS_INPUT_REGISTER_MAP_LOW, 0x10); // attribute n -> vn
        write(REG_VS_ATTRIBUTE_COUNT, 1); // two attributes per vertex
        write(REG_VIEWPORT_WIDTH, float24(4.0));
        write(REG_VIEWPORT_HEIGHT, float24(4.0));
        write(REG_COLOR_BUFFER_ADDRESS, COLOR_BUFFER >> 3);
        write(REG_FRAMEBUFFER_DIMENSIONS, 8 | (7 << 12));
        // color writes allowed, all four channels.
        write(REG_COLOR_BUFFER_WRITE, 0xF);
        write(REG_DEPTH_COLOR_MASK, 0xF << 8);
        write(REG_LOGIC_OP, 3);
        gpu
    }

    fn send_vertex(gpu: &mut Gpu, memory: &mut FlatMemory, renderer: &mut SoftwareRenderer, attributes: &[[f32; 4]]) {
        for attribute in attributes {
            for (i, word) in pack(*attribute).into_iter().enumerate() {
                gpu.write_internal(memory, renderer, REG_FIXED_ATTRIBUTE_DATA + i, word, 0xF);
            }
        }
    }

    #[test]
    fn immediate_mode_vertices_are_drawn() {
        let mut memory = FlatMemory(vec![0; 0x2000]);
        let mut renderer = SoftwareRenderer::default();
        let mut gpu = gpu_for_immediate_draws(&mut memory, &mut renderer);

        gpu.write_internal(&mut memory, &mut renderer, REG_FIXED_ATTRIBUTE_INDEX, 0xF, 0xF);
        let red = [1.0, 0.0, 0.0, 1.0];
        // one triangle big enough to cover the whole target.
        for position in [[-1.0, -1.0, 0.0, 1.0], [3.0, -1.0, 0.0, 1.0], [-1.0, 3.0, 0.0, 1.0]] {
            send_vertex(&mut gpu, &mut memory, &mut renderer, &[position, red]);
        }
        assert_eq!(gpu.draw_calls, 0, "nothing is drawn until the state moves on");

        // leaving immediate mode is an ordinary register write.
        gpu.write_internal(&mut memory, &mut renderer, 0x245, 1, 0xF);
        assert_eq!(gpu.draw_calls, 1);
        assert_eq!(gpu.vertices_drawn, 3);
        for pixel in memory.0[COLOR_BUFFER as usize..][..8 * 8 * 4].chunks(4) {
            assert_eq!(ColorFormat::Rgba8.decode(pixel), [255, 0, 0, 255]);
        }
    }

    #[test]
    fn fixed_attributes_below_fifteen_are_defaults_not_vertices() {
        let mut memory = FlatMemory(vec![0; 0x2000]);
        let mut renderer = SoftwareRenderer::default();
        let mut gpu = gpu_for_immediate_draws(&mut memory, &mut renderer);

        gpu.write_internal(&mut memory, &mut renderer, REG_FIXED_ATTRIBUTE_INDEX, 2, 0xF);
        send_vertex(&mut gpu, &mut memory, &mut renderer, &[[0.5, 0.25, 0.0, 1.0]]);
        assert_eq!(gpu.fixed_attributes[2], [0.5, 0.25, 0.0, 1.0]);
        // the index moves on to the next attribute.
        assert_eq!(gpu.internal[REG_FIXED_ATTRIBUTE_INDEX] & 0xF, 3);
        gpu.write_internal(&mut memory, &mut renderer, 0x245, 1, 0xF);
        assert_eq!(gpu.draw_calls, 0);
    }

    /// a point goes in, the geometry shader emits a triangle covering the
    /// whole target from it, the way particle systems expand their points.
    #[test]
    fn a_geometry_shader_expands_a_point() {
        let mut memory = FlatMemory(vec![0; 0x2000]);
        let mut renderer = SoftwareRenderer::default();
        let mut gpu = gpu_for_immediate_draws(&mut memory, &mut renderer);

        let mov = |dest: u32, source: u32| (0x13 << 26) | (dest << 21) | (source << 12);
        let setemit = |slot: u32, primitive: bool| (0x2B << 26) | (slot << 24) | ((primitive as u32) << 23);
        let emit = 0x2A << 26;
        let program = [
            setemit(0, false),
            mov(0, 0x20), // o0 = c0
            mov(1, 1),    // o1 = v1, the point's color
            emit,
            setemit(1, false),
            mov(0, 0x21),
            emit,
            setemit(2, true),
            mov(0, 0x22),
            emit,
            0x22 << 26,
        ];
        gpu.geometry_shader.program[..program.len()].copy_from_slice(&program);
        gpu.geometry_shader.descriptors[0] = 0xF | (0b00_01_10_11 << 5) | (0b00_01_10_11 << 14);
        gpu.geometry_shader.float_uniforms[0] = [-1.0, -1.0, 0.0, 1.0];
        gpu.geometry_shader.float_uniforms[1] = [3.0, -1.0, 0.0, 1.0];
        gpu.geometry_shader.float_uniforms[2] = [-1.0, 3.0, 0.0, 1.0];

        let mut write = |register: usize, value: u32| {
            gpu.write_internal(&mut memory, &mut renderer, register, value, 0xF)
        };
        write(REG_GEOSTAGE_CONFIG, 2);
        write(REG_VS_COM_MODE, 1);
        write(REG_VS_OUTPUT_TOTAL, 1); // two attributes per vertex
        write(REG_GS_BLOCK + SHADER_INPUT_CONFIG, 1); // one vertex per invocation
        write(REG_GS_BLOCK + SHADER_INPUT_MAP_LOW, 0x10);
        write(REG_PRIMITIVE_CONFIG, 3 << 8);
        write(REG_FIXED_ATTRIBUTE_INDEX, 0xF);

        let green = [0.0, 1.0, 0.0, 1.0];
        send_vertex(&mut gpu, &mut memory, &mut renderer, &[[0.0, 0.0, 0.0, 1.0], green]);
        gpu.write_internal(&mut memory, &mut renderer, 0x245, 1, 0xF);

        for pixel in memory.0[COLOR_BUFFER as usize..][..8 * 8 * 4].chunks(4) {
            assert_eq!(ColorFormat::Rgba8.decode(pixel), [0, 255, 0, 255]);
        }
    }

    /// unless the geometry unit is configured separately, it runs the same
    /// program the vertex shader was given.
    #[test]
    fn the_geometry_unit_mirrors_the_vertex_program_by_default() {
        let mut memory = FlatMemory(vec![0; 0x100]);
        let mut renderer = SoftwareRenderer::default();
        let mut gpu = Gpu::new();
        let mut write = |register: usize, value: u32| {
            gpu.write_internal(&mut memory, &mut renderer, register, value, 0xF)
        };
        write(REG_VS_BLOCK + SHADER_PROGRAM_INDEX, 0);
        write(REG_VS_BLOCK + SHADER_PROGRAM_DATA, 0x1234_5678);
        write(REG_VS_COM_MODE, 1);
        write(REG_VS_BLOCK + SHADER_PROGRAM_DATA, 0x9ABC_DEF0);
        assert_eq!(gpu.vertex_shader.program[..2], [0x1234_5678, 0x9ABC_DEF0]);
        assert_eq!(gpu.geometry_shader.program[..2], [0x1234_5678, 0]);
    }

    /// one register write, as a command list encodes it.
    fn command(register: usize, value: u32) -> [u32; 2] {
        [value, register as u32 | (0xF << 16)]
    }

    /// the main list jumps into a sub-buffer through channel 0, and the
    /// sub-buffer jumps back to the rest of the main list through
    /// channel 1, commands on both sides of the jump run.
    #[test]
    fn command_buffers_jump_into_each_other() {
        const MAIN: u32 = 0x100;
        const RETURN: u32 = 0x140;
        const SUB: u32 = 0x200;
        let mut memory = FlatMemory(vec![0; 0x400]);
        let mut write_words = |address: u32, words: &[u32]| {
            for (i, word) in words.iter().enumerate() {
                let at = address as usize + i * 4;
                memory.0[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        };
        let main: Vec<u32> = [
            command(REG_CMDBUF_ADDR0, SUB >> 3),
            command(REG_CMDBUF_SIZE0, 32 >> 3),
            command(REG_CMDBUF_ADDR0 + 1, RETURN >> 3),
            command(REG_CMDBUF_SIZE0 + 1, 8 >> 3),
            command(REG_CMDBUF_JUMP0, 1),
            // never reached, the jump leaves this list for good.
            command(0x0101, 0xDEAD),
        ]
        .concat();
        write_words(MAIN, &main);
        write_words(RETURN, &command(0x0102, 0x2222));
        let sub: Vec<u32> = [
            command(0x0100, 0x1111),
            command(REG_CMDBUF_JUMP1, 1),
            [0, 0],
            [0, 0],
        ]
        .concat();
        write_words(SUB, &sub);

        let mut gpu = Gpu::new();
        let mut renderer = SoftwareRenderer::default();
        gpu.process_command_list(&mut memory, &mut renderer, MAIN, main.len() as u32 * 4);
        assert_eq!(gpu.internal[0x0100], 0x1111, "the sub-buffer ran");
        assert_eq!(gpu.internal[0x0102], 0x2222, "the jump back ran");
        assert_eq!(gpu.internal[0x0101], 0, "a jump does not return");
    }

    #[test]
    fn a_downscaling_transfer_averages_each_block() {
        let mut memory = FlatMemory(vec![0; 0x200]);
        let reds = [[0u8, 100, 200, 0], [100, 0, 0, 200]];
        for (y, row) in reds.iter().enumerate() {
            for (x, red) in row.iter().enumerate() {
                let at = 0x100 + (y * 4 + x) * 4;
                ColorFormat::Rgba8.encode([*red, 0, 0, 255], &mut memory.0[at..at + 4]);
            }
        }
        let mut gpu = Gpu::new();
        // linear in and out, RGBA8, 2x2 downscale, the output's size counts
        // input pixels
        let flags = (1 << 1) | (1 << 5) | (2 << 24);
        gpu.display_transfer(&mut memory, 0x100, 0x180, 4 | (2 << 16), 4 | (2 << 16), flags);
        let red_at = |i: usize| ColorFormat::Rgba8.decode(&memory.0[0x180 + i * 4..0x184 + i * 4])[0];
        assert_eq!([red_at(0), red_at(1)], [50, 100]);
    }

    #[test]
    fn a_right_eye_miss_pauses_the_skipping_until_it_keeps_coming() {
        let mut gpu = Gpu::new();
        for _ in 0..2 {
            gpu.right_eye_seen = 200;
            gpu.right_eye_off("a long run broke");
        }
        assert!(!gpu.right_eye_wrong, "misses after long runs pause it");
        assert_eq!(gpu.right_eye_seen, 0, "and it learns the eye again");
        for _ in 0..2 {
            gpu.right_eye_seen = 5;
            gpu.right_eye_off("a short run broke");
        }
        assert!(!gpu.right_eye_wrong, "two short runs are still paused");
        gpu.right_eye_seen = 5;
        gpu.right_eye_off("a short run broke");
        assert!(gpu.right_eye_wrong, "the third short run turns it off for good");
    }
}
