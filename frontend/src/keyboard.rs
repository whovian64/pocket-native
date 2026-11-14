//! the software keyboard a title opens for a name or other text, over the
//! bottom screen as the console's own: the game waits under it, the d-pad
//! moves over the keys, A types, B deletes, X changes the keys, Y types a
//! space, Start presses the game's confirming button and Select its first,
//! and a finger types on the key under it.

use std::collections::HashSet;

use zakuro_core::services::keyboard::Request;

use crate::font::{rgba, Canvas};
use crate::input::{A, B, DOWN, LEFT, RIGHT, SELECT, START, UP, X, Y};

const PANEL: [u8; 3] = [13, 20, 32];
const EDGE: [u8; 3] = [40, 110, 210];
const KEY: [u8; 3] = [24, 36, 56];
const HIGHLIGHT: [u8; 3] = [24, 64, 124];
const TEXT: [u8; 3] = [230, 238, 248];
const DIM: [u8; 3] = [130, 152, 182];
const WARNING: [u8; 3] = [255, 196, 90];
const BIG: usize = 0;
const SMALL: usize = 1;

/// three pages of ten keys in four rows, the menu's font has Latin-1.
const PAGES: [[&str; 4]; 3] = [
    ["1234567890", "qwertyuiop", "asdfghjkl'", "zxcvbnm,.-"],
    ["!?@#&()+=/", "QWERTYUIOP", "ASDFGHJKL\"", "ZXCVBNM:;_"],
    ["àáâäãåæçèé", "êëìíîïñòóô", "õöùúûüýÿßø", "ÀÉÑÖÜÇ¡¿·~"],
];
const PAGE_NAMES: [&str; 3] = ["ABC", "àéñ", "abc"];
/// where the keys start, their size and the room between them.
const TOP_EDGE: f32 = 118.0;
const WIDE: f32 = 58.0;
const TALL: f32 = 54.0;
const GAP: f32 = 4.0;
/// the last row in key widths, as wide as ten keys.
const ACROSS: f32 = 10.0;

/// the keys of the last row, after the letters.
#[derive(Clone, Copy, PartialEq)]
enum Item {
    Page,
    Space,
    Delete,
    Button(usize),
}

pub enum Outcome {
    None,
    Redraw,
    /// the text and the button pressed, zero the leftmost.
    Answer(String, usize),
}

pub struct Keyboard {
    request: Request,
    text: String,
    page: usize,
    row: usize,
    column: usize,
    last: HashSet<u16>,
    held: u32,
    touching: bool,
    note: Option<String>,
}

impl Keyboard {
    /// the keys and finger still down from opening it count as held, not
    /// pressed.
    pub fn new(request: Request, keys: &HashSet<u16>, touching: bool) -> Keyboard {
        let text = request.text.clone();
        let row = if request.numpad.is_some() { 0 } else { 1 };
        Keyboard { request, text, page: 0, row, column: 0, last: keys.clone(), held: 0, touching, note: None }
    }

    /// the rows of keys showing, a number pad's or the page's letters.
    fn rows(&self) -> Vec<Vec<char>> {
        match self.request.numpad {
            Some([left, right]) => {
                let mut rows: Vec<Vec<char>> = ["123", "456", "789"].iter().map(|row| row.chars().collect()).collect();
                rows.push(left.into_iter().chain(['0']).chain(right).collect());
                rows
            }
            None => PAGES[self.page].iter().map(|row| row.chars().collect()).collect(),
        }
    }

    /// the last row's keys, their first column and how many they span.
    fn items(&self) -> Vec<(Item, f32, f32)> {
        let buttons = self.request.buttons.len().max(1);
        let (mut items, from) = match self.request.numpad {
            Some(_) => (vec![(Item::Delete, 0.0, 4.0)], 4.0),
            None => (vec![(Item::Page, 0.0, 1.5), (Item::Space, 1.5, 3.0), (Item::Delete, 4.5, 1.5)], 6.0),
        };
        let each = (ACROSS - from) / buttons as f32;
        items.extend((0..buttons).map(|button| (Item::Button(button), from + button as f32 * each, each)));
        items
    }

    /// a key's area on the bottom screen, in a row of count key widths
    /// centred, from its first column on, spanning span.
    fn area(row: usize, first: f32, span: f32, count: f32) -> [f32; 4] {
        let left = (640.0 - (count * (WIDE + GAP) - GAP)) / 2.0 + first * (WIDE + GAP);
        let top = TOP_EDGE + row as f32 * (TALL + GAP);
        [left, top, left + span * (WIDE + GAP) - GAP, top + TALL]
    }

