//! getting pictures onto the panels: KMS and GBM, EGL and its windows,
//! the layout of the screens, the overlay and presenting.

use super::*;

#[repr(C)]
pub(super) struct DrmModeRes {
    pub(super) count_fbs: i32,
    pub(super) fbs: *mut u32,
    pub(super) count_crtcs: i32,
    pub(super) crtcs: *mut u32,
    pub(super) count_connectors: i32,
    pub(super) connectors: *mut u32,
    pub(super) count_encoders: i32,
    pub(super) encoders: *mut u32,
    pub(super) min_width: u32,
    pub(super) max_width: u32,
    pub(super) min_height: u32,
    pub(super) max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct DrmModeInfo {
    pub(super) clock: u32,
    pub(super) hdisplay: u16,
    pub(super) hsync_start: u16,
    pub(super) hsync_end: u16,
    pub(super) htotal: u16,
    pub(super) hskew: u16,
    pub(super) vdisplay: u16,
    pub(super) vsync_start: u16,
    pub(super) vsync_end: u16,
    pub(super) vtotal: u16,
    pub(super) vscan: u16,
    pub(super) vrefresh: u32,
    pub(super) flags: u32,
    pub(super) kind: u32,
    pub(super) name: [u8; 32],
}

#[repr(C)]
pub(super) struct DrmModeObjectProperties {
    pub(super) count_props: u32,
    pub(super) props: *mut u32,
    pub(super) prop_values: *mut u64,
}

#[repr(C)]
pub(super) struct DrmModeProperty {
    pub(super) prop_id: u32,
    pub(super) flags: u32,
    pub(super) name: [u8; 32],
}

#[repr(C)]
pub(super) struct DrmModeConnector {
    pub(super) connector_id: u32,
    pub(super) encoder_id: u32,
    pub(super) connector_type: u32,
    pub(super) connector_type_id: u32,
    pub(super) connection: u32,
    pub(super) mm_width: u32,
    pub(super) mm_height: u32,
    pub(super) subpixel: u32,
    pub(super) count_modes: i32,
    pub(super) modes: *mut DrmModeInfo,
    pub(super) count_props: i32,
    pub(super) props: *mut u32,
    pub(super) prop_values: *mut u64,
    pub(super) count_encoders: i32,
    pub(super) encoders: *mut u32,
}

#[repr(C)]
pub(super) struct DrmModeEncoder {
    pub(super) encoder_id: u32,
    pub(super) encoder_type: u32,
    pub(super) crtc_id: u32,
    pub(super) possible_crtcs: u32,
    pub(super) possible_clones: u32,
}

pub(super) type FlipHandler = extern "C" fn(i32, u32, u32, u32, *mut c_void);

#[repr(C)]
pub(super) struct DrmEventContext {
    pub(super) version: i32,
    pub(super) vblank_handler: Option<FlipHandler>,
    pub(super) page_flip_handler: Option<FlipHandler>,
}

/// the panel's refresh count and the time of it, in microseconds of the
/// monotonic clock, at the last flip.
pub(super) static FLIPPED_AT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub(super) static FLIPPED_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// the flips that happened since the last were asked for, 1 for the panel's
/// and 2 for the bottom panel's on a handheld with two.
pub(super) static FLIPS_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(super) extern "C" fn flipped(_fd: i32, sequence: u32, seconds: u32, microseconds: u32, data: *mut c_void) {
    // the bottom panel refreshes a little apart from the top one, whose
    // refreshes the pacing goes by
    if data as usize == 2 {
        FLIPS_SEEN.fetch_or(2, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    FLIPPED_AT.store(sequence, std::sync::atomic::Ordering::Relaxed);
    FLIPPED_TIME.store(seconds as u64 * 1_000_000 + microseconds as u64, std::sync::atomic::Ordering::Relaxed);
    FLIPS_SEEN.fetch_or(1, std::sync::atomic::Ordering::Relaxed);
}

/// a connector's name as the kernel gives it in /sys/class/drm, DSI-1.
pub(super) fn connector_name(kind: u32, number: u32) -> String {
    let kind = match kind {
        1 => "VGA",
        7 => "LVDS",
        10 => "DP",
        11 => "HDMI-A",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        _ => "Unknown",
    };
    format!("{kind}-{number}")
}

/// a native window made by the program around the engine, an Android
/// activity's, that the screens go to in place of the panel, null for none.
pub(super) static WINDOW: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// whether the window is both of a dual-screen handheld's panels stacked,
/// as GammaOS's DualStack gives an app them, its top half on the top panel
/// and its bottom half on the bottom one.
pub(super) static STACKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// says the window set_window gives is two panels stacked, before the
/// renderer starts: it is never turned as a portrait panel would be, and the
/// menu goes on the top half.
pub fn set_stacked(stacked: bool) {
    STACKED.store(stacked, std::sync::atomic::Ordering::Relaxed);
}

/// whether the window is two panels stacked.
pub fn stacked() -> bool {
    STACKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// a second native window, on a second display, that one of the screens
/// goes to by itself, null for none, and the one the GL thread has a
/// surface on, which set_second_window waits to see go before its window
/// may go away.
pub(super) static SECOND_WINDOW: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
pub(super) static SECOND_IN_USE: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
/// whether the second window shows the top screen rather than the bottom.
pub(super) static SECOND_SHOWS_TOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// whether the second window's screen is stretched over all of it.
pub(super) static SECOND_STRETCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// how much of the top screen's width is cut off each side, f32 bits, 0
/// for none.
pub(super) static TOP_CROP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// cuts that much of the top screen's width off each side, 0.1 for the 4:3
/// middle of its 5:3, the area it is shown in the part that is left.
pub fn set_top_crop(each_side: f32) {
    TOP_CROP.store(each_side.clamp(0.0, 0.45).to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// a screen's area in the framebuffer widened for the crop, and the part of
/// it that shows, the area as given, from the bottom left as GL counts it.
pub(super) fn cropped(area: [i32; 4], top: bool, height: i32) -> ([i32; 4], Option<[i32; 4]>) {
    let each_side = f32::from_bits(TOP_CROP.load(std::sync::atomic::Ordering::Relaxed));
    if !top || each_side <= 0.0 {
        return (area, None);
    }
    let width = (area[2] as f32 / (1.0 - 2.0 * each_side)).round() as i32;
    ([area[0] - (width - area[2]) / 2, area[1], width, area[3]], Some([area[0], height - area[1] - area[3], area[2], area[3]]))
}

/// whether the second window's screen fills all of it, out of shape, rather
/// than keeping its shape with bars.
pub fn set_second_stretched(stretched: bool) {
    SECOND_STRETCHED.store(stretched, std::sync::atomic::Ordering::Relaxed);
}

/// hands the renderer a second window, on a second display, for one of the
/// screens by itself, or takes it back with null. Taking it back waits, a
/// half second at most, for the GL thread to stop drawing into it, after
/// which the caller may let the window go.
pub fn set_second_window(window: *mut c_void) {
    SECOND_WINDOW.store(window, std::sync::atomic::Ordering::Release);
    if window.is_null() {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !SECOND_IN_USE.load(std::sync::atomic::Ordering::Acquire).is_null() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// whether there is a second window for a screen to go to.
pub fn second_window() -> bool {
    !SECOND_WINDOW.load(std::sync::atomic::Ordering::Acquire).is_null()
}

/// which screen the second window shows, the top one or the bottom one.
pub fn set_second_shows_top(top: bool) {
    SECOND_SHOWS_TOP.store(top, std::sync::atomic::Ordering::Relaxed);
}

/// the window the panel's surface is on, which set_window waits to see go
/// before its window may go away.
pub(super) static WINDOW_IN_USE: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// hands the renderer a native window to show the screens in, an
/// ANativeWindow on Android, before the title is loaded, or a new one when
/// the activity got its window back. Null takes it back while the app is
/// away: that waits, a half second at most, for the GL thread to let go of
/// it, which it does at its next present, after which the caller may let the
/// window go.
pub fn set_window(window: *mut c_void) {
    WINDOW.store(window, std::sync::atomic::Ordering::Release);
    if window.is_null() {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !WINDOW_IN_USE.load(std::sync::atomic::Ordering::Acquire).is_null() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// the panel's surface onto the window set_window gave last: the old one let
/// go, the context current on no surface while there is no window, and a new
/// surface made for a new window. False when that failed.
/// SAFETY: on the GL thread, the window valid until set_window takes it back
pub(super) unsafe fn follow_window(egl: &Egl, wanted: *mut c_void) -> bool {
    let old = egl.surface();
    if !old.is_null() {
        if egl.egl.MakeCurrent(egl.display, egl::NO_SURFACE, egl::NO_SURFACE, egl.context) == 0 {
            log::warn!("could not let go of the window, 0x{:X}", egl.egl.GetError());
            return false;
        }
        egl.egl.DestroySurface(egl.display, old);
        egl.surface.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        WINDOW_IN_USE.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        log::info!("the window went");
    }
    if wanted.is_null() {
        return true;
    }
    let surface = egl.egl.CreateWindowSurface(egl.display, egl.config, wanted as glutin_egl_sys::EGLNativeWindowType, std::ptr::null());
    if surface.is_null() || egl.egl.MakeCurrent(egl.display, surface, surface, egl.context) == 0 {
        log::warn!("could not show the screens in the new window, 0x{:X}", egl.egl.GetError());
        return false;
    }
    egl.egl.SwapInterval(egl.display, 1);
    egl.surface.store(surface as *mut c_void, std::sync::atomic::Ordering::Release);
    WINDOW_IN_USE.store(wanted, std::sync::atomic::Ordering::Release);
    log::info!("the screens went to a new window");
    true
}

/// the panel's refresh period, and how long before its refresh the last
/// picture was handed over, in nanoseconds, 0 when not known.
pub(super) static PANEL_PERIOD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// the panel's "panel orientation" property, how the kernel says it is
/// mounted: 2 its left side up, 3 its right side up, as on handhelds whose
/// panel is portrait under a mainline kernel, as ROCKNIX's are. -1 unknown.
pub(super) static PANEL_ORIENTATION: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
pub(super) static PANEL_SLACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// the panel's refresh period, and how long before its refresh the last
/// picture was handed over, when one was since the last call.
pub fn panel_timing() -> Option<(std::time::Duration, Option<std::time::Duration>)> {
    let period = PANEL_PERIOD.load(std::sync::atomic::Ordering::Relaxed);
    let slack = PANEL_SLACK.swap(0, std::sync::atomic::Ordering::Relaxed);
    (period > 0).then(|| (std::time::Duration::from_nanos(period), (slack > 0).then(|| std::time::Duration::from_nanos(slack))))
}

pub(super) fn monotonic_micros() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes the timespec it is given
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1000
}

pub(super) const GBM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// the panel, driven through KMS from a GBM surface the context draws into.
pub(super) struct Kms {
    pub(super) drm: libloading::Library,
    pub(super) gbm: libloading::Library,
    pub(super) fd: i32,
    pub(super) device: *mut c_void,
    pub(super) surface: *mut c_void,
    pub(super) crtc: u32,
    pub(super) connector: u32,
    pub(super) mode: DrmModeInfo,
    /// the CRTC and connector of a dual-screen handheld's bottom panel, the
    /// same size as the top one, which shows the bottom half of a surface
    /// twice the panel's height, the top panel its top half.
    pub(super) second: Option<(u32, u32)>,
    pub(super) shown: Option<(*mut c_void, u32)>,
    pub(super) pending: Option<(*mut c_void, u32)>,
    pub(super) set: bool,
    /// how many pictures stayed up for 1, 2, 3 and 4 or more panel
    /// refreshes, logged every few seconds: at an even frame rate they all
    /// stay up as long.
    pub(super) held: [u32; 4],
    pub(super) held_since: std::time::Instant,
    pub(super) last_flip: u32,
    /// when the flip still queued was asked for, in microseconds of the
    /// monotonic clock, and the time from asking to flipping summed and
    /// counted since the last log.
    pub(super) asked: u64,
    pub(super) slack: (u64, u32),
}

macro_rules! call {
    ($library:expr, $name:literal, fn($($arg:ty),*) -> $ret:ty, $($value:expr),*) => {{
        let address = symbol(&$library, $name);
        if address.is_null() {
            return Err(format!("no {}", $name));
        }
        let function: extern "C" fn($($arg),*) -> $ret = std::mem::transmute(address);
        function($($value),*)
    }};
}

impl Kms {
    /// SAFETY: plain libdrm and GBM calls on the descriptor and device this
    /// opens, with the structs they return read before they are freed
    pub(super) unsafe fn open() -> Result<Kms, String> {
        let drm = open(&["libdrm.so.2", "libdrm.so"]).map_err(|why| format!("could not load libdrm, {why}"))?;
        let gbm = open(&["libgbm.so.1", "libgbm.so"]).map_err(|why| format!("could not load libgbm, {why}"))?;
        let path = CString::new("/dev/dri/card0").map_err(|error| error.to_string())?;
        let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            return Err("could not open /dev/dri/card0".into());
        }
        let device = call!(gbm, "gbm_create_device", fn(i32) -> *mut c_void, fd);
        if device.is_null() {
            return Err("no GBM device".into());
        }
        let resources = call!(drm, "drmModeGetResources", fn(i32) -> *mut DrmModeRes, fd);
        if resources.is_null() {
            return Err("no KMS resources".into());
        }
        let res = &*resources;
        let mut panels = Vec::new();
        for i in 0..res.count_connectors as usize {
            let connector = call!(drm, "drmModeGetConnector", fn(i32, u32) -> *mut DrmModeConnector, fd, *res.connectors.add(i));
            if connector.is_null() {
                continue;
            }
            let c = &*connector;
            if c.connection == 1 && c.count_modes > 0 {
                let modes = std::slice::from_raw_parts(c.modes, c.count_modes as usize);
                let preferred = *modes.iter().find(|mode| mode.kind & (1 << 3) != 0).unwrap_or(&modes[0]);
                // its own rate, or the one of its size nearest 60 Hz (ROCKNIX
                // gives the R36S 80 Hz first)
                let off = |mode: &&DrmModeInfo| (mode.clock as f64 * 1000.0 / (mode.htotal as f64 * mode.vtotal as f64) - 60.0).abs();
                let mode = *modes
                    .iter()
                    .filter(|mode| (mode.hdisplay, mode.vdisplay) == (preferred.hdisplay, preferred.vdisplay))
                    .min_by(|a, b| off(a).total_cmp(&off(b)))
                    .unwrap_or(&preferred);
                // the CRTC it is on, and those it may be on, from its
                // encoder, or its first when it has none yet
                let (mut crtc, mut possible) = (0, 0);
                let encoder_id = if c.encoder_id == 0 && c.count_encoders > 0 { *c.encoders } else { c.encoder_id };
                let encoder = call!(drm, "drmModeGetEncoder", fn(i32, u32) -> *mut DrmModeEncoder, fd, encoder_id);
                if !encoder.is_null() {
                    crtc = (*encoder).crtc_id;
                    possible = (*encoder).possible_crtcs;
                    call!(drm, "drmModeFreeEncoder", fn(*mut DrmModeEncoder) -> (), encoder);
                }
                panels.push((c.connector_id, connector_name(c.connector_type, c.connector_type_id), mode, crtc, possible));
            }
            call!(drm, "drmModeFreeConnector", fn(*mut DrmModeConnector) -> (), connector);
        }
        let crtcs = std::slice::from_raw_parts(res.crtcs, res.count_crtcs.max(0) as usize).to_vec();
        call!(drm, "drmModeFreeResources", fn(*mut DrmModeRes) -> (), resources);
        if panels.is_empty() {
            return Err("no connected display".into());
        }
        // the panel the top screen goes on: ZAKURO_KMS_TOP=DSI-1 names it,
        // DSI-2 on the RG DS under ROCKNIX, else the first connected
        let top = std::env::var("ZAKURO_KMS_TOP").unwrap_or_else(|_| "DSI-2".into());
        let (connector, name, mode, crtc, possible) = panels.remove(panels.iter().position(|panel| panel.1 == top).unwrap_or(0));
        // how the kernel says the panel is mounted
        let properties = call!(drm, "drmModeObjectGetProperties", fn(i32, u32, u32) -> *mut DrmModeObjectProperties, fd, connector, 0xC0C0_C0C0);
        if !properties.is_null() {
            let found = &*properties;
            for i in 0..found.count_props as usize {
                let property = call!(drm, "drmModeGetProperty", fn(i32, u32) -> *mut DrmModeProperty, fd, *found.props.add(i));
                if property.is_null() {
                    continue;
                }
                if (*property).name.starts_with(b"panel orientation\0") {
                    let orientation = *found.prop_values.add(i) as i32;
                    log::info!("the panel's orientation is {orientation}");
                    PANEL_ORIENTATION.store(orientation, std::sync::atomic::Ordering::Relaxed);
                }
                call!(drm, "drmModeFreeProperty", fn(*mut DrmModeProperty) -> (), property);
            }
            call!(drm, "drmModeFreeObjectProperties", fn(*mut DrmModeObjectProperties) -> (), properties);
        }
        // another panel the same size is the bottom one, both shown as one
        // picture stacked, unless ZAKURO_KMS_SINGLE=1 keeps to one
        let bottom = panels
            .into_iter()
            .find(|panel| (panel.2.hdisplay, panel.2.vdisplay) == (mode.hdisplay, mode.vdisplay))
            .filter(|_| std::env::var_os("ZAKURO_KMS_SINGLE").is_none());
        // a CRTC each, the one it is on unless the other panel has it
        let mut used = Vec::new();
        let mut pick = |crtc: u32, possible: u32| {
            let free = |crtc: &u32| !used.contains(crtc);
            let picked = Some(crtc)
                .filter(|crtc| *crtc != 0 && free(crtc))
                .or_else(|| crtcs.iter().enumerate().find(|(i, crtc)| possible & (1 << i) != 0 && free(crtc)).map(|(_, crtc)| *crtc))
                .or_else(|| crtcs.iter().copied().find(free))
                .unwrap_or(0);
            used.push(picked);
            picked
        };
        let crtc = pick(crtc, possible);
        let second = bottom.map(|(connector, bottom_name, _, crtc, possible)| {
            log::info!("two panels, {name} on top and {bottom_name} under it");
            (pick(crtc, possible), connector)
        });
        let second = second.filter(|&(bottom, _)| bottom != 0 && bottom != crtc);
        set_stacked(second.is_some());
        // sway lets go of the panels a moment after it stopped, as under
        // ROCKNIX, and a modeset before then fails. only busy is worth
        // waiting out: ArkOS's kernel keeps the call to root, where the
        // panel is already ours as the first to open it
        let master = symbol(&drm, "drmSetMaster");
        if !master.is_null() {
            let master: extern "C" fn(i32) -> i32 = std::mem::transmute(master);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while master(fd) != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let refresh = mode.clock as f64 * 1000.0 / (mode.htotal as f64 * mode.vtotal as f64);
        log::info!("panel {}x{} at {refresh:.3} Hz", mode.hdisplay, mode.vdisplay);
        if (50.0..70.0).contains(&refresh) {
            PANEL_PERIOD.store((1e9 / refresh) as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let surface = call!(
            gbm,
            "gbm_surface_create",
            fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void,
            device,
            mode.hdisplay as u32,
            mode.vdisplay as u32 * (1 + second.is_some() as u32),
            GBM_FORMAT_XRGB8888,
            1 | 4
        );
        if surface.is_null() {
            return Err("no GBM surface".into());
        }
        Ok(Kms { drm, gbm, fd, device, surface, crtc, connector, mode, second, shown: None, pending: None, set: false, held: [0; 4], held_since: std::time::Instant::now(), last_flip: 0, asked: 0, slack: (0, 0) })
    }

    /// the size of the picture, both panels' when there are two.
    pub(super) fn size(&self) -> (i32, i32) {
        (self.mode.hdisplay as i32, self.mode.vdisplay as i32 * (1 + self.second.is_some() as i32))
    }

    /// waits for the flip queued last to happen, then lets go of the
    /// buffer it replaced.
    pub(super) fn settle(&mut self) -> Result<(), String> {
        let Some(pending) = self.pending.take() else { return Ok(()) };
        // SAFETY: as in open, on the descriptor and buffers this holds
        unsafe {
            let mut context = DrmEventContext { version: 2, vblank_handler: None, page_flip_handler: Some(flipped) };
            let mut poll = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
            // both panels' flips, when there are two
            let wanted = if self.second.is_some() { 3 } else { 1 };
            let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while FLIPS_SEEN.load(std::sync::atomic::Ordering::Relaxed) & wanted != wanted {
                let left = until.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
                if left <= 0 || libc::poll(&mut poll, 1, left) <= 0 {
                    break;
                }
                call!(self.drm, "drmHandleEvent", fn(i32, *mut DrmEventContext) -> i32, self.fd, &mut context);
            }
            if FLIPS_SEEN.load(std::sync::atomic::Ordering::Relaxed) & 1 != 0 {
                let at = FLIPPED_AT.load(std::sync::atomic::Ordering::Relaxed);
                let slack = FLIPPED_TIME.load(std::sync::atomic::Ordering::Relaxed).wrapping_sub(self.asked);
                if slack < 100_000 {
                    PANEL_SLACK.store(slack * 1000, std::sync::atomic::Ordering::Relaxed);
                    self.slack = (self.slack.0 + slack, self.slack.1 + 1);
                }
                let refreshes = at.wrapping_sub(self.last_flip) as usize;
                if self.last_flip != 0 && refreshes > 0 {
                    self.held[refreshes.min(4) - 1] += 1;
                }
                self.last_flip = at;
                if self.held_since.elapsed() >= std::time::Duration::from_secs(5) {
                    let [one, two, three, more] = std::mem::take(&mut self.held);
                    let (sum, count) = std::mem::take(&mut self.slack);
                    log::info!("pictures up for 1/2/3/4+ panel refreshes: {one}/{two}/{three}/{more}, handed over {:.1} ms before the refresh", sum as f64 / count.max(1) as f64 / 1000.0);
                    self.held_since = std::time::Instant::now();
                }
            }
            if let Some((bo, fb)) = self.shown.replace(pending) {
                call!(self.drm, "drmModeRmFB", fn(i32, u32) -> i32, self.fd, fb);
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
            }
        }
        Ok(())
    }

    /// queues what the context just swapped to for the panel's next
    /// vertical blank, waiting only for a flip still queued from before.
    /// a picture goes up no sooner than as many refreshes after the last as
    /// the game's frames between them, two at most, so none flashes by for
    /// one refresh and a game behind is never held up further.
    pub(super) fn flip(&mut self, frames: u32) -> Result<(), String> {
        self.settle()?;
        let period = PANEL_PERIOD.load(std::sync::atomic::Ordering::Relaxed) / 1000;
        let mut paced = false;
        if frames > 1 && period > 0 && self.last_flip != 0 {
            let due = FLIPPED_TIME.load(std::sync::atomic::Ordering::Relaxed) + (frames as u64 - 1) * period + 1000;
            let now = monotonic_micros();
            if due > now && due - now < 4 * period {
                std::thread::sleep(std::time::Duration::from_micros(due - now));
                paced = true;
            }
        }
        // SAFETY: as in open, the buffer object is the surface's front one
        unsafe {
            let bo = call!(self.gbm, "gbm_surface_lock_front_buffer", fn(*mut c_void) -> *mut c_void, self.surface);
            if bo.is_null() {
                return Err("no front buffer".into());
            }
            let handle = call!(self.gbm, "gbm_bo_get_handle", fn(*mut c_void) -> u64, bo) as u32;
            let stride = call!(self.gbm, "gbm_bo_get_stride", fn(*mut c_void) -> u32, bo);
            let (width, height) = self.size();
            let mut fb = 0u32;
            let added = call!(
                self.drm,
                "drmModeAddFB",
                fn(i32, u32, u32, u8, u8, u32, u32, *mut u32) -> i32,
                self.fd,
                width as u32,
                height as u32,
                24,
                32,
                stride,
                handle,
                &mut fb
            );
            if added != 0 {
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
                return Err("could not make a framebuffer".into());
            }
            FLIPS_SEEN.store(0, std::sync::atomic::Ordering::Relaxed);
            if !self.set {
                let mut connector = self.connector;
                let mut mode = self.mode;
                let result = call!(
                    self.drm,
                    "drmModeSetCrtc",
                    fn(i32, u32, u32, u32, u32, *mut u32, i32, *mut DrmModeInfo) -> i32,
                    self.fd,
                    self.crtc,
                    fb,
                    0,
                    0,
                    &mut connector,
                    1,
                    &mut mode
                );
                if result != 0 {
                    return Err(format!("could not set the display mode, {result}"));
                }
                // the bottom panel shows the picture's bottom half
                if let Some((crtc, mut connector)) = self.second {
                    let result = call!(
                        self.drm,
                        "drmModeSetCrtc",
                        fn(i32, u32, u32, u32, u32, *mut u32, i32, *mut DrmModeInfo) -> i32,
                        self.fd,
                        crtc,
                        fb,
                        0,
                        self.mode.vdisplay as u32,
                        &mut connector,
                        1,
                        &mut mode
                    );
                    if result != 0 {
                        return Err(format!("could not set the bottom panel's display mode, {result}"));
                    }
                }
                self.set = true;
                self.shown = Some((bo, fb));
            } else if call!(self.drm, "drmModePageFlip", fn(i32, u32, u32, u32, *mut c_void) -> i32, self.fd, self.crtc, fb, 1, std::ptr::without_provenance_mut(1)) == 0 {
                // a held picture says nothing of how early pictures come
                self.asked = if paced { 0 } else { monotonic_micros() };
                self.pending = Some((bo, fb));
                // the bottom panel flips to the same picture; were it still
                // busy, it is set there now, as the buffer it shows goes
                if let Some((crtc, mut connector)) = self.second {
                    if call!(self.drm, "drmModePageFlip", fn(i32, u32, u32, u32, *mut c_void) -> i32, self.fd, crtc, fb, 1, std::ptr::without_provenance_mut(2)) != 0 {
                        let mut mode = self.mode;
                        call!(
                            self.drm,
                            "drmModeSetCrtc",
                            fn(i32, u32, u32, u32, u32, *mut u32, i32, *mut DrmModeInfo) -> i32,
                            self.fd,
                            crtc,
                            fb,
                            0,
                            self.mode.vdisplay as u32,
                            &mut connector,
                            1,
                            &mut mode
                        );
                        FLIPS_SEEN.fetch_or(2, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            } else {
                call!(self.drm, "drmModeRmFB", fn(i32, u32) -> i32, self.fd, fb);
                call!(self.gbm, "gbm_surface_release_buffer", fn(*mut c_void, *mut c_void) -> (), self.surface, bo);
            }
        }
        Ok(())
    }
}

/// the size the frontend lays the screens out in, the panel's shape with
/// its long side across, 480 tall or 640 wide: 640x480 on a 4:3 panel,
/// 853x480 on a 16:9 one, 640x640 on a square one. A window that turns
/// with the phone changes it. Width and height packed, 0 until set.
pub(super) static LAYOUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn layout_size() -> (u32, u32) {
    match LAYOUT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => (640, 480),
        packed => ((packed >> 32) as u32, packed as u32),
    }
}

/// the panel's pixels to one of the layout's, f32 bits.
pub(super) static LAYOUT_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// how many of the panel's pixels one of the layout's is, for scaling the
/// screens by whole pixels.
pub fn layout_scale() -> f32 {
    match LAYOUT_SCALE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => 1.0,
        bits => f32::from_bits(bits),
    }
}

/// lays the screens out for a panel this size, as it is seen once turned.
pub(super) fn set_layout(panel: (i32, i32)) {
    let (width, height) = layout_for(panel);
    LAYOUT.store((width as u64) << 32 | height as u64, std::sync::atomic::Ordering::Relaxed);
    LAYOUT_SCALE.store((panel.0 as f32 / width as f32).to_bits(), std::sync::atomic::Ordering::Relaxed);
    log::info!("screens laid out in {width}x{height}");
}

pub(super) fn layout_for((width, height): (i32, i32)) -> (i32, i32) {
    if width * 3 >= height * 4 {
        ((480 * width + height / 2) / height, 480)
    } else {
        (640, (640 * height + width / 2) / width)
    }
}

/// the size ZAKURO_PANEL_SHOT's panel stands in at, 640x480 unless
/// ZAKURO_PANEL_SIZE=1920x1080 says, for a look at a bigger screen.
pub(super) fn offscreen_size() -> (i32, i32) {
    static SIZE: std::sync::OnceLock<(i32, i32)> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let given = std::env::var("ZAKURO_PANEL_SIZE").ok();
        given.and_then(|size| Some((size.split_once('x')?.0.parse().ok()?, size.split_once('x')?.1.parse().ok()?))).unwrap_or((640, 480))
    })
}

/// the panel's size and the way its picture is turned, none on a landscape
/// panel.
pub(super) fn panel_shape(output: &Output) -> ((i32, i32), Option<i32>) {
    let size = match output {
        Output::Kms(kms) => kms.size(),
        Output::Fbdev(window) => (window[0] as i32, window[1] as i32),
        Output::Window(size) => *size,
        Output::Offscreen(_) => offscreen_size(),
    };
    // a window turns with the device itself
    let turn = match output {
        Output::Kms(_) | Output::Fbdev(_) => panel_turn(size.0, size.1),
        Output::Window(_) | Output::Offscreen(_) => None,
    };
    (size, turn)
}

/// the layout scaled to fit what it is drawn into and centred, the menu
/// made for 640x480 in the middle of it.
#[derive(Clone, Copy)]
pub(super) struct Fit {
    pub(super) scale: f32,
    pub(super) left: f32,
    pub(super) top: f32,
}

impl Fit {
    pub(super) fn new((width, height): (i32, i32), (into_width, into_height): (i32, i32)) -> Fit {
        let scale = (into_width as f32 / width as f32).min(into_height as f32 / height as f32);
        Fit { scale, left: (into_width as f32 - width as f32 * scale) / 2.0, top: (into_height as f32 - height as f32 * scale) / 2.0 }
    }

    /// the same, from a point of the layout on.
    pub(super) fn inset(self, x: i32, y: i32) -> Fit {
        Fit { left: self.left + x as f32 * self.scale, top: self.top + y as f32 * self.scale, ..self }
    }

    pub(super) fn x(self, x: i32) -> i32 {
        (self.left + x as f32 * self.scale).round() as i32
    }

    pub(super) fn y(self, y: i32) -> i32 {
        (self.top + y as f32 * self.scale).round() as i32
    }

    /// left, top, width and height.
    pub(super) fn area(self, [left, top, width, height]: [i32; 4]) -> [i32; 4] {
        let (x, y) = (self.x(left), self.y(top));
        [x, y, self.x(left + width) - x, self.y(top + height) - y]
    }
}

pub(super) enum Part {
    Cursor { x: i32, y: i32 },
    /// a screen's buffer, the top one's version when it is interpolated.
    Surface { texture: glow::Texture, screen: [i32; 4], area: [i32; 4], smooth: bool, top: bool, picture: Option<u64> },
    Pixels { index: usize, pixels: Vec<u8>, width: u32, height: u32, area: [i32; 4], smooth: bool },
    Overlay(Arc<crate::Overlay>),
}

/// draws a user interface over the screens, its textures made the first time
/// they come and kept by their id, in space's size from where fit starts.
/// SAFETY: on the GL thread, with the panel's framebuffer bound
pub(super) unsafe fn draw_overlay(gl: &glow::Context, panel: &mut Panel, overlay: &crate::Overlay, height: i32, fit: Fit, space: (i32, i32)) -> Result<(), String> {
    let (program, array, vertices, indices) = match panel.overlay {
        Some(made) => made,
        None => {
            let program = link(gl, OVERLAY_VERTEX_SOURCE, OVERLAY_FRAGMENT_SOURCE)?;
            let (array, vertices, indices) = (gl.create_vertex_array()?, gl.create_buffer()?, gl.create_buffer()?);
            gl.bind_vertex_array(Some(array));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(indices));
            let stride = std::mem::size_of::<crate::OverlayVertex>() as i32;
            for (slot, count, kind, normalized, offset) in [(0, 2, glow::FLOAT, false, 0), (1, 2, glow::FLOAT, false, 8), (2, 4, glow::UNSIGNED_BYTE, true, 16)] {
                gl.enable_vertex_attrib_array(slot);
                gl.vertex_attrib_pointer_f32(slot, count, kind, normalized, stride, offset);
            }
            panel.overlay = Some((program, array, vertices, indices));
            (program, array, vertices, indices)
        }
    };
    for texture in &overlay.textures {
        let made = match panel.overlay_textures.get(&texture.id) {
            Some(&(made, size)) if texture.offset.is_some() || size == texture.size => {
                if texture.offset.is_none() {
                    continue;
                }
                made
            }
            old => {
                if let Some(&(old, _)) = old {
                    gl.delete_texture(old);
                }
                let made = gl.create_texture()?;
                gl.bind_texture(glow::TEXTURE_2D, Some(made));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, texture.size[0] as i32, texture.size[1] as i32);
                let filter = if texture.linear { glow::LINEAR } else { glow::NEAREST } as i32;
                for (parameter, value) in [(glow::TEXTURE_MIN_FILTER, filter), (glow::TEXTURE_MAG_FILTER, filter), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32)] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value);
                }
                panel.overlay_textures.insert(texture.id, (made, texture.size));
                made
            }
        };
        let [x, y] = texture.offset.unwrap_or_default();
        gl.bind_texture(glow::TEXTURE_2D, Some(made));
        let rows = texture.pixels.len() as u32 / 4 / texture.size[0].max(1);
        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, x as i32, y as i32, texture.size[0] as i32, rows as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&texture.pixels)));
    }
    for id in &overlay.free {
        if let Some((texture, _)) = panel.overlay_textures.remove(id) {
            gl.delete_texture(texture);
        }
    }
    let [x, y, width, tall] = fit.area([0, 0, space.0, space.1]);
    gl.viewport(x, height - y - tall, width, tall);
    gl.use_program(Some(program));
    gl.uniform_2_f32(gl.get_uniform_location(program, "size").as_ref(), space.0 as f32, space.1 as f32);
    gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
    gl.bind_vertex_array(Some(array));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
    gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(indices));
    gl.active_texture(glow::TEXTURE0);
    gl.bind_sampler(0, None);
    gl.enable(glow::BLEND);
    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
    gl.enable(glow::SCISSOR_TEST);
    for mesh in &overlay.meshes {
        let Some(&(texture, _)) = panel.overlay_textures.get(&mesh.texture) else { continue };
        let [left, top, right, bottom] = mesh.clip.map(|edge| edge as i32);
        let [left, top, right, bottom] = [fit.x(left), fit.y(top), fit.x(right), fit.y(bottom)];
        gl.scissor(left, height - bottom, right - left, bottom - top);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let bytes = std::slice::from_raw_parts(mesh.vertices.as_ptr().cast::<u8>(), std::mem::size_of_val(mesh.vertices.as_slice()));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
        let index_bytes = std::slice::from_raw_parts(mesh.indices.as_ptr().cast::<u8>(), std::mem::size_of_val(mesh.indices.as_slice()));
        gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, index_bytes, glow::STREAM_DRAW);
        gl.draw_elements(glow::TRIANGLES, mesh.indices.len() as i32, glow::UNSIGNED_INT, 0);
    }
    gl.disable(glow::SCISSOR_TEST);
    gl.disable(glow::BLEND);
    Ok(())
}

