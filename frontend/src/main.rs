//! Pocket Native: a recompiled Nintendo 3DS game on a handheld without a
//! window system, an R36S under ArkOS: the screens on the panel through KMS,
//! the buttons from evdev, the sound through ALSA.
//!
//! ZAKURO_KMS=1 ZAKURO_RECOMPILED=title.so ZAKURO_DATA=dir pocket-native <rom>
//!
//! A tap of FN, or L3 and R2 together, opens the menu, FN+Start quits, a tap
//! of L2 or FN+L1 swaps the screens, FN+R1 shows both or one big, the right
//! stick moves the touch cursor and R2 touches, or tilts the console with L2
//! held. Reset in the menu exits with 3, for the launcher to start the game
//! again.
//!
//! pocket-native --probe <rom> [frames] tries a game as fast as it runs and
//! writes a report of how it went, see probe.rs.

mod font;
mod input;
mod layout;
mod menu;
mod probe;
mod profile;
mod profiler;
mod readahead;
mod settings;
mod sound;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use zakuro_core::services::hid::{parse_input_script, scripted_input, InputState};
use zakuro_core::{loader, Config, FrameOutcome, System};
use zakuro_gpu::Overlay;

use crate::font::{Canvas, Font};
use crate::input::{Pad, FN, L1, L2, L3, R1, R2, R3, START};
use crate::menu::{Action, Menu, Status};
use crate::profile::Profile;
use crate::settings::{Advanced, GameSettings, Layout, Power, Settings};

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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let probing = args.first().is_some_and(|arg| arg == "--probe");
    if probing {
        args.remove(0);
        probe::start_logging();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
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
    let config = Config {
        recompiled: std::env::var("ZAKURO_RECOMPILED").ok().filter(|_| advanced.recompiled).map(Into::into),
        hardware_renderer: true,
        software_draws: !advanced.gpu_drawing,
        data_dir: Some(data.clone()),
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
    let settings_path = settings::config_dir().join("settings.toml");
    let mut settings: Settings = settings::load(&settings_path);
    let base_drops = std::env::var("ZAKURO_DROP_DRAWS").unwrap_or_default();
    apply_options(&mut system, &game, &profile, &base_drops);
    let mut probe = probing.then(|| probe::Probe::new(title_id, args.get(1).and_then(|frames| frames.parse().ok()).unwrap_or(3600)));
    if probe.is_none() {
        apply_power(settings.power);
    }

    let read_ahead = readahead::ReadAhead::start(&path, &data, advanced.read_ahead);
    let pad = Arc::new(Mutex::new(Pad::default()));
    match input::gamepad().filter(|_| std::env::var_os("ZAKURO_INPUT").is_none() && probe.is_none()) {
        Some((device, name)) => input::read_pad(device, name, pad.clone()),
        None => log::warn!("no gamepad found"),
    }
    let (sound, samples) = mpsc::channel();
    sound::play(samples);

    let font = Font::load();
    let mut menu = Menu::default();
    let mut overlay: Option<Arc<Overlay>> = None;
    let mut menu_drawn = false;
    let (mut fps, mut status, mut fps_since) = (0u32, None, Instant::now());
    let mut fn_alone = false;
    let mut exit_code = 0;

    let mut frame = Duration::from_secs_f64(1.0 / 60.0);
    let mut next = Instant::now();
    // ZAKURO_FPS_CAP=n shows the screens at most n times a second whatever
    // the menu says, counted in the title's 60 frames
    let fixed_cap: Option<f64> = std::env::var("ZAKURO_FPS_CAP").ok().and_then(|v| v.parse().ok()).filter(|&n: &f64| n > 0.0);
    let mut presented = 0u64;
    let mut cursor = (160.0f32, 120.0f32);
    let mut held = HashSet::new();
    // when L2 went down, and whether the stick tilted the console since
    let mut tilt_from = (Instant::now(), false);
    let fixed_skip: Option<u32> = std::env::var("ZAKURO_FRAMESKIP").ok().and_then(|value| value.parse().ok());
    let mut most_skipped = fixed_skip.unwrap_or(advanced.frame_skip);
    let mut shown_only = advanced.shown_only;
    let mut skipped = 0;
    // interpolation waits for a second without skipping, it has no time then
    let mut last_skip = 0u64;
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
    let mut logs = 0u32;
    // ZAKURO_SLOW_FRAMES=ms logs every frame that took longer to run
    let slow: Option<f64> = std::env::var("ZAKURO_SLOW_FRAMES").ok().and_then(|ms| ms.parse().ok());
    loop {
        let began = Instant::now();
        let (mut keys, axes) = {
            let pad = pad.lock().unwrap();
            (pad.keys.clone(), pad.axes)
        };
        if let Some((_, scripted)) = keys_script.iter().rev().find(|(from, _)| *from <= tick) {
            keys = scripted.clone();
        }
        tick += 1;
        let pressed = |key: u16| keys.contains(&key) && !held.contains(&key);
        let [fn_pressed, start, l1, l2, r1] = [FN, START, L1, L2, R1].map(pressed);
        // FN pressed and let go on its own opens and closes the menu
        if fn_pressed {
            fn_alone = true;
        } else if keys.contains(&FN) && keys.iter().any(|key| *key != FN && !held.contains(key)) {
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
                Action::Quit => break,
            }
            if menu.open {
                if !menu_drawn {
                    let mut canvas = Canvas::new(&font, PANEL);
                    menu.draw(&mut canvas, &settings, &game, &profile, &title);
                    overlay = Some(Arc::new(canvas.finish()));
                    system.present_panel(layout::areas(settings.layout, settings.swapped), None, overlay.clone(), settings.smooth, false);
                    menu_drawn = true;
                }
                std::thread::sleep(frame);
                continue;
            }
        }

        let hotkey = keys.contains(&FN);
        if hotkey && start {
            break;
        }
        if l2 {
            tilt_from = (Instant::now(), false);
        }
        let reversed = |reverse: bool| if reverse { -1.0 } else { 1.0 };
        let tilt = if keys.contains(&L2) {
            (input::stick(axes[3]) * reversed(settings.reverse_tilt_x), input::stick(axes[4]) * reversed(settings.reverse_tilt_y))
        } else {
            (0.0, 0.0)
        };
        tilt_from.1 |= tilt != (0.0, 0.0);
        // L2 let go soon and without tilting swaps the screens
        let l2_tapped = held.contains(&L2) && !keys.contains(&L2) && !tilt_from.1 && tilt_from.0.elapsed() < Duration::from_millis(300);
        if hotkey && l1 || l2_tapped {
            settings.swapped = !settings.swapped;
        }
        if hotkey && r1 {
            settings.layout = if settings.layout == Layout::TopOnly { Layout::BigTop } else { Layout::TopOnly };
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
        let (buttons, circle_x, circle_y) = if hotkey { (Default::default(), 0.0, 0.0) } else { input::controls(&keys, &axes, &settings) };
        let step = input::cursor_step(settings.touch_speed);
        if !keys.contains(&L2) {
            cursor.0 = (cursor.0 + input::stick(axes[3]) * step).clamp(0.0, 319.0);
            cursor.1 = (cursor.1 + input::stick(axes[4]) * step).clamp(0.0, 239.0);
        }
        let touching = keys.contains(&R2) || keys.contains(&R3);
        if probe.is_some() && script.is_empty() {
            system.set_input(InputState { buttons: probe::Probe::buttons(frame_number), ..Default::default() });
        } else if script.is_empty() {
            system.set_input(InputState { buttons, circle_x, circle_y, touch: touching.then_some((cursor.0 as u16, cursor.1 as u16)), tilt });
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
                "speed {speed:.0}%, rendered {rendered} of 300 frames, {} transfers, {} top pictures, {} bottom, rom {}ms/{}KiB, {} shown",
                transfers - counted_transfers,
                top - counted_top,
                bottom - counted_bottom,
                read_us / 1000,
                read_bytes / 1024,
                shown - counted_shown
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
            if fps_since.elapsed() >= Duration::from_secs(1) {
                let now = Status::now(&settings, fps);
                if now != status {
                    overlay = now.as_ref().map(|now| {
                        let mut canvas = Canvas::new(&font, PANEL);
                        now.draw(&mut canvas, PANEL);
                        Arc::new(canvas.finish())
                    });
                    status = now;
                }
                (fps, fps_since) = (0, Instant::now());
            }
            let areas = layout::areas(settings.layout, settings.swapped);
            let pointer = areas[1].map(|[x, y, width, height]| (x + (cursor.0 * width as f32 / 320.0) as i32, y + (cursor.1 * height as f32 / 240.0) as i32));
            system.present_panel(areas, pointer, overlay.clone(), settings.smooth, interpolating);
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
    if let Some(probe) = probe {
        std::process::exit(probe.report(&mut system, &path, title_id, !profile.name.is_empty(), frame_number, stopped));
    }
    settings::save(&settings_path, &settings);
    settings::save(&game_path, &game);
    std::process::exit(exit_code);
}
