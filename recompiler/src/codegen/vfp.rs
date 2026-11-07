//! lowering VFPv2 to C, following zakuro-cpu's VFP, which computes singles
//! as f32 and doubles as f64, rounding once, and flushes results to zero
//! when fpscr asks.

use std::fmt::Write;

use super::Scope;

macro_rules! emit {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).unwrap()
    };
}

fn interpret(out: &mut String, scope: &Scope, a: u32, op: u32) -> bool {
    emit!(out, "    INTERPRET({}, 0x{op:08X}u);", scope.at(a));
    scope.interpreted.borrow_mut().push(a);
    true
}

fn sreg(vd: u32, extra: bool) -> u32 {
    ((vd << 1) | extra as u32) & 31
}

fn dreg(vd: u32, extra: bool) -> u32 {
    ((extra as u32) << 4) | vd
}

/// a register of either size as C, doubles taking the two singles at 2N.
fn get(double: bool, register: u32) -> String {
    if double { format!("vfp_d(ctx, {})", register & 15) } else { format!("vfp_s(ctx, {register})") }
}

fn set(double: bool, register: u32, value: &str) -> String {
    if double { format!("vfp_set_d(ctx, {}, {value});", register & 15) } else { format!("vfp_set_s(ctx, {register}, {value});") }
}

/// writes body, or when fpscr has the instruction working on a short
/// vector, which only a destination past the first bank can, the loop
/// over it. operation numbers the arithmetic the way vfp_apply does, and
/// registers are d, n and m.
fn scalar(out: &mut String, double: bool, operation: u32, registers: (u32, u32, u32), body: &str) -> bool {
    let (d, n, m) = registers;
    let bank = if double { 4 } else { 8 };
    if d < bank {
        emit!(out, "    {{ {body} }}");
    } else {
        emit!(
            out,
            "    if (UNLIKELY(*ctx->fpscr & FPSCR_LEN)) vfp_vector(ctx, {operation}, {}, {d}, {n}, {m}); else {{ {body} }}",
            double as u8
        );
    }
    true
}

/// the cdp space of coprocessors 10 and 11, the arithmetic, the unary
/// operations, comparisons and conversions.
pub fn data_processing(out: &mut String, scope: &Scope, a: u32, op: u32) -> bool {
    let coprocessor = (op >> 8) & 0xF;
    if coprocessor != 10 && coprocessor != 11 {
        return interpret(out, scope, a, op);
    }
    let double = coprocessor == 11;
    let (vd, vn, vm) = ((op >> 12) & 0xF, (op >> 16) & 0xF, op & 0xF);
    let (d_bit, n_bit, m_bit) = (op & (1 << 22) != 0, op & (1 << 7) != 0, op & (1 << 5) != 0);
    let (rd, rn, rm) = if double {
        (dreg(vd, d_bit), dreg(vn, n_bit), dreg(vm, m_bit))
    } else {
        (sreg(vd, d_bit), sreg(vn, n_bit), sreg(vm, m_bit))
    };
    let negate = op & (1 << 6) != 0;
    let (operation, expression) = match ((op >> 20) & 0b1011, negate) {
        (0b0000, false) => (0, "acc + a * b"),
        (0b0000, true) => (1, "acc - a * b"),
        (0b0001, false) => (2, "-acc + a * b"),
        (0b0001, true) => (3, "-acc - a * b"),
        (0b0010, false) => (4, "a * b"),
        (0b0010, true) => (5, "-(a * b)"),
        (0b0011, false) => (6, "a + b"),
        (0b0011, true) => (7, "a - b"),
        (0b1000, false) => (8, "a / b"),
        (0b1011, true) => return extension(out, scope, a, op, double, rd, rm),
        _ => return interpret(out, scope, a, op),
    };
    // a vector runs its operand m as a scalar when m is in the first bank,
    // which only matters to the interpreter
    let kind = if double { "double" } else { "float" };
    let body = format!(
        "{kind} a = {}, b = {}, acc = {}; {}",
        get(double, rn),
        get(double, rm),
        get(double, rd),
        set(double, rd, expression)
    );
    scalar(out, double, operation, (rd, rn, rm), &body)
}