/// a landscape canvas the screens are laid out in and drawn into, which is
/// turned a quarter onto a portrait panel and fitted with black bars.
#[derive(Clone, Copy)]
pub(super) struct Canvas {
    pub(super) texture: glow::Texture,
    pub(super) framebuffer: glow::Framebuffer,
    pub(super) program: glow::Program,
    pub(super) width: i32,
    pub(super) height: i32,
}

/// which way to turn the canvas onto the panel, 0 clockwise, 1 counter
/// clockwise, none when the panel is landscape. ZAKURO_PANEL_ROTATE=0 turns
/// nothing, 90 turns clockwise, 270 counter clockwise, and unset turns
/// clockwise on a portrait panel.
pub(super) fn panel_turn(width: i32, height: i32) -> Option<i32> {
    // two landscape panels stacked are taller than wide, and are not turned
    if stacked() {
        return None;
    }
    static CHOICE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let choice = CHOICE.get_or_init(|| std::env::var("ZAKURO_PANEL_ROTATE").ok());
    match choice.as_deref() {
        Some("0") | Some("off") => None,
        Some("90") => Some(0),
        Some("270") => Some(1),
        // as wlroots turns a panel with its left or right side up
        _ => match PANEL_ORIENTATION.load(std::sync::atomic::Ordering::Relaxed) {
            2 => Some(1),
            3 => Some(0),
            _ => (height > width).then_some(0),
        },
    }
}

