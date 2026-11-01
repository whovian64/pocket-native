//! Pocket Native's game on Android: the NativeActivity the launcher starts
//! in a process of its own for one game, whose window the screens go to and
//! whose key and motion events become the pad's buttons, sticks and touch.
//! Quitting ends the process, which leaves the launcher showing again.
//!
//! The launcher names the game's folder in the intent's "game" extra: its
//! game.3ds, sysdata/ and data/ (saves, the log), in the app's folder on
//! shared storage. The game's code is code/<title id>.so in the app's own
//! storage, the one place Android loads code from, and the profiles,
//! settings and the shader cache are beside it.
//!
//! On a handheld with a second display, the RG DS's bottom panel, one of
//! the screens goes there by itself: SecondScreen.java shows a Presentation
//! on it, whose surface and touches come back here through JNI. Under
//! GammaOS's DualStack the window is both panels stacked, and the screens go
//! on its halves, with no Presentation.

#![cfg(target_os = "android")]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use android_activity::input::{Axis, InputEvent, KeyAction, Keycode, MotionAction};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use jni::objects::{JClass, JObject, JValue};
use jni::sys::{jboolean, jfloat, jint, JNI_TRUE};
use jni::{JNIEnv, JavaVM, NativeMethod};

use pocket_native::input::{self, Pad, A, B, DOWN, FN, L1, L2, L3, LEFT, R1, R2, R3, RIGHT, SELECT, START, UP, X, Y};

/// how far the frontend's sticks go, as input::stick takes them.
const STICK: f32 = 1800.0;
/// AINPUT_SOURCE_CLASS_JOYSTICK and AINPUT_SOURCE_TOUCHSCREEN's bits.
const JOYSTICK: u32 = 0x0000_0010;
const TOUCHSCREEN: u32 = 0x0000_1002;

#[no_mangle]
fn android_main(app: AndroidApp) {
    let own = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp/pocket-native"));
    let folder = match game_extra(&app) {
        Ok(Some(folder)) => PathBuf::from(folder),
        // started without a game, from a shortcut to this activity
        _ => std::process::exit(2),
    };
    let data = folder.join("data");
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(own.join("config"));
    set_up(&own, &folder, &data);
    std::panic::set_hook(Box::new(|info| {
        log::error!("{info}");
        let _ = std::fs::write("data/panic.txt", info.to_string());
    }));

    // the screens can only go up once the activity has its window
    let mut window = None;
    while window.is_none() {
        app.poll_events(Some(Duration::from_millis(100)), |event| {
            if let PollEvent::Main(MainEvent::InitWindow { .. }) = event {
                window = app.native_window();
            }
        });
    }
    let window = window.unwrap();
    let (window_width, window_height) = (window.width(), window.height());
    // a window half again taller than wide is both panels stacked, as
    // GammaOS's DualStack gives it to an app on its list: 640x960 on the RG
    // DS, where the app's own is 640x480. A phone held upright is taller still
    let stacked = window_height * 2 == window_width * 3;
    zakuro_gpu::set_stacked(stacked);
    zakuro_gpu::set_window(window.ptr().as_ptr().cast());
    for axis in [Axis::Z, Axis::Rz, Axis::Rx, Axis::Ry, Axis::HatX, Axis::HatY, Axis::Ltrigger, Axis::Rtrigger, Axis::Gas, Axis::Brake] {
        app.enable_motion_axis(axis);
    }

    let pad = input::share();
    if stacked {
        log::info!("a {window_width}x{window_height} window, both panels stacked by GammaOS's DualStack");
    } else if !own.join("config").join("one-screen").exists() {
        match show_second_screen(&app) {
            Ok(true) => log::info!("asked for the second screen"),
            Ok(false) => log::info!("no second display, both screens on this one"),
            Err(error) => log::warn!("could not show the second screen, {error}"),
        }
    }
    ACTIVITY.store(app.activity_as_ptr(), Ordering::Relaxed);
    VM.store(app.vm_as_ptr(), Ordering::Relaxed);
    let _ = pocket_native::BEFORE_EXIT.set(finish);
    let rom = folder.join("game.3ds").to_string_lossy().into_owned();
    let game = std::thread::Builder::new().name("game".into()).spawn(move || pocket_native::run(vec![rom])).expect("a thread for the game");

    let mut window = Some(window);
    loop {
        app.poll_events(Some(Duration::from_millis(8)), |event| match event {
            // the app away holds the game still, back it goes on
            PollEvent::Main(MainEvent::Pause) => input::PAUSED.store(true, Ordering::Relaxed),
            PollEvent::Main(MainEvent::Resume { .. }) => input::PAUSED.store(false, Ordering::Relaxed),
            // its window goes while it is away, the renderer letting go of it
            // first, and a new one comes back
            PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
                zakuro_gpu::set_window(std::ptr::null_mut());
                window = None;
            }
            PollEvent::Main(MainEvent::InitWindow { .. }) => {
                window = app.native_window();
                if let Some(window) = &window {
                    zakuro_gpu::set_window(window.ptr().as_ptr().cast());
                }
            }
            // closed for good, which ends the game as quitting from the menu does
            PollEvent::Main(MainEvent::Destroy) => input::QUIT.store(true, Ordering::Relaxed),
            _ => {}
        });
        // the window's size now, which turns with the phone
        let size = window.as_ref().map(|window| (window.width(), window.height()));
        if let (Ok(mut events), Some(size)) = (app.input_events_iter(), size) {
            while events.next(|event| handle(event, &pad, size)) {}
        }
        // run only returns when it panicked, it ends the process otherwise
        if game.is_finished() {
            finish(1);
            std::process::exit(1);
        }
    }
}

