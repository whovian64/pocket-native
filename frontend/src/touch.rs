//! buttons and a circle pad drawn over the screens for a touch screen with
//! no gamepad, a phone's: where they go around the screens, or where the
//! player moved them and how big they made them, which of them the fingers
//! hold, and the overlay they are drawn in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zakuro_gpu::Overlay;

use crate::font::{rgba, Canvas, Font};
use crate::input::{A, B, DOWN, FN, L1, L2, LEFT, R1, R2, RIGHT, SELECT, START, UP, X, Y};

/// the font's faces, the menu's list and its small one.
const BIG: usize = 0;
const SMALL: usize = 1;
/// the tools along the middle while the controls are edited.
const TOOLS: [&str; 4] = ["Smaller", "Bigger", "Reset", "Done"];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Round(u16, &'static str),
    /// a small flat one, select, start and the menu's.
    Flat(u16, &'static str),
    Dpad,
    Stick,
}

#[derive(Clone, Copy)]
struct Control {
    kind: Kind,
    /// the controls that move together, the four face buttons one.
    group: &'static str,
    at: [f32; 2],
    /// a flat one's half width, and its half height.
    radius: f32,
    tall: f32,
}

/// what a finger does from when it came down: holds the circle pad or the
/// d-pad wherever it goes, presses the buttons it slides over, or touches
/// the screens.
#[derive(Clone, Copy)]
enum Grip {
    Control(usize),
    Buttons,
    Free,
}

/// what the fingers hold: keys, the circle pad from -1 to 1 across and
/// down, and the fingers on none of them, which touch the screens.
pub struct Held {
    pub keys: HashSet<u16>,
    pub stick: Option<(f32, f32)>,
    pub free: Vec<(f32, f32)>,
}

/// where the player put each group, its middle across and down the layout
/// from 0 to 1, and how many times its size, held upright and sideways.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Custom {
    portrait: BTreeMap<String, [f32; 3]>,
    landscape: BTreeMap<String, [f32; 3]>,
}

struct Editing {
    /// the group picked, and the finger dragging it with where the group's
    /// middle is from it.
    group: Option<&'static str>,
    drag: Option<(i32, [f32; 2])>,
    /// the fingers down the frame before, none on the first frame, whose
    /// fingers were down already.
    down: Option<HashSet<i32>>,
}

#[derive(Default)]
pub struct Controls {
    placed: Vec<Control>,
    /// the layout's size and the screens' areas they were placed around.
    around: Option<((u32, u32), [Option<[i32; 4]>; 2])>,
    /// the keys and circle pad the overlay last drawn shows held.
    drawn: Option<(Vec<u16>, Option<(i8, i8)>, Arc<Overlay>)>,
    grips: HashMap<i32, Grip>,
    custom: Custom,
    editing: Option<Editing>,
}

fn path() -> std::path::PathBuf {
    crate::settings::config_dir().join("touch.toml")
}

/// the control nearest a point, within its reach, of those which takes.
fn nearest(placed: &[Control], (x, y): (f32, f32), which: impl Fn(&Control) -> bool) -> Option<usize> {
    let reach = |control: &Control| control.radius * if control.kind == Kind::Stick { 1.6 } else { 1.3 };
    (0..placed.len())
        .filter(|&index| which(&placed[index]))
        .map(|index| ((x - placed[index].at[0]).hypot(y - placed[index].at[1]) / reach(&placed[index]), index))
        .filter(|(distance, _)| *distance < 1.0)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, index)| index)
}

impl Controls {
    /// the controls, as the player last edited them.
    pub fn load() -> Controls {
        Controls { custom: crate::settings::load(&path()), ..Default::default() }
    }

    fn custom_for(&mut self, (width, height): (u32, u32)) -> &mut BTreeMap<String, [f32; 3]> {
        if height > width { &mut self.custom.portrait } else { &mut self.custom.landscape }
    }