/// the surface on the second window set_second_window gave, made, remade
/// or let go as the window came, changed or went, and its size.
pub(super) fn second_surface(panel: &mut Panel) -> Option<(egl::types::EGLSurface, (i32, i32))> {
    let wanted = SECOND_WINDOW.load(std::sync::atomic::Ordering::Acquire);
    let egl = panel.egl;
    if panel.second.is_some_and(|(_, window, _)| window != wanted) {
        let (surface, ..) = panel.second.take().unwrap();
        // SAFETY: a surface this made on the display, not current
        unsafe { egl.egl.DestroySurface(egl.display, surface) };
        SECOND_IN_USE.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        log::info!("the second window went");
    }
    if panel.second.is_none() && !wanted.is_null() {
        // SAFETY: a window set_second_window keeps valid until it takes it
        // back, on the display and config the panel's context has
        unsafe {
            let surface = egl.egl.CreateWindowSurface(egl.display, egl.config, wanted as glutin_egl_sys::EGLNativeWindowType, std::ptr::null());
            if surface.is_null() {
                log::warn!("could not make a surface on the second window, 0x{:X}", egl.egl.GetError());
                return None;
            }
            let (mut width, mut height) = (0, 0);
            egl.egl.QuerySurface(egl.display, surface, egl::WIDTH as i32, &mut width);
            egl.egl.QuerySurface(egl.display, surface, egl::HEIGHT as i32, &mut height);
            // the panel's swap waits for its refresh already, this one need not
            egl.egl.MakeCurrent(egl.display, surface, surface, egl.context);
            egl.egl.SwapInterval(egl.display, 0);
            egl.egl.MakeCurrent(egl.display, egl.surface(), egl.surface(), egl.context);
            SECOND_IN_USE.store(wanted, std::sync::atomic::Ordering::Release);
            log::info!("a screen goes to a {width}x{height} second window");
            panel.second = Some((surface, wanted, (width, height)));
        }
    }
    panel.second.map(|(surface, _, size)| (surface, size))
}