/// the frontend's places and switches, as run.sh gives them on ArkOS.
fn set_up(own: &Path, folder: &Path, data: &Path) {
    let _ = std::env::set_current_dir(folder);
    std::env::set_var("POCKET_NATIVE_LOG", data.join("log.txt"));
    std::env::set_var("ZAKURO_FAST", "1");
    std::env::set_var("ZAKURO_DATA", data);
    if let Some(id) = folder.file_name() {
        std::env::set_var("ZAKURO_RECOMPILED", own.join("code").join(id).with_extension("so"));
    }
    // where the game ran code its build missed, in its folder on the card as
    // on ArkOS, which the kit's rebuild takes from games/<title id>: beside
    // the code, in the app's own storage, nobody could copy it out
    let hints = folder.join("title.hints");
    if let Some(id) = folder.file_name() {
        // what a game wrote beside its code before, copied over once
        let before = own.join("code").join(id).with_extension("hints");
        if !hints.exists() && std::fs::copy(&before, &hints).is_ok() {
            let _ = std::fs::remove_file(&before);
        }
    }
    std::env::set_var("ZAKURO_HINTS", hints);
    std::env::set_var("ZAKURO_SHADER_MANIFEST", data.join("shaders.txt"));
    std::env::set_var("POCKET_NATIVE_TITLES", own.join("titles"));
    std::env::set_var("POCKET_NATIVE_CONFIG", own.join("config"));
    let drops = data.join("drops.txt");
    if let Ok(rules) = std::fs::read_to_string(drops) {
        std::env::set_var("ZAKURO_DROP_DRAWS", rules.trim());
    }
    // the temporary directory, where profiles and screenshots go, is the
    // data directory: an app may not write the system's
    std::env::set_var("TMPDIR", data);
    // the cache, where the renderer keeps the shaders the GPU's driver
    // compiled for the next run. an app has no HOME, without this every run
    // compiled every shader again, the driver's compiler busy for minutes
    // and new effects late to show up
    std::env::set_var("XDG_CACHE_HOME", own.join("cache"));
    // data/env.txt, NAME=value a line, sets the switches a shell would, for
    // trying one without a new build: ZAKURO_HOST_PROFILE=1800,600 and the like
    let env = data.join("env.txt");
    let read = std::fs::read_to_string(&env);
    if let Err(error) = &read {
        // one adb wrote as root with no read for others, which the app may
        // not open: the log says so rather than nothing
        if env.exists() {
            std::env::set_var("POCKET_NATIVE_NOTE", format!("switches from data/env.txt: none, it could not be read ({error}); adb shell chmod 666 it"));
        }
    }
    if let Ok(text) = read {
        let mut taken = Vec::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
            if let Some((name, value)) = line.split_once('=') {
                std::env::set_var(name.trim(), value.trim());
                taken.push(format!("{}={}", name.trim(), value.trim()));
            }
        }
        std::env::set_var("POCKET_NATIVE_NOTE", format!("switches from data/env.txt: {}", taken.join(", ")));
    }
}