    /// the controls under the screens of a portrait window, else beside
    /// them, out of their way where there is room, then moved and sized as
    /// the player edited them.
    fn place(&mut self, font: &Font, size: (u32, u32), screens: [Option<[i32; 4]>; 2]) {
        if self.around == Some((size, screens)) {
            return;
        }
        (self.around, self.drawn) = (Some((size, screens)), None);
        let (w, h) = (size.0 as f32, size.1 as f32);
        let u = w.min(h) * 0.075;
        let placed = || screens.iter().flatten();
        let round = |key, name, group, x: f32, y: f32, radius: f32| Control { kind: Kind::Round(key, name), group, at: [x, y], radius: radius * u, tall: radius * u };
        let flat = |key, name, x: f32, y: f32| Control { kind: Kind::Flat(key, name), group: name, at: [x, y], radius: (font.faces[SMALL].width(name) / 2.0 + 0.3 * u).max(0.8 * u), tall: 0.42 * u };
        let other = |kind, group, x: f32, y: f32, radius: f32| Control { kind, group, at: [x, y], radius: radius * u, tall: radius * u };
        let face = |x: f32, y: f32| [(A, "A", 1.2, 0.0), (B, "B", 0.0, 1.2), (X, "X", 0.0, -1.2), (Y, "Y", -1.2, 0.0)].map(|(key, name, dx, dy)| round(key, name, "Face", x + dx * u, y + dy * u, 0.78));
        let mut controls = if h > w {
            let band = placed().map(|a| (a[1] + a[3]) as f32).fold(0.0, f32::max);
            let row = |fraction: f32| band + (h - band) * fraction;
            let top = band + 1.1 * u;
            vec![
                round(L2, "L2", "L2", 1.3 * u, top, 0.75),
                round(L1, "L", "L", 3.2 * u, top, 0.75),
                round(R1, "R", "R", w - 3.2 * u, top, 0.75),
                round(R2, "R2", "R2", w - 1.3 * u, top, 0.75),
                flat(FN, "Menu", w / 2.0, top),
                other(Kind::Stick, "Stick", 2.6 * u, row(0.42), 1.7),
                other(Kind::Dpad, "Dpad", 2.6 * u, row(0.78), 1.45),
                flat(SELECT, "Select", w / 2.0 - 1.3 * u, h - 0.9 * u),
                flat(START, "Start", w / 2.0 + 1.3 * u, h - 0.9 * u),
            ]
            .into_iter()
            .chain(face(w - 2.7 * u, row(0.58)))
            .collect::<Vec<_>>()
        } else {
            // the middle of the room left and right of the screens, or near
            // the edge over them when there is little
            let (left, right) = (placed().map(|a| a[0] as f32).fold(w, f32::min), placed().map(|a| (a[0] + a[2]) as f32).fold(0.0, f32::max));
            let left = if left >= 5.0 * u { left / 2.0 } else { 2.5 * u };
            let right = if w - right >= 5.0 * u { (right + w) / 2.0 } else { w - 2.5 * u };
            vec![
                round(L2, "L2", "L2", 1.3 * u, 1.0 * u, 0.75),
                round(L1, "L", "L", 3.3 * u, 1.0 * u, 0.75),
                round(R1, "R", "R", w - 3.3 * u, 1.0 * u, 0.75),
                round(R2, "R2", "R2", w - 1.3 * u, 1.0 * u, 0.75),
                other(Kind::Stick, "Stick", left, 0.40 * h, 1.6),
                other(Kind::Dpad, "Dpad", left, 0.74 * h, 1.4),
                flat(FN, "Menu", left, h - 0.75 * u),
                flat(SELECT, "Select", right - 1.2 * u, h - 0.75 * u),
                flat(START, "Start", right + 1.2 * u, h - 0.75 * u),
            ]
            .into_iter()
            .chain(face(right, 0.48 * h))
            .collect()
        };
        for (group, &[x, y, scale]) in self.custom_for(size).iter() {
            let members: Vec<usize> = (0..controls.len()).filter(|&index| controls[index].group == group).collect();
            let [cx, cy] = middle(&controls, &members);
            for index in members {
                let control = &mut controls[index];
                control.at = [x * w + (control.at[0] - cx) * scale, y * h + (control.at[1] - cy) * scale];
                control.radius *= scale;
                control.tall *= scale;
            }
        }
        self.placed = controls;
    }

