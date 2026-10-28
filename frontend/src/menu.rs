//! the menu a tap of FN opens: the game pauses under it, the d-pad moves and
//! changes, A picks, B or FN goes back to the game.

use std::collections::HashSet;

use crate::font::{rgba, Canvas};
use crate::input::{A, B, DOWN, LEFT, RIGHT, UP};
use crate::profile::Profile;
use crate::settings::{Advanced, Aspect, Buttons, Cursor, Dpad, GameSettings, Layout, Power, RightStick, Rotation, Settings, Speed, Stick, Stretch};

const PANEL: [u8; 3] = [13, 20, 32];
const EDGE: [u8; 3] = [40, 110, 210];
const HEADER: [u8; 3] = [18, 40, 76];
const HIGHLIGHT: [u8; 3] = [24, 64, 124];
const TEXT: [u8; 3] = [230, 238, 248];
const DIM: [u8; 3] = [130, 152, 182];
const ACCENT: [u8; 3] = [90, 170, 255];
const WARNING: [u8; 3] = [255, 196, 90];
/// the face sizes in the font, the list's and the small one.
const BIG: usize = 0;
const SMALL: usize = 1;
const ROW: f32 = 19.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Layout,
    Aspect,
    Resolution,
    Smooth,
    Stretch,
    Interpolate,
    ShowFps,
    ShowClock,
    Rotation,
    FrameCap,
    Power,
    Option(usize),
    TouchSpeed,
    Cursor,
    Buttons,
    LeftStick,
    Dpad,
    RightStick,
    ReverseTiltX,
    ReverseTiltY,
    TouchControls,
    EditControls,
    Resume,
    Advanced,
    Reset,
    Quit,
    Toggle(usize),
    FrameSkip,
    Language,
    Region,
    Restart,
    Compatible,
    Defaults,
    Back,
}

const ON_OFF: [&str; 2] = ["Off", "On"];

/// the advanced settings that turn on and off, see settings::Advanced: the
/// name, the setting, what off and on show as, and the help under the list.
#[allow(clippy::type_complexity)]
const TOGGLES: [(&str, fn(&mut Advanced) -> &mut bool, [&str; 2], &str); 31] = [
    ("Bottom screen skipping", |a| &mut a.skip_bottom, ON_OFF, "Draws the bottom screen less often while it stands still. Try Off if it flickers, lags or goes black."),
    ("Speed hacks", |a| &mut a.fast, ON_OFF, "Turns the ten speed hacks below on or off together. Off is the first thing to try when a game looks wrong."),
    ("Quick surface checks", |a| &mut a.quick_checks, ON_OFF, "Checks pictures against memory at a few spots only. Try Off if old pictures or garbage stay after a scene change."),
    ("Half-precision color", |a| &mut a.half_precision, ON_OFF, "Faster color math on the GPU. Try Off for banding, wrong colors or speckles."),
    ("Fast vertex math", |a| &mut a.plain_math, ON_OFF, "Faster shape math. Try Off if shapes are missing, stretch into spikes or turn black."),
    ("Depth tiles on the GPU", |a| &mut a.tile_copies, ON_OFF, "Moves depth between buffers on the GPU. Try Off if shadows, fog or outlines go wrong."),
    ("Clears on the GPU", |a| &mut a.gpu_clears, ON_OFF, "Clears whole buffers on the GPU alone. Try Off for leftover pictures or black screens."),
    ("Vertex arrays on the GPU", |a| &mut a.gpu_arrays, ON_OFF, "Keeps shapes in GPU memory. Try Off if shapes flicker or break apart."),
    ("Trust cached meshes", |a| &mut a.trust_meshes, ON_OFF, "Reuses shapes for a few pictures without reading them again. Try Off if things flicker, lag or menus glitch."),
    ("Trust cached textures", |a| &mut a.trust_textures, ON_OFF, "Reuses textures for a few pictures without reading them again. Try Off if a texture is late to change."),
    ("Small texture cache", |a| &mut a.small_cache, ON_OFF, "Keeps a quarter of the textures, to save memory. Try Off if busy scenes stutter."),
    ("Hide scenery out of view", |a| &mut a.cull, ON_OFF, "Skips big shapes that were not on screen a moment ago. Try Off if things pop in or flicker at the edges."),
    ("Renderer", |a| &mut a.gpu_drawing, ["Software", "GPU"], "Software is far too slow to play, for telling apart whether a glitch is the GPU's."),
    ("Vertex shading", |a| &mut a.gpu_vertices, ["CPU", "GPU"], "Where shapes are shaded. Try CPU if shapes are wrong, it is slower."),
    ("Display transfers", |a| &mut a.gpu_transfers, ["CPU", "GPU"], "How pictures reach the screens. Try CPU for garbage, wrong colors or the wrong picture on a screen."),
    ("Drawn texture copies", |a| &mut a.gpu_texture_copies, ["Memory", "GPU"], "Pictures a game draws and reuses as textures. Try Memory if it crashes or freezes, or for wrong reflections."),
    ("Drawn textures in place", |a| &mut a.in_place, ON_OFF, "Reads those pictures without copying them. Try Off for upside-down or shifted effects."),
    ("Textures past a drawing", |a| &mut a.past_drawing, ON_OFF, "Fills a texture bigger than its picture with the picture's edge. Try Off for streaks or missing glow."),
    ("Compressed textures", |a| &mut a.etc1, ON_OFF, "Leaves compressed textures to the GPU. Try Off for blocky or wrongly colored textures."),
    ("Generic shaders", |a| &mut a.generic_shaders, ON_OFF, "One program for every draw rather than one each. Slower to draw, but nothing new to build. Try On for hitches."),
    ("Generic lighting", |a| &mut a.generic_lighting, ON_OFF, "The same for lit draws only. Try On for hitches when lights change."),
    ("Vertex ranges", |a| &mut a.draw_ranges, ON_OFF, "Reads only the vertices a draw uses. Try Off if shapes are missing or broken."),
    ("GPU thread", |a| &mut a.gpu_thread, ON_OFF, "Draws on a core of its own. Try Off for crashes or glitches that come and go, it is slower."),
    ("Real-time GPU", |a| &mut a.realtime_gpu, ON_OFF, "Time goes on while the GPU draws, as on the console. Try On if the game runs too fast or its timing is off."),
    ("CPU", |a| &mut a.recompiled, ["Interpreter", "Recompiled"], "The interpreter is far too slow to play, for telling apart a crash of the recompiled code."),
    ("Exact event timing", |a| &mut a.exact_events, ON_OFF, "Runs the game's code up to each event's exact time. Try On for freezes or sound that stutters, it is a little slower."),
    ("Shader prewarm", |a| &mut a.prewarm, ON_OFF, "Builds the shaders this game used before while it starts. Try Off if it will not start."),
    ("ROM read-ahead", |a| &mut a.read_ahead, ON_OFF, "Reads the parts of the game it loaded last time ahead. Try Off if loading stalls."),
    ("Shader compiling", |a| &mut a.background_shaders, ["When needed", "Background"], "Background builds new shaders with no pause, a stand-in drawn meanwhile. Try When needed for brief glitches."),
    ("Skip the right eye", |a| &mut a.skip_right_eye, ON_OFF, "Leaves out the right eye's picture, which 2D never shows. Try Off if the top screen flickers or is wrong."),
    ("Draw only shown frames", |a| &mut a.shown_only, ON_OFF, "Leaves out the frames the frame cap never shows, easing the GPU. Try Off if it stutters."),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    Main,
    Advanced,
}