/// draws the second window's screen filling it, with the cursor over it,
/// swaps it, and makes the panel's surface current again.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn draw_second(
    gl: &glow::Context,
    panel: &mut Panel,
    program: glow::Program,
    linear: glow::Sampler,
    empty: glow::VertexArray,
    surface: egl::types::EGLSurface,
    (width, height): (i32, i32),
    parts: Vec<Part>,
) -> Result<(), String> {
    let egl = panel.egl;
    if egl.egl.MakeCurrent(egl.display, surface, surface, egl.context) == 0 {
        return Err(format!("could not make the second window current, 0x{:X}", egl.egl.GetError()));
    }
    // the screen's own size is the layout here, its area all of it
    let mut layout = parts
        .iter()
        .find_map(|part| match part {
            Part::Surface { area, .. } | Part::Pixels { area, .. } => Some((area[2].max(1), area[3].max(1))),
            _ => None,
        })
        .unwrap_or((320, 240));
    let mut parts = parts;
    if SECOND_STRETCHED.load(std::sync::atomic::Ordering::Relaxed) {
        // stretched, the window's own size is the layout, the screen all of
        // it and the cursor moved along
        let (across, down) = (width as f32 / layout.0 as f32, height as f32 / layout.1 as f32);
        for part in &mut parts {
            match part {
                Part::Surface { area, .. } | Part::Pixels { area, .. } => *area = [0, 0, width, height],
                Part::Cursor { x, y } => (*x, *y) = ((*x as f32 * across) as i32, (*y as f32 * down) as i32),
                Part::Overlay(_) => {}
            }
        }
        layout = (width, height);
    }
    let fit = Fit::new(layout, (width, height));
    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    gl.viewport(0, 0, width, height);
    gl.disable(glow::SCISSOR_TEST);
    gl.disable(glow::BLEND);
    gl.color_mask(true, true, true, true);
    gl.clear_color(0.0, 0.0, 0.0, 1.0);
    gl.clear(glow::COLOR_BUFFER_BIT);
    gl.use_program(Some(program));
    gl.bind_vertex_array(Some(empty));
    gl.active_texture(glow::TEXTURE0);
    gl.bind_sampler(0, None);
    gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
    let drawn = draw_parts(gl, panel, program, linear, None, height, fit, layout, parts);
    let swapped = egl.egl.SwapBuffers(egl.display, surface) != 0;
    egl.egl.MakeCurrent(egl.display, egl.surface(), egl.surface(), egl.context);
    drawn?;
    if !swapped {
        log::warn!("could not swap the second window's buffers, 0x{:X}", egl.egl.GetError());
    }
    Ok(())
}

