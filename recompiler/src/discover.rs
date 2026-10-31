//! finding the code in a program. functions are found from the entry points
//! by following calls and branches, and from words elsewhere that point into
//! the code, which is where vtables and callbacks live.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::arm::{self, Flow, Instruction};
use crate::image::Segment;
use crate::thumb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Entry,
    Call,
    /// a code address stored in a literal pool or in the data segments.
    Pointer,
    /// a symbol the program exports to other modules.
    Export,
    /// a code address a module's relocations store, in its vtables, jump
    /// tables and literal pools.
    Relocation,
    /// a code address another module takes from this one without a name.
    Import,
    /// the start of a gap nothing led to, or a push in one, which is how
    /// functions reached only through tables nobody can see begin.
    Scan,
    /// an address a function written by hand replaces, which its author
    /// says is code.
    Override,
    /// an address Zakuro had to interpret with the title's last library,
    /// code nothing else led to.
    Hint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    Arm,
    Thumb,
}

impl Mode {
    fn other(self) -> Mode {
        match self {
            Mode::Arm => Mode::Thumb,
            Mode::Thumb => Mode::Arm,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    pub source: Source,
    pub mode: Mode,
    /// the address of every instruction, in order.
    pub instructions: Vec<u32>,
    /// where execution can enter the function's code, its entry, the
    /// targets of its branches and the instructions after calls and svc.
    pub labels: BTreeSet<u32>,
}

/// what each byte of the text segment turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Byte {
    Unknown,
    Code,
    Literal,
}

pub struct Analysis {
    pub functions: BTreeMap<u32, Function>,
    pub map: Vec<Byte>,
    pub indirect_sites: usize,
    pub jump_tables: usize,
    pub svc_sites: usize,
    /// paths abandoned because they ran into something that cannot be code.
    pub dead_ends: usize,
}

impl Analysis {
    pub fn count(&self, kind: Byte) -> usize {
        self.map.iter().filter(|&&byte| byte == kind).count()
    }

    /// the longest runs of text nobody reached, as (start, length).
    pub fn largest_gaps(&self, base: u32, count: usize) -> Vec<(u32, usize)> {
        let mut gaps = Vec::new();
        let mut start = None;
        for (offset, &byte) in self.map.iter().chain(std::iter::once(&Byte::Code)).enumerate() {
            match (byte == Byte::Unknown, start) {
                (true, None) => start = Some(offset),
                (false, Some(s)) => {
                    gaps.push((base + s as u32, offset - s));
                    start = None;
                }
                _ => {}
            }
        }
        gaps.sort_by_key(|gap| std::cmp::Reverse(gap.1));
        gaps.truncate(count);
        gaps
    }
}

/// a piece of code to analyze and what is known about it up front.
pub struct Program {
    pub text: Segment,
    /// addresses that may start functions and how each is known, odd for
    /// Thumb.
    pub seeds: Vec<(u32, Source)>,
    /// the words in the code that hold addresses, when relocations say
    /// exactly which ones they are. without them any literal that lands in
    /// the code is taken for a pointer.
    pub slots: Option<BTreeSet<u32>>,
}

struct Discovery<'a> {
    text: &'a Segment,
    slots: Option<&'a BTreeSet<u32>>,
    analysis: Analysis,
    queue: VecDeque<(u32, Mode, Source)>,
    /// pointers found in data, followed only once nothing surer is left.
    guesses: VecDeque<(u32, Mode, Source)>,
    /// the push of a function the scan started a few instructions before it,
    /// a way in all the same.
    pushes: BTreeMap<u32, u32>,
    /// literals only Thumb code loads, which ARM code running into them
    /// goes on through when they read as its instructions: Thumb read out
    /// of ARM code by mistake loads them from the middle of it.
    thumb_literals: BTreeSet<u32>,
}