    /// what fingers at these places in the layout hold, by their ids: each
    /// keeps to what it came down on.
    pub fn hold(&mut self, font: &Font, size: (u32, u32), screens: [Option<[i32; 4]>; 2], fingers: &[(i32, f32, f32)]) -> Held {
        self.place(font, size, screens);
        self.grips.retain(|id, _| fingers.iter().any(|finger| finger.0 == *id));
        let mut held = Held { keys: HashSet::new(), stick: None, free: Vec::new() };
        let button = |control: &Control| matches!(control.kind, Kind::Round(..) | Kind::Flat(..));
        for &(id, x, y) in fingers {
            let grip = *self.grips.entry(id).or_insert_with(|| match nearest(&self.placed, (x, y), |_| true) {
                Some(index) if !button(&self.placed[index]) => Grip::Control(index),
                Some(_) => Grip::Buttons,
                None => Grip::Free,
            });
            let index = match grip {
                Grip::Free => {
                    held.free.push((x, y));
                    continue;
                }
                Grip::Buttons => match nearest(&self.placed, (x, y), button) {
                    Some(index) => index,
                    None => continue,
                },
                Grip::Control(index) => index,
            };
            let control = self.placed[index];
            let (dx, dy) = (x - control.at[0], y - control.at[1]);
            match control.kind {
                Kind::Round(key, _) | Kind::Flat(key, _) => {
                    held.keys.insert(key);
                }
                Kind::Dpad => {
                    // eight ways, the diagonals between
                    let distance = dx.hypot(dy);
                    if distance > control.radius * 0.15 {
                        if dx.abs() > distance * 0.38 {
                            held.keys.insert(if dx < 0.0 { LEFT } else { RIGHT });
                        }
                        if dy.abs() > distance * 0.38 {
                            held.keys.insert(if dy < 0.0 { UP } else { DOWN });
                        }
                    }
                }
                Kind::Stick => {
                    let (across, down) = (dx / control.radius, dy / control.radius);
                    let length = across.hypot(down).max(1.0);
                    held.stick = Some((across / length, down / length));
                }
            }
        }
        held
    }

    /// the controls drawn over the whole layout, the ones held lit, the
    /// same overlay again while nothing changed.
    pub fn overlay(&mut self, font: &Font, held: &Held) -> Arc<Overlay> {
        let mut keys: Vec<u16> = held.keys.iter().copied().collect();
        keys.sort_unstable();
        let stick = held.stick.map(|(x, y)| ((x * 8.0) as i8, (y * 8.0) as i8));
        if let Some((drawn_keys, drawn_stick, overlay)) = &self.drawn {
            if *drawn_keys == keys && *drawn_stick == stick {
                return overlay.clone();
            }
        }
        let mut canvas = Canvas::new(font, self.size());
        self.draw(&mut canvas, |control| match control.kind {
            Kind::Round(key, _) | Kind::Flat(key, _) => held.keys.contains(&key),
            Kind::Dpad => false,
            Kind::Stick => held.stick.is_some(),
        }, held);
        let overlay = finish(canvas);
        self.drawn = Some((keys, stick, overlay.clone()));
        overlay
    }

    fn size(&self) -> (u32, u32) {
        self.around.map_or((640, 480), |(size, _)| size)
    }

