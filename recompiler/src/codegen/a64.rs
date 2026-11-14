//! ARM functions written as AArch64 assembly beside their C, for the
//! handhelds. a function's blocks, from one budget label to the next, are
//! translated one by one, and one with anything the translator doesn't take
//! hands over to the C at its label, which dispatches there and charges the
//! same budget, so a function runs the same however much of it translates.
//!
//! x19 holds the context, x20 the view of memory, w21 the budget, x24 the
//! VFP registers, x25 fpscr, w0-w14 the guest's r0-r14 and NZCV its flags,
//! written back to the context and read again around every call. w15-w17,
//! w22-w23 and v16-v19 are scratch.

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
pub const PRELUDE: &str = "\t.text
\t.arch armv8-a
.macro pn_flush
\tstp w0, w1, [x19, #0]
\tstp w2, w3, [x19, #8]
\tstp w4, w5, [x19, #16]
\tstp w6, w7, [x19, #24]
\tstp w8, w9, [x19, #32]
\tstp w10, w11, [x19, #40]
\tstp w12, w13, [x19, #48]
\tstr w14, [x19, #56]
\tmrs x16, nzcv
\tubfx w17, w16, #31, #1
\tubfx w15, w16, #30, #1
\torr w17, w17, w15, lsl #8
\tubfx w15, w16, #29, #1
\torr w17, w17, w15, lsl #16
\tubfx w15, w16, #28, #1
\torr w17, w17, w15, lsl #24
\tstr w17, [x19, #64]
\tstr w21, [x19, #72]
.endm
.macro pn_reload
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
.endm
.macro pn_enter
\tstp x29, x30, [sp, #-80]!
\tmov x29, sp
\tstp x19, x20, [sp, #16]
\tstp x21, x22, [sp, #32]
\tstp x23, x24, [sp, #48]
\tstr x25, [sp, #64]
\tmov x19, x0
\tldr x20, [x19, #144]
\tldr x24, [x19, #112]
\tldr x25, [x19, #120]
.endm
.macro pn_leave
\tldp x19, x20, [sp, #16]
\tldp x21, x22, [sp, #32]
\tldp x23, x24, [sp, #48]
\tldr x25, [sp, #64]
\tldp x29, x30, [sp], #80
.endm
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
const EXIT_BUDGET: u32 = 2;
const EXIT_UNWIND: u32 = 3;
const DEPTH_LIMIT: u32 = 2048;

/// A32 condition codes as A64 writes them, in the same order.
const CONDITIONS: [&str; 14] = ["eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le"];

/// the name of a function's assembly.
pub fn name(entry: u32) -> String {
    format!("a_{entry:08X}")
}

/// a function being translated.
struct Writer<'a> {
    entry: u32,
    labels: &'a BTreeSet<u32>,
    /// what calls go to, by entry with bit 0 set for Thumb.
    functions: &'a BTreeMap<u32, String>,
    /// the C that runs the function, which every fallback ends in.
    fallback: String,
    /// local labels made so far.
    made: std::cell::Cell<u32>,
    /// whether the block being written has VFP arithmetic a short vector
    /// would change, which runs only with fpscr's length 1.
    vector: std::cell::Cell<bool>,
}

impl Writer<'_> {
    fn local(&self) -> String {
        let made = self.made.get();
        self.made.set(made + 1);
        format!(".La{:08X}_{made}", self.entry)
    }

    fn block(&self, address: u32) -> String {
        format!(".Lb{:08X}_{address:08X}", self.entry)
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
fn fallback(out: &mut String, w: &Writer, address: u32) {
    emit!(out, "\tpn_flush");
    constant(out, "w16", address);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
}

/// hands over to the C at the address in w22.
fn dispatch(out: &mut String, w: &Writer) {
    emit!(out, "\tpn_flush");
    emit!(out, "\tstr w22, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
}

/// a return to the address in w22, which may switch to Thumb.
fn return_to(out: &mut String) {
    emit!(out, "\tpn_flush");
    emit!(out, "\tand w16, w22, #1");
    emit!(out, "\tstrb w16, [x19, #{THUMB}]");
    emit!(out, "\ttst w22, #1");
    emit!(out, "\tand w17, w22, #0xfffffffe");
    emit!(out, "\tand w16, w22, #0xfffffffc");
    emit!(out, "\tcsel w16, w17, w16, ne");
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tpn_leave");
    emit!(out, "\tret");
}

/// a jump to the address in w22, which may switch to Thumb, through the
/// C's dispatch.
fn jump_to(out: &mut String, w: &Writer) {
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
    let go = w.local();
    emit!(out, "\tldr w16, [x19, #{DEPTH}]");
    emit!(out, "\tadd w16, w16, #1");
    emit!(out, "\tstr w16, [x19, #{DEPTH}]");
    emit!(out, "\tcmp w16, #{DEPTH_LIMIT}");
    emit!(out, "\tb.le {go}");
    emit!(out, "\tsub w16, w16, #1");
    emit!(out, "\tstr w16, [x19, #{DEPTH}]");
    emit!(out, "\tmov w16, #{EXIT_UNWIND}");
    emit!(out, "\tstr w16, [x19, #{EXIT}]");
    emit!(out, "\tpn_leave");
    emit!(out, "\tret");
    emit!(out, "{go}:");
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
    emit!(out, "\tldr w16, [x19, #{DEPTH}]");
    emit!(out, "\tsub w16, w16, #1");
    emit!(out, "\tstr w16, [x19, #{DEPTH}]");
}

/// a guest call with link the return address, as call in mod.rs writes it,
/// to Thumb code where thumb.
fn call(out: &mut String, w: &Writer, link: u32, target: u32, thumb: bool) -> Option<bool> {
    constant(out, "w14", link);
    emit!(out, "\tpn_flush");
    constant(out, "w16", target);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    if thumb {
        emit!(out, "\tmov w16, #1");
        emit!(out, "\tstrb w16, [x19, #{THUMB}]");
    }
    call_out(out, w, w.functions.get(&(target | thumb as u32)).map_or(Callee::Lookup, |name| Callee::Named(name)));
    returned(out, w, link);
    Some(true)
}

/// after a call: the caller leaves too when the host has to take over, and
/// anything but a plain return to link goes through the C's dispatch, the
/// context as it is.
fn returned(out: &mut String, w: &Writer, link: u32) {
    let (on, again) = (w.local(), w.local());
    // the host takes over, or anything but a plain return to the next
    // instruction goes through the C's dispatch, the context as it is
    emit!(out, "\tldr w16, [x19, #{EXIT}]");
    emit!(out, "\tcbz w16, {on}");
    emit!(out, "\tpn_leave");
    emit!(out, "\tret");
    emit!(out, "{on}:");
    emit!(out, "\tldr w16, [x19, #{R15}]");
    constant(out, "w17", link);
    emit!(out, "\tcmp w16, w17");
    emit!(out, "\tldrb w16, [x19, #{THUMB}]");
    emit!(out, "\tccmp w16, #0, #0, eq");
    emit!(out, "\tb.eq {again}");
    emit!(out, "\tmov x0, x19");
    emit!(out, "\tpn_leave");
    emit!(out, "\tb {}", w.fallback);
    emit!(out, "{again}:");
    emit!(out, "\tpn_reload");
}

/// a branch without link: to a label of this function, a known function as
/// a tail call, or the C's dispatch.
fn jump(out: &mut String, w: &Writer, target: u32) -> Option<bool> {
    if w.labels.contains(&target) {
        emit!(out, "\tb {}", w.block(target));
    } else if let Some(name) = w.functions.get(&target) {
        emit!(out, "\tpn_flush");
        constant(out, "w16", target);
        emit!(out, "\tstr w16, [x19, #{R15}]");
        call_out(out, w, Callee::Named(name));
        emit!(out, "\tpn_leave");
        emit!(out, "\tret");
    } else {
        constant(out, "w22", target);
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
        0b000 if op & 0x90 == 0x90 && (op >> 5) & 3 != 0 => extra_transfer(out, a, op),
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
        0b010 => single_transfer(out, a, op),
        0b011 if op & 0x10 == 0 => single_transfer(out, a, op),
        0b011 if (op >> 23) & 3 == 0b01 => pack_extend_reverse(out, op),
        0b100 => block_transfer(out, w, a, op),
        0b101 => {
            let target = (a + 8).wrapping_add(sign_extend_24(op & 0x00FF_FFFF) << 2);
            if op & (1 << 24) != 0 { call(out, w, a + 4, target, false) } else { jump(out, w, target) }
        }
        0b110 => vfp_load_store(out, a, op),
        0b111 if op & (1 << 24) != 0 => svc(out, w, a, op),
        0b111 if op & 0x10 != 0 && matches!((op >> 8) & 0xF, 10 | 11) => vfp_transfer(out, op),
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

/// an svc the host runs in place, the code going on unless it says not to.
fn svc(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let on = w.local();
    emit!(out, "\tpn_flush");
    constant(out, "w16", a + 4);
    emit!(out, "\tstr w16, [x19, #{R15}]");
    emit!(out, "\tmov x0, x19");
    constant(out, "w1", op & 0x00FF_FFFF);
    emit!(out, "\tldr x16, [x19, #{HOST}]");
    emit!(out, "\tldr x16, [x16, #{SVC}]");
    emit!(out, "\tblr x16");
    emit!(out, "\tldr w16, [x19, #{EXIT}]");
    emit!(out, "\tcbz w16, {on}");
    emit!(out, "\tpn_leave");
    emit!(out, "\tret");
    emit!(out, "{on}:");
    emit!(out, "\tpn_reload");
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
fn vfp_load(out: &mut String, double: bool, register: u32, v: u32) {
    if double {
        emit!(out, "\tldr d{v}, [x24, #{}]", 8 * (register & 15));
    } else {
        emit!(out, "\tldr s{v}, [x24, #{}]", 4 * register);
    }
}

fn vfp_store(out: &mut String, double: bool, register: u32, v: u32) {
    if double {
        emit!(out, "\tstr d{v}, [x24, #{}]", 8 * (register & 15));
    } else {
        emit!(out, "\tstr s{v}, [x24, #{}]", 4 * register);
    }
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
    vfp_load(out, double, rn, 16);
    vfp_load(out, double, rm, 17);
    if operation <= 3 {
        vfp_load(out, double, rd, 18);
        emit!(out, "\tfmul {v}19, {v}16, {v}17");
    }
    match operation {
        0 => emit!(out, "\tfadd {v}19, {v}19, {v}18"),
        1 => emit!(out, "\tfsub {v}19, {v}18, {v}19"),
        2 => emit!(out, "\tfsub {v}19, {v}19, {v}18"),
        3 => {
            emit!(out, "\tfneg {v}18, {v}18");
            emit!(out, "\tfsub {v}19, {v}18, {v}19");
        }
        4 => emit!(out, "\tfmul {v}19, {v}16, {v}17"),
        5 => emit!(out, "\tfnmul {v}19, {v}17, {v}16"),
        6 => emit!(out, "\tfadd {v}19, {v}16, {v}17"),
        7 => emit!(out, "\tfsub {v}19, {v}16, {v}17"),
        _ => emit!(out, "\tfdiv {v}19, {v}16, {v}17"),
    }
    vfp_store(out, double, rd, 19);
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
                vfp_load(out, double, rm, 16);
                emit!(out, "\tfsqrt {v}19, {v}16");
                vfp_store(out, double, rd, 19);
                return Some(true);
            }
            let change = |out: &mut String, register: &str| match (second, top) {
                (false, true) => emit!(out, "\tand {register}, {register}, #0x7fffffff"),
                (true, false) => emit!(out, "\teor {register}, {register}, #0x80000000"),
                _ => {}
            };
            if double {
                let (d, m) = (2 * (rd & 15), 2 * (rm & 15));
                emit!(out, "\tldp w16, w17, [x24, #{}]", 4 * m);
                change(out, "w17");
                emit!(out, "\tstp w16, w17, [x24, #{}]", 4 * d);
            } else {
                emit!(out, "\tldr w16, [x24, #{}]", 4 * rm);
                change(out, "w16");
                emit!(out, "\tstr w16, [x24, #{}]", 4 * rd);
            }
        }
        // vcmp against a register or zero, the result in fpscr's top bits
        // the way fcmp sets NZCV, the guest's flags kept
        (0b0100 | 0b0101, _) => {
            vfp_load(out, double, rd, 16);
            emit!(out, "\tmrs x23, nzcv");
            if (op >> 16) & 1 == 0 {
                vfp_load(out, double, rm, 17);
                emit!(out, "\tfcmp {v}16, {v}17");
            } else {
                emit!(out, "\tfcmp {v}16, #0.0");
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
                vfp_load(out, true, rm, 16);
                emit!(out, "\tfcvt s19, d16");
                vfp_store(out, false, sreg(vd, d_bit), 19);
            } else {
                vfp_load(out, false, sreg(vm, m_bit), 16);
                emit!(out, "\tfcvt d19, s16");
                vfp_store(out, true, dreg(vd, d_bit), 19);
            }
        }
        // vcvt from an integer, bit 7 set for signed
        (0b1000, _) => {
            emit!(out, "\tldr w16, [x24, #{}]", 4 * sreg(op & 0xF, op & (1 << 5) != 0));
            emit!(out, "\t{} {v}19, w16", if top { "scvtf" } else { "ucvtf" });
            vfp_store(out, double, rd, 19);
        }
        // vcvt to an integer rounding toward zero, saturating, NaN to 0
        (0b1100 | 0b1101, true) => {
            vfp_load(out, double, rm, 16);
            emit!(out, "\t{} w16, {v}16", if (op >> 16) & 1 != 0 { "fcvtzs" } else { "fcvtzu" });
            emit!(out, "\tstr w16, [x24, #{}]", 4 * sreg((op >> 12) & 0xF, op & (1 << 22) != 0));
        }
        _ => return None,
    }
    Some(true)
}

/// vmrs, and the moves between a core register and a single. vmsr, which
/// changes how later VFP code runs, is the C's.
fn vfp_transfer(out: &mut String, op: u32) -> Option<bool> {
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
            emit!(out, "\t{} w{rt}, [x24, #{}]", if load { "ldr" } else { "str" }, 4 * sn);
            Some(true)
        }
        _ => None,
    }
}

/// vldr, vstr, vldm and vstm, and the moves between two core registers and
/// a double or two singles, w17 walking up from the first address.
fn vfp_load_store(out: &mut String, a: u32, op: u32) -> Option<bool> {
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
        let verb = if load { "ldr" } else { "str" };
        emit!(out, "\t{verb} w{rt}, [x24, #{}]", 4 * low);
        emit!(out, "\t{verb} w{rt2}, [x24, #{}]", 4 * high);
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
        constant(out, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    match (up, pre && !writeback) {
        (true, true) => emit!(out, "\tadd w17, {base}, #{offset}"),
        (true, false) => emit!(out, "\tmov w17, {base}"),
        (false, _) => emit!(out, "\tsub w17, {base}, #{offset}"),
    }
    for i in 0..count.max(1) {
        let words: Vec<u32> = if double { vec![((first + i) * 2) & 31, ((first + i) * 2 + 1) & 31] } else { vec![(first + i) & 31] };
        for (j, word) in words.into_iter().enumerate() {
            let address = if j == 0 {
                "w17"
            } else {
                emit!(out, "\tadd w22, w17, #4");
                "w22"
            };
            if load {
                emit!(out, "\tldr w16, [x20, {address}, uxtw]");
                emit!(out, "\tstr w16, [x24, #{}]", 4 * word);
            } else {
                emit!(out, "\tldr w16, [x24, #{}]", 4 * word);
                emit!(out, "\tstr w16, [x20, {address}, uxtw]");
            }
        }
        emit!(out, "\tadd w17, w17, #{}", if double { 8 } else { 4 });
    }
    if writeback {
        emit!(out, "\t{} w{rn}, w{rn}, #{offset}", if up { "add" } else { "sub" });
    }
    Some(true)
}

/// bx and blx through a register: bx lr returns, any other bx jumps, and
/// blx calls whatever the host has there.
fn branch_exchange(out: &mut String, w: &Writer, a: u32, op: u32) -> Option<bool> {
    let rm = op & 0xF;
    if rm == 15 {
        constant(out, "w22", a + 8);
    } else {
        emit!(out, "\tmov w22, w{rm}");
    }
    if (op >> 4) & 0xF == 0b0011 {
        let next = a + 4;
        constant(out, "w14", next);
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
        return Some(true);
    }
    if rm == 14 {
        return_to(out);
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
fn shifter_operand(out: &mut String, a: u32, op: u32, carry: bool) -> Option<Carry> {
    if op & (1 << 25) != 0 {
        let rotate = ((op >> 8) & 0xF) * 2;
        let value = (op & 0xFF).rotate_right(rotate);
        constant(out, "w16", value);
        return Some(if rotate == 0 { Carry::Kept } else { Carry::Constant(value >> 31) });
    }
    if op & (1 << 4) != 0 {
        return None;
    }
    let rm = op & 0xF;
    let kind = (op >> 5) & 3;
    let amount = (op >> 7) & 0x1F;
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
    let carry = shifter_operand(out, a, op, set_flags && logical)?;
    let first = if matches!(opcode, 13 | 15) {
        String::new()
    } else if rn == 15 {
        constant(out, "w15", a + 8);
        "w15".to_owned()
    } else {
        format!("w{rn}")
    };
    let d = if compare || rd == 15 { "w22".to_owned() } else { format!("w{rd}") };
    let s = if set_flags { "s" } else { "" };
    match opcode {
        0 | 8 => emit!(out, "\tand {d}, {first}, w16"),
        1 | 9 => emit!(out, "\teor {d}, {first}, w16"),
        2 => emit!(out, "\tsub{s} {d}, {first}, w16"),
        3 => emit!(out, "\tsub{s} {d}, w16, {first}"),
        4 => emit!(out, "\tadd{s} {d}, {first}, w16"),
        5 => emit!(out, "\tadc{s} {d}, {first}, w16"),
        6 => emit!(out, "\tsbc{s} {d}, {first}, w16"),
        7 => emit!(out, "\tsbc{s} {d}, w16, {first}"),
        10 => emit!(out, "\tcmp {first}, w16"),
        11 => emit!(out, "\tcmn {first}, w16"),
        12 => emit!(out, "\torr {d}, {first}, w16"),
        13 => emit!(out, "\tmov {d}, w16"),
        14 => emit!(out, "\tbic {d}, {first}, w16"),
        _ => emit!(out, "\tmvn {d}, w16"),
    }
    if set_flags && logical {
        logical_flags(out, &d, carry);
    }
    if compare || rd != 15 {
        return Some(true);
    }
    // mov pc, lr returns, any other write to pc jumps
    if opcode == 13 && op & 0x0200_0FFF == 14 {
        emit!(out, "\tpn_flush");
        emit!(out, "\tand w16, w22, #0xfffffffc");
        emit!(out, "\tstr w16, [x19, #{R15}]");
        emit!(out, "\tpn_leave");
        emit!(out, "\tret");
    } else {
        emit!(out, "\tand w22, w22, #0xfffffffc");
        dispatch(out, w);
    }
    Some(false)
}

/// ldr, str, ldrb and strb.
fn single_transfer(out: &mut String, a: u32, op: u32) -> Option<bool> {
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
        constant(out, "w15", a + 8);
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
        emit!(out, "\tldr{suffix} w16, [x20, {address}, uxtw]");
        if writeback && rn != rd {
            emit!(out, "\tmov w{rn}, w17");
        }
        if rd == 15 {
            emit!(out, "\tmov w22, w16");
            return_to(out);
            return Some(false);
        }
        emit!(out, "\tmov w{rd}, w16");
    } else {
        // storing r15 stores the address plus 12
        let value = if rd == 15 {
            constant(out, "w16", a + 12);
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
fn extra_transfer(out: &mut String, a: u32, op: u32) -> Option<bool> {
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
        constant(out, "w16", a + 8);
        "w16".to_owned()
    } else {
        format!("w{}", op & 0xF)
    };
    let base = if rn == 15 {
        constant(out, "w15", a + 8);
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
            emit!(out, "\t{read} w16, [x20, {address}, uxtw]");
            back(out);
            emit!(out, "\tmov w{rd}, w16");
        }
        (1, false) => {
            // a halfword of r15 stores the address plus 8
            let value = if rd == 15 {
                constant(out, "w16", a + 8);
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
                    return_to(out);
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
                constant(out, "w16", a + 12);
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
        let mut text = Some(String::new());
        w.vector.set(false);
        for (j, &address) in instructions[i..i + run].iter().enumerate() {
            let op = program.text.read32(address).unwrap_or(0);
            let Some((asm, continues)) = lower(w, address, op) else {
                text = None;
                break;
            };
            let body = text.as_mut().expect("still translating");
            body.push_str(&asm);
            let next = address + 4;
            if continues && instructions.get(i + j + 1) != Some(&next) {
                constant(body, "w22", next);
                dispatch(body, w);
            }
        }
        blocks.push((start, run, text, w.vector.get()));
        i += run;
    }
    blocks
}

/// how many of the function's instructions are in blocks the translator
/// takes, none when its entry's block isn't one.
pub fn coverage(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>) -> usize {
    if function.mode != Mode::Arm {
        return 0;
    }
    let w = Writer { entry, labels, functions, fallback: String::new(), made: Default::default(), vector: Default::default() };
    let blocks = blocks(program, &w, function);
    if !blocks.iter().any(|(start, _, text, _)| *start == entry && text.is_some()) {
        return 0;
    }
    blocks.iter().filter(|(_, _, text, _)| text.is_some()).map(|(_, run, ..)| run).sum()
}

/// what keeps the translator off each block it doesn't take: the kind of
/// its first instruction it can't, with how many instructions the block holds.
pub fn refusals(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>) -> Vec<(&'static str, usize)> {
    if function.mode != Mode::Arm {
        return vec![("thumb", function.instructions.len())];
    }
    let w = Writer { entry, labels, functions, fallback: String::new(), made: Default::default(), vector: Default::default() };
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
        0b010 | 0b011 if op & (1 << 25) != 0 && op & 0x10 != 0 => "media",
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
pub fn worth(program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>) -> bool {
    let translated = coverage(program, entry, function, labels, functions);
    translated > 0 && translated * 2 >= function.instructions.len()
}

/// the function's assembly, entered at its entry as the C is, the C named
/// c_name running it from anywhere else.
pub fn write(out: &mut String, program: &Program, entry: u32, function: &Function, labels: &BTreeSet<u32>, functions: &BTreeMap<u32, String>, c_name: &str) {
    let w = Writer { entry, labels, functions, fallback: c_name.to_owned(), made: Default::default(), vector: Default::default() };
    let name = name(entry);
    let enter = w.local();
    emit!(out, "\t.globl {name}");
    emit!(out, "\t.hidden {name}");
    emit!(out, "\t.type {name}, %function");
    emit!(out, "\t.p2align 4");
    emit!(out, "{name}:");
    // anywhere but the entry is the C's
    emit!(out, "\tldr w16, [x0, #{R15}]");
    emit!(out, "\tldrb w17, [x0, #{THUMB}]");
    constant(out, "w15", entry);
    emit!(out, "\tcmp w16, w15");
    emit!(out, "\tccmp w17, #0, #0, eq");
    emit!(out, "\tb.eq {enter}");
    emit!(out, "\tb {c_name}");
    emit!(out, "{enter}:");
    emit!(out, "\tpn_enter");
    emit!(out, "\tpn_reload");
    emit!(out, "\tb {}", w.block(entry));
    let mut stubs = String::new();
    for (start, run, text, vector) in blocks(program, &w, function) {
        emit!(out, "{}:", w.block(start));
        match text {
            Some(text) => {
                let (out_of_budget, on) = (w.local(), w.local());
                // a short vector is the C's
                if vector {
                    let scalar = w.local();
                    emit!(out, "\tldr w16, [x25]");
                    emit!(out, "\tand w16, w16, #0x{FPSCR_LEN:x}");
                    emit!(out, "\tcbz w16, {scalar}");
                    fallback(out, &w, start);
                    emit!(out, "{scalar}:");
                }
                if run < 4096 {
                    emit!(out, "\tsub w21, w21, #{run}");
                } else {
                    constant(out, "w16", run as u32);
                    emit!(out, "\tsub w21, w21, w16");
                }
                emit!(out, "\ttbz w21, #31, {on}");
                emit!(out, "\tb {out_of_budget}");
                emit!(out, "{on}:");
                out.push_str(&text);
                emit!(stubs, "{out_of_budget}:");
                if run < 4096 {
                    emit!(stubs, "\tadd w21, w21, #{run}");
                } else {
                    constant(&mut stubs, "w16", run as u32);
                    emit!(stubs, "\tadd w21, w21, w16");
                }
                emit!(stubs, "\tpn_flush");
                constant(&mut stubs, "w16", start);
                emit!(stubs, "\tstr w16, [x19, #{R15}]");
                emit!(stubs, "\tmov w16, #{EXIT_BUDGET}");
                emit!(stubs, "\tstr w16, [x19, #{EXIT}]");
                emit!(stubs, "\tpn_leave");
                emit!(stubs, "\tret");
            }
            None => fallback(out, &w, start),
        }
    }
    // past the last instruction is wherever it goes on to
    out.push_str(&stubs);
    emit!(out, "\t.size {name}, .-{name}\n");
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
            16..=30 => {
                let rm = if r.below(16) == 0 { 15 } else { r.below(15) };
                r.below(32) << 7 | r.below(4) << 5 | rm
            }
            // shifts by a register, left to the C
            _ => r.below(15) << 8 | r.below(4) << 5 | 1 << 4 | r.below(15),
        };
        r.condition() << 28 | opcode << 21 | s << 20 | rn << 16 | rd << 12 | operand
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
        match r.below(8) {
            0..=2 => {
                let op1 = [0b01010, 0b01011, 0b01110, 0b01111][r.below(4) as usize];
                form(op1, 0b011, r.below(4) << 3)
            }
            3 => form(0b01000, 2 * r.below(4), r.below(32)),
            4 => form([0b01011, 0b01111][r.below(2) as usize], [0b001, 0b101][r.below(2) as usize], 0x1E),
            5 => form(0b01000, 0b011, 0),
            6 => form(0b01010, 2 * r.below(4), r.below(32)),
            _ => form(0b00001, 0b000, 0x1E),
        }
    }

    /// VFP instructions of every kind the translator takes, and vmsr, which
    /// it doesn't and which can turn short vectors on.
    fn vfp(r: &mut Random) -> u32 {
        let coprocessor = 10 + r.below(2);
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
        let mut main: Vec<u32> = Vec::with_capacity(count + 4);
        // r10 at the callee, plus one for Thumb
        let at_callee = [0xE3A0_A601, 0xE28A_AB02];
        let at_thumb = [0xE3A0_A601, 0xE28A_AB03, 0xE38A_A001];
        // a third of the program from one kind, so that the rare ones get
        // dense somewhere: data processing, transfers, extra transfers,
        // ldm and stm, multiplies, calls, clz and mrs, branches
        let focus = r.below(10) as usize;
        let (from, span) = [(0, 25), (40, 13), (53, 6), (59, 6), (80, 4), (86, 4), (84, 2), (65, 6), (30, 10), (25, 5)][focus];
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
                25..=29 => main.push(media(&mut r)),
                30..=39 => main.push(vfp(&mut r)),
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
                73..=76 => main.push(r.condition() << 28 | 0x0B00_0000 | offset_to(CALLEE)),
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
        for _ in 0..1 + r.below(6) {
            words.push(data_processing(&mut r) & !(0xF << 12) | r.below(4) << 12);
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
    fn run(dir: &std::path::Path, files: &[(String, String)], trials: u32) -> String {
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
        let ran = Command::new("qemu-aarch64-static").arg(&binary).arg(trials.to_string()).output().unwrap();
        assert!(ran.status.success(), "{}: {}", dir.display(), String::from_utf8_lossy(&ran.stderr));
        String::from_utf8(ran.stdout).unwrap()
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
        let (mut translated, mut covered, mut total) = (0, 0, 0);
        for seed in 0..programs {
            let words = program(seed);
            let program = Program {
                text: Segment { base: BASE, bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect() },
                seeds: vec![(BASE, Source::Entry)],
                slots: None,
            };
            let analysis = discover::analyze(&program);
            let units = [Unit { module: None, program: &program, analysis: &analysis }];
            let plain = generate_with(&units, &[], false);
            let hybrid = generate_with(&units, &[], true);
            translated += hybrid.iter().filter(|(name, _)| name.ends_with(".S")).map(|(_, text)| text.matches("%function").count()).sum::<usize>();
            for (&entry, function) in &analysis.functions {
                let names: BTreeMap<u32, String> = analysis.functions.keys().map(|&e| (e, format!("f_{e:08X}"))).collect();
                covered += coverage(&program, entry, function, &function.labels, &names);
                total += function.instructions.len();
            }
            let expected = run(&root.join(format!("{seed}-c")), &plain, 60);
            let got = run(&root.join(format!("{seed}-a64")), &hybrid, 60);
            if expected != got {
                let line = expected.lines().zip(got.lines()).find(|(a, b)| a != b);
                panic!("program {seed} differs, first at {line:?}, files in {}", root.display());
            }
        }
        eprintln!("{programs} programs alike, {translated} functions as assembly, {covered} of {total} instructions in blocks it takes");
        let _ = std::fs::remove_dir_all(&root);
    }
}