/// draws each part into the framebuffer bound, height pixels high, the
/// layout fitted into it as fit says.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn draw_parts(
    gl: &glow::Context,
    panel: &mut Panel,
    program: glow::Program,
    linear: glow::Sampler,
    framebuffer: Option<glow::Framebuffer>,
    height: i32,
    fit: Fit,
    layout: (i32, i32),
    parts: Vec<Part>,
) -> Result<(), String> {
            let framebuffer_height = height;
            for part in parts {
                let (texture, screen, upright, area, smooth, clip) = match part {
                    Part::Cursor { x, y } => {
                        let (x, y) = (fit.x(x), fit.y(y));
                        gl.enable(glow::SCISSOR_TEST);
                        for (size, shade) in [(7, 0.0), (5, 1.0)].map(|(size, shade)| ((size as f32 * fit.scale) as i32 | 1, shade)) {
                            gl.scissor(x - size / 2, height - y - size / 2, size, size);
                            gl.clear_color(shade, shade, shade, 1.0);
                            gl.clear(glow::COLOR_BUFFER_BIT);
                        }
                        gl.disable(glow::SCISSOR_TEST);
                        continue;
                    }
                    Part::Surface { texture, screen, area, smooth, top, picture } => {
                        let (area, clip) = cropped(fit.area(area), top, height);
                        let step = match picture {
                            Some(version) => panel.between.step((texture, version)),
                            None => {
                                if top {
                                    panel.between.reset();
                                }
                                Step::Plain { keep: false }
                            }
                        };
                        let keep = match step {
                            Step::Plain { keep } => keep.then_some(0),
                            Step::Halfway => Some(1),
                        };
                        if let Some(slot) = keep {
                            panel.between.keep(gl, program, texture, screen, slot)?;
                            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
                        }
                        if let Step::Halfway = step {
                            let gl_area = [area[0], height - area[1] - area[3], area[2], area[3]];
                            panel.between.draw(gl, framebuffer, gl_area, clip, smooth.then_some(linear))?;
                            gl.use_program(Some(program));
                            continue;
                        }
                        gl.use_program(Some(program));
                        (texture, screen, 0, area, smooth, clip)
                    }
                    Part::Overlay(overlay) if overlay.whole => {
                        draw_overlay(gl, panel, &overlay, height, fit, layout)?;
                        continue;
                    }
                    Part::Overlay(overlay) => {
                        // the menu in the middle, or on the top panel of two
                        // stacked, or at the top of a portrait window, over the
                        // screens and clear of the controls under them
                        let down = if stacked() || layout.1 > layout.0 { 0 } else { (layout.1 - 480) / 2 };
                        draw_overlay(gl, panel, &overlay, height, fit.inset((layout.0 - 640) / 2, down), (640, 480))?;
                        continue;
                    }
                    Part::Pixels { index, pixels, width, height, area, smooth } => {
                        if index == 0 {
                            panel.between.reset();
                        }
                        let texture = match panel.textures[index] {
                            Some((texture, w, h)) if (w, h) == (width, height) => texture,
                            made => {
                                if let Some((old, ..)) = made {
                                    gl.delete_texture(old);
                                }
                                let texture = gl.create_texture()?;
                                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, width as i32, height as i32);
                                panel.textures[index] = Some((texture, width, height));
                                texture
                            }
                        };
                        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                        gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, width as i32, height as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&pixels)));
                        let (area, clip) = cropped(fit.area(area), index == 0, framebuffer_height);
                        (texture, [width as i32, height as i32, 0, 0], 1, area, smooth, clip)
                    }
                };
                let y = height - area[1] - area[3];
                gl.viewport(area[0], y, area[2], area[3]);
                gl.uniform_4_i32(gl.get_uniform_location(program, "area").as_ref(), area[0], y, area[2], area[3]);
                gl.uniform_4_i32(gl.get_uniform_location(program, "screen").as_ref(), screen[0], screen[1], screen[2], screen[3]);
                gl.uniform_1_i32(gl.get_uniform_location(program, "upright").as_ref(), upright);
                gl.uniform_1_i32(gl.get_uniform_location(program, "smooth_scale").as_ref(), smooth as i32);
                gl.bind_sampler(0, smooth.then_some(linear));
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                if let Some([x, y, width, height]) = clip {
                    gl.enable(glow::SCISSOR_TEST);
                    gl.scissor(x, y, width, height);
                }
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                gl.disable(glow::SCISSOR_TEST);
            }
    Ok(())
}