    fn draw(&self, canvas: &mut Canvas, lit: impl Fn(&Control) -> bool, held: &Held) {
        for control in &self.placed {
            let ([x, y], r) = (control.at, control.radius);
            let on = lit(control);
            match control.kind {
                Kind::Round(_, name) => {
                    canvas.disc([x, y], r, shade(on));
                    label(canvas, BIG, name, [x, y]);
                }
                Kind::Flat(_, name) => {
                    canvas.fill([x - r, y - control.tall, x + r, y + control.tall], shade(on));
                    label(canvas, SMALL, name, [x, y]);
                }
                Kind::Dpad => {
                    let arm = r * 0.36;
                    for (key, area) in [
                        (LEFT, [x - r, y - arm, x - arm, y + arm]),
                        (RIGHT, [x + arm, y - arm, x + r, y + arm]),
                        (UP, [x - arm, y - r, x + arm, y - arm]),
                        (DOWN, [x - arm, y + arm, x + arm, y + r]),
                    ] {
                        canvas.fill(area, shade(on || held.keys.contains(&key)));
                    }
                    canvas.fill([x - arm, y - arm, x + arm, y + arm], shade(on));
                }
                Kind::Stick => {
                    canvas.disc([x, y], r, rgba([255, 255, 255], 35));
                    let (across, down) = held.stick.unwrap_or_default();
                    canvas.disc([x + across * r * 0.55, y + down * r * 0.55], r * 0.5, shade(on));
                }
            }
        }
    }

    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn start_editing(&mut self) {
        self.editing = Some(Editing { group: None, drag: None, down: None });
    }

    /// the tools' middles and half widths, along the middle of the screens.
    fn tools(&self, font: &Font) -> Vec<([f32; 2], f32)> {
        let ((w, h), screens) = self.around.unwrap_or(((640, 480), [None, None]));
        let (w, h) = (w as f32, h as f32);
        let u = w.min(h) * 0.075;
        let y = if h > w { screens.iter().flatten().map(|a| (a[1] + a[3]) as f32).fold(0.0, f32::max) / 2.0 } else { h / 2.0 };
        let widths: Vec<f32> = TOOLS.iter().map(|tool| font.faces[BIG].width(tool) + 1.2 * u).collect();
        let mut x = (w - widths.iter().sum::<f32>() - 0.4 * u * 3.0) / 2.0;
        widths
            .iter()
            .map(|width| {
                let tool = ([x + width / 2.0, y], width / 2.0);
                x += width + 0.4 * u;
                tool
            })
            .collect()
    }

    /// a frame of editing with these fingers: one coming down on a control
    /// picks its group and drags it, on a tool uses it. True once Done, or
    /// when done anyway.
    pub fn edit(&mut self, font: &Font, size: (u32, u32), screens: [Option<[i32; 4]>; 2], fingers: &[(i32, f32, f32)], mut done: bool) -> bool {
        self.place(font, size, screens);
        let tools = self.tools(font);
        let Some(mut editing) = self.editing.take() else { return true };
        let down = editing.down.replace(fingers.iter().map(|finger| finger.0).collect());
        for &(id, x, y) in fingers.iter().filter(|finger| down.as_ref().is_some_and(|down| !down.contains(&finger.0))) {
            let tool = tools.iter().position(|([tx, ty], half)| (x - tx).abs() < *half && (y - ty).abs() < 0.8 * size.0.min(size.1) as f32 * 0.075);
            match tool {
                Some(0) | Some(1) => {
                    let by = if tool == Some(0) { 1.0 / 1.15 } else { 1.15 };
                    let groups: Vec<&str> = match editing.group {
                        Some(group) => vec![group],
                        None => self.placed.iter().map(|control| control.group).collect::<HashSet<_>>().into_iter().collect(),
                    };
                    for group in groups {
                        let [x, y] = self.group_middle(group);
                        let (w, h) = (size.0 as f32, size.1 as f32);
                        let entry = self.custom_for(size).entry(group.to_owned()).or_insert([x / w, y / h, 1.0]);
                        entry[2] = (entry[2] * by).clamp(0.5, 2.5);
                    }
                }
                Some(2) => {
                    self.custom_for(size).clear();
                    editing.group = None;
                }
                Some(_) => done = true,
                None => {
                    editing.group = nearest(&self.placed, (x, y), |_| true).map(|index| self.placed[index].group);
                    editing.drag = editing.group.map(|group| {
                        let [mx, my] = self.group_middle(group);
                        (id, [mx - x, my - y])
                    });
                }
            }
            self.around = None;
        }
        if let (Some((id, [dx, dy])), Some(group)) = (editing.drag, editing.group) {
            match fingers.iter().find(|finger| finger.0 == id) {
                Some(&(_, x, y)) => {
                    let (w, h) = (size.0 as f32, size.1 as f32);
                    let scale = self.custom_for(size).get(group).map_or(1.0, |entry| entry[2]);
                    self.custom_for(size).insert(group.to_owned(), [((x + dx) / w).clamp(0.0, 1.0), ((y + dy) / h).clamp(0.0, 1.0), scale]);
                    self.around = None;
                }
                None => editing.drag = None,
            }
        }
        self.place(font, size, screens);
        if done {
            crate::settings::save(&path(), &self.custom);
            self.drawn = None;
        } else {
            self.editing = Some(editing);
        }
        done
    }

