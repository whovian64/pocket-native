//! ARM functions written as AArch64 assembly beside their C, for the
//! handhelds. a function's blocks, from one budget label to the next, are
//! translated one by one, and one with anything the translator doesn't take
//! hands over to the C at its label, which dispatches there and charges the
//! same budget, so a function runs the same however much of it translates.
//!
//! x19 holds the context, x20 the view of memory, w21 the budget, x24 the
//! VFP registers, x25 fpscr, w0-w14 the guest's r0-r14 and NZCV its flags,
//! w27 a module's base and w28 the call depth, written back with the rest.
//! w15-w17, w22-w23 and v16-v19 are scratch.
//!
//! a function has two ways in: a_ from the C and the host, which takes the
//! guest's state from the context and leaves it there, and b_ from another
//! translated function, which keeps it in registers. a b_ call returning
//! the plain way hands back x26 set and the address it returns to in w22,
//! registers live; any other way out writes everything back to the context
//! and hands back x26 clear. the frame says which way the function came in,
//! and keeps the C caller's x26.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::discover::{Function, Mode, Program};

macro_rules! emit {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).unwrap()
    };
}

/// what every assembly file starts with: writing the guest's state back to
/// the context and reading it again, and the frame.
/// what each file starts with, a module's restoring w27 as its code leaves.
pub fn prelude(relative: bool) -> String {
    format!("\t.set pn_module, {}\n{PRELUDE}", relative as u8)
}