impl Field {
    /// picked with A rather than changed with left and right.
    fn action(self) -> bool {
        matches!(self, Field::Resume | Field::Advanced | Field::Reset | Field::Quit | Field::Restart | Field::Compatible | Field::Defaults | Field::Back | Field::EditControls)
    }
}

pub enum Action {
    None,
    Redraw,
    /// a setting changed, which the game takes up at once.
    Changed,
    Resume,
    Reset,
    Quit,
    /// the touch controls to move and size.
    EditControls,
}

/// the layouts the menu goes through, with the screens swapped or not.
const LAYOUTS: [(Layout, bool, &str); 7] = [
    (Layout::BigTop, false, "Big top"),
    (Layout::BigTop, true, "Swapped"),
    (Layout::TopOnly, false, "Top only"),
    (Layout::TopOnly, true, "Bottom only"),
    (Layout::SideBySide, false, "Side by side"),
    (Layout::BigBeside, false, "Big top, small beside"),
    (Layout::Original, false, "Original size"),
];
const CAPS: [u32; 3] = [20, 30, 60];
const RESOLUTIONS: [f32; 11] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0, 3.25, 3.5];
/// the languages and regions in cfg's order.
const LANGUAGES: [&str; 12] = ["Japanese", "English", "French", "German", "Italian", "Spanish", "Chinese (simplified)", "Korean", "Dutch", "Portuguese", "Russian", "Chinese (traditional)"];
const REGIONS: [&str; 7] = ["Japan", "USA", "Europe", "Australia", "China", "Korea", "Taiwan"];

#[derive(Default)]
pub struct Menu {
    pub open: bool,
    page: Page,
    cursor: usize,
    /// Reset or Quit waiting for A again.
    confirm: Option<usize>,
    last: HashSet<u16>,
    held: u32,
}

/// whether a field does anything on this handheld: touch controls where a
/// program around the frontend gives touches, an Android app's; filling the
/// displays with a screen on each; turning a panel the frontend drives
/// itself, which an Android window does on its own; and power where the
/// launcher gives a program for it.
fn useful(field: Field) -> bool {
    match field {
        Field::TouchControls | Field::EditControls => crate::input::shared().is_some(),
        Field::Stretch => zakuro_gpu::second_window() || zakuro_gpu::stacked(),
        Field::Rotation => crate::input::shared().is_none(),
        Field::Power => std::env::var_os("POCKET_NATIVE_POWER").is_some(),
        _ => true,
    }
}