    /// every key's area, row by row, the last row's after the letters.
    fn areas(&self) -> Vec<Vec<[f32; 4]>> {
        let mut areas: Vec<Vec<[f32; 4]>> = self
            .rows()
            .iter()
            .enumerate()
            .map(|(row, keys)| (0..keys.len()).map(|column| Self::area(row, column as f32, 1.0, keys.len() as f32)).collect())
            .collect();
        let last = areas.len();
        areas.push(self.items().iter().map(|&(_, first, span)| Self::area(last, first, span, ACROSS)).collect());
        areas
    }

    fn type_char(&mut self, c: char) -> Outcome {
        if self.text.encode_utf16().count() + c.len_utf16() > self.request.max_length {
            self.note = Some(format!("{} characters at most", self.request.max_length));
        } else {
            self.text.push(c);
            self.note = None;
        }
        Outcome::Redraw
    }

    fn press(&mut self, item: Item) -> Outcome {
        match item {
            Item::Page => {
                self.page = (self.page + 1) % PAGES.len();
                Outcome::Redraw
            }
            Item::Space => self.type_char(' '),
            Item::Delete => {
                self.text.pop();
                self.note = None;
                Outcome::Redraw
            }
            Item::Button(button) => self.button(button),
        }
    }

    /// the game's buttons: the last one confirms, when the text is one the
    /// game takes, the others answer as they are.
    fn button(&mut self, button: usize) -> Outcome {
        let last = self.request.buttons.len().saturating_sub(1);
        if button == last {
            if let Some(refusal) = self.request.refusal(&self.text) {
                self.note = Some(refusal);
                return Outcome::Redraw;
            }
        }
        Outcome::Answer(self.text.clone(), button)
    }

    /// the key under a point on the bottom screen.
    fn hit(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let inside = |&[left, top, right, bottom]: &[f32; 4]| (left..right).contains(&x) && (top..bottom).contains(&y);
        self.areas().iter().enumerate().find_map(|(row, keys)| keys.iter().position(inside).map(|column| (row, column)))
    }

    fn activate(&mut self) -> Outcome {
        let rows = self.rows();
        match rows.get(self.row) {
            Some(keys) => self.type_char(keys[self.column.min(keys.len() - 1)]),
            None => {
                let items = self.items();
                self.press(items[self.column.min(items.len() - 1)].0)
            }
        }
    }

    /// the keys held this frame, d-pad presses repeating while held, and a
    /// finger on the bottom screen, in its pixels.
    pub fn input(&mut self, keys: &HashSet<u16>, finger: Option<(f32, f32)>) -> Outcome {
        let direction = [UP, DOWN, LEFT, RIGHT].into_iter().find(|key| keys.contains(key));
        self.held = if direction.is_some_and(|key| self.last.contains(&key)) { self.held + 1 } else { 0 };
        let repeat = self.held == 0 || (self.held >= 18 && self.held.is_multiple_of(4));
        let pressed = |key: u16| keys.contains(&key) && !self.last.contains(&key);
        let (a, b, x, y, start, select) = (pressed(A), pressed(B), pressed(X), pressed(Y), pressed(START), pressed(SELECT));
        self.last = keys.clone();
        let touched = finger.is_some() && !self.touching;
        self.touching = finger.is_some();
        if let (true, Some((x, y))) = (touched, finger) {
            if let Some((row, column)) = self.hit(x, y) {
                (self.row, self.column) = (row, column);
                return self.activate();
            }
        }
        let areas = self.areas();
        let centre = |row: usize, column: usize| {
            let [left, _, right, _] = areas[row][column.min(areas[row].len() - 1)];
            (left + right) / 2.0
        };
        match direction.filter(|_| repeat) {
            Some(LEFT) | Some(RIGHT) => {
                let count = areas[self.row].len();
                let by = if direction == Some(LEFT) { count - 1 } else { 1 };
                self.column = (self.column + by) % count;
                return Outcome::Redraw;
            }
            Some(UP) | Some(DOWN) => {
                // to the key in the next row up or down nearest across
                let to = if direction == Some(UP) { (self.row + areas.len() - 1) % areas.len() } else { (self.row + 1) % areas.len() };
                let across = centre(self.row, self.column);
                self.column = (0..areas[to].len()).min_by(|&one, &other| (centre(to, one) - across).abs().total_cmp(&(centre(to, other) - across).abs())).unwrap_or(0);
                self.row = to;
                return Outcome::Redraw;
            }
            _ => {}
        }
        let letters = self.request.numpad.is_none();
        if a {
            return self.activate();
        }
        if b {
            return self.press(Item::Delete);
        }
        if x && letters {
            return self.press(Item::Page);
        }
        if y && letters {
            return self.press(Item::Space);
        }
        if start {
            return self.button(self.request.buttons.len().saturating_sub(1));
        }
        if select && self.request.buttons.len() > 1 {
            return self.button(0);
        }
        Outcome::None
    }

