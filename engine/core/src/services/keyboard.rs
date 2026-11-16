//! the software keyboard, the library applet a title starts to have a name
//! or some other text typed in. the title waits while it is open, the
//! frontend shows it and answers with the text and the button pressed.

use crate::kernel::object::{KObject, ObjectId};
use crate::services::apt::{self, Parameter};
use crate::System;

pub const APPLET_ID: u32 = 0x401;

// where the fields the keyboard reads and writes sit in its configuration
const KIND: usize = 0x00;
const BUTTON_COUNT: usize = 0x04;
const VALID_INPUT: usize = 0x08;
const PASSWORD: usize = 0x0C;
const FILTERS: usize = 0x18;
const MAX_TEXT_LENGTH: usize = 0x20;
const MAX_DIGITS: usize = 0x24;
/// three labels of up to 16 UTF-16 units, each with its terminating zero.
const BUTTON_TEXT: usize = 0x26;
const BUTTON_TEXT_UNITS: usize = 17;
/// the number pad's two keys beside the 0.
const NUMPAD_KEYS: usize = 0x8C;
const HINT_TEXT: usize = 0x90;
const HINT_TEXT_UNITS: usize = 65;
// eight flags, which buttons submit text and the language come between
const INITIAL_TEXT_OFFSET: usize = 0x120;
const RESULT: usize = 0x138;
const TEXT_OFFSET: usize = 0x144;
const TEXT_LENGTH: usize = 0x148;
/// the configuration is at least this long to hold the fields above.
const CONFIG_SIZE: usize = 0x14C;

/// what a title asks the keyboard for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Request {
    pub hint: String,
    /// the text the keyboard starts with.
    pub text: String,
    /// the longest text it takes, in UTF-16 units.
    pub max_length: usize,
    /// the buttons' labels, left to right, the last one confirms.
    pub buttons: Vec<String>,
    /// what the text has to be for the confirming button to take it.
    pub valid: Valid,
    /// a number pad, with the keys beside its 0, rather than letters.
    pub numpad: Option<[Option<char>; 2]>,
    /// the text hidden as it is typed.
    pub password: bool,
    /// characters the title's filters turn down.
    pub banned: Vec<char>,
    /// the most digits the title's filter lets through.
    pub max_digits: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Valid {
    #[default]
    Anything,
    NotEmpty,
    NotEmptyNotBlank,
    NotBlank,
    /// exactly the longest length.
    FixedLength,
}

impl Valid {
    fn from_raw(value: u32) -> Valid {
        match value {
            1 => Valid::NotEmpty,
            2 => Valid::NotEmptyNotBlank,
            3 => Valid::NotBlank,
            4 => Valid::FixedLength,
            _ => Valid::Anything,
        }
    }
}

impl Request {
    /// whether the confirming button takes text.
    pub fn accepts(&self, text: &str) -> bool {
        self.refusal(text).is_none()
    }

    /// why the confirming button turns text down, as the console's keyboard
    /// would.
    pub fn refusal(&self, text: &str) -> Option<String> {
        let length = text.encode_utf16().count();
        let blank = !text.is_empty() && text.chars().all(char::is_whitespace);
        if length > self.max_length {
            return Some(format!("{} characters at most", self.max_length));
        }
        if let Some(c) = text.chars().find(|c| self.banned.contains(c)) {
            return Some(format!("It can't have {c}"));
        }
        let digits = text.chars().filter(char::is_ascii_digit).count();
        if let Some(most) = self.max_digits.filter(|&most| digits > most) {
            return Some(format!("{most} numbers at most"));
        }
        match self.valid {
            Valid::NotEmpty | Valid::NotEmptyNotBlank if length == 0 => Some("It needs some text".into()),
            Valid::NotEmptyNotBlank | Valid::NotBlank if blank => Some("It can't be only spaces".into()),
            Valid::FixedLength if length != self.max_length => Some(format!("It needs {} characters", self.max_length)),
            _ => None,
        }
    }
}

/// a keyboard a title started and waits on.
pub struct Pending {
    pub request: Request,
    config: Vec<u8>,
    /// the block the text goes back in.
    memory: Option<ObjectId>,
}