fn sections(page: Page, profile: &Profile) -> Vec<(&'static str, Vec<Field>)> {
    let toggles = |range: std::ops::RangeInclusive<usize>| range.map(Field::Toggle).collect();
    let only = |fields: Vec<Field>| fields.into_iter().filter(|&field| useful(field)).collect();
    match page {
        Page::Main => vec![
            ("Display", only(vec![Field::Layout, Field::Aspect, Field::Resolution, Field::Smooth, Field::Stretch, Field::Interpolate, Field::ShowFps, Field::ShowClock, Field::Rotation])),
            ("Performance", only(vec![Field::FrameCap, Field::Power])),
            ("This game", (0..profile.options.len()).map(Field::Option).collect()),
            ("Controls", only(vec![Field::TouchSpeed, Field::Cursor, Field::Buttons, Field::LeftStick, Field::Dpad, Field::RightStick, Field::ReverseTiltX, Field::ReverseTiltY, Field::TouchControls, Field::EditControls])),
            ("Game", vec![Field::Resume, Field::Advanced, Field::Reset, Field::Quit]),
        ],
        Page::Advanced => vec![
            ("Applies at once", vec![Field::Toggle(0), Field::Toggle(29), Field::Toggle(30), Field::FrameSkip]),
            ("Speed hacks - requires restart", toggles(1..=11)),
            ("Graphics - requires restart", [toggles(12..=22), vec![Field::Toggle(28)]].concat()),
            ("CPU - requires restart", toggles(23..=24)),
            ("Loading - requires restart", toggles(25..=27)),
            ("System - requires restart", vec![Field::Language, Field::Region]),
            ("", vec![Field::Restart, Field::Compatible, Field::Defaults, Field::Back]),
        ],
    }
}

fn step<T: Copy + PartialEq>(values: &[T], current: T, by: isize) -> T {
    let at = values.iter().position(|&value| value == current).unwrap_or(0) as isize;
    values[(at + by).rem_euclid(values.len() as isize) as usize]
}

fn label(field: Field, settings: &Settings, game: &GameSettings, profile: &Profile) -> (String, String) {
    let (name, value) = match field {
        Field::Layout => {
            let named = LAYOUTS.iter().find(|(layout, swapped, _)| (*layout, *swapped) == (settings.layout, settings.swapped));
            let value = match named {
                Some((.., name)) => name.to_string(),
                None => format!("{}, swapped", LAYOUTS.iter().find(|(layout, ..)| *layout == settings.layout).map_or("", |(.., name)| name)),
            };
            ("Layout", value)
        }
        Field::Aspect => (
            "Aspect ratio",
            match settings.aspect {
                Aspect::Original => "Original",
                Aspect::Integer => "Integer scale",
                Aspect::FourThree => "4:3",
                Aspect::SixteenTen => "16:10",
                Aspect::SixteenNine => "16:9",
                Aspect::Stretch => "Stretch",
                Aspect::Crop => "Crop",
            }
            .into(),
        ),
        Field::Resolution => ("Resolution", if settings.resolution == 1.0 { "Native".into() } else { format!("{}x", settings.resolution) }),
        Field::Smooth => ("Smooth scaling", if settings.smooth { "On" } else { "Off" }.into()),
        Field::Stretch => (
            "Fill the display",
            match settings.stretched {
                Stretch::Off => "Off",
                Stretch::Top => "Top screen",
                Stretch::Bottom => "Bottom screen",
                Stretch::Both => "Both",
            }
            .into(),
        ),
        Field::Interpolate => ("Frame interpolation", if settings.interpolate_to == 0 { "Off".into() } else { format!("Up to {} fps", settings.interpolate_to) }),
        Field::ShowFps => ("Show fps", if settings.show_fps { "On" } else { "Off" }.into()),
        Field::ShowClock => ("Clock and battery", if settings.show_clock { "On" } else { "Off" }.into()),
        Field::Rotation => {
            let rotation = match settings.rotation {
                Rotation::Auto => "Auto",
                Rotation::Clockwise => "Clockwise",
                Rotation::CounterClockwise => "Counter clockwise",
                Rotation::Off => "Off",
            };
            ("Rotation", rotation.into())
        }
        Field::FrameCap => ("Frame cap", format!("{} fps", settings.frame_cap)),
        Field::Power => ("Power", format!("{:?}", settings.power)),
        Field::Option(index) => {
            let option = &profile.options[index];
            let on = game.options.get(&option.name).copied().unwrap_or(option.default);
            return (option.name.clone(), option.values[on as usize].clone());
        }
        Field::TouchSpeed => ("Touch cursor speed", format!("{:?}", settings.touch_speed)),
        Field::Cursor => ("Touch cursor", format!("{:?}", settings.cursor)),
        Field::Buttons => ("A/B layout", format!("{:?}", settings.buttons)),
        Field::LeftStick => ("Left stick", if settings.left_stick == Stick::CirclePad { "Circle pad" } else { "D-pad" }.into()),
        Field::Dpad => {
            let dpad = match settings.dpad {
                Dpad::Dpad => "D-pad",
                Dpad::CirclePad => "Circle pad",
                Dpad::Touch => "Touch cursor",
            };
            ("D-pad", dpad.into())
        }
        Field::RightStick => ("Right stick", if settings.right_stick == RightStick::CStick { "C-stick" } else { "Touch cursor" }.into()),
        Field::ReverseTiltX => ("Reverse tilt X", if settings.reverse_tilt_x { "On" } else { "Off" }.into()),
        Field::ReverseTiltY => ("Reverse tilt Y", if settings.reverse_tilt_y { "On" } else { "Off" }.into()),
        Field::TouchControls => ("Touch controls", if settings.touch_controls { "On" } else { "Off" }.into()),
        Field::EditControls => ("Edit touch controls", "→".into()),
        Field::Resume => ("Resume", String::new()),
        Field::Advanced => ("Advanced settings", "→".into()),
        Field::Reset => ("Reset", String::new()),
        Field::Quit => ("Quit", String::new()),
        Field::Toggle(index) => {
            let (name, setting, values, _) = TOGGLES[index];
            let mut advanced = Advanced::of(game, profile);
            return (name.to_owned(), values[*setting(&mut advanced) as usize].to_owned());
        }
        Field::FrameSkip => {
            let skip = Advanced::of(game, profile).frame_skip;
            ("Frame skip", if skip == 0 { "Off".into() } else { format!("Up to {skip}") })
        }
        Field::Language => ("Language", LANGUAGES.get(game.language as usize).copied().unwrap_or("?").into()),
        Field::Region => ("Region", REGIONS.get(game.region as usize).copied().unwrap_or("?").into()),
        Field::Restart => ("Restart game", String::new()),
        Field::Compatible => ("Most compatible", String::new()),
        Field::Defaults => ("Reset to defaults", String::new()),
        Field::Back => ("Back", String::new()),
    };
    (name.to_owned(), value)
}