fn extension(out: &mut String, scope: &Scope, a: u32, op: u32, double: bool, rd: u32, rm: u32) -> bool {
    let top = op & (1 << 7) != 0;
    match ((op >> 16) & 0xF, top) {
        // vmov, vabs, vneg and vsqrt, moves and signs change bits untouched,
        // a double's too, which can be two singles reading as a subnormal
        (0b0000 | 0b0001, _) => {
            let second = (op >> 16) & 1 != 0;
            let body = if double {
                let (d, m) = (2 * (rd & 15), 2 * (rm & 15));
                let high = match (second, top) {
                    (false, false) => format!("ctx->vfp[{}]", m + 1),
                    (false, true) => format!("ctx->vfp[{}] & 0x7FFFFFFFu", m + 1),
                    (true, false) => format!("ctx->vfp[{}] ^ 0x80000000u", m + 1),
                    (true, true) => String::new(),
                };
                if high.is_empty() {
                    format!("double v = {}; {}", get(true, rm), set(true, rd, "sqrt(v)"))
                } else {
                    format!("uint32_t low = ctx->vfp[{m}], high = {high}; ctx->vfp[{d}] = low; ctx->vfp[{}] = high;", d + 1)
                }
            } else {
                match (second, top) {
                    (false, false) => format!("ctx->vfp[{rd}] = ctx->vfp[{rm}];"),
                    (false, true) => format!("ctx->vfp[{rd}] = ctx->vfp[{rm}] & 0x7FFFFFFFu;"),
                    (true, false) => format!("ctx->vfp[{rd}] = ctx->vfp[{rm}] ^ 0x80000000u;"),
                    (true, true) => set(false, rd, &format!("sqrtf({})", get(false, rm))),
                }
            };
            scalar(out, double, 9 + ((second as u32) << 1 | top as u32), (rd, rd, rm), &body)
        }
        // vcmp against a register or zero, singles compared as doubles
        (0b0100, _) => {
            emit!(out, "    vfp_compare(ctx, {}, {});", get(double, rd), get(double, rm));
            true
        }
        (0b0101, _) => {
            emit!(out, "    vfp_compare(ctx, {}, 0.0);", get(double, rd));
            true
        }
        // vcvt between single and double
        (0b0111, true) => {
            let (vd, vm) = ((op >> 12) & 0xF, op & 0xF);
            let (d_bit, m_bit) = (op & (1 << 22) != 0, op & (1 << 5) != 0);
            if double {
                emit!(out, "    {}", set(false, sreg(vd, d_bit), &format!("(float){}", get(true, rm))));
            } else {
                emit!(out, "    {}", set(true, dreg(vd, d_bit), &format!("(double){}", get(false, sreg(vm, m_bit)))));
            }
            true
        }
        // vcvt from an integer, bit 7 set for signed
        (0b1000, _) => {
            let source = sreg(op & 0xF, op & (1 << 5) != 0);
            let integer = if top { format!("(double)(int32_t)ctx->vfp[{source}]") } else { format!("(double)ctx->vfp[{source}]") };
            let value = if double { integer } else { format!("(float){integer}") };
            // A converted i32/u32 is zero or finite with magnitude >= 1,
            // so neither FZ nor DN can change it. Initialize the bitcast
            // before writing either word, including overlapping operands.
            if double {
                let destination = 2 * (rd & 15);
                emit!(out, "    {{ union {{ double value; uint64_t bits; }} converted = {{ .value = {value} }}; ctx->vfp[{destination}] = (uint32_t)converted.bits; ctx->vfp[{}] = (uint32_t)(converted.bits >> 32); }}", destination + 1);
            } else {
                emit!(out, "    {{ union {{ float value; uint32_t bits; }} converted = {{ .value = {value} }}; ctx->vfp[{rd}] = converted.bits; }}");
            }
            true
        }
        // vcvt to an integer rounding toward zero, the other rounding modes
        // are rare enough for the interpreter
        (0b1100 | 0b1101, true) => {
            let source = if double { get(true, rm) } else { format!("(double){}", get(false, rm)) };
            let convert = if (op >> 16) & 1 != 0 { "vfp_to_s32" } else { "vfp_to_u32" };
            let destination = sreg((op >> 12) & 0xF, op & (1 << 22) != 0);
            emit!(out, "    ctx->vfp[{destination}] = {convert}({source});");
            true
        }
        _ => interpret(out, scope, a, op),
    }
}

