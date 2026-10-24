//! the handheld's buttons and sticks from evdev, and what they are to the
//! title under the controls settings.

use std::collections::HashSet;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use zakuro_core::services::hid::PadState;

use crate::settings::{Buttons, Dpad, Settings, Speed, Stick};

pub const A: u16 = 305;
pub const B: u16 = 304;
pub const X: u16 = 307;
pub const Y: u16 = 308;
pub const L1: u16 = 310;
pub const R1: u16 = 311;
pub const L2: u16 = 312;
pub const R2: u16 = 313;
pub const SELECT: u16 = 704;
pub const START: u16 = 705;
pub const L3: u16 = 706;
pub const R3: u16 = 707;
pub const FN: u16 = 708;
pub const UP: u16 = 544;
pub const DOWN: u16 = 545;
pub const LEFT: u16 = 546;
pub const RIGHT: u16 = 547;
const BUTTONS: [(u16, PadState); 12] = [
    (A, PadState::A),
    (B, PadState::B),
    (X, PadState::X),
    (Y, PadState::Y),
    (L1, PadState::L),
    (R1, PadState::R),
    (SELECT, PadState::SELECT),
    (START, PadState::START),
    (UP, PadState::UP),
    (DOWN, PadState::DOWN),
    (LEFT, PadState::LEFT),
    (RIGHT, PadState::RIGHT),
];
/// a stick's travel, the most it reports.
const STICK: f32 = 1800.0;

#[derive(Default)]
pub struct Pad {
    pub keys: HashSet<u16>,
    pub axes: [i32; 8],
    /// a finger on the screen, where it is in the layout's size, from a
    /// handheld's touch screen.
    pub touch: Option<(f32, f32)>,
    /// a finger on a second window's screen, across and down it from 0 to 1.
    pub second_touch: Option<(f32, f32)>,
}

/// the pad a program around the frontend fills, in place of evdev's.
static SHARED: std::sync::OnceLock<Arc<Mutex<Pad>>> = std::sync::OnceLock::new();

/// set when the program around the frontend wants the game to stop, which
/// saves the settings and ends it, as quitting from the menu does.
pub static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// the pad for the program around the frontend to fill, the same one each
/// call, which the frontend then reads in place of evdev.
#[allow(dead_code)]
pub fn share() -> Arc<Mutex<Pad>> {
    SHARED.get_or_init(Default::default).clone()
}

/// the pad shared, when there is one.
pub fn shared() -> Option<Arc<Mutex<Pad>>> {
    SHARED.get().cloned()
}

/// the handheld's gamepad device and its name.
pub fn gamepad() -> Option<(String, String)> {
    let devices = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    // muOS hides the pad it reads behind its own, so that one goes first
    let mut blocks: Vec<&str> = devices.split("\n\n").collect();
    blocks.sort_by_key(|block| !block.contains("N: Name=\"muOS-Keys\""));
    blocks.into_iter().find_map(|block| {
        // the name comes in quotes
        let name = block.lines().find_map(|line| line.strip_prefix("N: Name="))?.trim_matches('"');
        if !name.contains("Gamepad") && !name.contains("Controller") && !name.contains("Joypad") && !name.contains("joypad") && !h700_pad(name) && !rk3562_pad(name) && name != "muOS-Keys" {
            return None;
        }
        let handlers = block.lines().find_map(|line| line.strip_prefix("H: Handlers="))?;
        let event = handlers.split_whitespace().find(|handler| handler.starts_with("event"))?;
        Some((format!("/dev/input/{event}"), name.to_owned()))
    })
}

/// Anbernic's H700 handhelds, which number their buttons another way and
/// give the d-pad as a hat.
fn h700_pad(name: &str) -> bool {
    name == "Deeplay-keys" || name == "ANBERNIC-keys" || (name.starts_with("Anbernic RG") && !name.contains("ARC"))
}

/// the RK3562 handhelds' own pad (the RG52 Mini, the RG43H Pro), from a
/// kernel driver that gives the usual gamepad codes and sticks of 32767
/// either way. /proc/bus/input/devices puts the name in quotes.
fn rk3562_pad(name: &str) -> bool {
    name.trim_matches('"') == "rk3562-joystick"
}