fn change(field: Field, by: isize, settings: &mut Settings, game: &mut GameSettings, profile: &Profile) {
    match field {
        Field::Layout => {
            let at = LAYOUTS.iter().position(|(layout, swapped, _)| (*layout, *swapped) == (settings.layout, settings.swapped));
            let at = at.or_else(|| LAYOUTS.iter().position(|(layout, ..)| *layout == settings.layout)).unwrap_or(0) as isize;
            let (layout, swapped, _) = LAYOUTS[(at + by).rem_euclid(LAYOUTS.len() as isize) as usize];
            (settings.layout, settings.swapped) = (layout, swapped);
        }
        Field::Aspect => settings.aspect = step(&Aspect::ALL, settings.aspect, by),
        Field::Resolution => settings.resolution = step(&RESOLUTIONS, settings.resolution, by),
        Field::Smooth => settings.smooth = !settings.smooth,
        Field::Stretch => {
            let at = Stretch::ALL.iter().position(|&stretch| stretch == settings.stretched).unwrap_or(0) as isize;
            settings.stretched = Stretch::ALL[(at + by).rem_euclid(Stretch::ALL.len() as isize) as usize];
        }
        Field::Interpolate => settings.interpolate_to = step(&[0, 30, 60], settings.interpolate_to, by),
        Field::ShowFps => settings.show_fps = !settings.show_fps,
        Field::ShowClock => settings.show_clock = !settings.show_clock,
        Field::Rotation => settings.rotation = step(&[Rotation::Auto, Rotation::Clockwise, Rotation::CounterClockwise, Rotation::Off], settings.rotation, by),
        Field::FrameCap => settings.frame_cap = step(&CAPS, settings.frame_cap, by),
        Field::Power => settings.power = step(&[Power::Saver, Power::Balanced, Power::Performance], settings.power, by),
        Field::Option(index) => {
            let option = &profile.options[index];
            let on = game.options.get(&option.name).copied().unwrap_or(option.default);
            game.options.insert(option.name.clone(), !on);
        }
        Field::TouchSpeed => settings.touch_speed = step(&[Speed::Slow, Speed::Normal, Speed::Fast], settings.touch_speed, by),
        Field::Cursor => settings.cursor = step(&[Cursor::Auto, Cursor::Shown, Cursor::Hidden], settings.cursor, by),
        Field::Buttons => settings.buttons = step(&[Buttons::Nintendo, Buttons::Xbox], settings.buttons, by),
        Field::LeftStick => settings.left_stick = step(&[Stick::CirclePad, Stick::Dpad], settings.left_stick, by),
        Field::Dpad => settings.dpad = step(&[Dpad::Dpad, Dpad::CirclePad, Dpad::Touch], settings.dpad, by),
        Field::RightStick => settings.right_stick = step(&[RightStick::Cursor, RightStick::CStick], settings.right_stick, by),
        Field::ReverseTiltX => settings.reverse_tilt_x = !settings.reverse_tilt_x,
        Field::ReverseTiltY => settings.reverse_tilt_y = !settings.reverse_tilt_y,
        Field::TouchControls => settings.touch_controls = !settings.touch_controls,
        Field::Toggle(index) => {
            let mut advanced = Advanced::of(game, profile);
            let setting = (TOGGLES[index].1)(&mut advanced);
            *setting = !*setting;
            game.advanced = Some(advanced);
        }
        Field::FrameSkip => {
            let mut advanced = Advanced::of(game, profile);
            advanced.frame_skip = step(&[0, 1, 2, 3], advanced.frame_skip, by);
            game.advanced = Some(advanced);
        }
        Field::Language => game.language = (game.language as isize + by).rem_euclid(LANGUAGES.len() as isize) as u8,
        Field::Region => game.region = (game.region as isize + by).rem_euclid(REGIONS.len() as isize) as u8,
        Field::Compatible => game.advanced = Some(Advanced::compatible(Advanced::of(game, profile))),
        Field::Defaults => game.advanced = None,
        Field::Resume | Field::Advanced | Field::Reset | Field::Quit | Field::Restart | Field::Back | Field::EditControls => {}
    }
}

