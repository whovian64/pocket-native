//! what the menu sets: the screens, performance and controls for every game,
//! in the configuration directory, and each game's own options, in its data
//! directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zakuro_core::services::cfg::{LANGUAGE_ENGLISH, REGION_USA};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    BigTop,
    TopOnly,
    SideBySide,
    Original,
    BigBeside,
}

/// the screens stretched over the display they are on rather than kept in
/// shape, with a screen on each display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Stretch {
    #[default]
    Off,
    Top,
    Bottom,
    Both,
}

impl Stretch {
    pub const ALL: [Stretch; 4] = [Stretch::Off, Stretch::Top, Stretch::Bottom, Stretch::Both];

    /// whether the top screen, or else the bottom one, is stretched.
    pub fn stretches(self, top: bool) -> bool {
        matches!((self, top), (Stretch::Both, _) | (Stretch::Top, true) | (Stretch::Bottom, false))
    }
}

/// the screens' shape: the top one's as on the console, scaled by whole
/// pixels, made narrower or wider, both stretched over the room they have, or
/// the top one cut to its 4:3 middle, its sides left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Aspect {
    #[default]
    Original,
    Integer,
    FourThree,
    SixteenTen,
    SixteenNine,
    Stretch,
    Crop,
}

impl Aspect {
    pub const ALL: [Aspect; 7] = [Aspect::Original, Aspect::Integer, Aspect::FourThree, Aspect::SixteenTen, Aspect::SixteenNine, Aspect::Stretch, Aspect::Crop];

    /// the top screen's width over its height.
    pub fn ratio(self) -> f32 {
        match self {
            Aspect::FourThree | Aspect::Crop => 4.0 / 3.0,
            Aspect::SixteenTen => 16.0 / 10.0,
            Aspect::SixteenNine => 16.0 / 9.0,
            Aspect::Original | Aspect::Integer | Aspect::Stretch => 5.0 / 3.0,
        }
    }