/// one event into the pad, handled when it was the gamepad's or the touch
/// screen's, so that the system does not take B for back.
fn handle(event: &InputEvent, pad: &std::sync::Mutex<Pad>, size: (i32, i32)) -> InputStatus {
    match event {
        InputEvent::KeyEvent(key) => {
            let Some(code) = button(key.key_code()) else { return InputStatus::Unhandled };
            let mut pad = pad.lock().unwrap();
            // a phone's own back and menu keys leave the touch controls up
            pad.pad_used |= !matches!(key.key_code(), Keycode::Back | Keycode::Menu);
            match key.action() {
                KeyAction::Down => {
                    pad.keys.insert(code);
                }
                KeyAction::Up => {
                    pad.keys.remove(&code);
                    pad.let_go.insert(code);
                }
                _ => {}
            }
            InputStatus::Handled
        }
        InputEvent::MotionEvent(motion) => {
            let source: u32 = motion.source().into();
            let mut pad = pad.lock().unwrap();
            if source & TOUCHSCREEN == TOUCHSCREEN {
                let at = |index: usize| {
                    let pointer = motion.pointer_at_index(index);
                    let (x, y) = to_layout(pointer.x(), pointer.y(), size);
                    (pointer.pointer_id(), x, y)
                };
                // every finger down but the one going up
                let leaving = match motion.action() {
                    MotionAction::Up | MotionAction::PointerUp => Some(motion.pointer_index()),
                    _ => None,
                };
                pad.fingers = match motion.action() {
                    MotionAction::Cancel => Vec::new(),
                    _ => (0..motion.pointer_count()).filter(|&index| Some(index) != leaving).map(at).collect(),
                };
                if let Some(index) = leaving {
                    pad.lifted.push(at(index));
                }
                pad.pad_used = false;
                InputStatus::Handled
            } else if source & JOYSTICK == JOYSTICK {
                let pointer = motion.pointer_at_index(0);
                let axis = |axis: Axis| pointer.axis_value(axis);
                // the right stick is Z and RZ on most Android pads, RX and RY on some
                let (right_x, right_y) = if axis(Axis::Z) == 0.0 && axis(Axis::Rz) == 0.0 {
                    (axis(Axis::Rx), axis(Axis::Ry))
                } else {
                    (axis(Axis::Z), axis(Axis::Rz))
                };
                pad.pad_used |= [Axis::X, Axis::Y, Axis::HatX, Axis::HatY].iter().any(|&moved| axis(moved).abs() > 0.5);
                pad.axes[0] = (axis(Axis::X) * STICK) as i32;
                pad.axes[1] = (axis(Axis::Y) * STICK) as i32;
                pad.axes[3] = (right_x * STICK) as i32;
                pad.axes[4] = (right_y * STICK) as i32;
                // triggers and a d-pad that come as axes, as buttons
                let held = [
                    (L2, axis(Axis::Ltrigger).max(axis(Axis::Brake)) > 0.5),
                    (R2, axis(Axis::Rtrigger).max(axis(Axis::Gas)) > 0.5),
                    (LEFT, axis(Axis::HatX) < -0.5),
                    (RIGHT, axis(Axis::HatX) > 0.5),
                    (UP, axis(Axis::HatY) < -0.5),
                    (DOWN, axis(Axis::HatY) > 0.5),
                ];
                for (code, down) in held {
                    if down {
                        pad.keys.insert(code);
                    } else if !HATS_AS_KEYS.load(Ordering::Relaxed) {
                        pad.keys.remove(&code);
                    }
                }
                InputStatus::Handled
            } else {
                InputStatus::Unhandled
            }
        }
        _ => InputStatus::Unhandled,
    }
}