/// what the help under the list says about a field.
fn help(field: Field, profile: &Profile) -> &str {
    match field {
        Field::Layout => "Where the screens go. A tap of L2 swaps them, FN or L3 with R1 switch to one big screen.",
        Field::Aspect => "The screens' shape: as on the 3DS, by whole pixels, 4:3, 16:10, 16:9, stretched, or the top one's sides cropped.",
        Field::Resolution => "How sharp the 3D is drawn, times the 3DS's own. Higher takes more GPU. Restart the game for it.",
        Field::Smooth => "Smooths the picture as it is scaled up. Off is sharper, with uneven pixels.",
        Field::Stretch => "With a screen on each display, the screens stretched over theirs: no bars, but out of shape.",
        Field::Interpolate => "Fills in pictures halfway between a game's, up to 30 or 60 a second. 30 costs half as much GPU time.",
        Field::ShowFps => "Pictures a second and the temperature, in the bottom right corner.",
        Field::ShowClock => "The battery and a 12-hour clock, in the bottom left corner.",
        Field::Rotation => "Turns the picture for a screen that shows it the wrong way round. Takes effect when the game starts again.",
        Field::FrameCap => "The most pictures shown a second. Lower lasts longer and runs cooler.",
        Field::Power => "How fast the CPU and GPU run. Performance is fastest and warmest, Saver lasts longest.",
        Field::Option(index) => &profile.options[index].help,
        Field::TouchSpeed => "How fast the right stick moves the touch cursor. R2 or R3 touches.",
        Field::Cursor => "The cursor over the bottom screen. Auto hides it on two screens but while the stick moves it.",
        Field::Buttons => "Which face buttons are A and B: where Nintendo puts them, or where Xbox does.",
        Field::LeftStick => "The left stick as the circle pad, or as the d-pad.",
        Field::Dpad => "The d-pad as itself, the circle pad or the touch cursor. FN or L3 with Select switches it in game.",
        Field::RightStick => "The touch cursor, or the C-stick with L2 and R2 as ZL and ZR for New 3DS games. R3 switches it in game.",
        Field::ReverseTiltX => "Holding L2, the right stick tilts the console. On tilts it the other way left and right.",
        Field::ReverseTiltY => "Holding L2, the right stick tilts the console. On tilts it the other way up and down.",
        Field::TouchControls => "Buttons and a circle pad on the touch screen. They hide while a gamepad is used.",
        Field::EditControls => "Move and size the touch controls, for the way the screen is held now. Done or FN to finish.",
        Field::Resume => "Back to the game. B or FN does the same.",
        Field::Advanced => "This game's switches for speed and for games that do not run right, each with a hint.",
        Field::Reset => "Starts the game again from the beginning.",
        Field::Quit => if cfg!(target_os = "android") { "Back to the game list." } else { "Back to EmulationStation." },
        Field::Toggle(index) => TOGGLES[index].3,
        Field::FrameSkip => "The most pictures left out in a row while the game is behind. Off is smoother and slower.",
        Field::Language => "The console language the game is told. Games without it pick one of their own.",
        Field::Region => "The console region the game is told. A game from another region may want its own.",
        Field::Restart => "Starts the game again, for the settings that require a restart.",
        Field::Compatible => "Turns off every speed hack a game could trip over, then restart. If that helps, turn them back on one at a time.",
        Field::Defaults => "Back to what this game starts with.",
        Field::Back => "Back to the menu.",
    }
}

/// text cut short with "..." to be no wider than width.
fn shortened(canvas: &Canvas, face: usize, text: &str, width: f32) -> String {
    let mut text = text.to_owned();
    if canvas.measure(face, &text) <= width {
        return text;
    }
    while !text.is_empty() && canvas.measure(face, &format!("{}...", text.trim_end())) > width {
        text.pop();
    }
    format!("{}...", text.trim_end())
}

