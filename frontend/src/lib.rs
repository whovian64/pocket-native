//! Pocket Native: a recompiled Nintendo 3DS game on a handheld without a
//! window system, an R36S under ArkOS: the screens on the panel through KMS,
//! the buttons from evdev, the sound through ALSA.
//!
//! ZAKURO_KMS=1 ZAKURO_RECOMPILED=title.so ZAKURO_DATA=dir pocket-native <rom>
//!
//! A tap of FN, or L3 and R2 together, opens the menu, FN+Start quits, a tap
//! of L2 or FN+L1 swaps the screens, FN+R1 shows both or one big, FN+Select
//! turns the d-pad into the circle pad or the touch cursor, L3 doing for FN
//! in these, the right stick moves the touch cursor and R2 touches, or tilts
//! the console with L2 held, and R3 makes it the C-stick with L2 and R2 as
//! ZL and ZR, and back. Reset in the menu exits with 3, for the
//! launcher to start the game again.
//!
//! pocket-native --probe <rom> [frames] tries a game as fast as it runs and
//! writes a report of how it went, see probe.rs.

mod busy;
mod crash;
mod font;
pub mod input;
mod layout;
mod menu;
mod probe;
mod profile;
mod profiler;
mod readahead;
mod settings;
mod sound;
mod touch;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use zakuro_core::services::hid::{parse_input_script, scripted_input, InputState, PadState};
use zakuro_core::{loader, Config, FrameOutcome, System};
use zakuro_gpu::Overlay;

use crate::font::{Canvas, Font};
use crate::input::{Pad, FN, L1, L2, L3, R1, R2, R3, SELECT, START};
use crate::menu::{Action, Menu, Status};
use crate::profile::Profile;
use crate::settings::{Advanced, Cursor, Dpad, GameSettings, Layout, Power, RightStick, Rotation, Settings};

/// the exit code that asks the launcher to start the game again.
const RESET: i32 = 3;
const PANEL: (u32, u32) = (640, 480);

/// takes up the game's options: the draws they leave out, on top of
/// ZAKURO_DROP_DRAWS, the bottom screen's rate and skipping, and the right
/// eye's.
fn apply_options(system: &mut System, game: &GameSettings, profile: &Profile, base: &str) {
    let mut rules = vec![base.to_owned()];
    let mut full_rate = false;
    for option in &profile.options {
        let on = game.options.get(&option.name).copied().unwrap_or(option.default);
        if !on {
            rules.extend(option.drops_when_off.iter().cloned());
        }
        full_rate |= on && option.bottom_full_rate_when_on;
    }
    zakuro_gpu::set_drops(&rules.join(","));
    system.set_bottom_full_rate(full_rate);
    let advanced = Advanced::of(game, profile);
    system.set_skip_bottom(advanced.skip_bottom);
    system.set_bottom_relearn(profile.bottom_relearn);
    system.set_skip_right_eye(advanced.skip_right_eye);
}

/// a .3ds/.cci image's title id from its header, 0 for anything else.
fn title_id_of(path: &str) -> u64 {
    let mut header = [0u8; 0x110];
    let read = std::fs::File::open(path).and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header));
    if read.is_err() || &header[0x100..0x104] != b"NCSD" {
        return 0;
    }
    u64::from_le_bytes(header[0x108..0x110].try_into().unwrap())
}

/// POCKET_NATIVE_POWER names a program the launcher gives for the system's
/// CPU and GPU speeds, run with saver, balanced or performance.
fn apply_power(power: Power) {
    let Some(program) = std::env::var_os("POCKET_NATIVE_POWER") else { return };
    let mode = match power {
        Power::Saver => "saver",
        Power::Balanced => "balanced",
        Power::Performance => "performance",
    };
    if let Err(error) = std::process::Command::new(&program).arg(mode).spawn() {
        log::warn!("could not run {}, {error}", program.to_string_lossy());
    }
}