/// set once the d-pad has come as keys, after which the hat's axes no
/// longer let go of keys the d-pad holds.
static HATS_AS_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// an Android key as the R36S numbers it, which the frontend goes by.
fn button(key: Keycode) -> Option<u16> {
    let code = match key {
        Keycode::ButtonA => A,
        Keycode::ButtonB => B,
        Keycode::ButtonX => X,
        Keycode::ButtonY => Y,
        Keycode::ButtonL1 => L1,
        Keycode::ButtonR1 => R1,
        Keycode::ButtonL2 => L2,
        Keycode::ButtonR2 => R2,
        Keycode::ButtonThumbl => L3,
        Keycode::ButtonThumbr => R3,
        Keycode::ButtonSelect => SELECT,
        Keycode::ButtonStart => START,
        // Back too, which a phone has in place of FN
        Keycode::ButtonMode | Keycode::Menu | Keycode::Back => FN,
        Keycode::DpadUp | Keycode::DpadDown | Keycode::DpadLeft | Keycode::DpadRight => {
            HATS_AS_KEYS.store(true, Ordering::Relaxed);
            match key {
                Keycode::DpadUp => UP,
                Keycode::DpadDown => DOWN,
                Keycode::DpadLeft => LEFT,
                _ => RIGHT,
            }
        }
        _ => return None,
    };
    Some(code)
}

/// a place in the window in the layout's size, which the renderer scales to
/// fit the window and centres.
fn to_layout(x: f32, y: f32, (width, height): (i32, i32)) -> (f32, f32) {
    let (layout_width, layout_height) = zakuro_gpu::layout_size();
    let scale = (width as f32 / layout_width as f32).min(height as f32 / layout_height as f32);
    let left = (width as f32 - layout_width as f32 * scale) / 2.0;
    let top = (height as f32 - layout_height as f32 * scale) / 2.0;
    ((x - left) / scale, (y - top) / scale)
}

/// the second display's window, acquired from its surface, null for none.
static SECOND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// the activity and its VM, for finishing it from the game's thread.
static ACTIVITY: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static VM: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// finishes the activity with the game's exit code: 3 has the launcher start
/// the game again.
fn finish(code: i32) {
    let (vm, activity) = (VM.load(Ordering::Relaxed), ACTIVITY.load(Ordering::Relaxed));
    if vm.is_null() || activity.is_null() {
        return;
    }
    // SAFETY: the VM and activity android_main was given, which live as long
    // as the app
    let Ok(vm) = (unsafe { JavaVM::from_raw(vm.cast()) }) else { return };
    let Ok(mut env) = vm.attach_current_thread_permanently() else { return };
    let activity = unsafe { JObject::from_raw(activity.cast()) };
    let _ = env.call_method(&activity, "finishWith", "(I)V", &[JValue::Int(code)]);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

/// the intent's "game" extra, the folder of the game to play.
fn game_extra(app: &AndroidApp) -> Result<Option<String>, String> {
    // SAFETY: the activity's VM and activity, which live as long as the app
    let (vm, activity) = unsafe { (JavaVM::from_raw(app.vm_as_ptr().cast()).map_err(|e| e.to_string())?, JObject::from_raw(app.activity_as_ptr().cast())) };
    let mut env = vm.attach_current_thread_permanently().map_err(|e| e.to_string())?;
    let extra = (|| -> jni::errors::Result<Option<String>> {
        let intent = env.call_method(&activity, "getIntent", "()Landroid/content/Intent;", &[])?.l()?;
        let key = env.new_string("game")?;
        let value = env.call_method(&intent, "getStringExtra", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&key)])?.l()?;
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(env.get_string(&value.into())?.into()))
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    extra.map_err(|e| e.to_string())
}

