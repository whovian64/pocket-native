//! Pocket Native as an Android app: a NativeActivity whose window the
//! screens go to, whose key and motion events become the pad's buttons,
//! sticks and touch, and whose files directory holds the game.
//!
//! The app's files directory, /sdcard/Android/data/<package>/files, holds
//! sysdata/shared_font.bin and titles/, which the build's install script
//! pushes there; the frontend keeps data/ (saves, log) and config/ beside
//! them. game.3ds goes in the app's own folder on the SD card,
//! /storage/<card>/Android/data/<package>/files, which the app reads with no
//! permission, or beside the rest where there is no card. The title's recompiled code comes in the APK as
//! libtitle.so, loaded by name from the app's own libraries.
//!
//! On a handheld with a second display, the RG DS's bottom panel, one of
//! the screens goes there by itself: SecondScreen.java shows a Presentation
//! on it, whose surface and touches come back here through JNI. A file named
//! one-screen in the files directory keeps both screens on the first display.
//! Under GammaOS's DualStack the window is both panels stacked, and the
//! screens go on its halves, with no Presentation.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use android_activity::input::{Axis, InputEvent, KeyAction, Keycode, MotionAction};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use jni::objects::{JClass, JObject, JValue};
use jni::sys::{jboolean, jfloat, jint, JNI_TRUE};
use jni::{JNIEnv, JavaVM, NativeMethod};

use crate::input::{self, Pad, A, B, DOWN, FN, L1, L2, L3, LEFT, R1, R2, R3, RIGHT, SELECT, START, UP, X, Y};

/// how far the frontend's sticks go, as input::stick takes them.
const STICK: f32 = 1800.0;
/// AINPUT_SOURCE_CLASS_JOYSTICK and AINPUT_SOURCE_TOUCHSCREEN's bits.
const JOYSTICK: u32 = 0x0000_0010;
const TOUCHSCREEN: u32 = 0x0000_1002;

#[no_mangle]
fn android_main(app: AndroidApp) {
    let files = app.external_data_path().or_else(|| app.internal_data_path()).unwrap_or_else(|| PathBuf::from("/data/local/tmp/pocket-native"));
    let data = files.join("data");
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(files.join("config"));
    set_up(&files, &data);
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
    // DS, where the app's own is 640x480
    let stacked = window_height * 4 > window_width * 5;
    zakuro_gpu::set_stacked(stacked);
    zakuro_gpu::set_window(window.ptr().as_ptr().cast());
    for axis in [Axis::Z, Axis::Rz, Axis::Rx, Axis::Ry, Axis::HatX, Axis::HatY, Axis::Ltrigger, Axis::Rtrigger, Axis::Gas, Axis::Brake] {
        app.enable_motion_axis(axis);
    }

    let pad = input::share();
    if stacked {
        log::info!("a {window_width}x{window_height} window, both panels stacked by GammaOS's DualStack");
    } else if !files.join("one-screen").exists() {
        match show_second_screen(&app) {
            Ok(true) => log::info!("asked for the second screen"),
            Ok(false) => log::info!("no second display, both screens on this one"),
            Err(error) => log::warn!("could not show the second screen, {error}"),
        }
    }
    let rom = find_game(&app, &files).to_string_lossy().into_owned();
    let game = std::thread::Builder::new().name("game".into()).spawn(move || crate::run(vec![rom])).expect("a thread for the game");

    loop {
        app.poll_events(Some(Duration::from_millis(8)), |event| {
            // the window going away, the app sent to the background or closed,
            // ends the game as quitting from the menu does
            if let PollEvent::Main(MainEvent::TerminateWindow { .. } | MainEvent::Destroy | MainEvent::Pause) = event {
                input::QUIT.store(true, Ordering::Relaxed);
            }
        });
        if let Ok(mut events) = app.input_events_iter() {
            while events.next(|event| handle(event, &pad, (window_width, window_height))) {}
        }
        // run only returns when it panicked, it ends the process otherwise
        if game.is_finished() {
            std::process::exit(1);
        }
    }
}