/// opens the keyboard with the configuration a title sent and the block
/// it shares for the text.
pub fn start(system: &mut System, mut config: Vec<u8>, memory: Option<ObjectId>) {
    config.resize(config.len().max(CONFIG_SIZE), 0);
    let word = |at: usize| u32::from_le_bytes(config[at..at + 4].try_into().unwrap());
    let buttons = word(BUTTON_COUNT).min(2) as usize + 1;
    let defaults: &[&str] = match buttons {
        1 => &["OK"],
        2 => &["Cancel", "OK"],
        _ => &["Cancel", "I Forgot", "OK"],
    };
    let label = |slot: usize| utf16(&config[BUTTON_TEXT + slot * BUTTON_TEXT_UNITS * 2..], BUTTON_TEXT_UNITS);
    // two buttons can be labeled in the left and right slots, the middle
    // one left empty
    let slots: &[usize] = match buttons {
        2 if label(1).is_empty() => &[0, 2],
        _ => &[0, 1, 2],
    };
    let labels = (0..buttons)
        .map(|i| {
            let text = label(slots[i]);
            if text.is_empty() { defaults[i].to_owned() } else { text }
        })
        .collect();
    let block = block(system, memory);
    let half = |at: usize| u16::from_le_bytes([config[at], config[at + 1]]);
    // as much as the block holds at most, and when there is no limit
    let room = block.map_or(64, |(_, size)| (size as usize / 2).saturating_sub(1));
    let max_length = match half(MAX_TEXT_LENGTH) as usize {
        0 => room,
        length => length.min(room),
    };
    let filters = word(FILTERS);
    let initial = word(INITIAL_TEXT_OFFSET);
    let text = match block {
        Some((paddr, size)) if initial < size => {
            let units = ((size - initial) as usize / 2).min(max_length + 1);
            let mut bytes = vec![0; units * 2];
            if let Some(slice) = system.memory.phys.host_slice_mut(paddr + initial, bytes.len() as u32) {
                bytes.copy_from_slice(slice);
            }
            utf16(&bytes, units)
        }
        _ => String::new(),
    };
    let request = Request {
        hint: utf16(&config[HINT_TEXT..], HINT_TEXT_UNITS),
        text,
        max_length,
        buttons: labels,
        valid: Valid::from_raw(word(VALID_INPUT)),
        numpad: (word(KIND) == 2).then(|| [NUMPAD_KEYS, NUMPAD_KEYS + 2].map(|at| char::from_u32(half(at) as u32).filter(|&c| c != '\0'))),
        password: word(PASSWORD) != 0,
        banned: [(1, '@'), (2, '%'), (3, '\\')].into_iter().filter(|&(bit, _)| filters >> bit & 1 != 0).map(|(_, c)| c).collect(),
        max_digits: (filters & 1 != 0).then(|| half(MAX_DIGITS) as usize),
    };
    log::info!("keyboard: a title asks for text, {request:?}");
    system.services.apt.keyboard = Some(Pending { request, config, memory });
}

/// what the keyboard a title waits on asks for.
pub fn request(system: &System) -> Option<&Request> {
    system.services.apt.keyboard.as_ref().map(|pending| &pending.request)
}