/// text broken into lines no wider than width.
fn wrap(canvas: &Canvas, face: usize, text: &str, width: f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if canvas.measure(face, &format!("{line} {word}")) <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

impl Menu {
    pub fn show(&mut self) {
        (self.open, self.page, self.confirm) = (true, Page::Main, None);
    }

    /// back from the advanced settings to the entry that opened them.
    fn back(&mut self, profile: &Profile) -> Action {
        let fields: Vec<Field> = sections(Page::Main, profile).into_iter().flat_map(|(_, fields)| fields).collect();
        (self.page, self.confirm) = (Page::Main, None);
        self.cursor = fields.iter().position(|&field| field == Field::Advanced).unwrap_or(0);
        Action::Redraw
    }

    /// the keys held this frame, d-pad presses repeating while held.
    pub fn input(&mut self, keys: &HashSet<u16>, settings: &mut Settings, game: &mut GameSettings, profile: &Profile) -> Action {
        let direction = [UP, DOWN, LEFT, RIGHT].into_iter().find(|key| keys.contains(key));
        self.held = if direction.is_some_and(|key| self.last.contains(&key)) { self.held + 1 } else { 0 };
        let repeat = self.held == 0 || (self.held >= 18 && self.held.is_multiple_of(5));
        let pressed = |key: u16| keys.contains(&key) && !self.last.contains(&key);
        let (a, b) = (pressed(A), pressed(B));
        self.last = keys.clone();
        let fields: Vec<Field> = sections(self.page, profile).into_iter().flat_map(|(_, fields)| fields).collect();
        self.cursor = self.cursor.min(fields.len() - 1);
        let field = fields[self.cursor];
        match direction.filter(|_| repeat) {
            Some(UP) | Some(DOWN) => {
                let by = if direction == Some(UP) { fields.len() - 1 } else { 1 };
                (self.cursor, self.confirm) = ((self.cursor + by) % fields.len(), None);
                return Action::Redraw;
            }
            Some(key) if !field.action() => {
                change(field, if key == LEFT { -1 } else { 1 }, settings, game, profile);
                return Action::Changed;
            }
            _ => {}
        }
        if b {
            if self.confirm.take().is_some() {
                return Action::Redraw;
            }
            if self.page == Page::Advanced {
                return self.back(profile);
            }
            self.open = false;
            return Action::Resume;
        }
        if !a {
            return Action::None;
        }
        match field {
            Field::Resume => {
                self.open = false;
                Action::Resume
            }
            Field::Advanced => {
                (self.page, self.cursor) = (Page::Advanced, 0);
                Action::Redraw
            }
            Field::Back => self.back(profile),
            Field::EditControls => {
                self.open = false;
                Action::EditControls
            }
            Field::Reset | Field::Quit | Field::Restart if self.confirm != Some(self.cursor) => {
                self.confirm = Some(self.cursor);
                Action::Redraw
            }
            Field::Reset | Field::Restart => Action::Reset,
            Field::Quit => Action::Quit,
            field => {
                change(field, 1, settings, game, profile);
                Action::Changed
            }
        }
    }

    pub fn draw(&self, canvas: &mut Canvas, settings: &Settings, game: &GameSettings, profile: &Profile, title: &str) {
        canvas.fill([0.0, 0.0, 640.0, 480.0], rgba([0, 0, 0], 150));
        let (x0, y0, x1, y1) = (120.0, 16.0, 520.0, 464.0);
        canvas.fill([x0, y0, x1, y1], rgba(PANEL, 255));
        canvas.fill([x0 + 2.0, y0 + 2.0, x1 - 2.0, y0 + 30.0], rgba(HEADER, 255));
        canvas.outline([x0, y0, x1, y1], 2.0, rgba(EDGE, 255));
        let (heading, state) = if self.page == Page::Advanced { ("Advanced", "This game") } else { ("Pocket Native", "Paused") };
        canvas.text(BIG, heading, x0 + 14.0, y0 + 7.0, rgba(TEXT, 255), false);
        let mut left = x0 + 14.0 + canvas.measure(BIG, heading) + 16.0;
        if self.page == Page::Main {
            let version = concat!("v", env!("CARGO_PKG_VERSION"));
            canvas.text(SMALL, version, left - 8.0, y0 + 9.0, rgba(DIM, 255), false);
            left += canvas.measure(SMALL, version);
        }
        let state = format!(" · {state}");
        let title = shortened(canvas, SMALL, title, x1 - 14.0 - left - canvas.measure(SMALL, &state));
        canvas.text(SMALL, &format!("{title}{state}"), x1 - 14.0, y0 + 9.0, rgba(DIM, 255), true);

        // the headings and fields from the top of the list, scrolled to keep
        // the selected field in view
        let mut rows = Vec::new();
        let (mut y, mut index, mut selected) = (0.0, 0, 0.0);
        for (section, fields) in sections(self.page, profile) {
            y += 3.0;
            if !section.is_empty() {
                rows.push((y, None, section));
                y += ROW;
            }
            if fields.is_empty() {
                rows.push((y, None, "Nothing to set for this game"));
                y += ROW;
            }
            for field in fields {
                if index == self.cursor {
                    selected = y;
                }
                rows.push((y, Some((index, field)), ""));
                (y, index) = (y + ROW, index + 1);
            }
        }
        // two lines of help above the footer
        let help_top = y1 - 66.0;
        let (top, height) = (y0 + 41.0, help_top - 4.0 - (y0 + 41.0));
        let scroll = (selected + ROW - height).max(0.0);
        let more = match (scroll > 0.0, y - scroll > height) {
            (true, true) => "↑ ↓",
            (true, false) => "↑",
            (false, true) => "↓",
            (false, false) => "",
        };
        canvas.text(SMALL, more, (x0 + x1) / 2.0 + 6.0, y1 - 21.0, rgba(ACCENT, 255), false);
        for (y, field, text) in rows {
            let y = y - scroll;
            if y < 0.0 || y + ROW > height {
                continue;
            }
            let y = top + y;
            let Some((index, field)) = field else {
                let color = if text.starts_with("Nothing") { DIM } else { ACCENT };
                canvas.text(SMALL, text, x0 + if color == DIM { 22.0 } else { 14.0 }, y, rgba(color, 255), false);
                continue;
            };
            let selected = index == self.cursor;
            if selected {
                canvas.fill([x0 + 8.0, y - 2.0, x1 - 8.0, y + 17.0], rgba(HIGHLIGHT, 255));
            }
            let (name, value) = label(field, settings, game, profile);
            if self.confirm == Some(index) {
                let ask = match field {
                    Field::Quit => "Quit? Unsaved progress is lost",
                    Field::Restart => "Restart? Unsaved progress is lost",
                    _ => "Reset? Unsaved progress is lost",
                };
                canvas.text(BIG, ask, x0 + 22.0, y, rgba(WARNING, 255), false);
                canvas.text(SMALL, "A again", x1 - 18.0, y + 2.0, rgba(WARNING, 255), true);
            } else {
                canvas.text(BIG, &name, x0 + 22.0, y, rgba(TEXT, 255), false);
                let value = if selected && field == Field::Layout { format!("<  {value}  >") } else { value };
                canvas.text(BIG, &value, x1 - 18.0, y, rgba(if selected { TEXT } else { ACCENT }, 255), true);
            }
        }
        canvas.fill([x0 + 10.0, help_top, x1 - 10.0, help_top + 1.0], rgba([32, 48, 72], 255));
        let fields: Vec<Field> = sections(self.page, profile).into_iter().flat_map(|(_, fields)| fields).collect();
        if let Some(&field) = fields.get(self.cursor) {
            let lines = wrap(canvas, SMALL, help(field, profile), x1 - x0 - 28.0);
            for (line, text) in lines.iter().take(2).enumerate() {
                canvas.text(SMALL, text, x0 + 14.0, help_top + 6.0 + line as f32 * 16.0, rgba(DIM, 255), false);
            }
        }
        canvas.fill([x0 + 10.0, y1 - 27.0, x1 - 10.0, y1 - 26.0], rgba([32, 48, 72], 255));
        canvas.text(SMALL, "A  select    B  back", x0 + 14.0, y1 - 21.0, rgba(DIM, 255), false);
        canvas.text(SMALL, "Left/right  change", x1 - 14.0, y1 - 21.0, rgba(DIM, 255), true);
    }
}

/// what the bottom corners show while the game runs: the frame rate and the
/// temperature at the right, the battery and the time at the left.
#[derive(PartialEq)]
pub struct Status {
    fps: Option<String>,
    battery: Option<i64>,
    clock: Option<String>,
    /// what a hotkey just did, at the top.
    pub note: Option<&'static str>,
}

impl Status {
    /// none when the settings show nothing.
    pub fn now(settings: &Settings, fps: u32, note: Option<&'static str>) -> Option<Status> {
        let read = |path: &str| std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse::<i64>().ok());
        let status = Status {
            fps: settings.show_fps.then(|| match read("/sys/class/thermal/thermal_zone0/temp") {
                Some(milli) => format!("{fps} fps · {}°C", milli / 1000),
                None => format!("{fps} fps"),
            }),
            battery: settings.show_clock.then(battery).flatten(),
            clock: settings.show_clock.then(clock),
            note,
        };
        (status.fps.is_some() || status.clock.is_some() || note.is_some()).then_some(status)
    }

    pub fn draw(&self, canvas: &mut Canvas, (width, height): (u32, u32)) {
        let (width, height) = (width as f32, height as f32);
        let (top, bottom) = (height - 32.0, height - 8.0);
        let backing = rgba([0, 0, 0], 160);
        if let Some(note) = self.note {
            let half = canvas.measure(BIG, note) / 2.0 + 6.0;
            canvas.fill([width / 2.0 - half, 8.0, width / 2.0 + half, 32.0], backing);
            canvas.text(BIG, note, width / 2.0 - half + 6.0, 11.0, rgba(TEXT, 255), false);
        }
        if let Some(line) = &self.fps {
            canvas.fill([width - 20.0 - canvas.measure(BIG, line), top, width - 8.0, bottom], backing);
            canvas.text(BIG, line, width - 14.0, top + 3.0, rgba(TEXT, 255), true);
        }
        let Some(clock) = &self.clock else { return };
        let mut x = 14.0;
        let mut drawn = Vec::new();
        if let Some(battery) = self.battery {
            let color = rgba(if battery <= 15 { WARNING } else { TEXT }, 255);
            let y = top + 7.0;
            drawn.push(([x, y, x + 22.0, y + 11.0], 1.5, color));
            drawn.push(([x + 22.0, y + 3.5, x + 24.0, y + 7.5], 0.0, color));
            drawn.push(([x + 3.0, y + 3.0, x + 3.0 + 16.0 * battery.clamp(0, 100) as f32 / 100.0, y + 8.0], 0.0, color));
            x += 30.0;
        }
        let text = match self.battery {
            Some(battery) => format!("{battery}%   {clock}"),
            None => clock.clone(),
        };
        canvas.fill([8.0, top, x + canvas.measure(BIG, &text) + 6.0, bottom], backing);
        for (area, outline, color) in drawn {
            if outline > 0.0 { canvas.outline(area, outline, color) } else { canvas.fill(area, color) }
        }
        canvas.text(BIG, &text, x, top + 3.0, rgba(TEXT, 255), false);
    }
}