pub fn analyze(program: &Program) -> Analysis {
    let mut discovery = Discovery {
        text: &program.text,
        slots: program.slots.as_ref(),
        analysis: Analysis {
            functions: BTreeMap::new(),
            map: vec![Byte::Unknown; program.text.bytes.len()],
            indirect_sites: 0,
            jump_tables: 0,
            svc_sites: 0,
            dead_ends: 0,
        },
        queue: VecDeque::new(),
        guesses: VecDeque::new(),
        pushes: BTreeMap::new(),
        thumb_literals: BTreeSet::new(),
    };
    // where relocations say the code holds data, it cannot be code
    for &slot in program.slots.iter().flatten() {
        discovery.mark(slot, 4, Byte::Literal);
    }
    for &(address, source) in &program.seeds {
        discovery.code_pointer(address, source);
    }
    discovery.run();
    // once nothing surer is left, the gaps get looked at for how functions
    // begin, each round of them can lead to more
    loop {
        let starts = discovery.gap_starts();
        if starts.is_empty() {
            break;
        }
        discovery.queue.extend(starts.into_iter().map(|(address, mode)| (address, mode, Source::Scan)));
        discovery.run();
    }
    // a function explored before another's start was known took that one in
    // as its own code, and whatever it branched on to: explored again, a
    // branch to it is a tail call, every game's code a tenth to a quarter
    // smaller
    let mut redone = BTreeSet::new();
    loop {
        let functions = &discovery.analysis.functions;
        let absorbing: Vec<(u32, Mode)> = functions
            .iter()
            .filter(|&(entry, function)| {
                !redone.contains(entry) && function.labels.iter().any(|label| label != entry && functions.get(label).is_some_and(|other| other.mode == function.mode))
            })
            .map(|(&entry, function)| (entry, function.mode))
            .collect();
        if absorbing.is_empty() {
            break;
        }
        let counts = (discovery.analysis.indirect_sites, discovery.analysis.jump_tables, discovery.analysis.svc_sites, discovery.analysis.dead_ends);
        for (entry, mode) in absorbing {
            redone.insert(entry);
            let (instructions, labels) = discovery.function_at(entry, mode);
            let function = discovery.analysis.functions.get_mut(&entry).expect("still there");
            (function.instructions, function.labels) = (instructions, labels);
        }
        let analysis = &mut discovery.analysis;
        (analysis.indirect_sites, analysis.jump_tables, analysis.svc_sites, analysis.dead_ends) = counts;
        discovery.run();
    }
    // Thumb read as ARM, from a pointer into it and the calls out of that,
    // is conditional instructions of every kind where an ARM function's
    // nearly all run always: a big one like that is left to the
    // interpreter, should it ever run. Star Fox 64 3D had a hundred,
    // each a whole program's worth of branches
    let text = discovery.text;
    discovery.analysis.functions.retain(|_, function| {
        let guessed = matches!(function.source, Source::Call | Source::Pointer | Source::Scan);
        let always = function.instructions.iter().filter(|&&address| text.read32(address).is_some_and(|word| word >> 28 == 0xE)).count();
        !(guessed && function.mode == Mode::Arm && function.instructions.len() >= 4000 && always * 10 < function.instructions.len() * 6)
    });
    discovery.analysis
}