    /// how much of the top screen's width is left out each side: a tenth of
    /// its 400 pixels, for the 320 in the middle, cropped.
    pub fn crop(self) -> f32 {
        if self == Aspect::Crop { 0.1 } else { 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Power {
    Saver,
    Balanced,
    Performance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speed {
    Slow,
    Normal,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Buttons {
    Nintendo,
    Xbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stick {
    CirclePad,
    Dpad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dpad {
    Dpad,
    CirclePad,
    Touch,
}

/// whether the touch cursor is drawn over the bottom screen: always, never,
/// or on a handheld with two screens, whose bottom one takes a finger, only
/// for a moment after the stick or the d-pad moved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Cursor {
    #[default]
    Auto,
    Shown,
    Hidden,
}

/// the right stick as the touch cursor, or as the New 3DS's C-stick with L2
/// and R2 as ZL and ZR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RightStick {
    Cursor,
    CStick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    Auto,
    Clockwise,
    CounterClockwise,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub layout: Layout,
    /// the bottom screen in the top one's place.
    pub swapped: bool,
    pub smooth: bool,
    /// with a screen on each display, which of them fill their displays
    /// rather than keep their shape with bars.
    pub stretched: Stretch,
    pub aspect: Aspect,
    /// how many times the console's resolution the GPU draws at, in
    /// quarters from 1 to 3.5, from the next start.
    pub resolution: f32,
    /// pictures a second the panel is filled up to with ones halfway
    /// between each two of the top screen's, 30 or 60, 0 for none.
    pub interpolate_to: u32,
    pub show_fps: bool,
    /// the battery and the time under the screens.
    pub show_clock: bool,
    /// the picture turned onto a sideways panel, as ZAKURO_PANEL_ROTATE
    /// does, which Auto leaves to.
    pub rotation: Rotation,
    pub frame_cap: u32,
    pub power: Power,
    pub touch_speed: Speed,
    pub cursor: Cursor,
    pub buttons: Buttons,
    pub left_stick: Stick,
    /// the d-pad as the circle pad or the touch cursor, for handhelds short
    /// of sticks.
    pub dpad: Dpad,
    /// the right stick as the touch cursor or the C-stick, R3 switching it.
    pub right_stick: RightStick,
    /// tilting with L2 and the right stick the other way, left and right
    /// or up and down.
    pub reverse_tilt_x: bool,
    pub reverse_tilt_y: bool,
    /// buttons and sticks drawn over the screens, for a touch screen with
    /// no gamepad.
    pub touch_controls: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            layout: Layout::BigTop,
            swapped: false,
            smooth: true,
            stretched: Stretch::Off,
            aspect: Aspect::Original,
            resolution: 1.0,
            interpolate_to: 0,
            show_fps: false,
            show_clock: true,
            rotation: Rotation::Auto,
            frame_cap: 30,
            power: Power::Performance,
            touch_speed: Speed::Normal,
            cursor: Cursor::Auto,
            buttons: Buttons::Nintendo,
            left_stick: Stick::CirclePad,
            dpad: Dpad::Dpad,
            right_stick: RightStick::Cursor,
            reverse_tilt_x: false,
            reverse_tilt_y: false,
            touch_controls: cfg!(target_os = "android"),
        }
    }
}

/// a game's options by name, on or off, and its advanced settings when the
/// player changed them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// the console's language and region the game is told, as cfg numbers
    /// them.
    pub language: u8,
    pub region: u8,
    pub options: BTreeMap<String, bool>,
    pub advanced: Option<Advanced>,
}

impl Default for GameSettings {
    fn default() -> Self {
        GameSettings { language: LANGUAGE_ENGLISH, region: REGION_USA, options: BTreeMap::new(), advanced: None }
    }
}

/// the engine's speed hacks, each of which a game may not get along with. all
/// but skip_bottom, skip_right_eye, frame_skip and shown_only take effect
/// when the game starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// the bottom screen drawn less often while it changes little.
    pub skip_bottom: bool,
    /// the right eye's picture left out, the 3D effect being off.
    pub skip_right_eye: bool,
    /// most pictures skipped in a row to keep up.
    pub frame_skip: u32,
    /// the frames the frame cap leaves unshown not drawn, in a game that
    /// makes a picture every frame.
    pub shown_only: bool,
    /// the speed hacks, which the ones after it need.
    pub fast: bool,
    pub quick_checks: bool,
    pub half_precision: bool,
    pub plain_math: bool,
    pub tile_copies: bool,
    pub gpu_clears: bool,
    pub gpu_arrays: bool,
    pub trust_meshes: bool,
    pub trust_textures: bool,
    pub small_cache: bool,
    /// heavy meshes left out while they are out of view.
    pub cull: bool,
    pub gpu_drawing: bool,
    /// vertices shaded on the GPU through translated programs, rather than
    /// on the CPU.
    pub gpu_vertices: bool,
    /// display transfers done on the GPU, rather than on the CPU.
    pub gpu_transfers: bool,
    /// textures the game drew copied on the GPU, rather than read back from
    /// memory.
    pub gpu_texture_copies: bool,
    /// a drawn texture that is a whole surface sampled where it is.
    pub in_place: bool,
    /// a texture running past the drawing under it taken from the drawing,
    /// its last row repeated, rather than from memory.
    pub past_drawing: bool,
    /// ETC1 textures kept compressed for the GPU.
    pub etc1: bool,
    pub generic_shaders: bool,
    pub generic_lighting: bool,
    /// indexed draws given the range of vertices they use.
    pub draw_ranges: bool,
    pub gpu_thread: bool,
    /// time going on while the GPU thread works.
    pub realtime_gpu: bool,
    pub recompiled: bool,
    /// the interpreter stepping up to an event a recompiled block would run
    /// past.
    pub exact_events: bool,
    /// the shaders the game used before compiled when it starts.
    pub prewarm: bool,
    /// the ROM blocks earlier runs read read ahead of the game.
    pub read_ahead: bool,
    /// new shaders compiled on a thread of their own, draws going on with
    /// stand-ins meanwhile.
    pub background_shaders: bool,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            skip_bottom: true,
            skip_right_eye: true,
            frame_skip: 3,
            shown_only: false,
            fast: true,
            quick_checks: true,
            half_precision: true,
            plain_math: true,
            tile_copies: true,
            gpu_clears: true,
            gpu_arrays: true,
            trust_meshes: true,
            trust_textures: true,
            small_cache: true,
            cull: true,
            gpu_drawing: true,
            gpu_vertices: true,
            gpu_transfers: true,
            gpu_texture_copies: true,
            in_place: true,
            past_drawing: true,
            etc1: true,
            generic_shaders: false,
            generic_lighting: false,
            draw_ranges: true,
            gpu_thread: true,
            realtime_gpu: false,
            recompiled: true,
            exact_events: false,
            prewarm: true,
            read_ahead: true,
            background_shaders: true,
        }
    }
}