    fn group_middle(&self, group: &str) -> [f32; 2] {
        let members: Vec<usize> = (0..self.placed.len()).filter(|&index| self.placed[index].group == group).collect();
        middle(&self.placed, &members)
    }

    /// the controls as they are edited, the group picked lit, the tools
    /// and a word on what to do over the screens, darkened.
    pub fn edit_overlay(&self, font: &Font) -> Arc<Overlay> {
        let (w, h) = self.size();
        let mut canvas = Canvas::new(font, (w, h));
        canvas.fill([0.0, 0.0, w as f32, h as f32], rgba([0, 0, 0], 120));
        let group = self.editing.as_ref().and_then(|editing| editing.group);
        let held = Held { keys: HashSet::new(), stick: None, free: Vec::new() };
        self.draw(&mut canvas, |control| Some(control.group) == group, &held);
        let tools = self.tools(font);
        for (([x, y], half), name) in tools.iter().zip(TOOLS) {
            canvas.fill([x - half, y - 0.6 * w.min(h) as f32 * 0.075, x + half, y + 0.6 * w.min(h) as f32 * 0.075], rgba([47, 111, 224], 230));
            label(&mut canvas, BIG, name, [*x, *y]);
        }
        let hint = if group.is_some() { "Drag it, or Smaller and Bigger. Tap empty space to pick none." } else { "Drag a control to move it. Smaller and Bigger size all, or the one picked." };
        let y = tools.first().map_or(h as f32 / 2.0, |tool| tool.0[1]) - 1.5 * w.min(h) as f32 * 0.075;
        label(&mut canvas, SMALL, hint, [w as f32 / 2.0, y]);
        finish(canvas)
    }
}

/// the middle of some controls.
fn middle(controls: &[Control], members: &[usize]) -> [f32; 2] {
    let count = members.len().max(1) as f32;
    let sum = members.iter().fold([0.0, 0.0], |[x, y], &index| [x + controls[index].at[0], y + controls[index].at[1]]);
    [sum[0] / count, sum[1] / count]
}

fn shade(on: bool) -> [u8; 4] {
    rgba([255, 255, 255], if on { 130 } else { 50 })
}

/// text centred on a point.
fn label(canvas: &mut Canvas, face: usize, name: &str, [x, y]: [f32; 2]) {
    let width = canvas.measure(face, name);
    let top = y - canvas.ascent(face) * 0.6;
    canvas.text(face, name, x - width / 2.0, top, rgba([255, 255, 255], 220), false);
}

fn finish(canvas: Canvas) -> Arc<Overlay> {
    let mut overlay = canvas.finish();
    overlay.whole = true;
    Arc::new(overlay)
}