/// the mrc and mcr space of coprocessors 10 and 11, moves between core and
/// VFP registers and to and from fpscr.
pub fn register_transfer(out: &mut String, scope: &Scope, a: u32, op: u32) -> bool {
    let coprocessor = (op >> 8) & 0xF;
    let load = op & (1 << 20) != 0;
    let rt = (op >> 12) & 0xF;
    match (coprocessor, (op >> 21) & 7) {
        // vmrs and vmsr on fpscr
        (10 | 11, 7) if (op >> 16) & 0xF == 1 => {
            if !load {
                emit!(out, "    *ctx->fpscr = {};", if rt == 15 { scope.at(a + 8) } else { format!("ctx->r[{rt}]") });
            } else if rt == 15 {
                emit!(out, "    {{ uint32_t f = *ctx->fpscr; ctx->n = f >> 31; ctx->z = (f >> 30) & 1; ctx->c = (f >> 29) & 1; ctx->v = (f >> 28) & 1; }}");
            } else {
                emit!(out, "    ctx->r[{rt}] = *ctx->fpscr;");
            }
            true
        }
        // vmov between a core register and a single
        (10 | 11, 0) if rt != 15 => {
            let sn = sreg((op >> 16) & 0xF, op & (1 << 7) != 0);
            if load {
                emit!(out, "    ctx->r[{rt}] = ctx->vfp[{sn}];");
            } else {
                emit!(out, "    ctx->vfp[{sn}] = ctx->r[{rt}];");
            }
            true
        }
        _ => interpret(out, scope, a, op),
    }
}