/// the battery's percentage, from whichever power supply is the battery:
/// battery on the R36S, named another way on others, Knulli's among them.
fn battery() -> Option<i64> {
    static CAPACITY: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let capacity = CAPACITY.get_or_init(|| {
        let supplies = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let battery = supplies.flatten().map(|supply| supply.path()).find(|supply| std::fs::read_to_string(supply.join("type")).is_ok_and(|kind| kind.trim() == "Battery"))?;
        Some(battery.join("capacity"))
    });
    std::fs::read_to_string(capacity.as_ref()?).ok()?.trim().parse().ok()
}

fn clock() -> String {
    // SAFETY: time and localtime_r fill a tm of this function's
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    format!("{}:{:02} {}", (tm.tm_hour + 11) % 12 + 1, tm.tm_min, if tm.tm_hour < 12 { "AM" } else { "PM" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::Font;

    /// every field's help fits the two lines under the list.
    #[test]
    fn the_help_fits_its_two_lines() {
        let font = Font::load();
        let canvas = Canvas::new(&font, (640, 480));
        let profile = Profile::default();
        let long: Vec<String> = [Page::Main, Page::Advanced]
            .into_iter()
            .flat_map(|page| sections(page, &profile).into_iter().flat_map(|(_, fields)| fields))
            .filter_map(|field| {
                let lines = wrap(&canvas, SMALL, help(field, &profile), 400.0 - 28.0);
                (lines.is_empty() || lines.len() > 2).then(|| format!("{field:?}: {lines:?}"))
            })
            .collect();
        assert!(long.is_empty(), "{long:#?}");
    }

    /// every toggle is on the advanced page, once: one added to TOGGLES
    /// needs a place in a section.
    #[test]
    fn every_toggle_has_its_place() {
        let fields: Vec<Field> = sections(Page::Advanced, &Profile::default()).into_iter().flat_map(|(_, fields)| fields).collect();
        for index in 0..TOGGLES.len() {
            let count = fields.iter().filter(|&&field| field == Field::Toggle(index)).count();
            assert_eq!(count, 1, "{} is in the advanced settings {count} times", TOGGLES[index].0);
        }
    }

    #[test]
    fn a_long_name_is_cut_short_to_fit() {
        let font = Font::load();
        let canvas = Canvas::new(&font, (640, 480));
        let name = "Mario & Luigi: Bowser's Inside Story + Bowser Jr.'s Journey";
        let short = shortened(&canvas, SMALL, name, 200.0);
        assert!(short.ends_with("...") && canvas.measure(SMALL, &short) <= 200.0, "{short}");
        assert_eq!(shortened(&canvas, SMALL, "Pokémon Omega Ruby", 200.0), "Pokémon Omega Ruby");
    }

    /// and so does the help of every profile's options.
    #[test]
    fn the_profiles_help_fits_too() {
        let font = Font::load();
        let canvas = Canvas::new(&font, (640, 480));
        let titles = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../titles");
        for entry in std::fs::read_dir(titles).unwrap() {
            let path = entry.unwrap().path().join("profile.toml");
            let profile: Profile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            for option in &profile.options {
                let lines = wrap(&canvas, SMALL, &option.help, 400.0 - 28.0);
                assert!(lines.len() <= 2, "{}: {lines:?}", path.display());
            }
        }
    }
}