pub(super) fn show(gl: GlRef, parts: Vec<Part>, frames: u32) -> Result<(), String> {
    disturb();
    PANEL.with(|panel| {
        let mut panel = panel.borrow_mut();
        let Some(panel) = panel.as_mut() else { return Ok(()) };
        let program = match panel.program {
            Some(program) => program,
            None => {
                let program = link(&gl, PACK_VERTEX_SOURCE, PRESENT_SOURCE)?;
                panel.program = Some(program);
                program
            }
        };
        // SAFETY: on the GL thread, the window surface is current and every
        // texture read is the size the uniforms say
        unsafe {
            let empty = match panel.empty {
                Some(empty) => empty,
                None => {
                    let empty = gl.create_vertex_array()?;
                    panel.empty = Some(empty);
                    empty
                }
            };
            if let Output::Window(_) = panel.output {
                // the app away, or back with a new window
                let wanted = WINDOW.load(std::sync::atomic::Ordering::Acquire);
                if (wanted != WINDOW_IN_USE.load(std::sync::atomic::Ordering::Acquire) && !follow_window(panel.egl, wanted)) || wanted.is_null() {
                    return Ok(());
                }
            }
            if let Output::Window(size) = &mut panel.output {
                let (mut width, mut height) = (0, 0);
                panel.egl.egl.QuerySurface(panel.egl.display, panel.egl.surface(), egl::WIDTH as i32, &mut width);
                panel.egl.egl.QuerySurface(panel.egl.display, panel.egl.surface(), egl::HEIGHT as i32, &mut height);
                if width > 0 && height > 0 && (width, height) != *size {
                    log::info!("the window is {width}x{height} now");
                    *size = (width, height);
                    set_layout(*size);
                }
            }
            let ((panel_width, panel_height), turn) = panel_shape(&panel.output);
            let panel_framebuffer = match &panel.output {
                Output::Offscreen(framebuffer) => Some(*framebuffer),
                _ => None,
            };
            // a portrait panel under a landscape layout: everything is drawn
            // into a canvas the panel's size on its side, which is turned
            // onto the panel at the end
            let (width, height, framebuffer) = match turn {
                Some(_) => {
                    let (canvas_width, canvas_height) = (panel_height, panel_width);
                    let canvas = match panel.canvas {
                        Some(canvas) if (canvas.width, canvas.height) == (canvas_width, canvas_height) => canvas,
                        _ => {
                            let texture = gl.create_texture()?;
                            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, canvas_width, canvas_height);
                            let framebuffer = gl.create_framebuffer()?;
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
                            let program = link(&gl, PACK_VERTEX_SOURCE, ROTATE_SOURCE)?;
                            let made = Canvas { texture, framebuffer, program, width: canvas_width, height: canvas_height };
                            panel.canvas = Some(made);
                            log::info!("drawing a {canvas_width}x{canvas_height} picture turned onto the {panel_width}x{panel_height} panel");
                            made
                        }
                    };
                    (canvas_width, canvas_height, Some(canvas.framebuffer))
                }
                None => (panel_width, panel_height, panel_framebuffer),
            };
            let layout = layout_size();
            let layout = (layout.0 as i32, layout.1 as i32);
            let fit = Fit::new(layout, (width, height));
            gl.bind_framebuffer(glow::FRAMEBUFFER, framebuffer);
            gl.viewport(0, 0, width, height);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::CULL_FACE);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(program));
            let linear = match panel.linear {
                Some(linear) => linear,
                None => {
                    let linear = gl.create_sampler()?;
                    for (parameter, value) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
                        gl.sampler_parameter_i32(linear, parameter, value as i32);
                    }
                    panel.linear = Some(linear);
                    linear
                }
            };
            gl.bind_vertex_array(Some(empty));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_sampler(0, None);
            gl.uniform_1_i32(gl.get_uniform_location(program, "source").as_ref(), 0);
            // the screen the second window shows, and the cursor when that is
            // the bottom one, go there; the rest to the panel
            let second = second_surface(panel);
            let (parts, second_parts): (Vec<Part>, Vec<Part>) = match second {
                Some(_) => {
                    let top = SECOND_SHOWS_TOP.load(std::sync::atomic::Ordering::Relaxed);
                    parts.into_iter().partition(|part| match part {
                        Part::Surface { top: is_top, .. } => *is_top != top,
                        Part::Pixels { index, .. } => (*index == 0) != top,
                        Part::Cursor { .. } => top,
                        Part::Overlay(_) => true,
                    })
                }
                None => (parts, Vec::new()),
            };
            draw_parts(&gl, panel, program, linear, framebuffer, height, fit, layout, parts)?;
            if let Output::Offscreen(_) = panel.output {
                if let Some(path) = std::env::var_os("ZAKURO_PANEL_SHOT") {
                    let mut data = vec![0u8; (width * height * 4) as usize];
                    gl.read_pixels(0, 0, width, height, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut data)));
                    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
                    for row in data.chunks(width as usize * 4).rev() {
                        ppm.extend(row.chunks(4).flat_map(|pixel| &pixel[..3]));
                    }
                    // a % in the name numbers each picture rather than keeping the last
                    static SHOTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let path = path.to_string_lossy().replace('%', &format!("{:05}", SHOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
                    let _ = std::fs::write(path, ppm);
                }
                return Ok(());
            }
            if let (Some(turn), Some(canvas)) = (turn, panel.canvas) {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, panel_width, panel_height);
                gl.disable(glow::SCISSOR_TEST);
                gl.disable(glow::BLEND);
                gl.color_mask(true, true, true, true);
                gl.clear_color(0.0, 0.0, 0.0, 1.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                // the canvas on its side, fitted to the panel and centred
                let (turned_width, turned_height) = (canvas.height as f32, canvas.width as f32);
                let scale = (panel_width as f32 / turned_width).min(panel_height as f32 / turned_height);
                let (rect_width, rect_height) = (turned_width * scale, turned_height * scale);
                let (left, bottom) = ((panel_width as f32 - rect_width) / 2.0, (panel_height as f32 - rect_height) / 2.0);
                gl.use_program(Some(canvas.program));
                gl.uniform_1_i32(gl.get_uniform_location(canvas.program, "canvas").as_ref(), 0);
                gl.uniform_1_i32(gl.get_uniform_location(canvas.program, "turn").as_ref(), turn);
                gl.uniform_4_f32(gl.get_uniform_location(canvas.program, "rect").as_ref(), left, bottom, rect_width, rect_height);
                gl.bind_vertex_array(Some(empty));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_sampler(0, Some(linear));
                gl.bind_texture(glow::TEXTURE_2D, Some(canvas.texture));
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            if panel.egl.egl.SwapBuffers(panel.egl.display, panel.egl.surface()) == 0 {
                return Err("could not swap the panel's buffers".into());
            }
            if let Some((surface, size)) = second {
                draw_second(&gl, panel, program, linear, empty, surface, size, second_parts)?;
            }
        }
        match &mut panel.output {
            Output::Kms(kms) => kms.flip(frames),
            Output::Fbdev(_) | Output::Window(_) | Output::Offscreen(_) => Ok(()),
        }
    })
}

/// where the panel's picture goes: the handheld's screen, or for working on
/// a computer without one an image of its size, which ZAKURO_PANEL_SHOT=file
/// saves each time, numbered in place of a % in the name.
pub(super) enum Output {
    Kms(Kms),
    /// the framebuffer device's size, the window a Mali driver built for
    /// it takes, kept where EGL was given it.
    Fbdev(Box<[u16; 2]>),
    /// a window set_window was given, its size.
    Window((i32, i32)),
    Offscreen(glow::Framebuffer),
}

pub(super) struct Panel {
    pub(super) egl: &'static Egl,
    pub(super) output: Output,
    pub(super) program: Option<glow::Program>,
    pub(super) textures: [Option<(glow::Texture, u32, u32)>; 2],
    pub(super) empty: Option<glow::VertexArray>,
    /// linear filtering for the screen drawn smooth.
    pub(super) linear: Option<glow::Sampler>,
    /// the overlay's program, vertex array and buffers, and its textures.
    pub(super) overlay: Option<(glow::Program, glow::VertexArray, glow::Buffer, glow::Buffer)>,
    pub(super) overlay_textures: HashMap<u64, (glow::Texture, [u32; 2])>,
    pub(super) between: Between,
    /// the landscape canvas a portrait panel's picture is drawn into first.
    pub(super) canvas: Option<Canvas>,
    /// the surface on the second window, the window and its size.
    pub(super) second: Option<(egl::types::EGLSurface, *mut c_void, (i32, i32))>,
}

thread_local! {
    pub(super) static PANEL: std::cell::RefCell<Option<Panel>> = const { std::cell::RefCell::new(None) };
}

pub(super) struct Egl {
    pub(super) libraries: (libloading::Library, Option<libloading::Library>),
    pub(super) egl: egl::Egl,
    pub(super) display: egl::types::EGLDisplay,
    pub(super) config: egl::types::EGLConfig,
    pub(super) context: egl::types::EGLContext,
    /// the panel's surface, which an activity's window going and coming
    /// back changes.
    pub(super) surface: std::sync::atomic::AtomicPtr<c_void>,
}