/// registers SecondScreen's native methods and has it show the second
/// screen, false when there is no second display.
fn show_second_screen(app: &AndroidApp) -> Result<bool, String> {
    // SAFETY: the activity's VM and activity, which live as long as the app
    let (vm, activity) = unsafe { (JavaVM::from_raw(app.vm_as_ptr().cast()).map_err(|e| e.to_string())?, JObject::from_raw(app.activity_as_ptr().cast())) };
    let mut env = vm.attach_current_thread_permanently().map_err(|e| e.to_string())?;
    let found = (|| -> jni::errors::Result<bool> {
        // the app's own classes come from the activity's class loader, not the
        // one a thread attached from native code has
        let loader = env.call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string("io.github.pocketnative.SecondScreen")?;
        let class: JClass = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?.into();
        let methods = [
            NativeMethod { name: "surfaceReady".into(), sig: "(Landroid/view/Surface;)V".into(), fn_ptr: surface_ready as *mut c_void },
            NativeMethod { name: "surfaceGone".into(), sig: "()V".into(), fn_ptr: surface_gone as *mut c_void },
            NativeMethod { name: "touched".into(), sig: "(ZFFII)V".into(), fn_ptr: touched as *mut c_void },
        ];
        env.register_native_methods(&class, &methods)?;
        env.call_static_method(&class, "show", "(Landroid/app/Activity;)Z", &[JValue::Object(&activity)])?.z()
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    found.map_err(|e| e.to_string())
}

/// SecondScreen's surface, new or changed: its window goes to the renderer.
extern "system" fn surface_ready(env: JNIEnv, _class: JClass, surface: JObject) {
    // SAFETY: the surface the SurfaceView's holder gives, valid in this call;
    // the window comes acquired, and is released once the renderer let it go
    let window = unsafe { android_activity::ndk_sys::ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast()) };
    if window.is_null() {
        return;
    }
    let old = SECOND.swap(window.cast(), Ordering::AcqRel);
    if old == window.cast() {
        // the same window again, a size or format change the renderer finds itself
        // SAFETY: the reference fromSurface added
        unsafe { android_activity::ndk_sys::ANativeWindow_release(window) };
        return;
    }
    if !old.is_null() {
        zakuro_gpu::set_second_window(std::ptr::null_mut());
        // SAFETY: the window acquired for it before, which the renderer let go
        unsafe { android_activity::ndk_sys::ANativeWindow_release(old.cast()) };
    }
    zakuro_gpu::set_second_window(window.cast());
}

/// SecondScreen's surface going away: the renderer lets go of it first.
extern "system" fn surface_gone(_env: JNIEnv, _class: JClass) {
    let old = SECOND.swap(std::ptr::null_mut(), Ordering::AcqRel);
    zakuro_gpu::set_second_window(std::ptr::null_mut());
    if !old.is_null() {
        // SAFETY: as in surface_ready
        unsafe { android_activity::ndk_sys::ANativeWindow_release(old.cast()) };
    }
    input::share().lock().unwrap().second_touch = None;
}

/// a finger on SecondScreen's view, at x and y of its width and height,
/// across and down the screen it shows, fitted into the view as the renderer
/// fits it.
extern "system" fn touched(_env: JNIEnv, _class: JClass, pressed: jboolean, x: jfloat, y: jfloat, width: jint, height: jint) {
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (width / 320.0).min(height / 240.0);
    let (left, top) = ((width - 320.0 * scale) / 2.0, (height - 240.0 * scale) / 2.0);
    let at = ((x - left) / (320.0 * scale), (y - top) / (240.0 * scale));
    input::share().lock().unwrap().second_touch = (pressed == JNI_TRUE).then_some(at);
}