/// runs the frontend with the program's arguments, the ROM first, or
/// --probe, the ROM and frames. Ends the process when the game is done.
pub fn run(mut args: Vec<String>) {
    let probing = args.first().is_some_and(|arg| arg == "--probe");
    if probing {
        args.remove(0);
        probe::start_logging();
    } else {
        let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
        // POCKET_NATIVE_LOG=file logs there in place of standard error, which
        // goes nowhere in an Android app
        let file = std::env::var_os("POCKET_NATIVE_LOG").and_then(|path| std::fs::File::create(path).ok());
        let crash_log = file.as_ref().and_then(|file| file.try_clone().ok()).map(std::os::fd::IntoRawFd::into_raw_fd);
        if let Some(file) = file {
            logger.target(env_logger::Target::Pipe(Box::new(file)));
        }
        logger.init();
        crash::watch(crash_log);
        // a note from a program around the frontend, which has no log of its own
        if let Ok(note) = std::env::var("POCKET_NATIVE_NOTE") {
            log::info!("{note}");
        }
    }
    let Some(path) = args.first().cloned() else {
        eprintln!("usage: pocket-native <rom>, or pocket-native --probe <rom> [frames]");
        std::process::exit(2);
    };
    let data = PathBuf::from(std::env::var_os("ZAKURO_DATA").unwrap_or_else(|| ".".into()));
    let game_path = data.join("settings.toml");
    let mut game: GameSettings = settings::load(&game_path);
    let profile = profile::find(&profile::titles_dir(), title_id_of(&path)).unwrap_or_default();
    let advanced = Advanced::of(&game, &profile);
    advanced.export();
    let settings_path = settings::config_dir().join("settings.toml");
    let mut settings: Settings = settings::load(&settings_path);
    // the menu's rotation over ZAKURO_PANEL_ROTATE, before the panel starts
    let rotate = match settings.rotation {
        Rotation::Auto => None,
        Rotation::Clockwise => Some("90"),
        Rotation::CounterClockwise => Some("270"),
        Rotation::Off => Some("0"),
    };
    if let Some(rotate) = rotate {
        std::env::set_var("ZAKURO_PANEL_ROTATE", rotate);
    }
    let config = Config {
        recompiled: std::env::var("ZAKURO_RECOMPILED").ok().filter(|_| advanced.recompiled).map(Into::into),
        hardware_renderer: true,
        software_draws: !advanced.gpu_drawing,
        data_dir: Some(data.clone()),
        language: game.language,
        region: game.region,
        // ZAKURO_RESOLUTION=2.5 over the menu's, for trying one
        resolution: std::env::var("ZAKURO_RESOLUTION").ok().and_then(|v| v.parse().ok()).unwrap_or(settings.resolution),
        // ZAKURO_CLOCK=milliseconds since 1900 starts the clock there, as boot
        // takes it, so that a scripted run meets the same encounters
        clock: std::env::var("ZAKURO_CLOCK").ok().and_then(|v| v.parse().ok()),
        ..Config::default()
    };
    let mut system = match loader::load(&path, config) {
        Ok(system) => system,
        Err(error) => {
            eprintln!("failed to load {path}: {error}");
            std::process::exit(1);
        }
    };

    let title_id = system.title.as_ref().map_or(0, |title| title.program_id());
    let title = if profile.name.is_empty() { format!("{title_id:016X}") } else { profile.name.clone() };
    log::info!("Pocket Native {}, {title}, {} options", env!("CARGO_PKG_VERSION"), profile.options.len());
    let base_drops = std::env::var("ZAKURO_DROP_DRAWS").unwrap_or_default();
    apply_options(&mut system, &game, &profile, &base_drops);
    let mut probe = probing.then(|| probe::Probe::new(title_id, args.get(1).and_then(|frames| frames.parse().ok()).unwrap_or(3600)));
    if probe.is_none() {
        apply_power(settings.power);
    }

    let read_ahead = readahead::ReadAhead::start(&path, &data, advanced.read_ahead);
    // the buttons from a program around the frontend, an Android activity's
    // events, or else from evdev
    let pad = match input::shared() {
        Some(pad) => pad,
        None => {
            let pad = Arc::new(Mutex::new(Pad::default()));
            match input::gamepad().filter(|_| std::env::var_os("ZAKURO_INPUT").is_none() && probe.is_none()) {
                Some((device, name)) => input::read_pad(device, name, pad.clone()),
                None => log::warn!("no gamepad found"),
            }
            if std::env::var_os("ZAKURO_INPUT").is_none() && probe.is_none() {
                for (device, bottom) in input::touchscreens() {
                    input::read_touch(device, bottom, pad.clone());
                }
            }
            pad
        }
    };
    let (sound, samples) = mpsc::channel();
    sound::play(samples);

    let font = Font::load();
    let mut menu = Menu::default();
    let mut overlay: Option<Arc<Overlay>> = None;
    let mut controls = touch::Controls::load();
    // whether the menu went up, and with which of the touch controls' overlays
    let (mut menu_drawn, mut menu_controls) = (false, None);
    let (mut fps, mut last_fps, mut status, mut fps_since) = (0u32, 0u32, None, Instant::now());
    // what the screen says for a moment after a hotkey, and since when
    let mut note: Option<(&str, Instant)> = None;
    let mut fn_alone = false;
    let mut exit_code = 0;

    let mut frame = Duration::from_secs_f64(1.0 / 60.0);
    let mut next = Instant::now();
    // ZAKURO_FPS_CAP=n shows the screens at most n times a second whatever
    // the menu says, counted in the title's 60 frames
    let fixed_cap: Option<f64> = std::env::var("ZAKURO_FPS_CAP").ok().and_then(|v| v.parse().ok()).filter(|&n: &f64| n > 0.0);
    let mut presented = 0u64;
    let mut cursor = (160.0f32, 120.0f32);
    // when the stick or the d-pad last moved the cursor
    let mut cursor_moved: Option<Instant> = None;
    let mut held = HashSet::new();
    // when L2 went down, and whether the stick tilted the console since
    let mut tilt_from = (Instant::now(), false);
    let fixed_skip: Option<u32> = std::env::var("ZAKURO_FRAMESKIP").ok().and_then(|value| value.parse().ok());
    let mut most_skipped = fixed_skip.unwrap_or(advanced.frame_skip);
    let mut shown_only = advanced.shown_only;
    let mut skipped = 0;
    // interpolation waits for a second without skipping, it has no time then
    let mut last_skip = 0u64;
    // sound mixes beside the title, as on the console's DSP
    if std::env::var_os("ZAKURO_SYNC_SOUND").is_none() {
        system.enable_async_sound();
    }
    if std::env::var_os("ZAKURO_SYNC_GPU").is_none() {
        system.enable_async_gpu();
        system.realtime_gpu = std::env::var_os("ZAKURO_GPU_REALTIME").is_some();
        if system.realtime_gpu {
            log::info!("switch: real-time GPU, ZAKURO_GPU_REALTIME");
        }
    } else {
        log::info!("switch: GPU thread off, ZAKURO_SYNC_GPU");
    }
    // a script of buttons by frame in place of the pad, for timing a run,
    // as boot takes it
    let script = std::env::var("ZAKURO_INPUT").map(|spec| parse_input_script(&spec)).unwrap_or_default();
    // ZAKURO_SHOT_AT=frame,frame saves both screens at those frames into the
    // temporary directory, as boot does, or a probe's directory in place of
    // its own eight
    // ZAKURO_HOST_PROFILE=from,frames samples where this process's threads
    // spend their time over those frames, into the temporary directory
    let host_profile = std::env::var("ZAKURO_HOST_PROFILE").ok().and_then(|spec| {
        let (from, frames) = spec.split_once(',')?;
        Some((from.parse::<u64>().ok()?, frames.parse::<u64>().ok()?))
    });
    let shots: HashSet<u64> = match (&probe, std::env::var("ZAKURO_SHOT_AT")) {
        (_, Ok(frames)) => frames.split(',').filter_map(|frame| frame.parse().ok()).collect(),
        (Some(probe), Err(_)) => probe.shot_frames(),
        (None, Err(_)) => HashSet::new(),
    };
    let mut stopped = None;
    let keys_script = std::env::var("ZAKURO_KEYS").map(|spec| input::parse_keys(&spec)).unwrap_or_default();
    let mut tick = 0u64;
    let mut frame_number = 0u64;
    let (mut counted_transfers, mut counted_top, mut counted_bottom, mut counted_shown) = (0u64, 0u64, 0u64, 0u64);
    let (mut counted, mut rendered, mut since) = (0u32, 0u32, Instant::now());
    let mut busy = busy::Busy::start();
    let mut logs = 0u32;
    // ZAKURO_SLOW_FRAMES=ms logs every frame that took longer to run
    let slow: Option<f64> = std::env::var("ZAKURO_SLOW_FRAMES").ok().and_then(|ms| ms.parse().ok());
    loop {
        let began = Instant::now();
        let (mut keys, mut axes, fingers, second_touch, pad_used) = {
            let mut pad = pad.lock().unwrap();
            let keys: HashSet<u16> = pad.keys.union(&pad.let_go).copied().collect();
            let fingers: Vec<(i32, f32, f32)> = pad.fingers.iter().chain(&pad.lifted).copied().collect();
            pad.let_go.clear();
            pad.lifted.clear();
            (keys, pad.axes, fingers, pad.second_touch, pad.pad_used)
        };
        if input::QUIT.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!("asked to quit");
            break;
        }
        // away: nothing runs, the screens only go up now and then, which is
        // when the renderer lets go of a window that went
        if input::PAUSED.load(std::sync::atomic::Ordering::Relaxed) {
            system.present_panel(layout::shown(&settings), None, Vec::new(), settings.smooth, false);
            std::thread::sleep(frame);
            next = Instant::now();
            continue;
        }
        if let Some((_, scripted)) = keys_script.iter().rev().find(|(from, _)| *from <= tick) {
            keys = scripted.clone();
        }
        // the touch controls moved and sized with the game paused under them,
        // until Done or FN
        if controls.editing() {
            let fn_pressed = keys.contains(&FN) && !held.contains(&FN);
            if controls.edit(&font, zakuro_gpu::layout_size(), layout::shown(&settings), &fingers, fn_pressed) {
                next = Instant::now();
            } else {
                let edited = controls.edit_overlay(&font);
                system.present_panel(layout::shown(&settings), None, vec![edited], settings.smooth, false);
            }
            held = keys;
            std::thread::sleep(frame);
            continue;
        }
        // the touch controls hold keys and the circle pad as the gamepad
        // would, and the fingers on none of them touch the screens
        let on_screen = (settings.touch_controls && !pad_used).then(|| controls.hold(&font, zakuro_gpu::layout_size(), layout::shown(&settings), &fingers));
        let screen_touch = match &on_screen {
            Some(held) => {
                keys.extend(&held.keys);
                if let Some((across, down)) = held.stick {
                    axes[0] = (across * input::STICK) as i32;
                    axes[1] = (down * input::STICK) as i32;
                }
                held.free.first().copied()
            }
            None => fingers.first().map(|&(_, x, y)| (x, y)),
        };
        let controls_overlay = on_screen.as_ref().map(|held| controls.overlay(&font, held));
        tick += 1;
        let pressed = |key: u16| keys.contains(&key) && !held.contains(&key);
        let [fn_pressed, start, select, l1, l2, r1] = [FN, START, SELECT, L1, L2, R1].map(pressed);
        // FN pressed and let go on its own opens and closes the menu
        if fn_pressed {
            fn_alone = true;
        }
        if keys.contains(&FN) && keys.iter().any(|key| *key != FN && !held.contains(key)) {
            fn_alone = false;
        }
        let tapped = fn_alone && held.contains(&FN) && !keys.contains(&FN);
        fn_alone &= !tapped;
        // L3 and R2 together do the same, for handhelds with no FN
        let tapped = tapped || [L3, R2].iter().all(|key| keys.contains(key)) && ![L3, R2].iter().all(|key| held.contains(key));

        if menu.open || tapped {
            let action = if !menu.open {
                menu.show();
                Action::Redraw
            } else if tapped {
                menu.open = false;
                Action::Resume
            } else {
                menu.input(&keys, &mut settings, &mut game, &profile)
            };
            held = keys.clone();
            match action {
                Action::None => {}
                Action::Redraw => menu_drawn = false,
                Action::Changed => {
                    apply_options(&mut system, &game, &profile, &base_drops);
                    let advanced = Advanced::of(&game, &profile);
                    (most_skipped, shown_only) = (fixed_skip.unwrap_or(advanced.frame_skip), advanced.shown_only);
                    apply_power(settings.power);
                    menu_drawn = false;
                }
                Action::Resume => {
                    settings::save(&settings_path, &settings);
                    settings::save(&game_path, &game);
                    (overlay, status, next) = (None, None, Instant::now());
                }
                Action::Reset => {
                    exit_code = RESET;
                    break;
                }
                Action::EditControls => {
                    settings::save(&settings_path, &settings);
                    (overlay, status) = (None, None);
                    controls.start_editing();
                    continue;
                }
                Action::Quit => break,
            }
            if menu.open {
                let drawn_controls = controls_overlay.as_ref().map(|overlay| Arc::as_ptr(overlay) as usize);
                if !menu_drawn || drawn_controls != menu_controls {
                    if !menu_drawn {
                        let mut canvas = Canvas::new(&font, PANEL);
                        menu.draw(&mut canvas, &settings, &game, &profile, &title);
                        overlay = Some(Arc::new(canvas.finish()));
                    }
                    system.present_panel(layout::shown(&settings), None, overlays(&overlay, &controls_overlay), settings.smooth, false);
                    (menu_drawn, menu_controls) = (true, drawn_controls);
                }
                std::thread::sleep(frame);
                continue;
            }
        }

        // L3 stands in for FN on handhelds with no FN
        let hotkey = keys.contains(&FN) || keys.contains(&L3);
        if hotkey && start {
            break;
        }
        if l2 {
            tilt_from = (Instant::now(), false);
        }
        // the right stick as the C-stick, L2 and R2 as ZL and ZR, which
        // leaves tilting and touching to the cursor's way
        let c_stick = settings.right_stick == RightStick::CStick;
        let reversed = |reverse: bool| if reverse { -1.0 } else { 1.0 };
        let tilt = if keys.contains(&L2) && !c_stick {
            (input::stick(axes[3]) * reversed(settings.reverse_tilt_x), input::stick(axes[4]) * reversed(settings.reverse_tilt_y))
        } else {
            (0.0, 0.0)
        };
        tilt_from.1 |= tilt != (0.0, 0.0);
        // L2 let go soon and without tilting swaps the screens
        let l2_tapped = !c_stick && held.contains(&L2) && !keys.contains(&L2) && !tilt_from.1 && tilt_from.0.elapsed() < Duration::from_millis(300);
        if hotkey && l1 || l2_tapped {
            settings.swapped = !settings.swapped;
        }
        if hotkey && r1 {
            settings.layout = if settings.layout == Layout::TopOnly { Layout::BigTop } else { Layout::TopOnly };
        }
        if hotkey && select {
            let (dpad, said) = match settings.dpad {
                Dpad::Dpad => (Dpad::CirclePad, "D-pad: circle pad"),
                Dpad::CirclePad => (Dpad::Touch, "D-pad: touch cursor"),
                Dpad::Touch => (Dpad::Dpad, "D-pad: d-pad"),
            };
            (settings.dpad, note) = (dpad, Some((said, Instant::now())));
        }
        // R3 switches the right stick between the touch cursor and the C-stick
        if !hotkey && keys.contains(&R3) && !held.contains(&R3) {
            let (right_stick, said) = match settings.right_stick {
                RightStick::Cursor => (RightStick::CStick, "Right stick: C-stick"),
                RightStick::CStick => (RightStick::Cursor, "Right stick: touch cursor"),
            };
            (settings.right_stick, note) = (right_stick, Some((said, Instant::now())));
        }
        held = keys.clone();

        let behind = match probe.as_mut() {
            Some(probe) => probe.behind(),
            None => Instant::now() > next + frame / 2,
        };
        let skip = behind && skipped < most_skipped;
        skipped = if skip { skipped + 1 } else { 0 };
        if skip {
            last_skip = frame_number;
        }
        let interpolating = settings.interpolate_to > 0 && frame_number > last_skip + 60;
        // a frame the panel will not show, which a title making a picture
        // every frame need not draw
        let cap = fixed_cap.unwrap_or(settings.frame_cap as f64);
        let unshown = shown_only && probe.is_none() && settings.interpolate_to == 0 && frame_number + 1 < presented + (60.0 / cap).ceil() as u64;
        system.skip_frames(skip, unshown);
        let (mut buttons, circle_x, circle_y) = if hotkey { (Default::default(), 0.0, 0.0) } else { input::controls(&keys, &axes, &settings) };
        let (c_stick_x, c_stick_y) = if c_stick && !hotkey { (input::stick(axes[3]), -input::stick(axes[4])) } else { (0.0, 0.0) };
        if c_stick && !hotkey {
            buttons.set(PadState::ZL, keys.contains(&L2));
            buttons.set(PadState::ZR, keys.contains(&R2));
        }
        let step = input::cursor_step(settings.touch_speed);
        if c_stick || !keys.contains(&L2) {
            let (across, down) = if settings.dpad == Dpad::Touch { input::dpad(&keys) } else { (0.0, 0.0) };
            let (x, y) = if c_stick { (0.0, 0.0) } else { (input::stick(axes[3]), input::stick(axes[4])) };
            let was = cursor;
            cursor.0 = (cursor.0 + (x + across).clamp(-1.0, 1.0) * step).clamp(0.0, 319.0);
            cursor.1 = (cursor.1 + (y + down).clamp(-1.0, 1.0) * step).clamp(0.0, 239.0);
            if cursor != was {
                cursor_moved = Some(Instant::now());
            }
        }
        let mut touching = !c_stick && keys.contains(&R2);
        // a finger on the bottom screen where it is shown touches there, and
        // moves the cursor along: on the panel, unless the bottom screen is on
        // a second window by itself, or there
        let bottom_alone = zakuro_gpu::second_window() && !settings.swapped;
        if let Some((across, down)) = second_touch.filter(|_| bottom_alone) {
            cursor = ((across * 320.0).clamp(0.0, 319.0), (down * 240.0).clamp(0.0, 239.0));
            touching = true;
        }
        if let Some((x, y)) = screen_touch.filter(|_| !bottom_alone) {
            if let Some([left, top, width, height]) = layout::shown(&settings)[1] {
                let (across, down) = ((x - left as f32) / width as f32, (y - top as f32) / height as f32);
                if (0.0..1.0).contains(&across) && (0.0..1.0).contains(&down) {
                    cursor = (across * 320.0, down * 240.0);
                    touching = true;
                }
            }
        }
        if probe.is_some() && script.is_empty() {
            system.set_input(InputState { buttons: probe::Probe::buttons(frame_number), ..Default::default() });
        } else if script.is_empty() {
            system.set_input(InputState { buttons, circle_x, circle_y, c_stick_x, c_stick_y, touch: touching.then_some((cursor.0 as u16, cursor.1 as u16)), tilt });
        } else {
            system.set_input(scripted_input(&script, frame_number));
        }
        if let Some((from, frames)) = host_profile {
            if frame_number == from {
                profiler::start();
            } else if frame_number == from + frames {
                profiler::dump(&std::env::temp_dir().join("zakuro-host-profile.txt"));
            }
        }
        if let Some(probe) = probe.as_mut() {
            probe.ran(skip);
            if frame_number.is_multiple_of(10) {
                probe.watch(&mut system);
            }
            if shots.contains(&frame_number) {
                probe.shoot(&mut system, frame_number);
            }
        } else if shots.contains(&frame_number) {
            for (screen, name) in [(zakuro_common::Screen::Top, "top"), (zakuro_common::Screen::Bottom, "bottom")] {
                let (pixels, width, height) = system.read_screen_scaled(screen);
                let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
                ppm.extend(pixels.as_chunks::<4>().0.iter().flat_map(|pixel| &pixel[..3]));
                let _ = std::fs::write(std::env::temp_dir().join(format!("zakuro-{name}-{frame_number}.ppm")), ppm);
            }
        }
        frame_number += 1;

        match system.run_frame() {
            FrameOutcome::Completed => {}
            outcome => {
                log::info!("the title stopped, {outcome:?}");
                stopped = Some(outcome);
                break;
            }
        }
        if let Some(request) = system.keyboard_request() {
            let confirm = request.buttons.len().saturating_sub(1);
            system.answer_keyboard("Zakuro", confirm);
        }
        let audio = system.take_audio();
        if !audio.is_empty() && probe.is_none() {
            let _ = sound.send(audio);
        }

        counted += 1;
        if counted == 300 {
            let speed = 300.0 / 60.0 / since.elapsed().as_secs_f64() * 100.0;
            let (transfers, top, bottom, shown) = (system.transfers(), system.top_pictures(), system.bottom_pictures(), system.shown());
            let (read_us, read_bytes) = zakuro_core::services::fs::rom_reads();
            log::info!(
                "speed {speed:.0}%, rendered {rendered} of 300 frames, {} transfers, {} top pictures, {} bottom, rom {}ms/{}KiB, {} shown, {}",
                transfers - counted_transfers,
                top - counted_top,
                bottom - counted_bottom,
                read_us / 1000,
                read_bytes / 1024,
                shown - counted_shown,
                busy.report()
            );
            (counted_transfers, counted_top, counted_bottom, counted_shown) = (transfers, top, bottom, shown);
            (counted, rendered, since) = (0, 0, Instant::now());
            logs += 1;
            if logs.is_multiple_of(3) {
                read_ahead.save();
            }
        }
        if !skip {
            rendered += 1;
        }
        let every = if interpolating { 60 / settings.interpolate_to as u64 } else { (60.0 / cap).ceil().max(1.0) as u64 };
        if !skip && frame_number >= presented + every {
            presented = frame_number;
            fps += 1;
            let second = fps_since.elapsed() >= Duration::from_secs(1);
            if second {
                (last_fps, fps, fps_since) = (fps, 0, Instant::now());
            }
            let said = note.filter(|(_, at)| at.elapsed() < Duration::from_secs(2)).map(|(said, _)| said);
            if second || said != status.as_ref().and_then(|status: &Status| status.note) {
                let now = Status::now(&settings, last_fps, said);
                if now != status {
                    overlay = now.as_ref().map(|now| {
                        let mut canvas = Canvas::new(&font, PANEL);
                        now.draw(&mut canvas, PANEL);
                        Arc::new(canvas.finish())
                    });
                    status = now;
                }
            }
            let areas = layout::shown(&settings);
            // on two screens a finger touches the bottom one, and the
            // cursor shows only for a moment after the stick moved it
            let two_screens = zakuro_gpu::stacked() || zakuro_gpu::second_window();
            let cursor_shown = match settings.cursor {
                Cursor::Shown => true,
                Cursor::Hidden => false,
                Cursor::Auto => !two_screens || cursor_moved.is_some_and(|moved| moved.elapsed() < Duration::from_secs(2)),
            };
            let pointer = areas[1].filter(|_| cursor_shown).map(|[x, y, width, height]| (x + (cursor.0 * width as f32 / 320.0) as i32, y + (cursor.1 * height as f32 / 240.0) as i32));
            system.present_panel(areas, pointer, overlays(&overlay, &controls_overlay), settings.smooth, interpolating);
        }

        if let Some(limit) = slow {
            let took = began.elapsed().as_secs_f64() * 1000.0;
            if took > limit {
                log::info!("slow frame {}: {took:.0} ms{}", frame_number - 1, if skip { ", skipped" } else { "" });
            }
        }
        if probe.as_ref().is_some_and(|probe| frame_number >= probe.frames) {
            break;
        }
        next += frame;
        if probe.is_some() {
            continue;
        }
        // a frame lasts as long as the panel's refresh, and the frames move
        // to hand the panel its pictures halfway between two refreshes, where
        // a little late or early does not hold one up a refresh longer
        if let Some((period, slack)) = zakuro_gpu::panel_timing() {
            frame = period;
            if let Some(slack) = slack.filter(|_| !behind) {
                let error = slack.as_secs_f64() - period.as_secs_f64() / 2.0;
                let nudge = Duration::from_secs_f64((error.abs() / 4.0).min(0.001));
                next = if error > 0.0 { next + nudge } else { next - nudge };
            }
        }
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > frame * 4 {
            // a game this far behind stays four frames behind, so the next
            // frame still skips, rather than catching up after a long stall
            next = now - frame * 4;
        }
    }
    read_ahead.save();
    // a run that ends inside the profiled frames still stops the timer,
    // which left going holds up the exit, and writes what it sampled
    if profiler::running() {
        profiler::dump(&std::env::temp_dir().join("zakuro-host-profile.txt"));
    }
    if let Some(probe) = probe {
        let code = probe.report(&mut system, &path, title_id, !profile.name.is_empty(), frame_number, stopped);
        system.stop_gpu();
        std::process::exit(code);
    }
    settings::save(&settings_path, &settings);
    settings::save(&game_path, &game);
    system.stop_gpu();
    if let Some(before) = BEFORE_EXIT.get() {
        before(exit_code);
    }
    std::process::exit(exit_code);
}

/// the overlays drawn over the screens, the menu or the status line, then
/// the touch controls.
fn overlays(overlay: &Option<Arc<Overlay>>, controls: &Option<Arc<Overlay>>) -> Vec<Arc<Overlay>> {
    overlay.iter().chain(controls).cloned().collect()
}

/// what a program around the frontend does with the exit code as a game
/// ends, before the process does: an Android activity finishes with it.
pub static BEFORE_EXIT: std::sync::OnceLock<fn(i32)> = std::sync::OnceLock::new();