/// A backend owns ONLY one compiler's context/pbuffer on this display. It
/// borrows the intentionally static parent Egl; it never destroys that parent
/// context, its panel surfaces, the shared display or EGL libraries.
struct CompilerEgl<'a> { egl: &'a Egl }
impl super::compiler_context_owner::EglApi for CompilerEgl<'_> {
    fn create_context(&self) -> usize {
        let attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
        let context = unsafe { self.egl.egl.CreateContext(self.egl.display, self.egl.config, self.egl.context, attributes.as_ptr()) as usize };
        if context == 0 { log::warn!("no context to compile shaders in: error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        context
    }
    fn create_pbuffer(&self) -> usize {
        let attributes = [egl::WIDTH as i32, 16, egl::HEIGHT as i32, 16, egl::NONE as i32];
        let surface = unsafe { self.egl.egl.CreatePbufferSurface(self.egl.display, self.egl.config, attributes.as_ptr()) as usize };
        if surface == 0 { log::warn!("compiler EGL pbuffer create failed: error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        surface
    }
    fn make_current(&self, context: usize, surface: usize) -> bool {
        let ok = unsafe { self.egl.egl.MakeCurrent(self.egl.display, surface as egl::types::EGLSurface, surface as egl::types::EGLSurface, context as egl::types::EGLContext) != 0 };
        if !ok && (surface != 0 || context == 0) { log::warn!("compiler EGL bind/unbind failed: context={context:x} surface={surface:x} error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        ok
    }
    fn current_context(&self) -> usize { unsafe { self.egl.egl.GetCurrentContext() as usize } }
    fn destroy_surface(&self, surface: usize) -> bool {
        let ok = unsafe { self.egl.egl.DestroySurface(self.egl.display, surface as egl::types::EGLSurface) != 0 };
        if !ok { log::warn!("compiler EGL surface destroy failed: handle={surface:x} error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        ok
    }
    fn destroy_context(&self, context: usize) -> bool {
        let ok = unsafe { self.egl.egl.DestroyContext(self.egl.display, context as egl::types::EGLContext) != 0 };
        if !ok { log::warn!("compiler EGL context destroy failed: handle={context:x} error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        ok
    }
    fn release_thread(&self) -> bool {
        let ok = unsafe { self.egl.egl.ReleaseThread() != 0 };
        if !ok { log::warn!("compiler EGL thread release failed: error=0x{:x}", unsafe { self.egl.egl.GetError() }); }
        ok
    }
}

pub(super) struct SharedCompilerContext<'a> {
    gl: std::mem::ManuallyDrop<glow::Context>,
    owner: super::compiler_context_owner::ContextOwner<CompilerEgl<'a>>,
}
impl std::ops::Deref for SharedCompilerContext<'_> {
    type Target = glow::Context;
    fn deref(&self) -> &Self::Target { &self.gl }
}
impl Drop for SharedCompilerContext<'_> {
    fn drop(&mut self) {
        let current_for_cleanup = self.owner.ensure_current_for_cleanup();
        if current_for_cleanup {
            // Worker-owned standalone shader objects only; linked programs,
            // shared separable-stage leases and main renderer objects survive.
            super::programs::release_worker_shaders(&self.gl);
            // Glow::Context::drop may unset a debug callback through GL.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.gl) };
        } else {
            // No GL call on a missing/foreign context. ManuallyDrop intentionally
            // retains the wrapper/callback allocation in this exceptional path;
            // shader/driver reclamation is unknown, not falsely called complete.
            log::warn!("compiler context unavailable for GL cache/Glow cleanup; retained shader and callback reclamation unknown");
        }
        let report = self.owner.close();
        if report.failed() { log::warn!("compiler EGL cleanup incomplete: {report:?}"); }
        // ContextOwner::drop is now an explicit no-op; no second destroy attempt.
    }
}

// SAFETY: EGL's entry points can be called from any thread, and the handles
// here are only read once made
unsafe impl Send for Egl {}
unsafe impl Sync for Egl {}

/// glDrawRangeElementsBaseVertex, which glow does not wrap, as the loader
/// found it.
pub(super) static DRAW_RANGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn remember(name: &str, address: *const c_void) -> *const c_void {
    if name == "glDrawRangeElementsBaseVertex" {
        DRAW_RANGE.store(address as usize, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(index) = PIPELINE_NAMES.iter().position(|&wanted| wanted == name) {
        PIPELINE_FNS[index].store(address as usize, std::sync::atomic::Ordering::Relaxed);
    }
    address
}

pub(super) fn symbol(library: &libloading::Library, name: &str) -> *const c_void {
    let Ok(name) = CString::new(name) else { return std::ptr::null() };
    // SAFETY: the symbol is only read as an address
    unsafe { library.get::<*const c_void>(name.as_bytes_with_nul()).map_or(std::ptr::null(), |symbol| *symbol) }
}

/// the first of the names that loads, else why the first did not.
pub(super) fn open(names: &[&str]) -> Result<libloading::Library, String> {
    let mut first = None;
    for name in names {
        // SAFETY: loading the system's EGL and GLES libraries runs their
        // initializers, which is what using them takes
        match unsafe { libloading::Library::new(name) } {
            Ok(library) => return Ok(library),
            Err(why) => _ = first.get_or_insert(why.to_string()),
        }
    }
    Err(first.unwrap_or_default())
}

/// the EGL, GBM and Mali libraries this process loaded, and whether it can
/// use the Mali device, for telling why EGL would not start.
pub(super) fn drawing_libraries() -> String {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut found: Vec<&str> = maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter(|path| {
            let file = path.rsplit('/').next().unwrap_or(path).to_lowercase();
            ["egl", "gbm", "gles", "mali"].iter().any(|name| file.contains(name))
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    // SAFETY: access only looks the path up
    let mali = unsafe { libc::access(c"/dev/mali0".as_ptr(), libc::R_OK | libc::W_OK) } == 0;
    format!("loaded {}, /dev/mali0 {}", found.join(" "), if mali { "usable" } else { "not usable" })
}

pub(super) fn gbm_display(loaded: &egl::Egl) -> Option<egl::types::EGLDisplay> {
    let library = open(&["libgbm.so.1", "libgbm.so"]).ok()?;
    let create = symbol(&library, "gbm_create_device");
    if create.is_null() {
        return None;
    }
    // SAFETY: gbm_create_device takes a DRM file descriptor and returns a
    // device the process keeps for as long as it draws, so neither is closed
    unsafe {
        let create: extern "C" fn(i32) -> *mut c_void = std::mem::transmute(create);
        let found = ["/dev/dri/card0", "/dev/dri/renderD128"].iter().find_map(|path| {
            let path = CString::new(*path).ok()?;
            let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let device = create(fd);
            let display = if device.is_null() { std::ptr::null() } else { loaded.GetDisplay(device as glutin_egl_sys::EGLNativeDisplayType) };
            if display.is_null() {
                libc::close(fd);
                return None;
            }
            Some(display)
        });
        if found.is_some() {
            std::mem::forget(library);
        }
        found
    }
}

impl Egl {
    pub(super) fn surface(&self) -> egl::types::EGLSurface {
        self.surface.load(std::sync::atomic::Ordering::Acquire) as egl::types::EGLSurface
    }

    pub(super) fn new() -> Result<(Egl, glow::Context), String> {
        let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
        let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
        let loaded = egl::Egl::load_with(|name| symbol(&library, name));
        // SAFETY: plain EGL calls with attribute lists that end in NONE and
        // live as long as each call
        unsafe {
            let mut display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                display = gbm_display(&loaded).ok_or("no EGL display")?;
            }
            if loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL, 0x{:X}", loaded.GetError()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let mut config = std::ptr::null();
            let mut pbuffer = false;
            for surface_type in [egl::PBUFFER_BIT, 0] {
                let attributes = [
                    egl::RENDERABLE_TYPE as i32,
                    egl::OPENGL_ES3_BIT as i32,
                    egl::SURFACE_TYPE as i32,
                    surface_type as i32,
                    egl::NONE as i32,
                ];
                let mut count = 0;
                if loaded.ChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) != 0 && count > 0 {
                    pbuffer = surface_type != 0;
                    break;
                }
                config = std::ptr::null();
            }
            if config.is_null() {
                return Err("no EGL config for OpenGL ES 3".into());
            }
            let context_attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
            let context = loaded.CreateContext(display, config, egl::NO_CONTEXT, context_attributes.as_ptr());
            if context.is_null() {
                return Err(format!("could not create an OpenGL ES 3.1 context, 0x{:X}", loaded.GetError()));
            }
            let surface_attributes = [egl::WIDTH as i32, 16, egl::HEIGHT as i32, 16, egl::NONE as i32];
            let surface = match pbuffer {
                true => loaded.CreatePbufferSurface(display, config, surface_attributes.as_ptr()),
                false => egl::NO_SURFACE,
            };
            if loaded.MakeCurrent(display, surface, surface, context) == 0 {
                return Err(format!("could not make the context current, 0x{:X}", loaded.GetError()));
            }
            let gl = glow::Context::from_loader_function(|name| {
                let direct = gles.as_ref().map_or(std::ptr::null(), |gles| symbol(gles, name));
                if !direct.is_null() {
                    return remember(name, direct);
                }
                let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
                remember(name, loaded.GetProcAddress(c_name.as_ptr()) as *const c_void)
            });
            Ok((Egl { libraries: (library, gles), egl: loaded, display, config, context, surface: std::sync::atomic::AtomicPtr::new(surface as *mut c_void) }, gl))
        }
    }

    /// a context drawing into a GBM surface on the panel, for showing the
    /// screens through KMS.
    pub(super) fn new_kms() -> Result<(Egl, glow::Context, Kms), String> {
        // SAFETY: as in new, and the window is the GBM surface Kms made
        unsafe {
            let kms = Kms::open()?;
            // the Mali driver is GBM, EGL and GLES in one library, and its GBM
            // device only works with its own EGL; dArkOS points libEGL.so.1
            // at glvnd and Mesa instead
            let together = !symbol(&kms.gbm, "eglGetDisplay").is_null();
            let (egl_names, gles_names): (&[&str], &[&str]) = match together {
                true => (&["libgbm.so.1", "libgbm.so"], &["libgbm.so.1", "libgbm.so"]),
                false => (&["libEGL.so.1", "libEGL.so"], &["libGLESv2.so.2", "libGLESv2.so"]),
            };
            let library = open(egl_names).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(gles_names).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let (mut major, mut minor) = (0, 0);
            // the platform call first: the Mali blob's legacy eglGetDisplay
            // returns no display for a GBM device (g29p1 on the RG52 Mini), or
            // takes it for an X11 one where it is built for X11 too (ROCKNIX
            // on the R36S) and crashes
            const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
            let platform = ["eglGetPlatformDisplay", "eglGetPlatformDisplayEXT"]
                .iter()
                .map(|name| symbol(&library, name))
                .find(|address| !address.is_null());
            let (mut display, mut ready) = (std::ptr::null(), false);
            if let Some(address) = platform {
                let get: extern "C" fn(u32, *mut c_void, *const i32) -> egl::types::EGLDisplay = std::mem::transmute(address);
                display = get(EGL_PLATFORM_GBM_KHR, kms.device, std::ptr::null());
                ready = !display.is_null() && loaded.Initialize(display, &mut major, &mut minor) != 0;
            }
            log::info!("EGL on the GBM device, {}, {}", if ready { "by platform" } else { "by the legacy call" }, drawing_libraries());
            if !ready {
                display = loaded.GetDisplay(kms.device as glutin_egl_sys::EGLNativeDisplayType);
                ready = !display.is_null() && loaded.Initialize(display, &mut major, &mut minor) != 0;
            }
            if !ready {
                return Err(format!("could not initialize EGL on the GBM device, 0x{:X}, {}", loaded.GetError(), drawing_libraries()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [egl::RENDERABLE_TYPE as i32, egl::OPENGL_ES3_BIT as i32, egl::SURFACE_TYPE as i32, egl::WINDOW_BIT as i32, egl::NONE as i32];
            let mut configs = [std::ptr::null(); 64];
            let mut count = 0;
            loaded.ChooseConfig(display, attributes.as_ptr(), configs.as_mut_ptr(), 64, &mut count);
            let config = configs[..count.max(0) as usize]
                .iter()
                .copied()
                .find(|&config| {
                    let mut visual = 0;
                    loaded.GetConfigAttrib(display, config, egl::NATIVE_VISUAL_ID as i32, &mut visual);
                    visual as u32 == GBM_FORMAT_XRGB8888
                })
                .ok_or("no EGL config for the panel")?;
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, kms.surface as glutin_egl_sys::EGLNativeWindowType, 0)?;
            Ok((egl, gl, kms))
        }
    }

    /// screens through the framebuffer device, for a Mali driver built for
    /// it rather than for GBM, as on Allwinner's H700 under Knulli.
    pub(super) fn new_fbdev() -> Result<(Egl, glow::Context, Box<[u16; 2]>), String> {
        // SAFETY: FBIOGET_VSCREENINFO fills the 160 bytes of the screen's
        // fb_var_screeninfo, its size first, and the EGL calls are as in new
        unsafe {
            let fd = libc::open(c"/dev/fb0".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return Err("could not open /dev/fb0".into());
            }
            let mut info = [0u32; 40];
            let read = libc::ioctl(fd, 0x4600, info.as_mut_ptr());
            libc::close(fd);
            if read < 0 {
                return Err("could not read the size of /dev/fb0".into());
            }
            let window = Box::new([info[0] as u16, info[1] as u16]);
            let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL for /dev/fb0, 0x{:X}, {}", loaded.GetError(), drawing_libraries()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [
                egl::RENDERABLE_TYPE as i32,
                egl::OPENGL_ES3_BIT as i32,
                egl::SURFACE_TYPE as i32,
                egl::WINDOW_BIT as i32,
                egl::RED_SIZE as i32,
                8,
                egl::GREEN_SIZE as i32,
                8,
                egl::BLUE_SIZE as i32,
                8,
                egl::NONE as i32,
            ];
            let (mut config, mut count) = (std::ptr::null(), 0);
            if loaded.ChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0 || count == 0 {
                return Err("no EGL config for /dev/fb0".into());
            }
            // the driver flips on the framebuffer, at the next refresh
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, window.as_ptr() as glutin_egl_sys::EGLNativeWindowType, 1)?;
            Ok((egl, gl, window))
        }
    }

    /// a context drawing into a native window set_window was given, and the
    /// window's size.
    pub(super) fn new_window(window: *mut c_void) -> Result<(Egl, glow::Context, (i32, i32)), String> {
        // SAFETY: as in new, and the window stays valid as set_window asks
        unsafe {
            let library = open(&["libEGL.so.1", "libEGL.so"]).map_err(|why| format!("could not load libEGL, {why}"))?;
            let gles = open(&["libGLESv2.so.2", "libGLESv2.so"]).ok();
            let loaded = egl::Egl::load_with(|name| symbol(&library, name));
            let display = loaded.GetDisplay(egl::DEFAULT_DISPLAY);
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || loaded.Initialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("could not initialize EGL for the window, 0x{:X}", loaded.GetError()));
            }
            loaded.BindAPI(egl::OPENGL_ES_API);
            let attributes = [
                egl::RENDERABLE_TYPE as i32,
                egl::OPENGL_ES3_BIT as i32,
                egl::SURFACE_TYPE as i32,
                egl::WINDOW_BIT as i32,
                egl::RED_SIZE as i32,
                8,
                egl::GREEN_SIZE as i32,
                8,
                egl::BLUE_SIZE as i32,
                8,
                egl::ALPHA_SIZE as i32,
                0,
                egl::DEPTH_SIZE as i32,
                0,
                egl::NONE as i32,
            ];
            let mut configs = [std::ptr::null(); 64];
            let mut count = 0;
            if loaded.ChooseConfig(display, attributes.as_ptr(), configs.as_mut_ptr(), 64, &mut count) == 0 || count <= 0 {
                return Err("no EGL config for the window".into());
            }
            // the first config with exactly eight bits a channel and no alpha,
            // which the window's buffers take without a conversion
            let exact = configs[..count as usize].iter().copied().find(|&config| {
                let size = |attribute: u32| {
                    let mut value = 0;
                    loaded.GetConfigAttrib(display, config, attribute as i32, &mut value);
                    value
                };
                [size(egl::RED_SIZE), size(egl::GREEN_SIZE), size(egl::BLUE_SIZE), size(egl::ALPHA_SIZE)] == [8, 8, 8, 0]
            });
            let config = exact.unwrap_or(configs[0]);
            let (egl, gl) = Egl::on_window((library, gles), loaded, display, config, window as glutin_egl_sys::EGLNativeWindowType, 1)?;
            let (mut width, mut height) = (0, 0);
            egl.egl.QuerySurface(egl.display, egl.surface(), egl::WIDTH as i32, &mut width);
            egl.egl.QuerySurface(egl.display, egl.surface(), egl::HEIGHT as i32, &mut height);
            if width <= 0 || height <= 0 {
                return Err(format!("the window has no size, {width}x{height}"));
            }
            Ok((egl, gl, (width, height)))
        }
    }

    /// an OpenGL ES 3.1 context drawing to a window, swapping its buffers
    /// interval refreshes apart.
    pub(super) unsafe fn on_window(
        (library, gles): (libloading::Library, Option<libloading::Library>),
        loaded: egl::Egl,
        display: egl::types::EGLDisplay,
        config: egl::types::EGLConfig,
        window: glutin_egl_sys::EGLNativeWindowType,
        interval: i32,
    ) -> Result<(Egl, glow::Context), String> {
        let context_attributes = [egl::CONTEXT_CLIENT_VERSION as i32, 3, egl::CONTEXT_MINOR_VERSION as i32, 1, egl::NONE as i32];
        let context = loaded.CreateContext(display, config, egl::NO_CONTEXT, context_attributes.as_ptr());
        if context.is_null() {
            return Err(format!("could not create an OpenGL ES 3.1 context, 0x{:X}", loaded.GetError()));
        }
        let mut surface = loaded.CreateWindowSurface(display, config, window, std::ptr::null());
        let mut error = if surface.is_null() { loaded.GetError() } else { 0 };
        // a framebuffer driver that takes no window for the screen, as
        // PowerVR's does, refuses the size Mali's takes
        if surface.is_null() && error == egl::BAD_NATIVE_WINDOW as i32 && !window.is_null() {
            surface = loaded.CreateWindowSurface(display, config, std::ptr::null_mut(), std::ptr::null());
            error = if surface.is_null() { loaded.GetError() } else { 0 };
        }
        if surface.is_null() {
            return Err(format!("could not make a window surface, 0x{error:X}, {}", drawing_libraries()));
        }
        if loaded.MakeCurrent(display, surface, surface, context) == 0 {
            return Err(format!("could not make the context current, 0x{:X}", loaded.GetError()));
        }
        loaded.SwapInterval(display, interval);
        let gl = glow::Context::from_loader_function(|name| {
            let direct = gles.as_ref().map_or(std::ptr::null(), |gles| symbol(gles, name));
            if !direct.is_null() {
                return remember(name, direct);
            }
            let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
            remember(name, loaded.GetProcAddress(c_name.as_ptr()) as *const c_void)
        });
        Ok((Egl { libraries: (library, gles), egl: loaded, display, config, context, surface: std::sync::atomic::AtomicPtr::new(surface as *mut c_void) }, gl))
    }

    /// a context sharing this one's objects, made current on the calling
    /// thread with no surface, or a small pbuffer where EGL wants one.
    pub(super) fn shared_context(&self) -> Option<SharedCompilerContext<'_>> {
        let owner = match super::compiler_context_owner::ContextOwner::create(CompilerEgl { egl: self }) {
            Ok(owner) => owner,
            Err(error) => {
                log::warn!("compiler EGL context startup failed: {error:?}");
                return None;
            }
        };
        // The owner already exists: if Glow construction panics, its raw EGL
        // context/pbuffer still have a matching cleanup owner on this thread.
        let gles = self.libraries.1.as_ref();
        let gl = unsafe { glow::Context::from_loader_function(|name| {
            let direct = gles.map_or(std::ptr::null(), |gles| symbol(gles, name));
            if !direct.is_null() { return direct; }
            let Ok(c_name) = CString::new(name) else { return std::ptr::null() };
            self.egl.GetProcAddress(c_name.as_ptr()) as *const c_void
        }) };
        Some(SharedCompilerContext { gl: std::mem::ManuallyDrop::new(gl), owner })
    }

    pub(super) fn new_with_panel() -> Result<(Egl, glow::Context, Option<Output>), String> {
        let window = WINDOW.load(std::sync::atomic::Ordering::Acquire);
        if !window.is_null() {
            let (egl, gl, size) = Egl::new_window(window)?;
            WINDOW_IN_USE.store(window, std::sync::atomic::Ordering::Release);
            log::info!("the screens go to a {}x{} window", size.0, size.1);
            return Ok((egl, gl, Some(Output::Window(size))));
        }
        if std::env::var_os("ZAKURO_KMS").is_some() {
            return match Egl::new_kms() {
                Ok((egl, gl, kms)) => Ok((egl, gl, Some(Output::Kms(kms)))),
                Err(error) if std::path::Path::new("/dev/fb0").exists() => {
                    let (egl, gl, window) = Egl::new_fbdev().map_err(|fbdev| format!("{error}, and {fbdev}"))?;
                    log::info!("{error}, the screens go through /dev/fb0, {}x{}", window[0], window[1]);
                    Ok((egl, gl, Some(Output::Fbdev(window))))
                }
                Err(error) => Err(error),
            };
        }
        let (egl, gl) = Egl::new()?;
        Ok((egl, gl, None))
    }
}