    /// over the whole bottom screen, as the console's own keyboard.
    pub fn draw(&self, canvas: &mut Canvas) {
        let (x0, y0, x1, y1) = (0.0, 0.0, 640.0, 480.0);
        canvas.fill([x0, y0, x1, y1], rgba(PANEL, 255));
        canvas.fill([x0, y0, x1, y0 + 2.0], rgba(EDGE, 255));
        let hint = if self.request.hint.is_empty() { "Type here" } else { &self.request.hint };
        canvas.text(BIG, hint, x0 + 14.0, y0 + 10.0, rgba(DIM, 255), false);
        // the text so far, hidden for a password, its end showing when it
        // is longer than the box
        let field = [x0 + 12.0, y0 + 44.0, x1 - 12.0, y0 + 88.0];
        canvas.fill(field, rgba(KEY, 255));
        let room = field[2] - field[0] - 90.0;
        let mut shown: String = if self.request.password { "*".repeat(self.text.chars().count()) } else { self.text.clone() };
        while !shown.is_empty() && canvas.measure(BIG, &shown) > room {
            shown.remove(0);
        }
        canvas.text(BIG, &format!("{shown}_"), field[0] + 10.0, field[1] + 11.0, rgba(TEXT, 255), false);
        let count = format!("{}/{}", self.text.encode_utf16().count(), self.request.max_length);
        canvas.text(SMALL, &count, field[2] - 10.0, field[1] + 14.0, rgba(DIM, 255), true);
        if let Some(note) = &self.note {
            canvas.text(SMALL, note, x0 + 14.0, y0 + 94.0, rgba(WARNING, 255), false);
        }
        let (rows, items, areas) = (self.rows(), self.items(), self.areas());
        let confirm = self.request.buttons.len().saturating_sub(1);
        for (row, keys) in areas.iter().enumerate() {
            for (column, &area) in keys.iter().enumerate() {
                let selected = (self.row, self.column) == (row, column);
                canvas.fill(area, rgba(if selected { HIGHLIGHT } else { KEY }, 255));
                let (label, color) = match rows.get(row) {
                    Some(keys) => (keys[column].to_string(), TEXT),
                    None => match items[column].0 {
                        Item::Page => (PAGE_NAMES[self.page].to_owned(), TEXT),
                        Item::Space => ("Space".to_owned(), TEXT),
                        Item::Delete => ("Delete".to_owned(), TEXT),
                        Item::Button(button) => {
                            let ready = button != confirm || self.request.accepts(&self.text);
                            (self.request.buttons.get(button).cloned().unwrap_or_default(), if ready { TEXT } else { DIM })
                        }
                    },
                };
                let face = if canvas.measure(BIG, &label) <= area[2] - area[0] - 8.0 { BIG } else { SMALL };
                canvas.text(face, &label, (area[0] + area[2] - canvas.measure(face, &label)) / 2.0, area[1] + if face == BIG { 16.0 } else { 19.0 }, rgba(color, 255), false);
            }
        }
        canvas.fill([x0 + 10.0, y1 - 36.0, x1 - 10.0, y1 - 35.0], rgba([32, 48, 72], 255));
        let help = if self.request.numpad.is_some() { "A  type    B  delete" } else { "A  type    B  delete    X  keys    Y  space" };
        canvas.text(SMALL, help, x0 + 14.0, y1 - 28.0, rgba(DIM, 255), false);
        let start = self.request.buttons.last().map_or("OK", String::as_str);
        canvas.text(SMALL, &format!("Start  {start}"), x1 - 14.0, y1 - 28.0, rgba(DIM, 255), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::Font;
    use zakuro_core::services::keyboard::Valid;

    fn request(max_length: usize, buttons: &[&str], valid: Valid) -> Request {
        Request { hint: "Name".into(), max_length, buttons: buttons.iter().map(|&label| label.into()).collect(), valid, ..Request::default() }
    }

    fn press(keyboard: &mut Keyboard, key: u16) -> Outcome {
        keyboard.input(&HashSet::new(), None);
        keyboard.input(&HashSet::from([key]), None)
    }

    #[test]
    fn every_key_has_a_glyph() {
        let font = Font::load();
        let canvas = Canvas::new(&font, (640, 480));
        let missing: String = PAGES.iter().flatten().flat_map(|row| row.chars()).filter(|c| canvas.measure(BIG, &c.to_string()) == 0.0).collect();
        assert!(missing.is_empty(), "{missing}");
    }

    #[test]
    fn a_held_a_from_opening_it_types_nothing() {
        let mut keyboard = Keyboard::new(request(10, &["Cancel", "OK"], Valid::Anything), &HashSet::from([A]), true);
        keyboard.input(&HashSet::from([A]), Some((60.0, 160.0)));
        assert_eq!(keyboard.text, "");
        press(&mut keyboard, A);
        assert_eq!(keyboard.text, "q");
    }

    #[test]
    fn it_stops_at_the_longest_text() {
        let mut keyboard = Keyboard::new(request(2, &["OK"], Valid::Anything), &HashSet::new(), false);
        for _ in 0..3 {
            press(&mut keyboard, Y);
        }
        assert_eq!(keyboard.text, "  ");
        assert!(keyboard.note.is_some());
    }

    #[test]
    fn the_confirming_button_waits_for_text_the_game_takes() {
        let mut keyboard = Keyboard::new(request(10, &["Cancel", "OK"], Valid::NotEmptyNotBlank), &HashSet::new(), false);
        assert!(matches!(press(&mut keyboard, START), Outcome::Redraw));
        assert_eq!(keyboard.note.as_deref(), Some("It needs some text"));
        press(&mut keyboard, A);
        assert!(matches!(press(&mut keyboard, START), Outcome::Answer(text, 1) if text == "q"));
        assert!(matches!(press(&mut keyboard, SELECT), Outcome::Answer(_, 0)));
    }

    #[test]
    fn the_dpad_walks_into_the_last_row_and_out() {
        let mut keyboard = Keyboard::new(request(10, &["Cancel", "OK"], Valid::Anything), &HashSet::new(), false);
        for _ in 0..3 {
            press(&mut keyboard, RIGHT);
        }
        for _ in 0..3 {
            press(&mut keyboard, DOWN);
        }
        // the space bar is under the fourth key
        assert_eq!((keyboard.row, keyboard.column), (4, 1));
        press(&mut keyboard, A);
        assert_eq!(keyboard.text, " ");
        press(&mut keyboard, UP);
        assert_eq!(keyboard.row, 3);
        press(&mut keyboard, A);
        assert_eq!(keyboard.text, " c");
    }

    #[test]
    fn a_finger_types_the_key_under_it() {
        let mut keyboard = Keyboard::new(request(10, &["OK"], Valid::Anything), &HashSet::new(), false);
        let [left, top, right, bottom] = Keyboard::area(2, 1.0, 1.0, 10.0);
        keyboard.input(&HashSet::new(), Some(((left + right) / 2.0, (top + bottom) / 2.0)));
        keyboard.input(&HashSet::new(), Some(((left + right) / 2.0, (top + bottom) / 2.0)));
        assert_eq!(keyboard.text, "s");
        let [left, top, ..] = Keyboard::area(4, 6.0, 4.0, 10.0);
        keyboard.input(&HashSet::new(), None);
        assert!(matches!(keyboard.input(&HashSet::new(), Some((left + 5.0, top + 5.0))), Outcome::Answer(text, 0) if text == "s"));
    }

    /// a number pad types digits and the keys beside its 0, and nothing
    /// else, the console keeping to them too.
    #[test]
    fn a_number_pad_types_only_its_keys() {
        let request = Request { numpad: Some([Some('-'), None]), ..request(4, &["Cancel", "OK"], Valid::Anything) };
        let mut keyboard = Keyboard::new(request, &HashSet::new(), false);
        press(&mut keyboard, A);
        press(&mut keyboard, X);
        press(&mut keyboard, Y);
        for _ in 0..3 {
            press(&mut keyboard, DOWN);
        }
        press(&mut keyboard, A);
        assert_eq!(keyboard.text, "1-");
        assert_eq!(keyboard.areas().len(), 5);
        assert!(keyboard.areas().iter().flatten().all(|&[left, top, right, bottom]| left >= 0.0 && right <= 640.0 && top >= 0.0 && bottom <= 480.0));
    }
}