/// the ldc and stc space of coprocessors 10 and 11, vldr, vstr, vldm and
/// vstm, and the moves between two core registers and a double.
pub fn load_store(out: &mut String, scope: &Scope, a: u32, op: u32) -> bool {
    let coprocessor = (op >> 8) & 0xF;
    if coprocessor != 10 && coprocessor != 11 {
        return interpret(out, scope, a, op);
    }
    let double = coprocessor == 11;
    if (op >> 21) & 0x7F == 0b110_0010 {
        return move_pair(out, scope, a, op, double);
    }
    let load = op & (1 << 20) != 0;
    let pre = op & (1 << 24) != 0;
    let up = op & (1 << 23) != 0;
    let writeback = op & (1 << 21) != 0;
    let rn = (op >> 16) & 0xF;
    if writeback && rn == 15 {
        return interpret(out, scope, a, op);
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
    let base = if rn == 15 { scope.at(a + 8) } else { format!("ctx->r[{rn}]") };
    let start = match (up, pre && !writeback) {
        (true, true) => format!("base + 0x{offset:X}u"),
        (true, false) => "base".to_owned(),
        (false, _) => format!("base - 0x{offset:X}u"),
    };
    emit!(out, "    {{ uint32_t base = {base}, p = {start};");
    for i in 0..count.max(1) {
        let words: Vec<u32> = if double {
            let index = first + i;
            vec![(index * 2) & 31, (index * 2 + 1) & 31]
        } else {
            vec![(first + i) & 31]
        };
        for (j, word) in words.into_iter().enumerate() {
            let address = if j == 0 { "p".to_owned() } else { "p + 4".to_owned() };
            if load {
                emit!(out, "    ctx->vfp[{word}] = mem_read32(ctx, {address});");
            } else {
                emit!(out, "    mem_write32(ctx, {address}, ctx->vfp[{word}]);");
            }
        }
        emit!(out, "    p += {};", if double { 8 } else { 4 });
    }
    if writeback {
        let sign = if up { '+' } else { '-' };
        emit!(out, "    ctx->r[{rn}] = base {sign} 0x{offset:X}u;");
    }
    emit!(out, "    (void)p; }}");
    true
}

/// vmov between two core registers and a double or two singles.
fn move_pair(out: &mut String, scope: &Scope, a: u32, op: u32, double: bool) -> bool {
    let load = op & (1 << 20) != 0;
    let rt = (op >> 12) & 0xF;
    let rt2 = (op >> 16) & 0xF;
    if rt == 15 || rt2 == 15 {
        return interpret(out, scope, a, op);
    }
    let (vm, m_bit) = (op & 0xF, op & (1 << 5) != 0);
    let (low, high) = if double {
        let dm = dreg(vm, m_bit);
        ((dm * 2) & 31, (dm * 2 + 1) & 31)
    } else {
        let sm = sreg(vm, m_bit);
        (sm, (sm + 1) & 31)
    };
    if load {
        emit!(out, "    ctx->r[{rt}] = ctx->vfp[{low}]; ctx->r[{rt2}] = ctx->vfp[{high}];");
    } else {
        emit!(out, "    ctx->vfp[{low}] = ctx->r[{rt}]; ctx->vfp[{high}] = ctx->r[{rt2}];");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generated_integer_conversions_preserve_state_and_fpscr() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("recomp-integer-vfp-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let scratch = Scratch(dir);
        let labels = BTreeSet::new();
        let functions = BTreeMap::new();
        let interpreted = RefCell::new(Vec::new());
        let scope = Scope { labels: &labels, functions: &functions, prefix: "", relative: false, interpreted: &interpreted };
        // Actual opcodes from emitted game functions, including sources
        // overlapping each half of a double destination and single outputs.
        let cases = [
            (0xEEB8_3AC3, 6, 6, false, true),
            (0xEEB8_0A40, 0, 0, false, false),
            (0xEEB8_3BE2, 5, 3, true, true),
            (0xEEB8_5B41, 2, 5, true, false),
            (0xEEB8_0BC0, 0, 0, true, true),
            (0xEEB8_0BE0, 1, 0, true, true),
        ];
        let mut generated = String::new();
        for (index, &(opcode, _, _, _, _)) in cases.iter().enumerate() {
            let mut body = String::new();
            assert!(data_processing(&mut body, &scope, 0x100000, opcode));
            assert!(!body.contains("vfp_set_"), "integer output still normalized: {body}");
            emit!(generated, "static NOINLINE void generated_{index}(Context *ctx) {{\n{body}}}");
        }
        assert!(interpreted.borrow().is_empty());
        let mut arithmetic = String::new();
        assert!(data_processing(&mut arithmetic, &scope, 0x100000, 0xEE20_0A01));
        assert!(arithmetic.contains("vfp_set_s"), "ordinary arithmetic must still normalize");
        let entries = (0..cases.len()).map(|index| format!("generated_{index}")).collect::<Vec<_>>().join(",");
        emit!(generated, "static void (*generated[])(Context *) = {{{entries}}};");
        let sources = cases.iter().map(|case| case.1.to_string()).collect::<Vec<_>>().join(",");
        let destinations = cases.iter().map(|case| case.2.to_string()).collect::<Vec<_>>().join(",");
        let wide = cases.iter().map(|case| u8::from(case.3).to_string()).collect::<Vec<_>>().join(",");
        let signed = cases.iter().map(|case| u8::from(case.4).to_string()).collect::<Vec<_>>().join(",");
        emit!(generated, "static const unsigned sources[] = {{{sources}}}, destinations[] = {{{destinations}}}, wide_outputs[] = {{{wide}}}, signed_inputs[] = {{{signed}}};");
        std::fs::write(scratch.0.join("generated.h"), generated).unwrap();
        std::fs::write(scratch.0.join("recomp.h"), recomp_abi::HEADER).unwrap();
        std::fs::write(scratch.0.join("test.c"), include_str!("../../tests/fixtures/vfp_integer_conversion.c")).unwrap();
        let compiler = std::env::var_os("CC").filter(|value| !value.is_empty()).unwrap_or_else(|| "cc".into());
        for optimization in ["-O2", "-O3"] {
            let executable = scratch.0.join(format!("conversion{optimization}{}", std::env::consts::EXE_SUFFIX));
            let compiled = Command::new(&compiler).current_dir(&scratch.0)
                .args(["-std=gnu11", optimization, "-fno-fast-math", "-frounding-math", "-ffp-contract=off", "test.c", "-o"])
                .arg(&executable).arg("-lm").output().expect("the recompiler's C compiler must be available");
            assert!(compiled.status.success(), "{optimization}: {}", String::from_utf8_lossy(&compiled.stderr));
            let result = Command::new(&executable).output().unwrap();
            assert!(result.status.success(), "{optimization}: {}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            println!("{optimization}: {}", String::from_utf8_lossy(&result.stdout).trim());
        }
    }
}