impl Discovery<'_> {
    fn run(&mut self) {
        while let Some((entry, mode, source)) = self.queue.pop_front().or_else(|| self.guesses.pop_front()) {
            if !self.analysis.functions.contains_key(&entry) {
                let (instructions, labels) = self.function_at(entry, mode);
                self.analysis.functions.insert(entry, Function { source, mode, instructions, labels });
            }
        }
    }

    /// a word only Thumb loads that reads as an ARM instruction run always,
    /// as nearly all of an ARM function's are: Thumb read out of ARM code
    /// by mistake loads those. what real Thumb code loads, addresses and
    /// numbers, hardly ever does, and ARM going on through those ran into
    /// the Thumb after them, Star Fox 64 3D's functions ten times their
    /// size
    fn thumb_literal_instruction(&self, address: u32) -> bool {
        self.thumb_literals.contains(&(address & !3)) && self.text.read32(address & !3).is_some_and(|word| word >> 28 == 0xE)
    }

    /// the instructions and labels of the function at entry, the push a
    /// scan started it before among them.
    fn function_at(&mut self, entry: u32, mode: Mode) -> (Vec<u32>, BTreeSet<u32>) {
        let (instructions, mut labels) = self.explore(entry, mode);
        if let Some(&push) = self.pushes.get(&entry).filter(|push| instructions.binary_search(push).is_ok()) {
            labels.insert(push);
        }
        (instructions, labels)
    }

    fn mark(&mut self, address: u32, size: u32, kind: Byte) {
        for byte in address..address + size {
            if let Some(offset) = byte.checked_sub(self.text.base) {
                if let Some(slot) = self.analysis.map.get_mut(offset as usize) {
                    // code wins over a literal guess
                    if *slot != Byte::Code {
                        *slot = kind;
                    }
                }
            }
        }
    }

    fn byte(&self, address: u32) -> Byte {
        self.analysis.map[(address - self.text.base) as usize]
    }

    /// likely ARM functions in the text nothing reached, the first word of
    /// each gap when it is an unconditional instruction, and every push of
    /// registers onto the stack, one register pushed alone with str too, or
    /// the few ordinary instructions before one when data comes before them.
    /// Thumb only where a gap right after Thumb code pushes lr, or follows a
    /// return and is not padding, too much data looks like Thumb otherwise.
    /// an ARM push wins over Thumb, Thumb over the first word of a gap.
    fn gap_starts(&mut self) -> Vec<(u32, Mode)> {
        let mut starts = Vec::new();
        let mut thumb_starts = BTreeSet::new();
        let thumb: std::collections::HashSet<u32> =
            self.analysis.functions.values().filter(|f| f.mode == Mode::Thumb).flat_map(|f| f.instructions.iter().copied()).collect();
        for offset in (2..self.analysis.map.len()).step_by(2) {
            let address = self.text.base + offset as u32;
            if self.analysis.map[offset] == Byte::Unknown
                && self.analysis.map[offset - 1] == Byte::Code
                && (thumb.contains(&(address - 2)) || thumb.contains(&(address - 4)))
                && self.text.read16(address).is_some_and(|halfword| {
                    let returned = thumb.contains(&(address - 2)) && self.text.read16(address - 2).is_some_and(|last| last & 0xFF00 == 0xBD00 || last == 0x4770);
                    halfword & 0xFF00 == 0xB500 || returned && halfword != 0 && halfword != 0x46C0
                })
                && !self.analysis.functions.contains_key(&address)
            {
                thumb_starts.insert(address);
            }
        }
        let mut in_gap = false;
        let mut address = self.text.base.next_multiple_of(4);
        while let Some(word) = self.text.read32(address) {
            let offset = (address - self.text.base) as usize;
            let unknown = self.analysis.map[offset..offset + 4].iter().all(|&byte| byte == Byte::Unknown);
            if unknown && !self.analysis.functions.contains_key(&address) {
                let instruction = arm::decode(word, address);
                let push = word & 0xFFFF_0000 == 0xE92D_0000 && word & 0xFFFF != 0 || word & 0xFFFF_0FFF == 0xE52D_0004;
                let starts_gap = !in_gap && instruction.condition == arm::ALWAYS && instruction.flow != Flow::Undefined;
                if push {
                    let start = self.loads_before(address);
                    if start != address {
                        self.pushes.insert(start, address);
                    }
                    starts.push((start, Mode::Arm));
                    thumb_starts.remove(&address);
                } else if starts_gap && !thumb_starts.contains(&address) {
                    starts.push((address, Mode::Arm));
                }
            }
            in_gap = unknown;
            address += 4;
        }
        starts.extend(thumb_starts.into_iter().map(|address| (address, Mode::Thumb)));
        starts
    }

    /// where a function whose push is at address starts: up to three
    /// ordinary instructions before it nobody reached, a pointer loaded
    /// first, when what comes before those is not an instruction.
    fn loads_before(&self, address: u32) -> u32 {
        let ordinary = |at: u32| {
            let offset = (at - self.text.base) as usize;
            let word = self.text.read32(at).filter(|_| self.analysis.map[offset..offset + 4].iter().all(|&byte| byte == Byte::Unknown));
            word.is_some_and(|word| {
                let instruction = arm::decode(word, at);
                instruction.condition == arm::ALWAYS && matches!(instruction.flow, Flow::Other | Flow::LiteralLoad { .. })
            })
        };
        let mut start = address;
        while address - start < 12 && start >= self.text.base + 4 && ordinary(start - 4) {
            start -= 4;
        }
        let data_before = start < self.text.base + 4 || self.text.read32(start - 4).is_some_and(|word| word == 0 || word >> 28 != 0xE);
        if data_before { start } else { address }
    }

    /// whether the word at address may hold a code address.
    fn may_point(&self, address: u32) -> bool {
        self.slots.is_none_or(|slots| slots.contains(&address))
    }

    /// a value that may be the address of a function, odd for Thumb.
    fn code_pointer(&mut self, value: u32, source: Source) {
        let (address, mode) = if value & 1 != 0 {
            (value & !1, Mode::Thumb)
        } else if value & 3 == 0 {
            (value, Mode::Arm)
        } else {
            return;
        };
        if source == Source::Pointer && mode == Mode::Thumb && self.arm_around(address) {
            return;
        }
        if self.text.contains(address) && !self.analysis.functions.contains_key(&address) {
            if source == Source::Pointer {
                self.guesses.push_back((address, mode, source));
            } else {
                self.queue.push_back((address, mode, source));
            }
        }
    }

    /// whether the word at address and six of the eight from it read as ARM
    /// run always: Thumb-1, all the 3DS runs, has only branches and the
    /// second half of blx up there, a pointer to Thumb in ARM code is a
    /// number. a bx pc veneer's own word is not
    fn arm_around(&self, address: u32) -> bool {
        let always = |at: u32| self.text.read32(at).is_some_and(|word| word >> 28 == 0xE);
        always(address & !3) && (0..8).filter(|i| always((address & !3) + i * 4)).count() >= 6
    }

    fn call(&mut self, target: u32, mode: Mode) {
        if self.text.contains(target) && !self.analysis.functions.contains_key(&target) {
            self.queue.push_back((target, mode, Source::Call));
        }
    }

    fn decode(&self, address: u32, mode: Mode) -> Option<(Instruction, u32)> {
        match mode {
            Mode::Arm => Some((arm::decode(self.text.read32(address)?, address), 4)),
            Mode::Thumb => {
                let halfword = self.text.read16(address)?;
                Some(thumb::decode(halfword, self.text.read16(address + 2), address))
            }
        }
    }

    /// follows every path through one function, returning its instructions
    /// and labels.
    fn explore(&mut self, entry: u32, mode: Mode) -> (Vec<u32>, BTreeSet<u32>) {
        let mut blocks = vec![entry];
        let mut seen = BTreeSet::new();
        let mut code = BTreeSet::new();
        let mut labels = BTreeSet::new();

        while let Some(start) = blocks.pop() {
            labels.insert(start);
            let mut address = start;
            while self.text.contains(address) && seen.insert(address) {
                if self.byte(address) == Byte::Literal && !(mode == Mode::Arm && self.thumb_literal_instruction(address)) {
                    // ran into a literal pool, the path is wrong or it ended
                    break;
                }
                let Some((instruction, size)) = self.decode(address, mode) else { break };
                if instruction.flow == Flow::Undefined {
                    self.analysis.dead_ends += 1;
                    break;
                }
                self.mark(address, size, Byte::Code);
                code.insert(address);
                let conditional = instruction.is_conditional();

                let returns = matches!(
                    instruction.flow,
                    Flow::Branch { link: true, .. }
                        | Flow::CallOtherMode { .. }
                        | Flow::BranchRegister { link: true, .. }
                        | Flow::Svc
                );
                if returns {
                    // execution comes back after a call or an svc
                    labels.insert(address + size);
                }
                match instruction.flow {
                    Flow::Branch { target, link: true } => self.call(target, mode),
                    Flow::CallOtherMode { target } => self.call(target, mode.other()),
                    Flow::Branch { target, link: false } => {
                        // to another function's start a tail call, not more of this one
                        let tail = target != entry && self.analysis.functions.get(&target).is_some_and(|function| function.mode == mode);
                        if self.text.contains(target) && !tail {
                            blocks.push(target);
                        }
                        if !conditional {
                            break;
                        }
                    }
                    Flow::BranchRegister { register, link } => {
                        self.analysis.indirect_sites += 1;
                        if let Some(target) = self.set_before(address, mode, register) {
                            self.code_pointer(target, Source::Call);
                        }
                        if !link {
                            self.returns_to(address, mode, &mut blocks);
                            if !conditional {
                                break;
                            }
                        }
                    }
                    Flow::IndirectJump => {
                        self.analysis.indirect_sites += 1;
                        self.returns_to(address, mode, &mut blocks);
                        self.offset_table(address, mode, &mut blocks);
                        if !conditional {
                            break;
                        }
                    }
                    Flow::Return => {
                        if !conditional {
                            break;
                        }
                    }
                    Flow::JumpTable => {
                        self.branch_table(address, &mut blocks);
                        break;
                    }
                    Flow::AddressTable { index } => {
                        self.address_table(address, index, &mut blocks);
                        if !conditional {
                            break;
                        }
                    }
                    Flow::LoadPcLiteral { literal } => {
                        self.mark(literal, 4, Byte::Literal);
                        if let Some(target) = self.text.read32(literal).filter(|_| self.may_point(literal)) {
                            self.code_pointer(target, Source::Pointer);
                        }
                        if !conditional {
                            break;
                        }
                    }
                    Flow::LiteralLoad { literal, size } => {
                        // a load from something already decoded as code is
                        // left alone
                        if self.text.contains(literal) && self.byte(literal) != Byte::Code {
                            if mode == Mode::Thumb && self.byte(literal) == Byte::Unknown {
                                self.thumb_literals.insert(literal & !3);
                            }
                            self.mark(literal, size, Byte::Literal);
                            if size == 4 && self.may_point(literal) {
                                if let Some(value) = self.text.read32(literal) {
                                    self.code_pointer(value, Source::Pointer);
                                }
                            }
                        }
                    }
                    Flow::Svc => self.analysis.svc_sites += 1,
                    Flow::Undefined | Flow::Other => {}
                }
                address += size;
            }
        }
        labels.retain(|label| code.contains(label));
        (code.into_iter().collect(), labels)
    }

    /// the address a register was given pc relative in the few ARM
    /// instructions before a jump, none between writing it.
    fn set_before(&self, address: u32, mode: Mode, register: u32) -> Option<u32> {
        self.set_within(address, mode, register, 4)
    }

    fn set_within(&self, address: u32, mode: Mode, register: u32, window: u32) -> Option<u32> {
        if mode != Mode::Arm {
            return None;
        }
        for back in 1..=window {
            let at = address.checked_sub(back * 4)?;
            let word = self.text.read32(at)?;
            if let Some((rd, value)) = arm::pc_relative(word, at) {
                if rd == register {
                    return Some(value);
                }
            }
            // anything that may write it, or a block transfer or branch
            if (word >> 12) & 0xF == register || word & 0x0E00_0000 == 0x0800_0000 || word & 0x0E00_0000 == 0x0A00_0000 {
                return None;
            }
        }
        None
    }

    /// a jump with lr set just before it by hand is a call, which comes
    /// back to where lr says.
    fn returns_to(&self, address: u32, mode: Mode, blocks: &mut Vec<u32>) {
        if let Some(back) = self.set_before(address, mode, arm::LR).filter(|&back| back & 3 == 0 && back > address && self.text.contains(back)) {
            blocks.push(back);
        }
    }

    /// add pc, rt, rb with rt loaded from [rb, ri lsl 2] and rb set pc
    /// relative jumps through a table of offsets from rb, as the MobiClip
    /// video decoder most titles carry does. the entries go on while they
    /// land a little way ahead in the code.
    fn offset_table(&mut self, address: u32, mode: Mode, blocks: &mut Vec<u32>) {
        let Some(word) = self.text.read32(address).filter(|word| mode == Mode::Arm && word & 0xFFF0_FFF0 == 0xE080_F000) else { return };
        let operands = [(word >> 16) & 0xF, word & 0xF];
        let load = (1..=16).find_map(|back| {
            let at = address.checked_sub(back * 4)?;
            let load = self.text.read32(at)?;
            let (loaded, base) = ((load >> 12) & 0xF, (load >> 16) & 0xF);
            (load & 0xFFF0_0FF0 == 0xE790_0100 && loaded != base && operands.contains(&loaded) && operands.contains(&base)).then_some((at, base))
        });
        let Some(base) = load.and_then(|(at, base)| self.set_within(at, mode, base, 16)) else { return };
        self.analysis.jump_tables += 1;
        for entry in (base..).step_by(4).take(256) {
            let Some(offset) = self.text.read32(entry) else { break };
            let target = base.wrapping_add(offset);
            if offset == 0 || offset & 3 != 0 || offset >= 0x10000 || !self.text.contains(target) {
                break;
            }
            self.mark(entry, 4, Byte::Literal);
            blocks.push(target);
        }
    }

    /// add pc, pc, rm lsl 2 jumps into a run of branches that starts with
    /// the default case right after it.
    fn branch_table(&mut self, address: u32, blocks: &mut Vec<u32>) {
        self.analysis.jump_tables += 1;
        let mut entry = address + 4;
        while let Some(word) = self.text.read32(entry) {
            match arm::decode(word, entry) {
                Instruction { condition: arm::ALWAYS, flow: Flow::Branch { link: false, .. } } => {
                    blocks.push(entry);
                    entry += 4;
                }
                _ => break,
            }
        }
    }

    /// ldr pc, [pc, rm lsl 2] loads its target from the table of addresses
    /// right after the default branch. its size comes from the cmp just
    /// before it, or failing that from how far the entries keep pointing
    /// into the code.
    fn address_table(&mut self, address: u32, index: u32, blocks: &mut Vec<u32>) {
        self.analysis.jump_tables += 1;
        let entries = address
            .checked_sub(4)
            .and_then(|previous| self.text.read32(previous))
            .and_then(arm::compare_immediate)
            .filter(|&(register, _)| register == index)
            .map(|(_, limit)| limit as usize + 1);
        let table = address + 8;
        for i in 0..entries.unwrap_or(256) {
            let slot = table + i as u32 * 4;
            let Some(target) = self.text.read32(slot) else { break };
            if !self.text.contains(target & !1) || !self.may_point(slot) {
                break;
            }
            self.mark(slot, 4, Byte::Literal);
            if target & 3 == 0 {
                blocks.push(target);
            } else {
                self.code_pointer(target, Source::Pointer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x0010_0000;

    fn analyze_words(words: &[u32]) -> Analysis {
        analyze(&program(words))
    }

    fn program(words: &[u32]) -> Program {
        Program {
            text: Segment { base: BASE, bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect() },
            seeds: vec![(BASE, Source::Entry)],
            slots: None,
        }
    }

    #[test]
    fn follows_calls_and_marks_literals() {
        let analysis = analyze_words(&[
            0xEB00_0002, // bl 0x100010
            0xEF00_0000, // svc 0
            0xEAFF_FFFE, // b .
            0xDEAD_BEEF, // never reached
            0xE59F_0000, // ldr r0, [pc] (literal at 0x100018)
            0xE12F_FF1E, // bx lr
            0x0010_0000, // the literal
        ]);
        assert_eq!(analysis.functions.keys().copied().collect::<Vec<_>>(), [BASE, BASE + 0x10]);
        assert_eq!(analysis.functions[&BASE].labels, BTreeSet::from([BASE, BASE + 4, BASE + 8]));
        assert_eq!(analysis.functions[&(BASE + 0x10)].source, Source::Call);
        assert_eq!(analysis.count(Byte::Code), 5 * 4);
        assert_eq!(analysis.count(Byte::Literal), 4);
        assert_eq!(analysis.largest_gaps(BASE, 1), [(BASE + 0xC, 4)]);
        assert_eq!(analysis.svc_sites, 1);
    }

    #[test]
    fn a_branch_to_a_function_found_later_is_a_tail_call() {
        let analysis = analyze_words(&[
            0xEB00_0001, // bl 0x10000c
            0xEA00_0000, // b 0x10000c, the same function
            0xDEAD_BEEF, // never reached
            0xE3A0_0001, // mov r0, 1
            0xE12F_FF1E, // bx lr
        ]);
        assert_eq!(analysis.functions[&BASE].instructions, [BASE, BASE + 4]);
        assert_eq!(analysis.functions[&BASE].labels, BTreeSet::from([BASE, BASE + 4]));
        assert_eq!(analysis.functions[&(BASE + 0xC)].instructions, [BASE + 0xC, BASE + 0x10]);
    }

    #[test]
    fn a_switch_table_leads_to_every_case() {
        let analysis = analyze_words(&[
            0xE350_0001, // cmp r0, 1
            0x908F_F100, // addls pc, pc, r0 lsl 2
            0xEA00_0002, // b default (0x100018)
            0xEA00_0002, // b case 0 (0x10001c)
            0xEA00_0002, // b case 1 (0x100020)
            0x0000_0000, // data between the table and the cases
            0xE12F_FF1E, // default
            0xE12F_FF1E, // case 0
            0xE12F_FF1E, // case 1
        ]);
        assert_eq!(analysis.jump_tables, 1);
        assert_eq!(analysis.count(Byte::Code), 8 * 4);
        let labels: Vec<_> = analysis.functions[&BASE].labels.iter().map(|l| l - BASE).collect();
        assert_eq!(labels, [0, 8, 0xC, 0x10, 0x18, 0x1C, 0x20]);
    }

    #[test]
    fn an_address_table_is_sized_by_its_compare() {
        let analysis = analyze_words(&[
            0xE350_0001, // cmp r0, 1
            0x979F_F100, // ldrls pc, [pc, r0 lsl 2]
            0xE12F_FF1E, // bx lr, the default case
            BASE + 0x14, // case 0
            BASE + 0x18, // case 1
            0xE12F_FF1E, // case 0
            0xE12F_FF1E, // case 1
        ]);
        assert_eq!(analysis.jump_tables, 1);
        assert_eq!(analysis.count(Byte::Literal), 8);
        assert_eq!(analysis.count(Byte::Code), 5 * 4);
    }

    #[test]
    fn a_thumb_pointer_into_arm_code_is_a_number() {
        let mut program = program(&[
            0xE92D_4010, // push {r4, lr}
            0xE3A0_4001, // mov r4, 1
            0xE3A0_0002, // mov r0, 2
            0xE080_0004, // add r0, r0, r4
            0xE280_0001, // add r0, r0, 1
            0xE240_0001, // sub r0, r0, 1
            0xE1A0_1000, // mov r1, r0
            0xE8BD_8010, // pop {r4, pc}
        ]);
        program.seeds.push((BASE + 7, Source::Pointer));
        assert!(!analyze(&program).functions.contains_key(&(BASE + 6)));
    }

    #[test]
    fn blx_leads_into_thumb_code() {
        let analysis = analyze_words(&[
            0xFA00_0000, // blx 0x100008
            0xE12F_FF1E, // bx lr
            0x4770_2001, // movs r0, 1 then bx lr, in Thumb
        ]);
        let thumb = &analysis.functions[&(BASE + 8)];
        assert_eq!(thumb.mode, Mode::Thumb);
        assert_eq!(thumb.instructions, [BASE + 8, BASE + 10]);
    }

    #[test]
    fn pointer_seeds_become_functions() {
        let mut program = program(&[0xE12F_FF1E, 0x4770_4770, 0xE12F_FF1E]);
        program.seeds.extend([(BASE + 8, Source::Pointer), (BASE + 5, Source::Pointer)]);
        let analysis = analyze(&program);
        assert_eq!(analysis.functions[&(BASE + 8)].source, Source::Pointer);
        assert_eq!(analysis.functions[&(BASE + 4)].mode, Mode::Thumb);
    }

    #[test]
    fn with_slots_only_relocated_literals_are_pointers() {
        let words = [
            0xE59F_0000, // ldr r0, [pc] (literal at 0x100008)
            0xE12F_FF1E, // bx lr
            BASE + 0xC,  // looks like a code address
            0xE12F_FF1E, // bx lr
        ];
        assert_eq!(analyze_words(&words).functions[&(BASE + 0xC)].source, Source::Pointer);
        let mut program = program(&words);
        program.slots = Some(BTreeSet::new());
        // the code is still there for the scan to find, only not through
        // the literal
        assert_eq!(analyze(&program).functions[&(BASE + 0xC)].source, Source::Scan);
    }

    #[test]
    fn a_table_of_offsets_from_its_own_address_leads_to_every_case() {
        let analysis = analyze_words(&[
            0xE28F_3010, // add r3, pc, 0x10 (the table at 0x100018)
            0xE793_8100, // ldr r8, [r3, r0 lsl 2]
            0xE088_F003, // add pc, r8, r3
            0xE12F_FF1E,
            0xE12F_FF1E,
            0xE12F_FF1E,
            0x0000_0008, // case 0 at 0x100020
            0x0000_000C, // case 1 at 0x100024
            0xE12F_FF1E, // case 0
            0xE12F_FF1E, // case 1
        ]);
        let labels = &analysis.functions[&BASE].labels;
        assert!(labels.contains(&(BASE + 0x20)) && labels.contains(&(BASE + 0x24)));
        assert_eq!(analysis.count(Byte::Literal), 8);
    }

    #[test]
    fn arm_code_goes_on_through_what_only_thumb_loads() {
        let analysis = analyze_words(&[
            0xFA00_0001, // blx 0x10000c
            0xEB00_0002, // bl 0x100014
            0xE12F_FF1E, // bx lr
            0x4770_4802, // ldr r0, [pc, 8] (0x100018) then bx lr, in Thumb
            0x0000_0000,
            0xE3A0_0001, // mov r0, 1
            0xE3A0_1002, // mov r1, 2, which the Thumb loads
            0xE12F_FF1E, // bx lr
        ]);
        assert!(analysis.functions[&(BASE + 0x14)].instructions.contains(&(BASE + 0x1C)));
    }

    #[test]
    fn arm_code_stops_at_what_only_thumb_loads_when_it_is_not_an_instruction() {
        let analysis = analyze_words(&[
            0xFA00_0001, // blx 0x10000c
            0xEB00_0002, // bl 0x100014
            0xE12F_FF1E, // bx lr
            0x4770_4802, // ldr r0, [pc, 8] (0x100018) then bx lr, in Thumb
            0x0000_0000,
            0xE3A0_0001, // mov r0, 1
            0x0010_2000, // an address, which the Thumb loads
            0xE12F_FF1E, // bx lr
        ]);
        assert!(!analysis.functions[&(BASE + 0x14)].instructions.contains(&(BASE + 0x1C)));
    }

    #[test]
    fn a_stub_into_thumb_leads_there() {
        let analysis = analyze_words(&[
            0xE28F_C001, // add ip, pc, 1
            0xE12F_FF1C, // bx ip
            0x4770_2001, // movs r0, 1 then bx lr, in Thumb
        ]);
        assert_eq!(analysis.functions[&(BASE + 8)].mode, Mode::Thumb);
    }

    #[test]
    fn a_jump_with_lr_set_by_hand_comes_back() {
        let analysis = analyze_words(&[
            0xE28F_E000, // add lr, pc, 0
            0xE591_F000, // ldr pc, [r1]
            0xE12F_FF1E, // bx lr, where it comes back to
        ]);
        assert!(analysis.functions[&BASE].labels.contains(&(BASE + 8)));
        assert_eq!(analysis.count(Byte::Code), 3 * 4);
    }

    #[test]
    fn a_push_of_lr_alone_or_after_thumb_starts_a_function() {
        let analysis = analyze_words(&[
            0xFA00_0002, // blx 0x100010
            0xE12F_FF1E, // bx lr
            0x1234_5678, // data
            0xE52D_E004, // str lr, [sp, -4]!
            0x4770_2001, // movs r0, 1 then bx lr, in Thumb
            0xBD10_B510, // push {r4, lr} then pop {r4, pc}
        ]);
        assert_eq!(analysis.functions[&(BASE + 12)].source, Source::Scan);
        assert_eq!(analysis.functions[&(BASE + 20)].mode, Mode::Thumb);
    }

    #[test]
    fn a_function_loading_a_pointer_before_its_push_starts_there() {
        let analysis = analyze_words(&[
            0xE12F_FF1E, // bx lr, the entry
            0x0000_0000, // data
            0xE59F_0004, // ldr r0, [pc, 4]
            0xE92D_4010, // push {r4, lr}
            0xE8BD_8010, // pop {r4, pc}
            0x1234_5678, // the literal
        ]);
        let function = &analysis.functions[&(BASE + 8)];
        assert_eq!(function.source, Source::Scan);
        assert!(function.labels.contains(&(BASE + 12)));
    }

    /// code nothing leads to still becomes functions where a gap begins
    /// with an instruction or a push saves registers, data does not.
    #[test]
    fn gaps_are_scanned_for_functions() {
        let analysis = analyze_words(&[
            0xE12F_FF1E, // bx lr, the entry
            0xE3A0_0001, // mov r0, #1, the start of a gap
            0xE12F_FF1E, // bx lr
            0x1234_5678, // data
            0xE92D_4010, // push {r4, lr}
            0xE8BD_8010, // pop {r4, pc}
        ]);
        assert_eq!(analysis.functions[&(BASE + 4)].source, Source::Scan);
        assert_eq!(analysis.functions[&(BASE + 16)].source, Source::Scan);
        assert!(!analysis.functions.contains_key(&(BASE + 12)));
    }
}