const PRELUDE: &str = "\t.text
\t.arch armv8-a
.macro pn_flush_inline
\tstp w0, w1, [x19, #0]
\tstp w2, w3, [x19, #8]
\tstp w4, w5, [x19, #16]
\tstp w6, w7, [x19, #24]
\tstp w8, w9, [x19, #32]
\tstp w10, w11, [x19, #40]
\tstp w12, w13, [x19, #48]
\tstr w14, [x19, #56]
\tmrs x16, nzcv
\trbit w16, w16
\tmovz w17, #0x4081
\tmovk w17, #0x20, lsl #16
\tmul w16, w16, w17
\tand w16, w16, #0x01010101
\tstr w16, [x19, #64]
\tstr w21, [x19, #72]
\tstr w28, [x19, #84]
.endm
.macro pn_reload_inline
\tldp w0, w1, [x19, #0]
\tldp w2, w3, [x19, #8]
\tldp w4, w5, [x19, #16]
\tldp w6, w7, [x19, #24]
\tldp w8, w9, [x19, #32]
\tldp w10, w11, [x19, #40]
\tldp w12, w13, [x19, #48]
\tldr w14, [x19, #56]
\tldr w16, [x19, #64]
\tmovz w17, #0x2010
\tmovk w17, #0x8040, lsl #16
\tmul w16, w16, w17
\tand w16, w16, #0xf0000000
\tmsr nzcv, x16
\tldr w21, [x19, #72]
\tldr w28, [x19, #84]
.endm
.macro pn_enter
\tstp x29, x30, [sp, #-112]!
\tmov x29, sp
\tstp x19, x20, [sp, #16]
\tstp x21, x22, [sp, #32]
\tstp x23, x24, [sp, #48]
\tstp x25, x26, [sp, #64]
\tstp xzr, x27, [sp, #80]
\tstr x28, [sp, #96]
\tmov x19, x0
\tldr x20, [x19, #144]
\tldr x24, [x19, #112]
\tldr x25, [x19, #120]
.endm
.macro pn_enter_inner
\tstp x29, x30, [sp, #-112]!
\tmov x29, sp
\tstp x19, x20, [sp, #16]
\tstp x21, x22, [sp, #32]
\tstp x23, x24, [sp, #48]
\tstp x25, xzr, [sp, #64]
\tmov x16, #1
\tstp x16, x27, [sp, #80]
.endm
.macro pn_leave
\tldp x19, x20, [sp, #16]
\tldp x21, x22, [sp, #32]
\tldp x23, x24, [sp, #48]
\tldp x25, x26, [sp, #64]
.if pn_module
\tldr x27, [sp, #88]
.endif
\tldr x28, [sp, #96]
\tldp x29, x30, [sp], #112
.endm
.macro pn_flush
\tbl pn_flush_sub
.endm
.macro pn_reload
\tbl pn_reload_sub
.endm
// shared by the file's functions, each of which has saved x30 by then:
// writing back and reading again, the way out of the budget with the
// block's address in w22 and its count in w23, and leaving
\t.p2align 4
pn_flush_sub:
\tpn_flush_inline
\tret
pn_reload_sub:
\tpn_reload_inline
\tret
pn_budget_sub:
\tadd w21, w21, w23
\tpn_flush
\tstr w22, [x19, #60]
\tmov w16, #2
\tstr w16, [x19, #76]
pn_leave_sub:
\tpn_leave
\tret
";

// the context's fields the code touches, as recomp.h lays them out
const R15: u32 = 60;
const Q: u32 = 68;
const THUMB: u32 = 69;
const GE: u32 = 70;
const EXCLUSIVE: u32 = 71;
const TLS: u32 = 92;
const HOST: u32 = 128;
/// the host's lookup and svc, in its table of functions.
const LOOKUP: u32 = 56;
const SVC: u32 = 64;
/// fpscr's short vector length.
const FPSCR_LEN: u32 = 7 << 16;
const EXIT: u32 = 76;
const DEPTH: u32 = 84;
const EXIT_UNWIND: u32 = 3;
const DEPTH_LIMIT: u32 = 2048;

/// A32 condition codes as A64 writes them, in the same order.
const CONDITIONS: [&str; 14] = ["eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le"];

/// the name of a function's assembly.
pub fn name(prefix: &str, entry: u32) -> String {
    format!("{prefix}a_{entry:08X}")
}

/// a function being translated.
struct Writer<'a> {
    entry: u32,
    labels: &'a BTreeSet<u32>,
    /// what calls go to, by entry with bit 0 set for Thumb.
    functions: &'a BTreeMap<u32, String>,
    /// the C that runs the function, which every fallback ends in.
    fallback: String,
    /// what the unit's names start with, a module's not empty: its code
    /// moves, its addresses offsets from where the host loaded it.
    prefix: &'a str,
    relative: bool,
    /// local labels made so far.
    made: std::cell::Cell<u32>,
    /// whether the block being written has VFP arithmetic a short vector
    /// would change, which runs only with fpscr's length 1.
    vector: std::cell::Cell<bool>,
    /// the singles the block keeps in host registers, by guest register, and
    /// what a first writing of it found: the singles it reads or writes,
    /// those it writes, and whether it works on doubles, which it then
    /// leaves in memory with everything else.
    cache: std::cell::RefCell<Option<BTreeMap<u32, u32>>>,
    used: std::cell::RefCell<BTreeMap<u32, usize>>,
    written: std::cell::RefCell<BTreeSet<u32>>,
    doubles: std::cell::Cell<bool>,
    /// after a call till the fill: the host registers hold whatever the
    /// callee left, and memory the singles, so nothing spills.
    stale: std::cell::Cell<bool>,
    /// what the code seldom runs, past the function's end, and the stubs
    /// shared there it used: leaving, the C's dispatch as the context is,
    /// and the way out over the depth limit.
    cold: std::cell::RefCell<String>,
    shared: std::cell::Cell<[bool; 4]>,
}

impl<'a> Writer<'a> {
    fn new(entry: u32, labels: &'a BTreeSet<u32>, functions: &'a BTreeMap<u32, String>, fallback: String, prefix: &'a str) -> Self {
        let relative = !prefix.is_empty();
        Writer { entry, labels, functions, fallback, prefix, relative, made: Default::default(), vector: Default::default(), cache: Default::default(), used: Default::default(), written: Default::default(), doubles: Default::default(), stale: Default::default(), cold: Default::default(), shared: Default::default() }
    }

    /// a stub shared past the end: 0 leaves, 1 hands over to the C as
    /// the context is, 2 leaves over the depth limit with w16 one past it,
    /// 3 leaves after call_inner's slow way back.
    fn stub(&self, which: usize) -> String {
        let mut shared = self.shared.get();
        shared[which] = true;
        self.shared.set(shared);
        format!(".L{}{:08X}", ["l", "r", "u", "d"][which], self.entry)
    }

    fn local(&self) -> String {
        let made = self.made.get();
        self.made.set(made + 1);
        format!(".La{:08X}_{made}", self.entry)
    }

    fn block(&self, address: u32) -> String {
        format!(".Lb{:08X}_{address:08X}", self.entry)
    }

    /// the host register a single is kept in in this block, if it is.
    fn cached(&self, single: u32, write: bool) -> Option<u32> {
        *self.used.borrow_mut().entry(single).or_default() += 1;
        if write {
            self.written.borrow_mut().insert(single);
        }
        self.cache.borrow().as_ref().and_then(|cache| cache.get(&single).copied())
    }

    /// the singles the block wrote, back to memory, before anything leaves
    /// the block or calls out of it.
    fn spill(&self, out: &mut String) {
        if self.stale.get() {
            return;
        }
        if let Some(cache) = self.cache.borrow().as_ref() {
            for single in self.written.borrow().iter() {
                if let Some(host) = cache.get(single) {
                    emit!(out, "\tstr s{host}, [x24, #{}]", 4 * single);
                }
            }
        }
    }

    /// the cached singles read from memory again, at the block's start and
    /// after a call.
    fn fill(&self, out: &mut String) {
        if let Some(cache) = self.cache.borrow().as_ref() {
            for (single, host) in cache {
                emit!(out, "\tldr s{host}, [x24, #{}]", 4 * single);
            }
        }
        self.stale.set(false);
    }
}

/// the host registers singles are kept in, past v16-v19's scratch.
const CACHE: [u32; 20] = [20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0, 1, 2, 3, 4, 5, 6, 7];

/// a guest single's bits into or out of a w register.
fn word_load(out: &mut String, w: &Writer, single: u32, register: &str) {
    match w.cached(single, false) {
        Some(host) => emit!(out, "\tfmov {register}, s{host}"),
        None => emit!(out, "\tldr {register}, [x24, #{}]", 4 * single),
    }
}

fn word_store(out: &mut String, w: &Writer, single: u32, register: &str) {
    match w.cached(single, true) {
        Some(host) => emit!(out, "\tfmov s{host}, {register}"),
        None => emit!(out, "\tstr {register}, [x24, #{}]", 4 * single),
    }
}

/// a guest address into a register: in a module, an offset from its base,
/// which w27 holds.
fn guest(out: &mut String, w: &Writer, register: &str, at: u32) {
    if !w.relative {
        constant(out, register, at);
    } else if at < 4096 {
        emit!(out, "\tadd {register}, w27, #{at}");
    } else {
        constant(out, register, at);
        emit!(out, "\tadd {register}, {register}, w27");
    }
}

/// a 32-bit constant into a register.
fn constant(out: &mut String, register: &str, value: u32) {
    emit!(out, "\tmovz {register}, #0x{:X}", value & 0xFFFF);
    if value >> 16 != 0 {
        emit!(out, "\tmovk {register}, #0x{:X}, lsl #16", value >> 16);
    }
}

/// hands over to the C at address, everything written back.
fn fallback(out: &mut String, w: &Writer, at: u32) {
    w.spill(out);
    emit!(out, "\tpn_flush");
    guest(out, w, "w16", at);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
}

/// hands over to the C at the address in w22.
fn dispatch(out: &mut String, w: &Writer) {
    w.spill(out);
    emit!(out, "\tpn_flush");
    emit!(out, "\tstr w22, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
}

/// a return to the address in w22, which may switch to Thumb: to a
/// translated caller with the registers live, to anything else through the
/// context.
fn return_to(out: &mut String, w: &Writer) {
    w.spill(out);
    let outer = w.local();
    emit!(out, "\tldr x16, [sp, #80]");
    emit!(out, "\tcbz x16, {outer}");
    emit!(out, "\tldp x29, x30, [sp], #112");
    emit!(out, "\tmov x26, #1");
    emit!(out, "\tret");
    emit!(out, "{outer}:");
    emit!(out, "\tpn_flush");
    emit!(out, "\tand w16, w22, #1");
    emit!(out, "\tstrb w16, [x19, #{THUMB}]");
    emit!(out, "\ttst w22, #1");
    emit!(out, "\tand w17, w22, #0xfffffffe");
    emit!(out, "\tand w16, w22, #0xfffffffc");
    emit!(out, "\tcsel w16, w17, w16, ne");
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tb pn_leave_sub");
}

/// a jump to the address in w22, which may switch to Thumb, through the
/// C's dispatch.
fn jump_to(out: &mut String, w: &Writer) {
    w.spill(out);
    emit!(out, "\tpn_flush");
    emit!(out, "\tand w16, w22, #1");
    emit!(out, "\tstrb w16, [x19, #{THUMB}]");
    emit!(out, "\ttst w22, #1");
    emit!(out, "\tand w17, w22, #0xfffffffe");
    emit!(out, "\tand w16, w22, #0xfffffffc");
    emit!(out, "\tcsel w16, w17, w16, ne");
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
}

/// what a call runs: a function by name, or whatever the host has for r15,
/// as recomp_call does.
enum Callee<'a> {
    Named(&'a str),
    Lookup,
}

/// a call, everything written back, r15 already in the context, counting
/// depth as CALL does.
fn call_out(out: &mut String, w: &Writer, callee: Callee) {
    w.spill(out);
    emit!(out, "\tldr w16, [x19, #{DEPTH}]");
    emit!(out, "\tadd w16, w16, #1");
    emit!(out, "\tstr w16, [x19, #{DEPTH}]");
    emit!(out, "\tcmp w16, #{DEPTH_LIMIT}");
    emit!(out, "\tb.gt {}", w.stub(2));
    match callee {
        Callee::Named(name) => {
            emit!(out, "\tmov x0, x19");
            emit!(out, "\tbl {name}");
        }
        Callee::Lookup => {
            let (none, done) = (w.local(), w.local());
            emit!(out, "\tmov x0, x19");
            emit!(out, "\tldr x16, [x19, #{HOST}]");
            emit!(out, "\tldr x16, [x16, #{LOOKUP}]");
            emit!(out, "\tldr w1, [x19, #{R15}]");
            emit!(out, "\tldrb w17, [x19, #{THUMB}]");
            emit!(out, "\torr w1, w1, w17");
            emit!(out, "\tblr x16");
            emit!(out, "\tcbz x0, {none}");
            emit!(out, "\tmov x16, x0");
            emit!(out, "\tmov x0, x19");
            emit!(out, "\tblr x16");
            emit!(out, "\tb {done}");
            emit!(out, "{none}:");
            emit!(out, "\tmov w16, #{EXIT_UNWIND}");
            emit!(out, "\tstr w16, [x19, #{EXIT}]");
            emit!(out, "{done}:");
        }
    }
    w.stale.set(true);
    emit!(out, "\tldr w16, [x19, #{DEPTH}]");
    emit!(out, "\tsub w16, w16, #1");
    emit!(out, "\tstr w16, [x19, #{DEPTH}]");
}

/// the way into a translated function of the unit for another, or none.
fn inner(w: &Writer, name: &str) -> Option<String> {
    name.strip_prefix(w.prefix)?.strip_prefix("a_").map(|rest| format!("{}b_{rest}", w.prefix))
}

/// a call into a translated function the registers live, counting depth
/// as CALL does without touching NZCV, then dec as it comes back. over the
/// limit everything goes back to the context with r15 at target.
fn call_inner(out: &mut String, w: &Writer, target: u32, name: &str) {
    w.spill(out);
    let over = w.local();
    emit!(out, "\tadd w28, w28, #1");
    emit!(out, "\tsub w17, w28, #{}", DEPTH_LIMIT + 1);
    emit!(out, "{TEST_MARK}tbz w17, #31, {over}");
    {
        let mut cold = w.cold.borrow_mut();
        emit!(cold, "{over}:");
        emit!(cold, "\tsub w28, w28, #1");
        emit!(cold, "\tpn_flush");
        guest(&mut cold, w, "w16", target);
        emit!(cold, "\tstr w16, [x19, #{R15}]");
        emit!(cold, "\tmov w16, #{EXIT_UNWIND}");
        emit!(cold, "\tstr w16, [x19, #{EXIT}]");
        emit!(cold, "\tb pn_leave_sub");
    }
    emit!(out, "\tbl {name}");
    w.stale.set(true);
}

/// after call_inner the slow way back, the callee gone through the
/// context, whose depth is one too many.
fn depth_back(out: &mut String) {
    emit!(out, "\tldr w28, [x19, #{DEPTH}]");
    emit!(out, "\tsub w28, w28, #1");
    emit!(out, "\tstr w28, [x19, #{DEPTH}]");
}

/// a guest call with link the return address, as call in mod.rs writes it,
/// to Thumb code where thumb.
fn call(out: &mut String, w: &Writer, link: u32, target: u32, thumb: bool) -> Option<bool> {
    if let Some(name) = w.functions.get(&target).and_then(|name| inner(w, name)).filter(|_| !thumb) {
        guest(out, w, "w14", link);
        call_inner(out, w, target, &name);
        let (slow, other, done) = (w.local(), w.local(), w.local());
        // back the plain way: on unless it isn't to the next instruction
        emit!(out, "\tcbz x26, {slow}");
        emit!(out, "\tsub w28, w28, #1");
        guest(out, w, "w17", link);
        emit!(out, "\tsub w16, w22, w17");
        emit!(out, "\tcbnz w16, {other}");
        let mut cold = String::new();
        emit!(cold, "{slow}:");
        depth_back(&mut cold);
        returned(&mut cold, w, link);
        emit!(cold, "\tb {done}");
        emit!(cold, "{other}:");
        jump_to(&mut cold, w);
        w.cold.borrow_mut().push_str(&cold);
        emit!(out, "{done}:");
        w.fill(out);
        return Some(true);
    }
    guest(out, w, "w14", link);
    emit!(out, "\tpn_flush");
    guest(out, w, "w16", target);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    if thumb {
        emit!(out, "\tmov w16, #1");
        emit!(out, "\tstrb w16, [x19, #{THUMB}]");
    }
    call_out(out, w, w.functions.get(&(target | thumb as u32)).map_or(Callee::Lookup, |name| Callee::Named(name)));
    returned(out, w, link);
    w.fill(out);
    Some(true)
}

/// after a call: the caller leaves too when the host has to take over, and
/// anything but a plain return to link goes through the C's dispatch, the
/// context as it is.
fn returned(out: &mut String, w: &Writer, link: u32) {
    // the host takes over, or anything but a plain return to the next
    // instruction goes through the C's dispatch, the context as it is
    emit!(out, "\tldr w16, [x19, #{EXIT}]");
    emit!(out, "\tcbnz w16, {}", w.stub(0));
    emit!(out, "\tldr w16, [x19, #{R15}]");
    guest(out, w, "w17", link);
    emit!(out, "\tcmp w16, w17");
    emit!(out, "\tldrb w16, [x19, #{THUMB}]");
    emit!(out, "\tccmp w16, #0, #0, eq");
    emit!(out, "\tb.ne {}", w.stub(1));
    emit!(out, "\tpn_reload");
}

/// a branch without link: to a label of this function, a known function as
/// a tail call, or the C's dispatch.
fn jump(out: &mut String, w: &Writer, target: u32) -> Option<bool> {
    if w.labels.contains(&target) {
        w.spill(out);
        emit!(out, "\tb {}", w.block(target));
    } else if let Some(name) = w.functions.get(&target).and_then(|name| inner(w, name)) {
        // a tail call: its return is this function's
        call_inner(out, w, target, &name);
        emit!(out, "\tcbz x26, {}", w.stub(3));
        emit!(out, "\tsub w28, w28, #1");
        return_to(out, w);
        // not taken, the registers are as they were
        w.stale.set(false);
    } else if let Some(name) = w.functions.get(&target) {
        emit!(out, "\tpn_flush");
        guest(out, w, "w16", target);
        emit!(out, "\tstr w16, [x19, #{R15}]");
        call_out(out, w, Callee::Named(name));
        emit!(out, "\tb pn_leave_sub");
        w.stale.set(false);
    } else {
        guest(out, w, "w22", target);
        dispatch(out, w);
    }
    Some(false)
}

fn sign_extend_24(value: u32) -> u32 {
    (((value << 8) as i32) >> 8) as u32
}

/// the instruction at address as assembly, and whether execution carries
/// on to the next one, or None when the translator doesn't take it.
fn lower(w: &Writer, address: u32, op: u32) -> Option<(String, bool)> {
    let condition = op >> 28;
    if condition == 0xF {
        let mut body = String::new();
        return unconditional(&mut body, w, address, op).map(|continues| (body, continues));
    }
    let mut body = String::new();
    let continues = instruction(&mut body, w, address, op)?;
    if condition == 0xE {
        return Some((body, continues));
    }
    let skip = w.local();
    let mut out = String::new();
    emit!(out, "\tb.{} {skip}", CONDITIONS[(condition ^ 1) as usize]);
    out.push_str(&body);
    emit!(out, "{skip}:");
    Some((out, true))
}

/// blx to Thumb, clrex, pld and the barriers.
fn unconditional(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    if op & 0xFE00_0000 == 0xFA00_0000 {
        let offset = (sign_extend_24(op & 0x00FF_FFFF) << 2) | (((op >> 24) & 1) << 1);
        return call(out, w, a + 4, (a + 8).wrapping_add(offset) & !1, true);
    }
    if op & 0xFFF0_00F0 == 0xF570_0010 {
        emit!(out, "\tstrb wzr, [x19, #{EXCLUSIVE}]");
        return Some(true);
    }
    matches!((op >> 25) & 7, 0b010 | 0b011).then_some(true)
}

fn instruction(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    match (op >> 25) & 7 {
        0b000 if op & 0x90 == 0x90 && (op >> 5) & 3 != 0 => extra_transfer(out, w, a, op),
        0b000 if op & 0x90 == 0x90 && op & (1 << 24) != 0 => None,
        0b000 if op & 0x90 == 0x90 => match (op >> 21) & 7 {
            0b000 | 0b001 => multiply(out, op),
            0b010 | 0b100..=0b111 => multiply_long(out, op),
            _ => None,
        },
        0b000 if (op >> 23) & 3 == 0b10 && op & (1 << 20) == 0 => match (op >> 4) & 0xF {
            0b0000 if op & (1 << 22) == 0 && op & (1 << 21) == 0 => mrs(out, op),
            0b0001 if (op >> 21) & 3 == 0b11 => clz(out, op),
            0b0001 | 0b0011 => branch_exchange(out, w, a, op),
            _ => None,
        },
        0b000 => data_processing(out, w, a, op),
        // nop and the other hints
        0b001 if (op >> 23) & 3 == 0b10 && op & (1 << 20) == 0 => (op & (1 << 21) != 0 && (op >> 16) & 0xF == 0).then_some(true),
        0b001 => data_processing(out, w, a, op),
        0b010 => single_transfer(out, w, a, op),
        0b011 if op & 0x10 == 0 => single_transfer(out, w, a, op),
        0b011 if (op >> 23) & 3 == 0b01 => pack_extend_reverse(out, op),
        0b011 if (op >> 23) & 3 == 0b00 => parallel(out, op),
        0b100 => block_transfer(out, w, a, op),
        0b101 => {
            let target = (a + 8).wrapping_add(sign_extend_24(op & 0x00FF_FFFF) << 2);
            if op & (1 << 24) != 0 { call(out, w, a + 4, target, false) } else { jump(out, w, target) }
        }
        0b110 => vfp_load_store(out, w, a, op),
        0b111 if op & (1 << 24) != 0 => svc(out, w, a, op),
        0b111 if op & 0x10 != 0 && matches!((op >> 8) & 0xF, 10 | 11) => vfp_transfer(out, w, op),
        0b111 if op & 0x10 != 0 => {
            let rd = (op >> 12) & 0xF;
            let data_cache = op & 0x0FFF_0F10 == 0x0E07_0F10 && !matches!(op & 0xF, 5 | 7 | 13);
            if op & 0x0FFF_0FFF == 0x0E1D_0F70 && rd != 15 {
                emit!(out, "\tldr w{rd}, [x19, #{TLS}]");
                Some(true)
            } else {
                data_cache.then_some(true)
            }
        }
        _ => vfp_data(out, w, op),
    }
}

/// the media instructions gcc and games use most: the extends with an add
/// or not, pkhbt and pkhtb, rev, rev16, rbit and revsh. saturating, sel and
/// the packed extends are the C's.
fn pack_extend_reverse(out: &mut String, op: u32) -> Option<bool> {
    let op1 = (op >> 20) & 0x1F;
    let op2 = (op >> 5) & 7;
    let (rd, rn, rm) = ((op >> 12) & 0xF, (op >> 16) & 0xF, op & 0xF);
    if rd == 15 || rm == 15 {
        return None;
    }
    let shift = (op >> 7) & 0x1F;
    match (op1, op2) {
        (0b01000, 0 | 2 | 4 | 6) => {
            if rn == 15 {
                return None;
            }
            if op & (1 << 6) != 0 {
                emit!(out, "\tasr w16, w{rm}, #{}", if shift == 0 { 31 } else { shift });
                emit!(out, "\tmov w17, w{rn}");
                emit!(out, "\tbfxil w17, w16, #0, #16");
            } else {
                emit!(out, "\tlsl w17, w{rm}, #{shift}");
                emit!(out, "\tbfxil w17, w{rn}, #0, #16");
            }
            emit!(out, "\tmov w{rd}, w17");
        }
        (0b01010 | 0b01011 | 0b01110 | 0b01111, 0b011) => {
            let rotate = ((op >> 10) & 3) * 8;
            if rotate == 0 {
                emit!(out, "\tmov w16, w{rm}");
            } else {
                emit!(out, "\tror w16, w{rm}, #{rotate}");
            }
            let extend = ["", "", "sxtb", "sxth", "", "", "uxtb", "uxth"][(op1 & 7) as usize];
            emit!(out, "\t{extend} w16, w16");
            if rn == 15 {
                emit!(out, "\tmov w{rd}, w16");
            } else {
                emit!(out, "\tadd w{rd}, w{rn}, w16");
            }
        }
        // sxtb16 and uxtb16, adding halves apart
        (0b01000 | 0b01100, 0b011) => {
            let rotate = ((op >> 10) & 3) * 8;
            if rotate == 0 {
                emit!(out, "\tmov w16, w{rm}");
            } else {
                emit!(out, "\tror w16, w{rm}, #{rotate}");
            }
            if op1 == 0b01100 {
                emit!(out, "\tand w16, w16, #0x00ff00ff");
            } else {
                emit!(out, "\tsxtb w17, w16");
                emit!(out, "\tand w17, w17, #0xffff");
                emit!(out, "\tlsr w15, w16, #16");
                emit!(out, "\tsxtb w15, w15");
                emit!(out, "\torr w16, w17, w15, lsl #16");
            }
            if rn == 15 {
                emit!(out, "\tmov w{rd}, w16");
            } else {
                emit!(out, "\tadd w17, w{rn}, w16");
                emit!(out, "\tlsr w15, w{rn}, #16");
                emit!(out, "\tadd w15, w15, w16, lsr #16");
                emit!(out, "\tbfi w17, w15, #16, #16");
                emit!(out, "\tmov w{rd}, w17");
            }
        }
        // sel, each byte from n where its ge bit is set and from m where not
        (0b01000, 0b101) => {
            if rn == 15 {
                return None;
            }
            emit!(out, "\tldrb w16, [x19, #{GE}]");
            for i in 0..4 {
                emit!(out, "\tubfx w15, w16, #{i}, #1");
                emit!(out, "\tneg w15, w15");
                emit!(out, "\tbfi w17, w15, #{}, #8", 8 * i);
            }
            emit!(out, "\tand w15, w{rn}, w17");
            emit!(out, "\tbic w16, w{rm}, w17");
            emit!(out, "\torr w{rd}, w15, w16");
        }
        (0b01011, 0b001) => emit!(out, "\trev w{rd}, w{rm}"),
        (0b01011, 0b101) => emit!(out, "\trev16 w{rd}, w{rm}"),
        (0b01111, 0b001) => emit!(out, "\trbit w{rd}, w{rm}"),
        (0b01111, 0b101) => {
            emit!(out, "\trev16 w16, w{rm}");
            emit!(out, "\tsxth w{rd}, w16");
        }
        _ => return None,
    }
    Some(true)
}

/// the parallel adds and subtracts, sadd16 to uhsub8, lane by lane as
/// media_lane does: each lane widened and added or subtracted at 32 bits,
/// halved or saturated, ge from the sum for the plain forms alone.
/// saturating narrows through v16, which leaves NZCV alone.
fn parallel(out: &mut String, op: u32) -> Option<bool> {
    let op1 = (op >> 20) & 0x1F;
    let op2 = (op >> 5) & 7;
    let (rd, rn, rm) = ((op >> 12) & 0xF, (op >> 16) & 0xF, op & 0xF);
    if [rd, rn, rm].contains(&15) || op1 & 3 == 0 || matches!(op2, 5 | 6) {
        return None;
    }
    let signed = op1 & 4 == 0;
    let (saturating, halving) = (op1 & 3 == 2, op1 & 3 == 3);
    let plain = !saturating && !halving;
    // each lane: where in n, where in m, its width and whether it subtracts
    let lanes: Vec<(u32, u32, u32, bool)> = if op2 <= 3 {
        let cross = op2 == 1 || op2 == 2;
        vec![(0, if cross { 16 } else { 0 }, 16, op2 == 1 || op2 == 3), (16, if cross { 0 } else { 16 }, 16, op2 == 2 || op2 == 3)]
    } else {
        (0..4).map(|i| (8 * i, 8 * i, 8, op2 == 7)).collect()
    };
    let extract = if signed { "sbfx" } else { "ubfx" };
    if plain {
        emit!(out, "\tmov w23, #0");
    }
    for (index, &(at_n, at_m, width, subtract)) in lanes.iter().enumerate() {
        emit!(out, "\t{extract} w16, w{rn}, #{at_n}, #{width}");
        emit!(out, "\t{extract} w17, w{rm}, #{at_m}, #{width}");
        emit!(out, "\t{} w16, w16, w17", if subtract { "sub" } else { "add" });
        if plain {
            if signed || subtract {
                emit!(out, "\tlsr w15, w16, #31");
                emit!(out, "\teor w15, w15, #1");
            } else {
                emit!(out, "\tlsr w15, w16, #{width}");
            }
            let bits = if width == 16 { 2 } else { 1 };
            for bit in 0..bits {
                emit!(out, "\torr w23, w23, w15, lsl #{}", index as u32 * bits + bit);
            }
        } else if halving {
            emit!(out, "\tasr w16, w16, #1");
        } else {
            emit!(out, "\tfmov s16, w16");
            match (signed, width) {
                (true, 16) => emit!(out, "\tsqxtn h16, s16"),
                (true, _) => {
                    emit!(out, "\tsqxtn h16, s16");
                    emit!(out, "\tsqxtn b16, h16");
                }
                (false, 16) => emit!(out, "\tsqxtun h16, s16"),
                (false, _) => {
                    emit!(out, "\tsqxtun h16, s16");
                    emit!(out, "\tuqxtn b16, h16");
                }
            }
            emit!(out, "\tfmov w16, s16");
        }
        emit!(out, "\tbfi w22, w16, #{at_n}, #{width}");
    }
    emit!(out, "\tmov w{rd}, w22");
    if plain {
        emit!(out, "\tstrb w23, [x19, #{GE}]");
    }
    Some(true)
}

/// an svc the host runs in place, the code going on unless it says not to.
fn svc(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    w.spill(out);
    emit!(out, "\tpn_flush");
    guest(out, w, "w16", a + 4);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    constant(out, "w1", op & 0x00FF_FFFF);
    emit!(out, "\tldr x16, [x19, #{HOST}]");
    emit!(out, "\tldr x16, [x16, #{SVC}]");
    emit!(out, "\tblr x16");
    emit!(out, "\tldr w16, [x19, #{EXIT}]");
    emit!(out, "\tcbnz w16, {}", w.stub(0));
    emit!(out, "\tpn_reload");
    w.fill(out);
    Some(true)
}

fn sreg(vd: u32, extra: bool) -> u32 {
    ((vd << 1) | extra as u32) & 31
}

fn dreg(vd: u32, extra: bool) -> u32 {
    ((extra as u32) << 4) | vd
}

/// a VFP register of either size into or out of v, doubles taking the two
/// singles at 2N.
fn vfp_load(out: &mut String, w: &Writer, double: bool, register: u32, v: u32) {
    if double {
        w.doubles.set(true);
        emit!(out, "\tldr d{v}, [x24, #{}]", 8 * (register & 15));
    } else if let Some(host) = w.cached(register, false) {
        emit!(out, "\tfmov s{v}, s{host}");
    } else {
        emit!(out, "\tldr s{v}, [x24, #{}]", 4 * register);
    }
}

fn vfp_store(out: &mut String, w: &Writer, double: bool, register: u32, v: u32) {
    if double {
        w.doubles.set(true);
        emit!(out, "\tstr d{v}, [x24, #{}]", 8 * (register & 15));
    } else if let Some(host) = w.cached(register, true) {
        emit!(out, "\tfmov s{host}, s{v}");
    } else {
        emit!(out, "\tstr s{v}, [x24, #{}]", 4 * register);
    }
}

/// a VFP register to read: the host register a cached single is in, else
/// loaded into v.
fn vfp_operand(out: &mut String, w: &Writer, double: bool, register: u32, v: u32) -> String {
    if let Some(host) = w.cached(register, false).filter(|_| !double) {
        return format!("s{host}");
    }
    vfp_load(out, w, double, register, v);
    format!("{}{v}", if double { 'd' } else { 's' })
}

/// where a result goes: the host register of a cached single, else v19,
/// which vfp_store writes back after.
fn vfp_result(w: &Writer, double: bool, register: u32) -> Option<String> {
    w.cached(register, true).filter(|_| !double).map(|host| format!("s{host}"))
}

/// notes a destination past the first bank, which a short vector would run
/// over: the block checks fpscr's length before it starts.
fn scalar(w: &Writer, double: bool, d: u32) {
    if d >= if double { 4 } else { 8 } {
        w.vector.set(true);
    }
}

/// the arithmetic, unary operations, comparisons and conversions, as gcc
/// writes vfp.rs's C: products rounded before they are added, never fused.
fn vfp_data(out: &mut String, w: &Writer, op: u32) -> Option<bool> {
    let coprocessor = (op >> 8) & 0xF;
    if coprocessor != 10 && coprocessor != 11 {
        return None;
    }
    let double = coprocessor == 11;
    let (vd, vn, vm) = ((op >> 12) & 0xF, (op >> 16) & 0xF, op & 0xF);
    let (d_bit, n_bit, m_bit) = (op & (1 << 22) != 0, op & (1 << 7) != 0, op & (1 << 5) != 0);
    let (rd, rn, rm) = if double { (dreg(vd, d_bit), dreg(vn, n_bit), dreg(vm, m_bit)) } else { (sreg(vd, d_bit), sreg(vn, n_bit), sreg(vm, m_bit)) };
    let negate = op & (1 << 6) != 0;
    let v = if double { 'd' } else { 's' };
    let operation = match ((op >> 20) & 0b1011, negate) {
        (0b1011, true) => return vfp_extension(out, w, op, double, rd, rm),
        (0b0000..=0b0011, _) | (0b1000, false) => ((op >> 20) & 3) << 1 | negate as u32 | ((op >> 23) & 1) << 3,
        _ => return None,
    };
    scalar(w, double, rd);
    let a = vfp_operand(out, w, double, rn, 16);
    let b = vfp_operand(out, w, double, rm, 17);
    let acc = if operation <= 3 { vfp_operand(out, w, double, rd, 18) } else { String::new() };
    let target = vfp_result(w, double, rd);
    let d = target.clone().unwrap_or(format!("{v}19"));
    if operation <= 3 {
        emit!(out, "\tfmul {v}19, {a}, {b}");
    }
    match operation {
        0 => emit!(out, "\tfadd {d}, {v}19, {acc}"),
        1 => emit!(out, "\tfsub {d}, {acc}, {v}19"),
        2 => emit!(out, "\tfsub {d}, {v}19, {acc}"),
        3 => {
            emit!(out, "\tfneg {v}18, {acc}");
            emit!(out, "\tfsub {d}, {v}18, {v}19");
        }
        4 => emit!(out, "\tfmul {d}, {a}, {b}"),
        5 => emit!(out, "\tfnmul {d}, {b}, {a}"),
        6 => emit!(out, "\tfadd {d}, {a}, {b}"),
        7 => emit!(out, "\tfsub {d}, {a}, {b}"),
        _ => emit!(out, "\tfdiv {d}, {a}, {b}"),
    }
    if target.is_none() {
        vfp_store(out, w, double, rd, 19);
    }
    Some(true)
}

fn vfp_extension(out: &mut String, w: &Writer, op: u32, double: bool, rd: u32, rm: u32) -> Option<bool> {
    let top = op & (1 << 7) != 0;
    let v = if double { 'd' } else { 's' };
    match ((op >> 16) & 0xF, top) {
        // vmov, vabs, vneg and vsqrt: moves and sign changes leave the bits
        // as they are
        (0b0000 | 0b0001, _) => {
            scalar(w, double, rd);
            let second = (op >> 16) & 1 != 0;
            if second && top {
                let m = vfp_operand(out, w, double, rm, 16);
                match vfp_result(w, double, rd) {
                    Some(d) => emit!(out, "\tfsqrt {d}, {m}"),
                    None => {
                        emit!(out, "\tfsqrt {v}19, {m}");
                        vfp_store(out, w, double, rd, 19);
                    }
                }
                return Some(true);
            }
            // a single kept in a register on either side moves as a float,
            // the sign changes as bit operations all the same
            if !double && (w.cache.borrow().as_ref().is_some_and(|cache| cache.contains_key(&rm) || cache.contains_key(&rd))) {
                let m = vfp_operand(out, w, false, rm, 16);
                let operation = match (second, top) {
                    (false, true) => "fabs",
                    (true, false) => "fneg",
                    _ => "fmov",
                };
                match vfp_result(w, false, rd) {
                    Some(d) => emit!(out, "\t{operation} {d}, {m}"),
                    None => {
                        emit!(out, "\t{operation} s19, {m}");
                        vfp_store(out, w, false, rd, 19);
                    }
                }
                return Some(true);
            }
            let change = |out: &mut String, register: &str| match (second, top) {
                (false, true) => emit!(out, "\tand {register}, {register}, #0x7fffffff"),
                (true, false) => emit!(out, "\teor {register}, {register}, #0x80000000"),
                _ => {}
            };
            if double {
                w.doubles.set(true);
                let (d, m) = (2 * (rd & 15), 2 * (rm & 15));
                emit!(out, "\tldp w16, w17, [x24, #{}]", 4 * m);
                change(out, "w17");
                emit!(out, "\tstp w16, w17, [x24, #{}]", 4 * d);
            } else {
                word_load(out, w, rm, "w16");
                change(out, "w16");
                word_store(out, w, rd, "w16");
            }
        }
        // vcmp against a register or zero, the result in fpscr's top bits
        // the way fcmp sets NZCV, the guest's flags kept
        (0b0100 | 0b0101, _) => {
            let d = vfp_operand(out, w, double, rd, 16);
            emit!(out, "\tmrs x23, nzcv");
            if (op >> 16) & 1 == 0 {
                let m = vfp_operand(out, w, double, rm, 17);
                emit!(out, "\tfcmp {d}, {m}");
            } else {
                emit!(out, "\tfcmp {d}, #0.0");
            }
            emit!(out, "\tmrs x16, nzcv");
            emit!(out, "\tmsr nzcv, x23");
            emit!(out, "\tldr w17, [x25]");
            emit!(out, "\tand w17, w17, #0x0fffffff");
            emit!(out, "\torr w17, w17, w16");
            emit!(out, "\tstr w17, [x25]");
        }
        // vcvt between single and double
        (0b0111, true) => {
            let (vd, vm) = ((op >> 12) & 0xF, op & 0xF);
            let (d_bit, m_bit) = (op & (1 << 22) != 0, op & (1 << 5) != 0);
            if double {
                vfp_load(out, w, true, rm, 16);
                emit!(out, "\tfcvt s19, d16");
                vfp_store(out, w, false, sreg(vd, d_bit), 19);
            } else {
                vfp_load(out, w, false, sreg(vm, m_bit), 16);
                emit!(out, "\tfcvt d19, s16");
                vfp_store(out, w, true, dreg(vd, d_bit), 19);
            }
        }
        // vcvt from an integer, bit 7 set for signed
        (0b1000, _) => {
            word_load(out, w, sreg(op & 0xF, op & (1 << 5) != 0), "w16");
            emit!(out, "\t{} {v}19, w16", if top { "scvtf" } else { "ucvtf" });
            vfp_store(out, w, double, rd, 19);
        }
        // vcvt to an integer rounding toward zero, saturating, NaN to 0
        (0b1100 | 0b1101, true) => {
            vfp_load(out, w, double, rm, 16);
            emit!(out, "\t{} w16, {v}16", if (op >> 16) & 1 != 0 { "fcvtzs" } else { "fcvtzu" });
            word_store(out, w, sreg((op >> 12) & 0xF, op & (1 << 22) != 0), "w16");
        }
        _ => return None,
    }
    Some(true)
}

/// vmrs, and the moves between a core register and a single. vmsr, which
/// changes how later VFP code runs, is the C's.
fn vfp_transfer(out: &mut String, w: &Writer, op: u32) -> Option<bool> {
    let load = op & (1 << 20) != 0;
    let rt = (op >> 12) & 0xF;
    match (op >> 21) & 7 {
        7 if (op >> 16) & 0xF == 1 && load => {
            if rt == 15 {
                emit!(out, "\tldr w16, [x25]");
                emit!(out, "\tand w16, w16, #0xf0000000");
                emit!(out, "\tmsr nzcv, x16");
            } else {
                emit!(out, "\tldr w{rt}, [x25]");
            }
            Some(true)
        }
        0 if rt != 15 => {
            let sn = sreg((op >> 16) & 0xF, op & (1 << 7) != 0);
            if load {
                word_load(out, w, sn, &format!("w{rt}"));
            } else {
                word_store(out, w, sn, &format!("w{rt}"));
            }
            Some(true)
        }
        _ => None,
    }
}

/// vldr, vstr, vldm and vstm, and the moves between two core registers and
/// a double or two singles, w17 walking up from the first address.
fn vfp_load_store(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let coprocessor = (op >> 8) & 0xF;
    if coprocessor != 10 && coprocessor != 11 {
        return None;
    }
    let double = coprocessor == 11;
    let load = op & (1 << 20) != 0;
    if (op >> 21) & 0x7F == 0b110_0010 {
        let (rt, rt2) = ((op >> 12) & 0xF, (op >> 16) & 0xF);
        if rt == 15 || rt2 == 15 {
            return None;
        }
        let (vm, m_bit) = (op & 0xF, op & (1 << 5) != 0);
        let (low, high) = if double {
            let dm = dreg(vm, m_bit);
            ((dm * 2) & 31, (dm * 2 + 1) & 31)
        } else {
            let sm = sreg(vm, m_bit);
            (sm, (sm + 1) & 31)
        };
        for (single, core) in [(low, rt), (high, rt2)] {
            if load {
                word_load(out, w, single, &format!("w{core}"));
            } else {
                word_store(out, w, single, &format!("w{core}"));
            }
        }
        return Some(true);
    }
    let pre = op & (1 << 24) != 0;
    let up = op & (1 << 23) != 0;
    let writeback = op & (1 << 21) != 0;
    let rn = (op >> 16) & 0xF;
    if writeback && rn == 15 {
        return None;
    }
    let offset = (op & 0xFF) * 4;
    let (vd, d_bit) = ((op >> 12) & 0xF, op & (1 << 22) != 0);
    let first = if double { dreg(vd, d_bit) } else { sreg(vd, d_bit) };
    let count = if pre && !writeback {
        1
    } else if double {
        (op & 0xFF) / 2
    } else {
        op & 0xFF
    };
    let base = if rn == 15 {
        guest(out, w, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    // each address from the base, which the transfers leave as it is: a
    // double's two words as one, a cached single straight to its register
    let from = match (up, pre && !writeback) {
        (true, true) => offset as i32,
        (true, false) => 0,
        (false, _) => -(offset as i32),
    };
    for i in 0..count.max(1) {
        let at = from + (i * if double { 8 } else { 4 }) as i32;
        let address = match at {
            0 => base.clone(),
            1.. => {
                emit!(out, "\tadd w17, {base}, #{at}");
                "w17".to_owned()
            }
            _ => {
                emit!(out, "\tsub w17, {base}, #{}", -at);
                "w17".to_owned()
            }
        };
        // as one unless the block keeps either single in a register
        let pair = if double { pair_singles(first + i).map(|single| w.cached(single, load)) } else { [None, None] };
        if double && pair == [None, None] {
            let slot = 8 * ((first + i) & 15);
            if load {
                emit!(out, "\tldr x16, [x20, {address}, uxtw]");
                emit!(out, "\tstr x16, [x24, #{slot}]");
            } else {
                emit!(out, "\tldr x16, [x24, #{slot}]");
                emit!(out, "\tstr x16, [x20, {address}, uxtw]");
            }
            continue;
        }
        let words = if double { vec![(pair[0], pair_singles(first + i)[0]), (pair[1], pair_singles(first + i)[1])] } else { vec![(w.cached((first + i) & 31, load), (first + i) & 31)] };
        for (j, (host, single)) in words.into_iter().enumerate() {
            let address = if j == 0 {
                address.clone()
            } else {
                emit!(out, "\tadd w22, {address}, #4");
                "w22".to_owned()
            };
            match (load, host) {
                (true, Some(host)) => emit!(out, "\tldr s{host}, [x20, {address}, uxtw]"),
                (false, Some(host)) => emit!(out, "\tstr s{host}, [x20, {address}, uxtw]"),
                (true, None) => {
                    emit!(out, "\tldr w16, [x20, {address}, uxtw]");
                    emit!(out, "\tstr w16, [x24, #{}]", 4 * single);
                }
                (false, None) => {
                    emit!(out, "\tldr w16, [x24, #{}]", 4 * single);
                    emit!(out, "\tstr w16, [x20, {address}, uxtw]");
                }
            }
        }
    }
    if writeback {
        emit!(out, "\t{} w{rn}, w{rn}, #{offset}", if up { "add" } else { "sub" });
    }
    Some(true)
}

/// the singles a double is made of.
fn pair_singles(double: u32) -> [u32; 2] {
    [(double * 2) & 31, (double * 2 + 1) & 31]
}

/// bx and blx through a register: bx lr returns, any other bx jumps, and
/// blx calls whatever the host has there.
fn branch_exchange(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let rm = op & 0xF;
    if rm == 15 {
        guest(out, w, "w22", a + 8);
    } else {
        emit!(out, "\tmov w22, w{rm}");
    }
    if (op >> 4) & 0xF == 0b0011 {
        let next = a + 4;
        guest(out, w, "w14", next);
        emit!(out, "\tpn_flush");
        emit!(out, "\tand w16, w22, #1");
        emit!(out, "\tstrb w16, [x19, #{THUMB}]");
        emit!(out, "\ttst w22, #1");
        emit!(out, "\tand w17, w22, #0xfffffffe");
        emit!(out, "\tand w16, w22, #0xfffffffc");
        emit!(out, "\tcsel w16, w17, w16, ne");
        emit!(out, "\tstr w16, [x19, #{R15}]");
        call_out(out, w, Callee::Lookup);
        returned(out, w, next);
        w.fill(out);
        return Some(true);
    }
    if rm == 14 {
        return_to(out, w);
    } else {
        jump_to(out, w);
    }
    Some(false)
}

/// mrs of the cpsr: the flags, q and ge over system mode.
fn mrs(out: &mut String, op: u32) -> Option<bool> {
    let rd = (op >> 12) & 0xF;
    if rd == 15 {
        return None;
    }
    emit!(out, "\tmrs x16, nzcv");
    emit!(out, "\tand w16, w16, #0xf0000000");
    emit!(out, "\tldrb w17, [x19, #{Q}]");
    emit!(out, "\torr w16, w16, w17, lsl #27");
    emit!(out, "\tldrb w17, [x19, #{GE}]");
    emit!(out, "\tand w17, w17, #0xf");
    emit!(out, "\torr w16, w16, w17, lsl #16");
    emit!(out, "\torr w{rd}, w16, #0x1f");
    Some(true)
}

fn clz(out: &mut String, op: u32) -> Option<bool> {
    let (rd, rm) = ((op >> 12) & 0xF, op & 0xF);
    if rd == 15 || rm == 15 {
        return None;
    }
    emit!(out, "\tclz w{rd}, w{rm}");
    Some(true)
}

/// mul and mla, which set N and Z alone.
fn multiply(out: &mut String, op: u32) -> Option<bool> {
    let (rd, rn, rs, rm) = ((op >> 16) & 0xF, (op >> 12) & 0xF, (op >> 8) & 0xF, op & 0xF);
    let accumulate = op & (1 << 21) != 0;
    if [rd, rs, rm].contains(&15) || accumulate && rn == 15 {
        return None;
    }
    if accumulate {
        emit!(out, "\tmadd w{rd}, w{rm}, w{rs}, w{rn}");
    } else {
        emit!(out, "\tmul w{rd}, w{rm}, w{rs}");
    }
    if op & (1 << 20) != 0 {
        logical_flags(out, &format!("w{rd}"), Carry::Kept);
    }
    Some(true)
}

/// umull, umlal, smull, smlal and umaal, hi written last.
fn multiply_long(out: &mut String, op: u32) -> Option<bool> {
    let (hi, lo, rs, rm) = ((op >> 16) & 0xF, (op >> 12) & 0xF, (op >> 8) & 0xF, op & 0xF);
    if [hi, lo, rs, rm].contains(&15) {
        return None;
    }
    let kind = (op >> 21) & 7;
    // the guest's registers are w, their top halves always clear
    match kind {
        0b010 => {
            emit!(out, "\tumaddl x16, w{rm}, w{rs}, x{lo}");
            emit!(out, "\tadd x16, x16, x{hi}");
        }
        0b100 => emit!(out, "\tumull x16, w{rm}, w{rs}"),
        0b110 => emit!(out, "\tsmull x16, w{rm}, w{rs}"),
        _ => {
            emit!(out, "\torr x23, x{lo}, x{hi}, lsl #32");
            let op = if kind == 0b101 { "umaddl" } else { "smaddl" };
            emit!(out, "\t{op} x16, w{rm}, w{rs}, x23");
        }
    }
    emit!(out, "\tlsr x17, x16, #32");
    emit!(out, "\tmov w{lo}, w16");
    emit!(out, "\tmov w{hi}, w17");
    if kind != 0b010 && op & (1 << 20) != 0 {
        logical_flags(out, "x16", Carry::Kept);
    }
    Some(true)
}

/// where the shifter's carry comes from, for the logical ops that set flags.
enum Carry {
    Kept,
    Constant(u32),
    /// in bit 0 of w17.
    Register,
}

/// the second operand into w16, and when carry the shifter's carry out.
fn shifter_operand(out: &mut String, w: &Writer, a: u32, op: u32, carry: bool) -> Option<Carry> {
    if op & (1 << 25) != 0 {
        let rotate = ((op >> 8) & 0xF) * 2;
        let value = (op & 0xFF).rotate_right(rotate);
        constant(out, "w16", value);
        return Some(if rotate == 0 { Carry::Kept } else { Carry::Constant(value >> 31) });
    }
    if op & (1 << 4) != 0 {
        return register_shift(out, w, op, carry);
    }
    let rm = op & 0xF;
    let kind = (op >> 5) & 3;
    let amount = (op >> 7) & 0x1F;
    if rm == 15 && w.relative {
        // in a module only as it is, the shift at run time not worth it
        if kind != 0 || amount != 0 {
            return None;
        }
        guest(out, w, "w16", a + 8);
        return Some(Carry::Kept);
    }
    if rm == 15 {
        // the address the pipeline gives r15, shifted here
        let m = a + 8;
        let (value, carry_out) = match (kind, amount) {
            (0, 0) => (m, None),
            (0, n) => (m << n, Some((m >> (32 - n)) & 1)),
            (1, 0) => (0, Some(m >> 31)),
            (1, n) => (m >> n, Some((m >> (n - 1)) & 1)),
            (2, 0) => (((m as i32) >> 31) as u32, Some(m >> 31)),
            (2, n) => (((m as i32) >> n) as u32, Some((m >> (n - 1)) & 1)),
            (_, 0) => return None,
            (_, n) => (m.rotate_right(n), Some((m >> (n - 1)) & 1)),
        };
        constant(out, "w16", value);
        return Some(match carry_out {
            None => Carry::Kept,
            Some(bit) => Carry::Constant(bit),
        });
    }
    let m = format!("w{rm}");
    let bit = |out: &mut String, at: u32| {
        if carry {
            emit!(out, "\tubfx w17, {m}, #{at}, #1");
        }
    };
    match (kind, amount) {
        (0, 0) => {
            emit!(out, "\tmov w16, {m}");
            return Some(Carry::Kept);
        }
        (0, n) => {
            bit(out, 32 - n);
            emit!(out, "\tlsl w16, {m}, #{n}");
        }
        (1, 0) => {
            bit(out, 31);
            emit!(out, "\tmov w16, wzr");
        }
        (1, n) => {
            bit(out, n - 1);
            emit!(out, "\tlsr w16, {m}, #{n}");
        }
        (2, 0) => {
            bit(out, 31);
            emit!(out, "\tasr w16, {m}, #31");
        }
        (2, n) => {
            bit(out, n - 1);
            emit!(out, "\tasr w16, {m}, #{n}");
        }
        // ror 0 is rrx, the carry in at the top
        (_, 0) => {
            bit(out, 0);
            emit!(out, "\tcset w16, hs");
            emit!(out, "\textr w16, w16, {m}, #1");
        }
        (_, n) => {
            bit(out, n - 1);
            emit!(out, "\tror w16, {m}, #{n}");
        }
    }
    Some(Carry::Register)
}

/// a shift by the bottom byte of a register, as shift_lsl and the others in
/// recomp.h: none keeps the value and the carry, 32 and past shift it all
/// out, ror goes round. the cases picked by branches, which leave NZCV as
/// it is, the carry kept read first.
fn register_shift(out: &mut String, w: &Writer, op: u32, carry: bool) -> Option<Carry> {
    let (rm, rs, kind) = (op & 0xF, (op >> 8) & 0xF, (op >> 5) & 3);
    if rm == 15 || rs == 15 {
        return None;
    }
    let m = format!("w{rm}");
    let (small, big, done) = (w.local(), w.local(), w.local());
    if carry {
        emit!(out, "\tcset w17, hs");
    }
    emit!(out, "\tand w15, w{rs}, #0xff");
    emit!(out, "\tmov w16, {m}");
    emit!(out, "\tcbz w15, {done}");
    if kind == 3 {
        emit!(out, "\tand w15, w15, #31");
        emit!(out, "\tcbnz w15, {small}");
        if carry {
            emit!(out, "\tlsr w17, {m}, #31");
        }
        emit!(out, "\tb {done}");
        emit!(out, "{small}:");
        emit!(out, "\tror w16, {m}, w15");
        if carry {
            emit!(out, "\tsub w22, w15, #1");
            emit!(out, "\tlsr w17, {m}, w22");
            emit!(out, "\tand w17, w17, #1");
        }
    } else {
        emit!(out, "\tsub w22, w15, #32");
        emit!(out, "\ttbz w22, #31, {big}");
        let shift = ["lsl", "lsr", "asr"][kind as usize];
        emit!(out, "\t{shift} w16, {m}, w15");
        if carry {
            if kind == 0 {
                emit!(out, "\tmov w22, #32");
                emit!(out, "\tsub w22, w22, w15");
            } else {
                emit!(out, "\tsub w22, w15, #1");
            }
            emit!(out, "\tlsr w17, {m}, w22");
            emit!(out, "\tand w17, w17, #1");
        }
        emit!(out, "\tb {done}");
        // 32 and past, w22 how far past
        emit!(out, "{big}:");
        match kind {
            0 | 1 => {
                emit!(out, "\tmov w16, wzr");
                if carry {
                    if kind == 0 {
                        emit!(out, "\tand w17, {m}, #1");
                    } else {
                        emit!(out, "\tlsr w17, {m}, #31");
                    }
                    emit!(out, "\tcbz w22, {done}");
                    emit!(out, "\tmov w17, wzr");
                }
            }
            _ => {
                emit!(out, "\tasr w16, {m}, #31");
                if carry {
                    emit!(out, "\tlsr w17, {m}, #31");
                }
            }
        }
    }
    emit!(out, "{done}:");
    Some(Carry::Register)
}

/// N and Z from the result in register, C from carry and V as it was, as
/// the logical ops set them.
fn logical_flags(out: &mut String, register: &str, carry: Carry) {
    emit!(out, "\tmrs x23, nzcv");
    match carry {
        Carry::Kept => emit!(out, "\tand w23, w23, #0x30000000"),
        Carry::Constant(bit) => {
            emit!(out, "\tand w23, w23, #0x10000000");
            if bit != 0 {
                emit!(out, "\torr w23, w23, #0x20000000");
            }
        }
        Carry::Register => {
            emit!(out, "\tand w23, w23, #0x10000000");
            emit!(out, "\torr w23, w23, w17, lsl #29");
        }
    }
    emit!(out, "\ttst {register}, {register}");
    emit!(out, "\tmrs x16, nzcv");
    emit!(out, "\torr w23, w23, w16");
    emit!(out, "\tmsr nzcv, x23");
}

/// whether AArch64's logical instructions take value as an immediate: a
/// repeated element that is a rotated run of ones, neither all nor none.
fn bitmask(value: u32) -> bool {
    if value == 0 || value == u32::MAX {
        return false;
    }
    let mut size = 32;
    while size > 2 && (value & ((1 << (size / 2)) - 1)) == (value >> (size / 2)) & ((1 << (size / 2)) - 1) {
        size /= 2;
    }
    let mask = if size == 32 { u32::MAX } else { (1 << size) - 1 };
    let element = (value & mask) as u64;
    (0..size).any(|r| {
        let rotated = ((element >> r) | (element << (size - r))) & mask as u64;
        rotated & (rotated + 1) == 0
    })
}

/// the second operand as AArch64 takes it straight, an immediate or a
/// register shifted by a constant, when the operation has such a form.
fn direct_operand(op: u32, opcode: u32) -> Option<String> {
    let arithmetic = matches!(opcode, 2 | 4 | 10 | 11);
    let logical = matches!(opcode, 0 | 1 | 8 | 9 | 12);
    if op & (1 << 25) != 0 {
        let value = (op & 0xFF).rotate_right(((op >> 8) & 0xF) * 2);
        return match () {
            _ if arithmetic && value <= 4095 => Some(format!("#{value}")),
            _ if arithmetic && value & 0xFFF == 0 && value >> 12 <= 4095 => Some(format!("#{}, lsl #12", value >> 12)),
            _ if logical && bitmask(value) => Some(format!("#0x{value:x}")),
            _ => None,
        };
    }
    let rm = op & 0xF;
    if op & (1 << 4) != 0 || rm == 15 {
        return None;
    }
    let m = format!("w{rm}");
    let shifted = arithmetic || logical || opcode == 14;
    match ((op >> 5) & 3, (op >> 7) & 0x1F) {
        (0, 0) => Some(m),
        (0, n) if shifted => Some(format!("{m}, lsl #{n}")),
        (1, n) if shifted && n > 0 => Some(format!("{m}, lsr #{n}")),
        (2, n) if shifted && n > 0 => Some(format!("{m}, asr #{n}")),
        (3, n) if (logical || opcode == 14) && n > 0 => Some(format!("{m}, ror #{n}")),
        _ => None,
    }
}

/// the shifter's carry for an operand direct_operand takes, into w17 when
/// it comes from the register.
fn direct_carry(out: &mut String, op: u32) -> Carry {
    if op & (1 << 25) != 0 {
        let rotate = ((op >> 8) & 0xF) * 2;
        return if rotate == 0 { Carry::Kept } else { Carry::Constant((op & 0xFF).rotate_right(rotate) >> 31) };
    }
    let (rm, kind, amount) = (op & 0xF, (op >> 5) & 3, (op >> 7) & 0x1F);
    if amount == 0 {
        return Carry::Kept;
    }
    let at = if kind == 0 { 32 - amount } else { amount - 1 };
    emit!(out, "\tubfx w17, w{rm}, #{at}, #1");
    Carry::Register
}

/// mov and mvn of an operand AArch64 takes straight, into d.
fn direct_move(out: &mut String, op: u32, d: &str, invert: bool) -> bool {
    if op & (1 << 25) != 0 {
        let value = (op & 0xFF).rotate_right(((op >> 8) & 0xF) * 2);
        constant(out, d, if invert { !value } else { value });
        return true;
    }
    let rm = op & 0xF;
    if op & (1 << 4) != 0 || rm == 15 {
        return false;
    }
    let m = format!("w{rm}");
    let shift = match ((op >> 5) & 3, (op >> 7) & 0x1F) {
        (0, 0) => None,
        (0, n) => Some(("lsl", n)),
        (1, n) if n > 0 => Some(("lsr", n)),
        (2, n) if n > 0 => Some(("asr", n)),
        (3, n) if n > 0 => Some(("ror", n)),
        _ => return false,
    };
    match (shift, invert) {
        (None, false) => emit!(out, "\tmov {d}, {m}"),
        (None, true) => emit!(out, "\tmvn {d}, {m}"),
        (Some((kind, n)), false) => emit!(out, "\t{kind} {d}, {m}, #{n}"),
        (Some((kind, n)), true) => emit!(out, "\tmvn {d}, {m}, {kind} #{n}"),
    }
    true
}

fn data_processing(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let opcode = (op >> 21) & 0xF;
    let set_flags = op & (1 << 20) != 0;
    let rn = (op >> 16) & 0xF;
    let rd = (op >> 12) & 0xF;
    let compare = (8..=11).contains(&opcode);
    if set_flags && rd == 15 && !compare {
        return None;
    }
    let logical = matches!(opcode, 0 | 1 | 8 | 9 | 12..=15);
    // a shift by a register reads r15 further on, which the C has
    if op & (1 << 25) == 0 && op & (1 << 4) != 0 && rn == 15 && !matches!(opcode, 13 | 15) {
        return None;
    }
    let d = if compare || rd == 15 { "w22".to_owned() } else { format!("w{rd}") };
    let direct = if matches!(opcode, 13 | 15) { None } else { direct_operand(op, opcode) };
    // the carry from the register before the result can overwrite it
    let mut carry = Carry::Kept;
    let moved = matches!(opcode, 13 | 15) && {
        let mut text = String::new();
        if set_flags {
            carry = direct_carry(&mut text, op);
        }
        let done = direct_move(&mut text, op, &d, opcode == 15);
        if done {
            out.push_str(&text);
        }
        done
    };
    let second = match &direct {
        Some(operand) => {
            if set_flags && logical {
                carry = direct_carry(out, op);
            }
            operand.clone()
        }
        None if moved => String::new(),
        None => {
            carry = shifter_operand(out, w, a, op, set_flags && logical)?;
            "w16".to_owned()
        }
    };
    let first = if matches!(opcode, 13 | 15) {
        String::new()
    } else if rn == 15 {
        guest(out, w, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    let s = if set_flags { "s" } else { "" };
    let b = second;
    match opcode {
        0 | 8 => emit!(out, "\tand {d}, {first}, {b}"),
        1 | 9 => emit!(out, "\teor {d}, {first}, {b}"),
        2 => emit!(out, "\tsub{s} {d}, {first}, {b}"),
        3 => emit!(out, "\tsub{s} {d}, {b}, {first}"),
        4 => emit!(out, "\tadd{s} {d}, {first}, {b}"),
        5 => emit!(out, "\tadc{s} {d}, {first}, {b}"),
        6 => emit!(out, "\tsbc{s} {d}, {first}, {b}"),
        7 => emit!(out, "\tsbc{s} {d}, {b}, {first}"),
        10 => emit!(out, "\tcmp {first}, {b}"),
        11 => emit!(out, "\tcmn {first}, {b}"),
        12 => emit!(out, "\torr {d}, {first}, {b}"),
        _ if moved => {}
        13 => emit!(out, "\tmov {d}, {b}"),
        14 => emit!(out, "\tbic {d}, {first}, {b}"),
        _ => emit!(out, "\tmvn {d}, {b}"),
    }
    if set_flags && logical {
        logical_flags(out, &d, carry);
    }
    if compare || rd != 15 {
        return Some(true);
    }
    // mov pc, lr returns, any other write to pc jumps
    emit!(out, "\tand w22, w22, #0xfffffffc");
    if opcode == 13 && op & 0x0200_0FFF == 14 {
        return_to(out, w);
    } else {
        dispatch(out, w);
    }
    Some(false)
}

/// ldr, str, ldrb and strb.
fn single_transfer(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let rn = (op >> 16) & 0xF;
    let rd = (op >> 12) & 0xF;
    let pre = op & (1 << 24) != 0;
    let up = op & (1 << 23) != 0;
    let byte = op & (1 << 22) != 0;
    let load = op & (1 << 20) != 0;
    let writeback = !pre || op & (1 << 21) != 0;
    let register_offset = op & (1 << 25) != 0;
    if writeback && rn == 15 || register_offset && op & 0xF == 15 {
        return None;
    }
    // ldr pc, [sp], 4 is a pop, any other load of pc the C's
    let pop = rn == 13 && !pre && up && !register_offset && op & 0xFFF == 4;
    if load && rd == 15 && !pop {
        return None;
    }
    let base = if rn == 15 {
        guest(out, w, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    let sign = if up { "add" } else { "sub" };
    if register_offset {
        let m = format!("w{}", op & 0xF);
        match ((op >> 5) & 3, (op >> 7) & 0x1F) {
            (0, n) => emit!(out, "\t{sign} w17, {base}, {m}, lsl #{n}"),
            (1, 0) => emit!(out, "\tmov w17, {base}"),
            (1, n) => emit!(out, "\t{sign} w17, {base}, {m}, lsr #{n}"),
            (2, 0) => emit!(out, "\t{sign} w17, {base}, {m}, asr #31"),
            (2, n) => emit!(out, "\t{sign} w17, {base}, {m}, asr #{n}"),
            (_, 0) => {
                emit!(out, "\tcset w16, hs");
                emit!(out, "\textr w16, w16, {m}, #1");
                emit!(out, "\t{sign} w17, {base}, w16");
            }
            (_, n) => {
                emit!(out, "\tror w16, {m}, #{n}");
                emit!(out, "\t{sign} w17, {base}, w16");
            }
        }
    } else {
        emit!(out, "\t{sign} w17, {base}, #{}", op & 0xFFF);
    }
    let address = if pre { "w17" } else { base.as_str() };
    let suffix = if byte { "b" } else { "" };
    if load {
        // straight into rd, which a writeback of the same register gives way to
        let into = if rd == 15 { "w22".to_owned() } else { format!("w{rd}") };
        emit!(out, "\tldr{suffix} {into}, [x20, {address}, uxtw]");
        if writeback && rn != rd {
            emit!(out, "\tmov w{rn}, w17");
        }
        if rd == 15 {
            return_to(out, w);
            return Some(false);
        }
    } else {
        // storing r15 stores the address plus 12
        let value = if rd == 15 {
            guest(out, w, "w16", a + 12);
            "w16".to_owned()
        } else {
            format!("w{rd}")
        };
        emit!(out, "\tstr{suffix} {value}, [x20, {address}, uxtw]");
        if writeback {
            emit!(out, "\tmov w{rn}, w17");
        }
    }
    Some(true)
}

/// ldrh, strh, ldrsb, ldrsh, ldrd and strd.
fn extra_transfer(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let rn = (op >> 16) & 0xF;
    let rd = (op >> 12) & 0xF;
    let pre = op & (1 << 24) != 0;
    let load = op & (1 << 20) != 0;
    let writeback = !pre || op & (1 << 21) != 0;
    let kind = (op >> 5) & 3;
    let pair = kind >= 2 && !load;
    if (writeback && rn == 15) || (load && rd == 15) || (pair && (rd & 1 != 0 || rd == 14)) {
        return None;
    }
    let offset = if op & (1 << 22) != 0 {
        format!("#{}", ((op >> 4) & 0xF0) | (op & 0xF))
    } else if op & 0xF == 15 {
        guest(out, w, "w16", a + 8);
        "w16".to_owned()
    } else {
        format!("w{}", op & 0xF)
    };
    let base = if rn == 15 {
        guest(out, w, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    let sign = if op & (1 << 23) != 0 { "add" } else { "sub" };
    emit!(out, "\t{sign} w17, {base}, {offset}");
    let address = if pre { "w17" } else { base.as_str() };
    let back = |out: &mut String| {
        if writeback {
            emit!(out, "\tmov w{rn}, w17");
        }
    };
    match (kind, load) {
        (1, true) | (2, true) | (3, true) => {
            let read = ["", "ldrh", "ldrsb", "ldrsh"][kind as usize];
            // the loaded value wins over a writeback of the same register
            if writeback && rn == rd {
                emit!(out, "\t{read} w16, [x20, {address}, uxtw]");
                back(out);
                emit!(out, "\tmov w{rd}, w16");
            } else {
                emit!(out, "\t{read} w{rd}, [x20, {address}, uxtw]");
                back(out);
            }
        }
        (1, false) => {
            // a halfword of r15 stores the address plus 8
            let value = if rd == 15 {
                guest(out, w, "w16", a + 8);
                "w16".to_owned()
            } else {
                format!("w{rd}")
            };
            emit!(out, "\tstrh {value}, [x20, {address}, uxtw]");
            back(out);
        }
        (2, false) => {
            emit!(out, "\tldr w16, [x20, {address}, uxtw]");
            emit!(out, "\tadd w22, {address}, #4");
            emit!(out, "\tldr w23, [x20, w22, uxtw]");
            back(out);
            emit!(out, "\tmov w{rd}, w16");
            emit!(out, "\tmov w{}, w23", rd + 1);
        }
        _ => {
            emit!(out, "\tstr w{rd}, [x20, {address}, uxtw]");
            emit!(out, "\tadd w22, {address}, #4");
            emit!(out, "\tstr w{}, [x20, w22, uxtw]", rd + 1);
            back(out);
        }
    }
    Some(true)
}

/// ldm and stm, w17 walking up from the lowest address, w23 the base after.
fn block_transfer(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let rn = (op >> 16) & 0xF;
    let pre = op & (1 << 24) != 0;
    let up = op & (1 << 23) != 0;
    let writeback = op & (1 << 21) != 0;
    let load = op & (1 << 20) != 0;
    let list = op & 0xFFFF;
    if op & (1 << 22) != 0 || list == 0 || rn == 15 {
        return None;
    }
    let span = list.count_ones() * 4;
    let base = format!("w{rn}");
    match (up, pre) {
        (true, false) => emit!(out, "\tmov w17, {base}"),
        (true, true) => emit!(out, "\tadd w17, {base}, #4"),
        (false, false) => emit!(out, "\tsub w17, {base}, #{}", span - 4),
        (false, true) => emit!(out, "\tsub w17, {base}, #{span}"),
    }
    emit!(out, "\t{} w23, {base}, #{span}", if up { "add" } else { "sub" });
    let registers: Vec<u32> = (0..16).filter(|i| list & (1 << i) != 0).collect();
    if load {
        if writeback && list & (1 << rn) == 0 {
            emit!(out, "\tmov w{rn}, w23");
        }
        for &register in &registers {
            if register == 15 {
                emit!(out, "\tldr w22, [x20, w17, uxtw]");
                if rn == 13 {
                    return_to(out, w);
                } else {
                    jump_to(out, w);
                }
                return Some(false);
            }
            emit!(out, "\tldr w{register}, [x20, w17, uxtw]");
            emit!(out, "\tadd w17, w17, #4");
        }
    } else {
        for &register in &registers {
            let value = if register == 15 {
                guest(out, w, "w16", a + 12);
                "w16".to_owned()
            } else if register == rn && writeback && list & ((1 << rn) - 1) != 0 {
                // the base when it is not the lowest register stores its new value
                "w23".to_owned()
            } else {
                format!("w{register}")
            };
            emit!(out, "\tstr {value}, [x20, w17, uxtw]");
            emit!(out, "\tadd w17, w17, #4");
        }
        if writeback {
            emit!(out, "\tmov w{rn}, w23");
        }
    }
    Some(true)
}

/// a function's blocks, each as its assembly or None for one the C runs,
/// with how many instructions it holds.
fn blocks(program: &Program, w: &Writer, function: &Function) -> Vec<(u32, usize, Option<String>, bool)> {
    let instructions = &function.instructions;
    let mut blocks = Vec::new();
    let mut i = instructions.iter().position(|a| w.labels.contains(a)).unwrap_or(instructions.len());
    while i < instructions.len() {
        let start = instructions[i];
        let run = instructions[i + 1..].iter().take_while(|a| !w.labels.contains(a)).count() + 1;
        let text = block(program, w, &instructions[i..], run);
        blocks.push((start, run, text, w.vector.get()));
        i += run;
    }
    blocks
}

/// a block's assembly: written once to see which singles it uses, then
/// again with them kept in host registers when it works on no doubles and
/// they fit, read at its start and written back on every way out.
fn block(program: &Program, w: &Writer, instructions: &[u32], run: usize) -> Option<String> {
    *w.cache.borrow_mut() = None;
    w.used.borrow_mut().clear();
    w.written.borrow_mut().clear();
    w.doubles.set(false);
    // the cold code of a writing thrown away goes too
    let mark = w.cold.borrow().len();
    let Some(plain) = block_text(program, w, instructions, run) else {
        w.cold.borrow_mut().truncate(mark);
        return None;
    };
    // the most used when they don't all fit, the rest left in memory, as
    // are those used once, reading and writing them back costing more
    let mut used: Vec<(u32, usize)> = w.used.borrow().iter().filter(|&(_, &uses)| uses > 1).map(|(&single, &uses)| (single, uses)).collect();
    if used.is_empty() || w.doubles.get() {
        return Some(plain);
    }
    used.sort_by_key(|&(single, uses)| (std::cmp::Reverse(uses), single));
    *w.cache.borrow_mut() = Some(used.iter().map(|&(single, _)| single).zip(CACHE).collect());
    w.cold.borrow_mut().truncate(mark);
    let cached = block_text(program, w, instructions, run);
    let mut text = String::new();
    w.fill(&mut text);
    *w.cache.borrow_mut() = None;
    let Some(cached) = cached else {
        w.cold.borrow_mut().truncate(mark);
        return None;
    };
    text.push_str(&cached);
    Some(text)
}

fn block_text(program: &Program, w: &Writer, instructions: &[u32], run: usize) -> Option<String> {
    let mut body = String::new();
    w.vector.set(false);
    let mut falls = false;
    for (j, &address) in instructions[..run].iter().enumerate() {
        let op = program.text.read32(address).unwrap_or(0);
        let (asm, continues) = lower(w, address, op)?;
        body.push_str(&asm);
        let next = address + 4;
        falls = continues && instructions.get(j + 1) == Some(&next);
        if continues && !falls {
            guest(&mut body, w, "w22", next);
            dispatch(&mut body, w);
        }
    }
    // on into the next block, which reads its own
    if falls {
        w.spill(&mut body);
    }
    Some(body)
}

/// how many of the function's instructions are in blocks the translator
/// takes, none when its entry's block isn't one.
pub fn coverage(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>, prefix: &str) -> usize {
    if function.mode != Mode::Arm {
        return 0;
    }
    let w = Writer::new(entry, labels, functions, String::new(), prefix);
    let blocks = blocks(program, &w, function);
    if !blocks.iter().any(|(start, _, text, _)| *start == entry && text.is_some()) {
        return 0;
    }
    blocks.iter().filter(|(_, _, text, _)| text.is_some()).map(|(_, run, ..)| run).sum()
}

/// what keeps the translator off each block it doesn't take: the kind of
/// its first instruction it can't, with how many instructions the block holds.
pub fn refusals(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>, prefix: &str) -> Vec<(&'static str, usize)> {
    if function.mode != Mode::Arm {
        return vec![("thumb", function.instructions.len())];
    }
    let w = Writer::new(entry, labels, functions, String::new(), prefix);
    let instructions = &function.instructions;
    blocks(program, &w, function)
        .into_iter()
        .filter(|(_, _, text, _)| text.is_none())
        .map(|(start, run, ..)| {
            let at = instructions.iter().position(|&a| a == start).unwrap_or(0);
            let refused = instructions[at..at + run].iter().map(|&a| program.text.read32(a).unwrap_or(0)).find(|&op| lower(&w, start, op).is_none());
            (refused.map_or("other", kind), run)
        })
        .collect()
}

/// the kind of an instruction, for saying what the translator lacks.
fn kind(op: u32) -> &'static str {
    if op >> 28 == 0xF {
        return "unconditional space";
    }
    match (op >> 25) & 7 {
        0b000 if op & 0x90 == 0x90 && (op >> 5) & 3 != 0 => "halfword, signed and double transfers",
        0b000 if op & 0x90 == 0x90 && op & (1 << 24) != 0 => "swap and exclusive",
        0b000 if op & 0x90 == 0x90 => "multiplies",
        0b000 if (op >> 23) & 3 == 0b10 && op & (1 << 20) == 0 => "status, bx, clz and the rest",
        0b000 if op & 0x10 != 0 => "shifts by a register",
        0b001 if (op >> 23) & 3 == 0b10 && op & (1 << 20) == 0 => "msr",
        0b000 | 0b001 => "data processing writing pc",
        0b010 | 0b011 if op & (1 << 25) != 0 && op & 0x10 != 0 => match ((op >> 23) & 3, (op >> 20) & 7, (op >> 5) & 7) {
            (0b00, ..) => "media: parallel adds and subtracts",
            (0b01, 0b000, 0b011) | (0b01, 0b100, 0b011) => "media: packed extends",
            (0b01, 0b000, 0b101) => "media: sel",
            (0b01, ..) => "media: saturating",
            (0b10, ..) => "media: dual multiplies",
            _ => "media: usad8 and the rest",
        },
        0b010 | 0b011 => "loads of pc and literal writebacks",
        0b100 => "ldm and stm",
        0b101 => "calls to unknown targets",
        0b110 => "vfp transfers",
        0b111 if op & (1 << 24) != 0 => "svc",
        _ => "vfp and coprocessor",
    }
}

/// whether the function is worth translating: ARM code whose entry the
/// translator takes, with half its instructions or more.
pub fn worth(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>, prefix: &str) -> bool {
    let translated = coverage(program, entry, function, labels, functions, prefix);
    // short of the 1 MiB conditional branches reach to the stubs past the end
    translated > 0 && translated * 2 >= function.instructions.len() && function.instructions.len() <= 20_000
}

/// the function's assembly, entered at its entry as the C is, the C named
/// c_name running it from anywhere else.
pub fn write(out: &mut String, program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>, c_name: &str, prefix: &str) {
    let w = Writer::new(entry, labels, functions, c_name.to_owned(), prefix);
    let name = name(prefix, entry);
    let elsewhere = w.local();
    emit!(out, "\t.globl {name}");
    emit!(out, "\t.hidden {name}");
    emit!(out, "\t.type {name}, %function");
    emit!(out, "\t.p2align 4");
    emit!(out, "{name}:");
    // anywhere but the entry is the C's. a module's base goes in w27 once
    // the caller's is saved, the guest's registers free till then
    emit!(out, "\tldr w16, [x0, #{R15}]");
    emit!(out, "\tldrb w17, [x0, #{THUMB}]");
    if w.relative {
        emit!(out, "\tadrp x15, {prefix}base");
        emit!(out, "\tldr w15, [x15, :lo12:{prefix}base]");
        constant(out, "w1", entry);
        emit!(out, "\tadd w1, w1, w15");
        emit!(out, "\tcmp w16, w1");
    } else {
        constant(out, "w15", entry);
        emit!(out, "\tcmp w16, w15");
    }
    emit!(out, "\tccmp w17, #0, #0, eq");
    emit!(out, "\tb.ne {elsewhere}");
    emit!(out, "\tpn_enter");
    if w.relative {
        emit!(out, "\tmov w27, w15");
    }
    emit!(out, "\tpn_reload");
    emit!(out, "\tb {}", w.block(entry));
    let inner = format!("{prefix}b_{entry:08X}");
    emit!(out, "\t.globl {inner}");
    emit!(out, "\t.hidden {inner}");
    emit!(out, "\t.type {inner}, %function");
    emit!(out, "{inner}:");
    emit!(out, "\tpn_enter_inner");
    emit!(out, "\tb {}", w.block(entry));
    // the way the code runs on falls through, what it seldom does in stubs
    // past the end
    let (mut body, mut stubs) = (String::new(), String::new());
    for (start, run, text, vector) in blocks(program, &w, function) {
        emit!(body, "{}:", w.block(start));
        match text {
            Some(text) => {
                let out_of_budget = w.local();
                // a short vector is the C's
                if vector {
                    let short = w.local();
                    emit!(body, "\tldr w16, [x25]");
                    emit!(body, "\tand w16, w16, #0x{FPSCR_LEN:x}");
                    emit!(body, "\tcbnz w16, {short}");
                    emit!(stubs, "{short}:");
                    fallback(&mut stubs, &w, start);
                }
                if run < 4096 {
                    emit!(body, "\tsub w21, w21, #{run}");
                } else {
                    constant(&mut body, "w16", run as u32);
                    emit!(body, "\tsub w21, w21, w16");
                }
                emit!(body, "{TEST_MARK}tbnz w21, #31, {out_of_budget}");
                body.push_str(&text);
                emit!(stubs, "{out_of_budget}:");
                guest(&mut stubs, &w, "w22", start);
                constant(&mut stubs, "w23", run as u32);
                emit!(stubs, "\tb pn_budget_sub");
            }
            None => fallback(&mut body, &w, start),
        }
    }
    // past the last instruction is wherever it goes on to, then what the
    // code seldom does
    let mut tail = stubs;
    tail.push_str(&w.cold.borrow());
    emit!(tail, "{elsewhere}:");
    emit!(tail, "\tb {c_name}");
    let shared = w.shared.get();
    if shared[0] {
        emit!(tail, "{}:", w.stub(0));
        emit!(tail, "\tb pn_leave_sub");
    }
    if shared[1] {
        emit!(tail, "{}:", w.stub(1));
        emit!(tail, "\tmov x0, x19");
        emit!(tail, "\tpn_leave");
        emit!(tail, "\tb {c_name}");
    }
    if shared[2] {
        emit!(tail, "{}:", w.stub(2));
        emit!(tail, "\tsub w16, w16, #1");
        emit!(tail, "\tstr w16, [x19, #{DEPTH}]");
        emit!(tail, "\tmov w16, #{EXIT_UNWIND}");
        emit!(tail, "\tstr w16, [x19, #{EXIT}]");
        emit!(tail, "\tb pn_leave_sub");
    }
    if shared[3] {
        emit!(tail, "{}:", w.stub(3));
        depth_back(&mut tail);
        emit!(tail, "\tb pn_leave_sub");
    }
    // tbz and tbnz reach 32 KiB: past that, round a b
    let near = instructions(&body) + instructions(&tail) < NEAR;
    for line in body.lines().chain(tail.lines()) {
        match line.strip_prefix(TEST_MARK) {
            Some(test) if near => emit!(out, "\t{test}"),
            Some(test) => {
                let (op, rest) = test.split_once(' ').expect("a test and its operands");
                let (operands, label) = rest.rsplit_once(", ").expect("a label");
                let on = w.local();
                emit!(out, "\t{} {operands}, {on}", if op == "tbz" { "tbnz" } else { "tbz" });
                emit!(out, "\tb {label}");
                emit!(out, "{on}:");
            }
            None => emit!(out, "{line}"),
        }
    }
    emit!(out, "\t.size {name}, .-{name}\n");
}

/// what write marks a tbz or tbnz with, for a branch it picks once it knows
/// how far its label is.
const TEST_MARK: &str = "#test ";
/// instructions tbz reaches past, with room to spare.
const NEAR: usize = 7000;
/// about how many instructions assembly holds, the macros counted as their
/// longest.
fn instructions(text: &str) -> usize {
    text.lines()
        .filter(|line| (line.starts_with('\t') && !line.starts_with("\t.")) || line.starts_with(TEST_MARK))
        .map(|line| if line.starts_with("\tpn_") { 10 } else { 1 })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::{generate_with, Unit};
    use crate::discover::{self, Source};
    use crate::image::Segment;
    use std::process::Command;

    const BASE: u32 = 0x0010_0000;
    const CALLEE: u32 = BASE + 0x800;
    /// a Thumb function: adds r0, 1, then bx lr.
    const THUMB_CALLEE: u32 = BASE + 0xC00;
    /// where the harness loads a module, and where a module's copy of a
    /// program starts in it.
    const MODULE_BASE: u32 = 0x0DA5_C000;
    const MODULE_OFFSET: u32 = 0xFA0;

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }

        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }

        fn condition(&mut self) -> u32 {
            if self.below(4) == 0 { self.below(14) } else { 14 }
        }

    }

    fn data_processing(r: &mut Random) -> u32 {
        let opcode = r.below(16);
        // the compares without s are other instructions
        let s = if (8..=11).contains(&opcode) { 1 } else { r.below(2) };
        let rn = if r.below(16) == 0 { 15 } else { r.below(15) };
        let rd = r.below(11);
        let operand = match r.below(32) {
            0..=15 => 1 << 25 | r.below(16) << 8 | r.below(256),
            16..=27 => {
                let rm = if r.below(16) == 0 { 15 } else { r.below(15) };
                r.below(32) << 7 | r.below(4) << 5 | rm
            }
            // shifts by a register
            _ => r.below(15) << 8 | r.below(4) << 5 | 1 << 4 | r.below(15),
        };
        r.condition() << 28 | opcode << 21 | s << 20 | rn << 16 | rd << 12 | operand
    }

    /// data processing shifting by a register whose value is small, an odd
    /// one the harness starts near 32, setting the flags most of the time.
    fn register_shifted(r: &mut Random) -> u32 {
        let opcode = [0, 1, 8, 9, 12, 13, 14, 15, 2, 4][r.below(10) as usize];
        let s = if (8..=11).contains(&opcode) || r.below(4) != 0 { 1 } else { 0 };
        let rs = 1 + 2 * r.below(5);
        r.condition() << 28 | opcode << 21 | s << 20 | r.below(11) << 16 | r.below(11) << 12 | rs << 8 | r.below(4) << 5 | 1 << 4 | r.below(11)
    }

    fn transfer(r: &mut Random) -> u32 {
        let pre = r.below(2);
        let writeback = if pre == 1 { r.below(2) } else { 0 };
        let rn = 12 + r.below(2);
        let offset = if r.below(2) == 0 { r.below(64) } else { 1 << 25 | r.below(4) << 7 | r.below(4) << 5 | 11 };
        r.condition() << 28 | 1 << 26 | pre << 24 | r.below(2) << 23 | r.below(2) << 22 | writeback << 21 | r.below(2) << 20 | rn << 16 | r.below(11) << 12 | offset
    }

    fn extra(r: &mut Random) -> u32 {
        let pre = r.below(2);
        let writeback = if pre == 1 { r.below(2) } else { 0 };
        let load = r.below(2);
        let kind = 1 + r.below(3);
        // ldrd and strd want an even register below r14
        let rd = if kind >= 2 && load == 0 { 2 * r.below(5) } else { r.below(11) };
        let offset = if r.below(2) == 0 { 1 << 22 | (r.below(16) << 8) | r.below(16) } else { 11 };
        r.condition() << 28 | pre << 24 | r.below(2) << 23 | writeback << 21 | load << 20 | (12 + r.below(2)) << 16 | rd << 12 | 1 << 7 | kind << 5 | 1 << 4 | offset
    }

    fn multiple(r: &mut Random) -> u32 {
        let load = r.below(2);
        let rn = 12 + r.below(2);
        // loads stay off the bases, so that the addresses stay near
        let mut list = r.below(0x800) | if r.below(4) == 0 { 1 << 14 } else { 0 };
        if load == 0 && r.below(3) == 0 {
            list |= 1 << rn;
        }
        if list == 0 {
            list = 1;
        }
        r.condition() << 28 | 0b100 << 25 | r.below(2) << 24 | r.below(2) << 23 | r.below(2) << 21 | load << 20 | rn << 16 | list
    }

    /// the media instructions the translator takes, and some it doesn't.
    fn media(r: &mut Random) -> u32 {
        let (rd, rm) = (r.below(11) << 12, r.below(11));
        let rn = if r.below(3) == 0 { 15 } else { r.below(11) };
        let cond = r.condition() << 28;
        let form = |op1: u32, op2: u32, middle: u32| cond | 0b011 << 25 | op1 << 20 | rn << 16 | rd | middle << 7 | op2 << 5 | 1 << 4 | rm;
        match r.below(12) {
            0..=2 => {
                let op1 = [0b01010, 0b01011, 0b01110, 0b01111][r.below(4) as usize];
                form(op1, 0b011, r.below(4) << 3)
            }
            8..=10 => {
                let op1 = [1, 2, 3, 5, 6, 7][r.below(6) as usize];
                form(op1, [0, 1, 2, 3, 4, 7][r.below(6) as usize], 0x1E)
            }
            11 => form(0b01000, 0b101, 0x1E),
            3 => form(0b01000, 2 * r.below(4), r.below(32)),
            4 => form([0b01011, 0b01111][r.below(2) as usize], [0b001, 0b101][r.below(2) as usize], 0x1E),
            5 => form([0b01000, 0b01100][r.below(2) as usize], 0b011, r.below(4) << 3),
            6 => form(0b01010, 2 * r.below(4), r.below(32)),
            _ => {
                if r.below(4) == 0 {
                    form(0b01000, 0b101, 0x1E)
                } else {
                    let op1 = [1, 2, 3, 5, 6, 7][r.below(6) as usize];
                    form(op1, [0, 1, 2, 3, 4, 7][r.below(6) as usize], 0x1E)
                }
            }
        }
    }

    /// VFP instructions of every kind the translator takes, and vmsr, which
    /// it doesn't and which can turn short vectors on.
    /// doubles in quarters: how often it works on doubles, none in some
    /// programs so that their blocks keep singles in registers.
    fn vfp(r: &mut Random, doubles: u32) -> u32 {
        let coprocessor = 10 + (r.below(4) < doubles) as u32;
        let cond = r.condition() << 28;
        let (d, n, m) = (r.below(2) << 22, r.below(2) << 7, r.below(2) << 5);
        let (vd, vn, vm) = (r.below(16) << 12, r.below(16) << 16, r.below(16));
        let core = r.below(11) << 12;
        match r.below(12) {
            0..=4 => {
                let operation = r.below(9);
                let (p, qr, negate) = if operation == 8 { (1, 0, 0) } else { (0, operation >> 1, operation & 1) };
                cond | 0b1110 << 24 | p << 23 | d | qr << 20 | vn | vd | coprocessor << 8 | n | negate << 6 | m | vm
            }
            5 | 6 => {
                let (kind, top) = [(0b0000, 2), (0b0001, 2), (0b0100, 2), (0b0101, 2), (0b0111, 1), (0b1000, 2), (0b1100, 1), (0b1101, 1)][r.below(8) as usize];
                let top = if top == 1 { 1 << 7 } else { r.below(2) << 7 };
                cond | 0b1110_1011 << 20 | d | kind << 16 | vd | coprocessor << 8 | top | 1 << 6 | m | vm
            }
            7 => cond | 0b1101 << 24 | r.below(2) << 23 | d | r.below(2) << 20 | (12 + r.below(2)) << 16 | vd | coprocessor << 8 | r.below(8),
            8 => {
                let (pre, up, writeback) = if r.below(2) == 0 { (0, 1, r.below(2)) } else { (1, 0, 1) };
                let count = if coprocessor == 11 { 2 * (1 + r.below(3)) } else { 1 + r.below(6) };
                cond | 0b110 << 25 | pre << 24 | up << 23 | d | writeback << 21 | r.below(2) << 20 | (12 + r.below(2)) << 16 | vd | coprocessor << 8 | count
            }
            9 => cond | 0x0E00_0A10 | r.below(2) << 20 | vn | core | n,
            10 => match r.below(4) {
                0 => cond | 0x0EF1_FA10,
                1 => cond | 0x0EF1_0A10 | core,
                2 => cond | 0x0C40_0A10 | (coprocessor - 10) << 8 | r.below(2) << 20 | r.below(11) << 16 | core | m | vm,
                _ => cond | 0x0EE1_0A10 | 10 << 12,
            },
            _ => cond | 0x0E1D_0F70 | core,
        }
    }

    /// a function of random instructions at BASE calling one at CALLEE and
    /// a Thumb one at THUMB_CALLEE.
    fn program(seed: u64) -> Vec<u32> {
        let mut r = Random(seed * 0x9E37_79B9 + 1);
        let count = 8 + r.below(40) as usize;
        let doubles = r.below(3);
        let mut main: Vec<u32> = Vec::with_capacity(count + 4);
        // r10 at the callee, plus one for Thumb
        let at_callee = [0xE3A0_A601, 0xE28A_AB02];
        let at_thumb = [0xE3A0_A601, 0xE28A_AB03, 0xE38A_A001];
        // a third of the program from one kind, so that the rare ones get
        // dense somewhere: data processing, transfers, extra transfers,
        // ldm and stm, multiplies, calls, clz and mrs, branches
        let focus = r.below(11) as usize;
        let (from, span) = [(0, 25), (40, 13), (53, 6), (59, 6), (80, 4), (86, 4), (84, 2), (65, 6), (30, 10), (25, 5), (100, 1)][focus];
        while main.len() < count {
            let i = main.len();
            let a = BASE + 4 * i as u32;
            let branch = |r: &mut Random, to: usize, link: bool| {
                let target = BASE + 4 * to as u32;
                let offset = (target.wrapping_sub(a + 8) >> 2) & 0x00FF_FFFF;
                r.condition() << 28 | 0b101 << 25 | (link as u32) << 24 | offset
            };
            let offset_to = |target: u32| (target.wrapping_sub(a + 8) >> 2) & 0x00FF_FFFF;
            // mostly what the translator takes, now and then what the C
            // runs, an svc, swp
            let roll = if r.below(10) < 3 { from + r.below(span) } else { r.below(100) };
            match roll {
                0..=24 | 93..=99 => main.push(data_processing(&mut r)),
                100 => main.push(register_shifted(&mut r)),
                25..=29 => main.push(media(&mut r)),
                30..=39 => main.push(vfp(&mut r, doubles)),
                40..=52 => main.push(transfer(&mut r)),
                53..=58 => main.push(extra(&mut r)),
                59..=64 => main.push(multiple(&mut r)),
                65..=70 => {
                    let to = i + 1 + r.below((count - i) as u32) as usize;
                    main.push(branch(&mut r, to, false));
                }
                71 | 72 => {
                    let to = r.below(i as u32 + 1) as usize;
                    main.push(branch(&mut r, to, false));
                }
                73..=75 => main.push(r.condition() << 28 | 0x0B00_0000 | offset_to(CALLEE)),
                // a tail call
                76 => main.push(r.condition() << 28 | 0x0A00_0000 | offset_to(CALLEE)),
                77 | 78 => main.push(r.condition() << 28 | 0x012F_FF1E),
                79 => main.push(0xE1A0_F00E),
                80 | 81 => main.push(r.condition() << 28 | r.below(2) << 21 | r.below(2) << 20 | r.below(11) << 16 | r.below(11) << 12 | r.below(11) << 8 | 0x90 | r.below(11)),
                82 | 83 => {
                    let kind = [0b010, 0b100, 0b101, 0b110, 0b111][r.below(5) as usize];
                    let s = if kind == 0b010 { 0 } else { r.below(2) };
                    main.push(r.condition() << 28 | kind << 21 | s << 20 | r.below(11) << 16 | r.below(11) << 12 | r.below(11) << 8 | 0x90 | r.below(11));
                }
                84 => main.push(r.condition() << 28 | 0x016F_0F10 | r.below(11) << 12 | r.below(11)),
                85 => main.push(r.condition() << 28 | 0x010F_0000 | r.below(11) << 12),
                86 => {
                    main.extend(at_callee);
                    main.push(r.condition() << 28 | 0x012F_FF3A);
                }
                87 => {
                    main.extend(at_thumb);
                    main.push(r.condition() << 28 | 0x012F_FF1A);
                }
                88 => main.push(0xFA00_0000 | offset_to(THUMB_CALLEE)),
                89 => main.push(0xEB00_0000 | offset_to(0x0020_0000)),
                90 => main.push(0xEF00_0000 | r.below(0x100)),
                91 => main.push(0xE100_0090 | r.below(11) << 16 | r.below(11) << 12 | r.below(11)),
                _ => main.push(0xF57F_F05F),
            }
        }
        main.push(0xE12F_FF1E);
        let mut words = main;
        words.resize(((CALLEE - BASE) / 4) as usize, 0xE1A0_0000);
        // its VFP clobbering what a caller keeps in host registers
        for _ in 0..1 + r.below(6) {
            words.push(if r.below(3) == 0 { vfp(&mut r, doubles) } else { data_processing(&mut r) & !(0xF << 12) | r.below(4) << 12 });
        }
        // now and then back to the instruction after the next, not the next
        if r.below(4) == 0 {
            words.push(0xE28E_E004);
        }
        words.push(0xE12F_FF1E);
        words.resize(((THUMB_CALLEE - BASE) / 4) as usize, 0xE1A0_0000);
        words.push(0x4770_3001);
        words
    }

    fn tool(name: &str) -> bool {
        Command::new(name).arg("--version").output().is_ok_and(|output| output.status.success())
    }

    /// the files built with the harness for qemu, its digests per trial.
    fn run(dir: &std::path::Path, files: &[(String, String)], trials: u32, entry: u32) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let mut sources = vec![dir.join("harness.c")];
        std::fs::write(&sources[0], include_str!("a64_harness.c")).unwrap();
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents).unwrap();
            if name.ends_with(".c") || name.ends_with(".S") {
                sources.push(dir.join(name));
            }
        }
        let binary = dir.join("run");
        let built = Command::new("aarch64-linux-gnu-gcc")
            .args(["-O2", "-ffp-contract=off", "-fno-math-errno", "-w", "-fno-stack-protector", "-DRECOMP_FASTMEM", "-static", "-I"])
            .arg(dir)
            .args(&sources)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        let ran = Command::new("qemu-aarch64-static").arg(&binary).arg(trials.to_string()).arg(format!("{entry:x}")).output().unwrap();
        assert!(ran.status.success(), "{}: {}", dir.display(), String::from_utf8_lossy(&ran.stderr));
        String::from_utf8(ran.stdout).unwrap()
    }

    /// mov rd, #value, value one an ARM immediate can hold.
    fn move_immediate(rd: u32, value: u32) -> u32 {
        let rotate = (0..16).find(|&rotate| value.rotate_left(2 * rotate) < 256).expect("an immediate");
        0xE3A0_0000 | rd << 12 | rotate << 8 | value.rotate_left(2 * rotate)
    }

    fn alike(seed: u64, words: &[u32]) {
        let program = Program {
            text: Segment { base: BASE, bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect() },
            seeds: vec![(BASE, Source::Entry)],
            slots: None,
        };
        let analysis = discover::analyze(&program);
        let units = [Unit { module: None, program: &program, analysis: &analysis }];
        let root = std::env::temp_dir().join(format!("a64-fixed-{}-{seed}", std::process::id()));
        let hybrid = generate_with(&units, &[], true);
        assert!(hybrid.iter().any(|(name, text)| name.ends_with(".S") && text.contains("a_00100000:")), "the program isn't translated");
        let expected = run(&root.join("c"), &generate_with(&units, &[], false), 60, BASE);
        let got = run(&root.join("a64"), &hybrid, 60, BASE);
        assert_eq!(expected, got, "files in {}", root.display());
        if std::env::var_os("A64_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// every shift by a register through the amounts where they change how
    /// they go, the value, flags and carry stored for each.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn shifts_by_a_register_run_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            return;
        }
        let mut words = Vec::new();
        for kind in 0..4 {
            for amount in [0, 1, 7, 31, 32, 33, 63, 64, 255, 256, 288] {
                words.push(move_immediate(1, amount));
                // movs r2, r0, kind r1; mrs r4, cpsr; add r5, r3, r0, kind r1
                words.extend([0xE1B0_2110 | kind << 5, 0xE10F_4000, 0xE083_5110 | kind << 5]);
                // str r2, [r12], 4; str r4, [r12], 4; str r5, [r12], 4
                words.extend([0xE48C_2004, 0xE48C_4004, 0xE48C_5004]);
            }
        }
        words.push(0xE12F_FF1E);
        alike(0, &words);
    }

    /// VFP loads and stores of every shape, doubles and singles, up and
    /// down, with and without writeback.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn vfp_transfers_run_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            return;
        }
        alike(2, &[
            0xED2D_8B06, // vpush {d8-d10}
            0xED9C_0B02, // vldr d0, [r12, #8]
            0xED1C_1B04, // vldr d1, [r12, #-16]
            0xECAC_0B04, // vstmia r12!, {d0-d1}
            0xEC9D_0A04, // vldmia r13, {s0-s3}
            0xED4C_0A01, // vstr s1, [r12, #-4]
            0xEE30_2B01, // vadd.f64 d2, d0, d1, so the block keeps nothing in registers
            0xED8C_2B03, // vstr d2, [r12, #12]
            0xECBD_8B06, // vpop {d8-d10}
            0xE12F_FF1E,
        ]);
    }

    /// a block on singles alone, all kept in host registers: moves, sign
    /// changes, square roots, the multiply-accumulates and a comparison.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn cached_singles_run_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            return;
        }
        alike(3, &[
            // from the registers, whose values change from trial to trial
            0xEE00_0A10, // vmov s0, r0
            0xEE00_2A90, // vmov s1, r2
            0xEE01_4A10, // vmov s2, r4
            0xEE01_6A90, // vmov s3, r6
            0xEEB0_2AC0, // vabs.f32 s4, s0
            0xEEF1_2A60, // vneg.f32 s5, s1
            0xEEB0_3A41, // vmov.f32 s6, s2
            0xEEF1_3AE1, // vsqrt.f32 s7, s3
            0xEE00_4A20, // vmla.f32 s8, s0, s1
            0xEE51_4A21, // vnmls.f32 s9, s2, s3
            0xEE10_5A41, // vnmla.f32 s10, s0, s2
            0xEEB4_2A62, // vcmp.f32 s4, s5
            0xEEF1_FA10, // vmrs APSR_nzcv, fpscr
            0xC3A0_0001, // movgt r0, #1
            0xEC8C_2A07, // vstmia r12, {s4-s10}
            0xE58C_0040, // str r0, [r12, #64]
            0xE12F_FF1E,
        ]);
    }

    /// more singles than host registers: the most used kept, the rest in
    /// memory.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn some_singles_kept_run_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            return;
        }
        alike(4, &[
            0xEC9D_0A18, // vldmia r13, {s0-s23}
            0xEE00_2A90, // vmov s1, r2
            0xEE3B_0A2B, // vadd.f32 s0, s22, s23
            0xEE70_BA20, // vadd.f32 s23, s0, s1
            0xEC8C_0A18, // vstmia r12, {s0-s23}
            0xE12F_FF1E,
        ]);
    }

    /// singles a block keeps in host registers, then a call and a tail call
    /// to a function using the same host registers for its own: what the
    /// callee leaves in memory stays.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn singles_kept_across_calls_run_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            return;
        }
        let b = |at: u32, link: bool| 0xEA00_0000 | (link as u32) << 24 | ((CALLEE.wrapping_sub(BASE + 4 * at + 8) >> 2) & 0x00FF_FFFF);
        // vadd.f32 s0, s1, s2; bl CALLEE; vadd.f32 s0, s0, s1; b CALLEE
        let mut words = vec![0xEE30_0A81, b(1, true), 0xEE30_0A20, b(3, false)];
        words.resize(((CALLEE - BASE) / 4) as usize, 0xE1A0_0000);
        // vadd.f32 s7, s5, s6; bx lr: s5 where the caller keeps s0
        words.extend([0xEE72_3A83, 0xE12F_FF1E]);
        alike(1, &words);
    }

    /// random programs, each built as C alone and as C with assembly, run
    /// from the same random contexts: everything they leave the same.
    /// A64_FUZZ=programs sets how many.
    #[test]
    #[ignore = "needs aarch64-linux-gnu-gcc and qemu-aarch64-static, run with --ignored"]
    fn the_assembly_runs_as_the_c_does() {
        if !tool("aarch64-linux-gnu-gcc") || !tool("qemu-aarch64-static") {
            eprintln!("no aarch64 compiler or qemu, skipped");
            return;
        }
        let programs: u64 = std::env::var("A64_FUZZ").ok().and_then(|n| n.parse().ok()).unwrap_or(12);
        let root = std::env::temp_dir().join(format!("a64-fuzz-{}", std::process::id()));
        let (mut translated, mut moved_translated, mut covered, mut total) = (0, 0, 0, 0);
        for seed in 0..programs {
            let words = program(seed);
            let at = |base: u32| Program {
                text: Segment { base, bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect() },
                seeds: vec![(base, Source::Entry)],
                slots: None,
            };
            let program = at(BASE);
            let analysis = discover::analyze(&program);
            // every third one a module too, run from there: its code moves,
            // at offsets on both sides of 4096 apart from where it's loaded,
            // and jumps through r10 go to the executable's copy
            let moved = at(MODULE_OFFSET);
            let moved_analysis = discover::analyze(&moved);
            let module = seed % 3 == 2;
            let units = [Unit { module: None, program: &program, analysis: &analysis }, Unit { module: Some("test.cro"), program: &moved, analysis: &moved_analysis }];
            let units = if module { &units[..] } else { &units[..1] };
            let entry = if module { MODULE_BASE + MODULE_OFFSET } else { BASE };
            let plain = generate_with(units, &[], false);
            let hybrid = generate_with(units, &[], true);
            translated += hybrid.iter().filter(|(name, _)| name.ends_with(".S")).map(|(_, text)| text.matches("%function").count()).sum::<usize>();
            moved_translated += hybrid.iter().filter(|(name, _)| name.ends_with(".S")).map(|(_, text)| text.matches("pn_module, 1").count()).sum::<usize>();
            for (&entry, function) in &analysis.functions {
                let names: BTreeMap<u32, String> = analysis.functions.keys().map(|&e| (e, format!("f_{e:08X}"))).collect();
                covered += coverage(&program, entry, function, &function.labels, &names, "");
                total += function.instructions.len();
            }
            let expected = run(&root.join(format!("{seed}-c")), &plain, 60, entry);
            let got = run(&root.join(format!("{seed}-a64")), &hybrid, 60, entry);
            if expected != got {
                let line = expected.lines().zip(got.lines()).find(|(a, b)| a != b);
                panic!("program {seed} differs, first at {line:?}, files in {}", root.display());
            }
        }
        eprintln!("{programs} programs alike, {translated} functions as assembly, {moved_translated} files of a module's, {covered} of {total} instructions in blocks it takes");
        let _ = std::fs::remove_dir_all(&root);
    }
}