impl Advanced {
    /// what a game runs with: the player's settings, else its profile's, else
    /// the defaults.
    pub fn of(game: &GameSettings, profile: &crate::profile::Profile) -> Advanced {
        game.advanced.clone().or_else(|| profile.advanced.clone()).unwrap_or_default()
    }

    /// these with every speed hack a game could trip over off, and those that
    /// would make it too slow to play kept: the first thing to try when a
    /// game does not run right.
    pub fn compatible(self) -> Advanced {
        Advanced {
            skip_bottom: false,
            skip_right_eye: false,
            shown_only: false,
            fast: false,
            gpu_texture_copies: false,
            in_place: false,
            past_drawing: false,
            draw_ranges: false,
            exact_events: true,
            background_shaders: false,
            ..self
        }
    }

    /// the switches the engine reads from the environment when it starts.
    pub fn export(&self) {
        let off: Vec<&str> = [
            ("spots", self.quick_checks),
            ("half", self.half_precision),
            ("plain-math", self.plain_math),
            ("tiles", self.tile_copies),
            ("clears", self.gpu_clears),
            ("arrays", self.gpu_arrays),
            ("meshes", self.trust_meshes),
            ("textures", self.trust_textures),
            ("small-cache", self.small_cache),
        ]
        .into_iter()
        .filter(|(_, on)| !on)
        .map(|(name, _)| name)
        .collect();
        std::env::set_var("ZAKURO_FAST_OFF", off.join(","));
        for (variable, on) in [
            ("ZAKURO_FAST", self.fast),
            ("ZAKURO_NO_CULL", !self.cull),
            ("ZAKURO_CPU_SHADERS", !self.gpu_vertices),
            ("ZAKURO_CPU_TRANSFERS", !self.gpu_transfers),
            ("ZAKURO_NO_GPU_TEXTURES", !self.gpu_texture_copies),
            ("ZAKURO_NO_IN_PLACE", !self.in_place),
            ("ZAKURO_NO_PAST_DRAWING", !self.past_drawing),
            ("ZAKURO_NO_ETC1", !self.etc1),
            ("ZAKURO_GENERIC_SHADERS", self.generic_shaders),
            ("ZAKURO_GENERIC_LIGHTING", self.generic_lighting),
            ("ZAKURO_NO_RANGE", !self.draw_ranges),
            ("ZAKURO_SYNC_GPU", !self.gpu_thread),
            ("ZAKURO_GPU_REALTIME", self.realtime_gpu),
            ("ZAKURO_EXACT_EVENTS", self.exact_events),
            ("ZAKURO_SYNC_SHADERS", !self.background_shaders),
        ] {
            if on {
                std::env::set_var(variable, "1");
            } else {
                std::env::remove_var(variable);
            }
        }
        if !self.prewarm {
            log::info!("switch: shader prewarm off");
            std::env::remove_var("ZAKURO_SHADER_MANIFEST");
        }
    }
}

/// POCKET_NATIVE_CONFIG, or pocket-native in the XDG configuration directory.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("POCKET_NATIVE_CONFIG") {
        return dir.into();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")));
    base.unwrap_or_else(|| ".".into()).join("pocket-native")
}

pub fn load<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|error| {
            log::warn!("{} is not readable, {error}, using the defaults", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) {
    let written = toml::to_string(value)
        .map_err(|error| error.to_string())
        .and_then(|text| path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| write_safely(path, text.as_bytes())).map_err(|error| error.to_string()));
    if let Err(error) = written {
        log::warn!("could not save {}, {error}", path.display());
    }
}

/// writes a file whole or not at all: the new one goes to the card beside
/// the old, which it then replaces, so power going mid-write leaves the old.
pub fn write_safely(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("new");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}