pub fn read_pad(path: String, name: String, pad: Arc<Mutex<Pad>>) {
    // they come in as the R36S's
    let h700 = h700_pad(&name);
    // Anbernic's RK3566 handhelds (RG353, RG503) give select, start, the
    // sticks' buttons and the right stick the usual numbers
    let rk3566 = name.starts_with("retrogame_joypad");
    // the RK3562 handhelds give them the usual numbers too, with the
    // sticks on the usual axes
    let rk3562 = rk3562_pad(&name);
    // muOS's own pad on the H700 handhelds, the usual gamepad numbers with
    // the d-pad as a hat
    let muos = name == "muOS-Keys";
    log::info!(
        "buttons from {name}{}",
        if h700 {
            ", numbered as on Anbernic's H700 handhelds"
        } else if rk3566 {
            ", numbered as on Anbernic's RK3566 handhelds"
        } else if rk3562 {
            ", numbered as on the RK3562 handhelds"
        } else if muos {
            ", numbered as on muOS"
        } else {
            ""
        }
    );
    // the R36S's stick each of axes 0 to 5 is, scaled to its travel
    let sticks = if h700 {
        Some([None, None, Some(0), Some(1), Some(3), Some(4)])
    } else if rk3566 {
        Some([Some(0), Some(1), Some(3), None, None, Some(4)])
    } else if rk3562 || muos {
        // left stick on ABS_X and ABS_Y, right on ABS_RX and ABS_RY, the
        // triggers on ABS_Z and ABS_RZ, which are buttons here
        Some([Some(0), Some(1), None, Some(3), Some(4), None])
    } else {
        None
    };
    std::thread::spawn(move || {
        let Ok(mut device) = std::fs::File::open(&path) else {
            log::error!("could not open {path}");
            return;
        };
        let ranges: [Option<(i32, i32)>; 6] = std::array::from_fn(|axis| sticks.and_then(|_| axis_range(&device, axis)));
        let mut event = [0u8; 24];
        while device.read_exact(&mut event).is_ok() {
            let kind = u16::from_ne_bytes([event[16], event[17]]);
            let code = u16::from_ne_bytes([event[18], event[19]]);
            let value = i32::from_ne_bytes([event[20], event[21], event[22], event[23]]);
            let mut pad = pad.lock().unwrap();
            match (kind, sticks) {
                (1, _) => {
                    let code = if h700 {
                        h700_button(code)
                    } else if rk3566 {
                        rk3566_button(code)
                    } else if rk3562 {
                        rk3562_button(code)
                    } else if muos {
                        muos_button(code)
                    } else {
                        code
                    };
                    if value == 0 {
                        pad.keys.remove(&code);
                    } else {
                        pad.keys.insert(code);
                    }
                }
                (3, _) if (h700 || muos) && matches!(code, 16 | 17) => {
                    let (less, more) = if code == 16 { (LEFT, RIGHT) } else { (UP, DOWN) };
                    pad.keys.remove(&less);
                    pad.keys.remove(&more);
                    if value != 0 {
                        pad.keys.insert(if value < 0 { less } else { more });
                    }
                }
                (3, Some(sticks)) => {
                    if let Some(axis) = sticks.get(code as usize).copied().flatten() {
                        pad.axes[axis] = match ranges[code as usize] {
                            Some((least, most)) => ((2 * value - least - most) as f32 / (most - least) as f32 * STICK) as i32,
                            None => value,
                        };
                    }
                }
                (3, None) if (code as usize) < pad.axes.len() => pad.axes[code as usize] = value,
                _ => {}
            }
        }
    });
}

/// an H700 handheld's button as the R36S numbers it.
fn h700_button(code: u16) -> u16 {
    match code {
        304 => A,
        305 => B,
        306 => Y,
        308 => L1,
        309 => R1,
        310 => SELECT,
        311 => START,
        312 => FN,
        313 => L3,
        314 => L2,
        315 => R2,
        316 => R3,
        other => other,
    }
}

/// an RK3566 handheld's button as the R36S numbers it.
fn rk3566_button(code: u16) -> u16 {
    match code {
        314 => SELECT,
        315 => START,
        316 => FN,
        317 => L3,
        318 => R3,
        other => other,
    }
}

/// an RK3562 handheld's button as the R36S numbers it: select, start, home
/// and the sticks' buttons have the usual gamepad codes (314 to 318), as on
/// the RK3566 handhelds, and the extra FN or back button the first
/// trigger-happy code (704), which the R36S numbers select, so it is
/// another FN here. The face buttons, shoulders, triggers and d-pad already
/// have the R36S's numbers.
fn rk3562_button(code: u16) -> u16 {
    match code {
        314 => SELECT,
        315 => START,
        316 => FN,
        317 => L3,
        318 => R3,
        704 => FN,
        other => other,
    }
}