/// closes the keyboard with the text typed and the button pressed, zero
/// being the leftmost. the text goes at the start of the shared block.
pub fn answer(system: &mut System, text: &str, button: usize) {
    let Some(pending) = system.services.apt.keyboard.take() else { return };
    let mut config = pending.config;
    let buttons = pending.request.buttons.len();
    let button = button.min(buttons - 1);
    let mut units: Vec<u16> = text.encode_utf16().take(pending.request.max_length).collect();
    if let Some((paddr, size)) = block(system, pending.memory) {
        units.truncate((size as usize / 2).saturating_sub(1));
        let bytes: Vec<u8> = units.iter().chain([&0]).flat_map(|unit| unit.to_le_bytes()).collect();
        system.memory.write_physical(paddr, &bytes);
    }
    // one button clicks as 0, two as 1 and 2, three as 3 to 5
    let result = match buttons {
        1 => 0,
        2 => 1 + button as u32,
        _ => 3 + button as u32,
    };
    config[RESULT..RESULT + 4].copy_from_slice(&result.to_le_bytes());
    config[TEXT_OFFSET..TEXT_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
    config[TEXT_LENGTH..TEXT_LENGTH + 2].copy_from_slice(&(units.len() as u16).to_le_bytes());
    log::info!("keyboard: closed with {text:?} on button {button}");
    apt::send_parameter(
        system,
        Parameter {
            sender: APPLET_ID,
            destination: apt::APPLICATION,
            signal: apt::SIGNAL_WAKEUP_BY_EXIT,
            buffer: config,
            object: None,
        },
    );
}

/// where the shared block is and how big.
fn block(system: &System, memory: Option<ObjectId>) -> Option<(u32, u32)> {
    match system.kernel.objects.get(memory?)? {
        KObject::SharedMemory(block) => Some((block.paddr, block.size)),
        _ => None,
    }
}

/// UTF-16 text of at most units units, up to its terminating zero.
fn utf16(bytes: &[u8], units: usize) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .take(units)
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_confirming_button_takes_only_valid_text() {
        let request = Request {
            hint: String::new(),
            text: String::new(),
            max_length: 8,
            buttons: vec!["Cancel".to_owned(), "OK".to_owned()],
            valid: Valid::NotEmptyNotBlank,
            ..Request::default()
        };
        assert!(request.accepts("Link"));
        assert!(!request.accepts(""));
        assert!(!request.accepts("   "));
        assert!(!request.accepts("Linkkkkkk"));
    }

    /// a number pad's keys, a hidden text and the title's filters come from
    /// the configuration, and the filters turn text down.
    #[test]
    fn the_configuration_says_the_keys_and_filters() {
        let mut system = System::new(crate::Config::default());
        let mut config = vec![0u8; 0x400];
        config[KIND] = 2;
        config[PASSWORD] = 1;
        config[FILTERS] = 1 | 2;
        config[MAX_TEXT_LENGTH] = 8;
        config[MAX_DIGITS] = 3;
        config[NUMPAD_KEYS] = b'-';
        start(&mut system, config, None);
        let asked = request(&system).unwrap().clone();
        assert_eq!(asked.numpad, Some([Some('-'), None]));
        assert!(asked.password);
        assert_eq!((asked.banned.as_slice(), asked.max_digits), (&['@'][..], Some(3)));
        assert!(asked.accepts("12-3"));
        assert_eq!(asked.refusal("1234").as_deref(), Some("3 numbers at most"));
        assert_eq!(asked.refusal("a@b").as_deref(), Some("It can't have @"));
    }

    /// the text and the button go back where titles look for them, in a
    /// configuration laid out the way the SDK lays it out.
    #[test]
    fn the_answer_goes_where_titles_read_it() {
        use crate::kernel::object::SharedMemory;
        let mut system = System::new(crate::Config::default());
        let block = system.memory.phys.allocate(crate::memory::MemoryRegion::Base, 0x1000).unwrap();
        let memory = system.kernel.objects.insert(KObject::SharedMemory(SharedMemory {
            name: "keyboard".into(),
            address: 0,
            size: 0x1000,
            paddr: block.addr,
            mapped_at: None,
        }));
        system.memory.write_physical(block.addr, &[0x4C, 0, 0x69, 0, 0, 0]);
        let mut config = vec![0u8; 0x400];
        config[BUTTON_COUNT] = 1;
        config[VALID_INPUT] = 2;
        config[MAX_TEXT_LENGTH] = 8;
        // no result yet, and the shared block's size and version after the
        // initial text's offset
        config[RESULT..RESULT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        config[0x130..0x134].copy_from_slice(&0x1000u32.to_le_bytes());

        start(&mut system, config, Some(memory));
        let asked = request(&system).unwrap();
        assert_eq!((asked.text.as_str(), asked.max_length), ("Li", 8));
        assert_eq!(asked.buttons, ["Cancel", "OK"]);

        answer(&mut system, "Tatl", 1);
        let parameter = system.services.apt.parameter.clone().unwrap();
        let config = parameter.buffer;
        assert_eq!(parameter.signal, apt::SIGNAL_WAKEUP_BY_EXIT);
        assert_eq!(config[RESULT..RESULT + 4], 2u32.to_le_bytes());
        assert_eq!(config[TEXT_OFFSET..TEXT_OFFSET + 4], [0; 4]);
        assert_eq!(config[TEXT_LENGTH..TEXT_LENGTH + 2], 4u16.to_le_bytes());
        assert_eq!(config[0x130..0x134], 0x1000u32.to_le_bytes());
        let mut text = [0u8; 10];
        text.copy_from_slice(system.memory.phys.host_slice_mut(block.addr, 10).unwrap());
        assert_eq!(utf16(&text, 5), "Tatl");
        assert!(request(&system).is_none());
    }

    #[test]
    fn labels_read_up_to_their_end() {
        let bytes: Vec<u8> = "OK".encode_utf16().chain([0, 0x41]).flat_map(u16::to_le_bytes).collect();
        assert_eq!(utf16(&bytes, 17), "OK");
    }
}