/// the frontend's places and switches, as run.sh gives them on ArkOS.
fn set_up(files: &Path, data: &Path) {
    let _ = std::env::set_current_dir(files);
    std::env::set_var("POCKET_NATIVE_LOG", data.join("log.txt"));
    std::env::set_var("ZAKURO_FAST", "1");
    std::env::set_var("ZAKURO_DATA", data);
    // by name, the app's own libraries are where the system looks first
    std::env::set_var("ZAKURO_RECOMPILED", "libtitle.so");
    std::env::set_var("ZAKURO_SHADER_MANIFEST", data.join("shaders.txt"));
    std::env::set_var("POCKET_NATIVE_TITLES", files.join("titles"));
    std::env::set_var("POCKET_NATIVE_CONFIG", files.join("config"));
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
    std::env::set_var("XDG_CACHE_HOME", files.join("cache"));
    // data/env.txt, NAME=value a line, sets the switches a shell would, for
    // trying one without a new build: ZAKURO_HOST_PROFILE=1800,600 and the like
    let env = data.join("env.txt");
    let read = std::fs::read_to_string(&env);
    if let Err(error) = &read {
        // one adb wrote as root with no read for others, which the app may
        // not open: the log says so rather than nothing
        if env.exists() {
            let _ = SWITCHES.set(vec![format!("none, it could not be read ({error}); adb shell chmod 666 it")]);
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
        let _ = SWITCHES.set(taken);
    }
}

/// what data/env.txt set, for the log once there is one.
static SWITCHES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// logs what data/env.txt set, so that a log says whether it was there.
pub fn log_switches() {
    if let Some(switches) = SWITCHES.get() {
        log::info!("switches from data/env.txt: {}", switches.join(", "));
    }
}

/// one event into the pad, handled when it was the gamepad's or the touch
/// screen's, so that the system does not take B for back.
fn handle(event: &InputEvent, pad: &std::sync::Mutex<Pad>, window: (i32, i32)) -> InputStatus {
    match event {
        InputEvent::KeyEvent(key) => {
            let Some(code) = button(key.key_code()) else { return InputStatus::Unhandled };
            let mut pad = pad.lock().unwrap();
            match key.action() {
                KeyAction::Down => {
                    pad.keys.insert(code);
                }
                KeyAction::Up => {
                    pad.keys.remove(&code);
                }
                _ => {}
            }
            InputStatus::Handled
        }
        InputEvent::MotionEvent(motion) => {
            let source: u32 = motion.source().into();
            let mut pad = pad.lock().unwrap();
            if source & TOUCHSCREEN == TOUCHSCREEN {
                pad.touch = match motion.action() {
                    MotionAction::Down | MotionAction::Move | MotionAction::PointerDown => {
                        let pointer = motion.pointer_at_index(0);
                        Some(to_layout(pointer.x(), pointer.y(), window))
                    }
                    _ => None,
                };
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
        Keycode::ButtonMode | Keycode::Menu => FN,
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

/// the game: game.3ds in the app's own folder on a removable card, where
/// install.sh puts it when there is one, or else beside the app's files.
fn find_game(app: &AndroidApp, files: &Path) -> PathBuf {
    let mut places = match external_dirs(app) {
        Ok(dirs) => dirs,
        Err(error) => {
            log::warn!("could not ask for the app's folders on the cards, {error}");
            Vec::new()
        }
    };
    // the first is the built-in storage's, the rest are cards'
    if !places.is_empty() {
        let builtin = places.remove(0);
        places.push(builtin);
    }
    places.push(files.to_path_buf());
    let found = places.iter().map(|dir| dir.join("game.3ds")).find(|path| path.is_file());
    match found {
        Some(path) => {
            log::info!("the game is {}", path.display());
            path
        }
        None => {
            log::error!("no game.3ds in any of {places:?}, scripts/android/install.sh puts it there");
            files.join("game.3ds")
        }
    }
}

/// the app's own files folder on each storage volume, the built-in one
/// first, which asking for them also makes on a card that lacks it.
fn external_dirs(app: &AndroidApp) -> Result<Vec<PathBuf>, String> {
    // SAFETY: the activity's VM and activity, which live as long as the app
    let (vm, activity) = unsafe { (JavaVM::from_raw(app.vm_as_ptr().cast()).map_err(|e| e.to_string())?, JObject::from_raw(app.activity_as_ptr().cast())) };
    let mut env = vm.attach_current_thread_permanently().map_err(|e| e.to_string())?;
    let dirs = (|| -> jni::errors::Result<Vec<PathBuf>> {
        let array: jni::objects::JObjectArray = env
            .call_method(&activity, "getExternalFilesDirs", "(Ljava/lang/String;)[Ljava/io/File;", &[JValue::Object(&JObject::null())])?
            .l()?
            .into();
        let mut dirs = Vec::new();
        for index in 0..env.get_array_length(&array)? {
            let dir = env.get_object_array_element(&array, index)?;
            // null for a volume that is not mounted
            if dir.is_null() {
                continue;
            }
            let path: jni::objects::JString = env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?.into();
            dirs.push(PathBuf::from(String::from(env.get_string(&path)?)));
        }
        Ok(dirs)
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    dirs.map_err(|e| e.to_string())
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