/// a button of muOS's pad as the R36S numbers it: A and B the other way
/// round, select, start, home and the sticks' buttons the usual gamepad
/// codes. X, Y, the shoulders and the triggers already have the R36S's.
fn muos_button(code: u16) -> u16 {
    match code {
        304 => A,
        305 => B,
        314 => SELECT,
        315 => START,
        316 => FN,
        317 => L3,
        318 => R3,
        other => other,
    }
}

/// an axis's least and most, as the device gives them.
fn axis_range(device: &std::fs::File, axis: usize) -> Option<(i32, i32)> {
    let mut info = [0i32; 6];
    // SAFETY: EVIOCGABS fills the six numbers of the axis's input_absinfo
    let read = unsafe { libc::ioctl(device.as_raw_fd(), (0x8018_4540 + axis) as _, info.as_mut_ptr()) };
    (read >= 0 && info[2] > info[1]).then_some((info[1], info[2]))
}

/// ZAKURO_KEYS=tick:KEY+KEY,... holds the handheld's keys by name from each
/// tick of the frontend's loop, which goes on while the game is paused, for
/// trying the menu without a gamepad.
pub fn parse_keys(spec: &str) -> Vec<(u64, HashSet<u16>)> {
    let code = |name: &str| match name {
        "A" => Some(A),
        "B" => Some(B),
        "X" => Some(X),
        "Y" => Some(Y),
        "L1" => Some(L1),
        "R1" => Some(R1),
        "L2" => Some(L2),
        "R2" => Some(R2),
        "L3" => Some(L3),
        "SELECT" => Some(SELECT),
        "START" => Some(START),
        "FN" => Some(FN),
        "UP" => Some(UP),
        "DOWN" => Some(DOWN),
        "LEFT" => Some(LEFT),
        "RIGHT" => Some(RIGHT),
        _ => None,
    };
    spec.split(',')
        .filter_map(|entry| {
            let (tick, keys) = entry.trim().split_once(':')?;
            Some((tick.parse().ok()?, keys.split('+').filter_map(|name| code(name.trim())).collect()))
        })
        .collect()
}

/// a stick's position from -1 to 1, nothing near the middle.
pub fn stick(value: i32) -> f32 {
    if value.abs() < 200 { 0.0 } else { (value as f32 / STICK).clamp(-1.0, 1.0) }
}

/// the title's buttons and circle pad for the keys held and the sticks.
pub fn controls(keys: &HashSet<u16>, axes: &[i32; 8], settings: &Settings) -> (PadState, f32, f32) {
    let mut buttons = PadState::empty();
    for (key, button) in BUTTONS {
        let key = match (settings.buttons, key) {
            (Buttons::Xbox, A) => B,
            (Buttons::Xbox, B) => A,
            (Buttons::Xbox, X) => Y,
            (Buttons::Xbox, Y) => X,
            _ => key,
        };
        if keys.contains(&key) && (settings.dpad == Dpad::Dpad || ![UP, DOWN, LEFT, RIGHT].contains(&key)) {
            buttons |= button;
        }
    }
    let (x, y) = match dpad(keys) {
        (across, down) if settings.dpad == Dpad::CirclePad && (across, down) != (0.0, 0.0) => (across, -down),
        _ => (stick(axes[0]), -stick(axes[1])),
    };
    match settings.left_stick {
        Stick::CirclePad => (buttons, x, y),
        Stick::Dpad => {
            for (pushed, button) in [(x < -0.5, PadState::LEFT), (x > 0.5, PadState::RIGHT), (y > 0.5, PadState::UP), (y < -0.5, PadState::DOWN)] {
                if pushed {
                    buttons |= button;
                }
            }
            (buttons, 0.0, 0.0)
        }
    }
}

/// the d-pad held as a stick's position, across and down.
pub fn dpad(keys: &HashSet<u16>) -> (f32, f32) {
    let axis = |less: u16, more: u16| keys.contains(&more) as i32 as f32 - keys.contains(&less) as i32 as f32;
    (axis(LEFT, RIGHT), axis(UP, DOWN))
}

/// how far the touch cursor goes a frame with the right stick held over.
pub fn cursor_step(speed: Speed) -> f32 {
    match speed {
        Speed::Slow => 2.0,
        Speed::Normal => 4.0,
        Speed::Fast => 7.0,
    }
}
