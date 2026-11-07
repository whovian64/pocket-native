//! the GLSL the renderer builds its programs from: the fixed sources
//! for transfers, copies and presenting, and the vertex and fragment
//! sources generated from a draw's state.

use super::*;

pub(super) const FRAGMENT_SOURCE: &str = include_str!("../../../shaders/raster.frag");

/// Remove only complete generic TEV helper definitions after their call
/// sites were replaced by specialized arithmetic. Keep every remaining
/// source byte unchanged and bypass generic stand-in shaders entirely.
const DEAD_TEV_HELPERS: [(&str, &str); 9] = [
    ("uvec4", "stage_registers"), ("vec3", "color_operand"),
    ("float", "alpha_operand"), ("vec3", "combine_rgb"),
    ("float", "combine_alpha"), ("uint", "operation"),
    ("float", "scale_factor"), ("vec4", "source_value"),
    ("void", "combine_stage"),
];

#[cfg(test)]
fn comment_blanks(source: &str) -> Vec<u8> {
    let mut clean = source.as_bytes().to_vec();
    let mut at = 0;
    while at + 1 < clean.len() {
        if &clean[at..at + 2] == b"//" {
            while at < clean.len() && clean[at] != b'\n' { clean[at] = b' '; at += 1; }
        } else if &clean[at..at + 2] == b"/*" {
            clean[at..at + 2].fill(b' ');
            at += 2;
            while at + 1 < clean.len() && &clean[at..at + 2] != b"*/" {
                if clean[at] != b'\n' { clean[at] = b' '; }
                at += 1;
            }
            assert!(at + 1 < clean.len(), "unterminated GLSL block comment");
            clean[at..at + 2].fill(b' ');
            at += 2;
        } else { at += 1; }
    }
    clean
}

#[cfg(test)]
fn function_span(clean: &str, kind: &str, name: &str) -> std::ops::Range<usize> {
    let signature = format!("\n{kind} {name}(");
    let starts: Vec<_> = clean.match_indices(&signature).map(|(at, _)| at + 1).collect();
    assert_eq!(starts.len(), 1, "expected exactly one definition of {name}");
    let start = starts[0];
    let open = start + clean[start..].find('{').expect("function body");
    let mut level = 0;
    for (offset, byte) in clean.as_bytes()[open..].iter().enumerate() {
        if *byte == b'{' { level += 1; }
        if *byte == b'}' {
            level -= 1;
            if level == 0 {
                let mut end = open + offset + 1;
                if clean.as_bytes().get(end) == Some(&b'\n') { end += 1; }
                return start..end;
            }
        }
    }
    panic!("unterminated GLSL function {name}");
}

#[cfg(test)]
fn has_call_token(clean: &[u8], name: &str) -> bool {
    let mut at = 0;
    while at < clean.len() {
        if clean[at].is_ascii_alphabetic() || clean[at] == b'_' {
            let start = at;
            while at < clean.len() && (clean[at].is_ascii_alphanumeric() || clean[at] == b'_') { at += 1; }
            if &clean[start..at] == name.as_bytes() {
                let mut after = at;
                while after < clean.len() && clean[after].is_ascii_whitespace() { after += 1; }
                if clean.get(after) == Some(&b'(') { return true; }
            }
        } else { at += 1; }
    }
    false
}

// One lexical pass records all fixed helper call names. Keep the same ASCII
// identifier/whitespace grammar as the original per-name safety scanner.
fn call_token_bit(name: &[u8]) -> u32 {
    match name {
        b"stage_registers" => 1 << 0, b"color_operand" => 1 << 1,
        b"alpha_operand" => 1 << 2, b"combine_rgb" => 1 << 3,
        b"combine_alpha" => 1 << 4, b"operation" => 1 << 5,
        b"scale_factor" => 1 << 6, b"source_value" => 1 << 7,
        b"combine_stage" => 1 << 8, b"shade" => 1 << 9,
        b"lookup" => 1 << 10, b"rotate" => 1 << 11,
        b"quantize" => 1 << 12, b"proctex_lookup" => 1 << 13,
        b"proctex_color" => 1 << 14, b"proctex_random" => 1 << 15,
        b"proctex_shift" => 1 << 16, b"proctex_clamp" => 1 << 17,
        b"proctex_combine" => 1 << 18, b"procedural" => 1 << 19,
        _ => 0,
    }
}

fn call_token_bits(clean: &[u8]) -> u32 {
    let (mut at, mut bits) = (0, 0);
    while at < clean.len() {
        if clean[at].is_ascii_alphabetic() || clean[at] == b'_' {
            let start = at;
            while at < clean.len() && (clean[at].is_ascii_alphanumeric() || clean[at] == b'_') { at += 1; }
            let bit = call_token_bit(&clean[start..at]);
            if bit != 0 {
                let mut after = at;
                while after < clean.len() && clean[after].is_ascii_whitespace() { after += 1; }
                if clean.get(after) == Some(&b'(') { bits |= bit; }
            }
        } else { at += 1; }
    }
    bits
}

// Callers establish sorted, nonoverlapping ranges. Copy only kept slices;
// endpoints still come from the unchanged signature/brace scanners.
fn without_ranges(source: &str, ranges: &[std::ops::Range<usize>]) -> String {
    let removed: usize = ranges.iter().map(|range| range.end - range.start).sum();
    let mut result = String::with_capacity(source.len() - removed);
    let mut kept_from = 0;
    for range in ranges {
        result.push_str(&source[kept_from..range.start]);
        kept_from = range.end;
    }
    result.push_str(&source[kept_from..]);
    result
}

// Strict test entrypoints exercise the production checked transforms. The
// independent old implementation remains below as the differential oracle.
#[cfg(test)]
fn static_helper_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> String {
    checked_helper_elision(constants, source).unwrap_or_else(|reason| panic!("{reason}"))
}

#[cfg(test)]
fn static_feature_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> String {
    checked_feature_elision(constants, source).unwrap_or_else(|reason| panic!("{reason}"))
}

const TEXTURE_CONFIG_BINDING: &str = "    uint texture_config = DYNAMIC != 0u ? (misc.w & 0x2307u) | (TEXTURE_CONFIG & 0x400u) : TEXTURE_CONFIG;";

type ElisionResult<T> = Result<T, String>;

macro_rules! elision_require {
    ($condition:expr, $($message:tt)*) => {
        if !$condition { return Err(format!($($message)*)); }
    };
}

fn checked_comment_blanks(source: &str) -> ElisionResult<Vec<u8>> {
    let mut clean = source.as_bytes().to_vec();
    let mut at = 0;
    while at + 1 < clean.len() {
        if &clean[at..at + 2] == b"//" {
            while at < clean.len() && clean[at] != b'\n' { clean[at] = b' '; at += 1; }
        } else if &clean[at..at + 2] == b"/*" {
            clean[at..at + 2].fill(b' ');
            at += 2;
            while at + 1 < clean.len() && &clean[at..at + 2] != b"*/" {
                if clean[at] != b'\n' { clean[at] = b' '; }
                at += 1;
            }
            elision_require!(at + 1 < clean.len(), "unterminated GLSL block comment");
            clean[at..at + 2].fill(b' ');
            at += 2;
        } else { at += 1; }
    }
    Ok(clean)
}

fn checked_clean_text(clean: &[u8]) -> ElisionResult<&str> {
    std::str::from_utf8(clean).map_err(|_| "invalid comment-blanked GLSL UTF-8".to_owned())
}

fn checked_brace_end(clean: &str, open: usize) -> ElisionResult<usize> {
    let mut level = 0usize;
    for (offset, byte) in clean.as_bytes()[open..].iter().enumerate() {
        if *byte == b'{' { level += 1; }
        if *byte == b'}' {
            level = level.checked_sub(1).ok_or("unbalanced GLSL braces")?;
            if level == 0 {
                let end = open + offset + 1;
                return Ok(end + usize::from(clean.as_bytes().get(end) == Some(&b'\n')));
            }
        }
    }
    Err("unterminated GLSL body".to_owned())
}

fn checked_function_span(clean: &str, kind: &str, name: &str) -> ElisionResult<std::ops::Range<usize>> {
    let signature = format!("\n{kind} {name}(");
    let mut starts = clean.match_indices(&signature).map(|(at, _)| at + 1);
    let start = starts.next().ok_or_else(|| format!("expected exactly one definition of {name}"))?;
    elision_require!(starts.next().is_none(), "expected exactly one definition of {name}");
    let open = start + clean[start..].find('{').ok_or_else(|| format!("missing function body: {name}"))?;
    Ok(start..checked_brace_end(clean, open)?)
}

fn checked_helper_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> ElisionResult<String> {
    if constants[29] != 0 { return Ok(source.to_owned()); }
    let clean_bytes = checked_comment_blanks(source)?;
    let clean = checked_clean_text(&clean_bytes)?;
    let mut ranges = DEAD_TEV_HELPERS.into_iter().map(|(kind, name)| checked_function_span(clean, kind, name)).collect::<ElisionResult<Vec<_>>>()?;
    ranges.sort_by_key(|range| range.start);
    elision_require!(ranges.windows(2).all(|pair| pair[0].end <= pair[1].start), "overlapping TEV definitions");
    let result = without_ranges(source, &ranges);
    let result_clean = checked_comment_blanks(&result)?;
    let calls = call_token_bits(&result_clean);
    for (_, name) in DEAD_TEV_HELPERS {
        elision_require!(calls & call_token_bit(name.as_bytes()) == 0, "{name} is still called; elision is unsafe");
    }
    let before = checked_function_span(clean, "void", "main")?;
    let after = checked_function_span(checked_clean_text(&result_clean)?, "void", "main")?;
    elision_require!(source[before] == result[after], "every main byte must remain unchanged");
    Ok(result)
}

fn checked_false_branch_span(clean: &str, condition: &str) -> ElisionResult<std::ops::Range<usize>> {
    let mut starts = clean.match_indices(condition).map(|(at, _)| at);
    let start = starts.next().ok_or_else(|| format!("expected exact feature branch: {condition}"))?;
    elision_require!(starts.next().is_none(), "expected exact feature branch: {condition}");
    let main = checked_function_span(clean, "void", "main")?;
    elision_require!(main.start < start && start < main.end, "feature branch must be inside main");
    let open = start + clean[start..].find('{').ok_or("missing feature body")?;
    let end = checked_brace_end(clean, open)?;
    elision_require!(end < main.end, "feature branch must end inside main");
    Ok(start..end)
}

fn checked_texture_config_binding(clean: &str) -> ElisionResult<()> {
    let main = &clean[checked_function_span(clean, "void", "main")?];
    elision_require!(main.lines().filter(|line| *line == TEXTURE_CONFIG_BINDING).count() == 1, "texture_config/source binding");
    let binding = main.find(TEXTURE_CONFIG_BINDING).ok_or("texture_config/source binding")?;
    let after = &main[binding + TEXTURE_CONFIG_BINDING.len()..];
    let bytes = after.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_alphabetic() || bytes[at] == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') { at += 1; }
            if &bytes[start..at] == b"texture_config" {
                let before = bytes[..start].iter().rfind(|byte| !byte.is_ascii_whitespace());
                let tail = after[at..].trim_start();
                elision_require!(before == Some(&b'(') &&
                    ((tail.starts_with('&') && !tail.starts_with("&=") && !tail.starts_with("&&")) ||
                     (tail.starts_with(">>") && !tail.starts_with(">>="))),
                    "texture_config must remain read-only after binding");
            }
        } else { at += 1; }
    }
    Ok(())
}

fn checked_feature_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> ElisionResult<String> {
    if constants[29] != 0 { return Ok(source.to_owned()); }
    let source_bytes = checked_comment_blanks(source)?;
    let source_clean = checked_clean_text(&source_bytes)?;
    for (name, value) in [("DYNAMIC", 0), ("LIGHTING", constants[26]), ("TEXTURE_CONFIG", constants[25])] {
        let declaration = format!("const uint {name} = {value}u;");
        elision_require!(source_clean.lines().filter(|line| *line == declaration).count() == 1, "constant/source binding: {name}");
    }
    checked_texture_config_binding(source_clean)?;
    let base = checked_helper_elision(constants, source)?;
    let clean_bytes = checked_comment_blanks(&base)?;
    let clean = checked_clean_text(&clean_bytes)?;
    let mut ranges = Vec::new();
    let mut removed = Vec::new();
    if constants[26] == 0 {
        ranges.push(checked_false_branch_span(clean, "    if (LIGHTING != 0u) {")?);
        removed.extend([("void", "shade"), ("float", "lookup"), ("vec3", "rotate"), ("float", "quantize")]);
    }
    if constants[25] & 0x400 == 0 {
        ranges.push(checked_false_branch_span(clean, "    if ((texture_config & (1u << 10)) != 0u) {")?);
        removed.extend([("float", "proctex_lookup"), ("vec4", "proctex_color"), ("float", "proctex_random"),
            ("float", "proctex_shift"), ("float", "proctex_clamp"), ("float", "proctex_combine"), ("vec4", "procedural")]);
    }
    for (kind, name) in &removed { ranges.push(checked_function_span(clean, kind, name)?); }
    ranges.sort_by_key(|range| range.start);
    elision_require!(ranges.windows(2).all(|pair| pair[0].end <= pair[1].start), "overlapping feature elision ranges");
    let result = without_ranges(&base, &ranges);
    let after = checked_comment_blanks(&result)?;
    let calls = if removed.is_empty() { 0 } else { call_token_bits(&after) };
    for (_, name) in removed {
        elision_require!(calls & call_token_bit(name.as_bytes()) == 0, "{name} still called outside removed closure");
    }
    let tail = "    // the color leaves as whole bytes";
    let before_tail = base.find(tail).ok_or("missing alpha/fog/depth tail marker")?;
    let after_tail = result.find(tail).ok_or("missing alpha/fog/depth tail marker after elision")?;
    elision_require!(base[before_tail..] == result[after_tail..], "alpha/fog/depth main tail remains byte identical");
    Ok(result)
}

fn elide_or_original_with_warning(
    constants: &[u32; FRAGMENT_CONSTANTS], source: String,
    warned: &std::sync::atomic::AtomicBool, warn: impl FnOnce(&str),
) -> String {
    match checked_feature_elision(constants, &source) {
        Ok(elided) => elided,
        Err(reason) => {
            if !warned.swap(true, std::sync::atomic::Ordering::Relaxed) { warn(&reason); }
            source
        }
    }
}

fn elide_or_original(constants: &[u32; FRAGMENT_CONSTANTS], source: String) -> String {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    elide_or_original_with_warning(constants, source, &WARNED, |reason| {
        log::warn!("static shader elision skipped; retaining the complete original source: {reason}");
    })
}

#[cfg(test)]
mod static_elision_reference {
    use super::FRAGMENT_CONSTANTS;
const DEAD_TEV_HELPERS: [(&str, &str); 9] = [
    ("uvec4", "stage_registers"), ("vec3", "color_operand"),
    ("float", "alpha_operand"), ("vec3", "combine_rgb"),
    ("float", "combine_alpha"), ("uint", "operation"),
    ("float", "scale_factor"), ("vec4", "source_value"),
    ("void", "combine_stage"),
];

fn comment_blanks(source: &str) -> Vec<u8> {
    let mut clean = source.as_bytes().to_vec();
    let mut at = 0;
    while at + 1 < clean.len() {
        if &clean[at..at + 2] == b"//" {
            while at < clean.len() && clean[at] != b'\n' { clean[at] = b' '; at += 1; }
        } else if &clean[at..at + 2] == b"/*" {
            clean[at..at + 2].fill(b' ');
            at += 2;
            while at + 1 < clean.len() && &clean[at..at + 2] != b"*/" {
                if clean[at] != b'\n' { clean[at] = b' '; }
                at += 1;
            }
            assert!(at + 1 < clean.len(), "unterminated GLSL block comment");
            clean[at..at + 2].fill(b' ');
            at += 2;
        } else { at += 1; }
    }
    clean
}

fn function_span(clean: &str, kind: &str, name: &str) -> std::ops::Range<usize> {
    let signature = format!("\n{kind} {name}(");
    let starts: Vec<_> = clean.match_indices(&signature).map(|(at, _)| at + 1).collect();
    assert_eq!(starts.len(), 1, "expected exactly one definition of {name}");
    let start = starts[0];
    let open = start + clean[start..].find('{').expect("function body");
    let mut level = 0;
    for (offset, byte) in clean.as_bytes()[open..].iter().enumerate() {
        if *byte == b'{' { level += 1; }
        if *byte == b'}' {
            level -= 1;
            if level == 0 {
                let mut end = open + offset + 1;
                if clean.as_bytes().get(end) == Some(&b'\n') { end += 1; }
                return start..end;
            }
        }
    }
    panic!("unterminated GLSL function {name}");
}

fn has_call_token(clean: &[u8], name: &str) -> bool {
    let mut at = 0;
    while at < clean.len() {
        if clean[at].is_ascii_alphabetic() || clean[at] == b'_' {
            let start = at;
            while at < clean.len() && (clean[at].is_ascii_alphanumeric() || clean[at] == b'_') { at += 1; }
            if &clean[start..at] == name.as_bytes() {
                let mut after = at;
                while after < clean.len() && clean[after].is_ascii_whitespace() { after += 1; }
                if clean.get(after) == Some(&b'(') { return true; }
            }
        } else { at += 1; }
    }
    false
}

pub(super) fn static_helper_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> String {
    if constants[29] != 0 { return source.to_owned(); }
    let clean = comment_blanks(source);
    let clean = std::str::from_utf8(&clean).unwrap();
    let mut ranges: Vec<_> = DEAD_TEV_HELPERS.into_iter().map(|(kind, name)| function_span(clean, kind, name)).collect();
    ranges.sort_by_key(|range| range.start);
    assert!(ranges.windows(2).all(|pair| pair[0].end <= pair[1].start));
    let mut result = source.to_owned();
    for range in ranges.into_iter().rev() { result.replace_range(range, ""); }
    let result_clean = comment_blanks(&result);
    for (_, name) in DEAD_TEV_HELPERS { assert!(!has_call_token(&result_clean, name), "{name} is still called; elision is unsafe"); }
    let before = function_span(clean, "void", "main");
    let after = function_span(std::str::from_utf8(&result_clean).unwrap(), "void", "main");
    assert_eq!(&source[before], &result[after], "every main byte must remain unchanged");
    result
}


/// Drop only literal-false lighting/procedural call blocks and their
/// now-unreferenced helper definitions; retain all other source bytes.
fn constant_false_branch_span(clean: &str, condition: &str) -> std::ops::Range<usize> {
    let starts: Vec<_> = clean.match_indices(condition).map(|(at, _)| at).collect();
    assert_eq!(starts.len(), 1, "expected exact feature branch: {condition}");
    let start = starts[0];
    let main = function_span(clean, "void", "main");
    assert!(main.start < start && start < main.end);
    let open = start + clean[start..].find('{').unwrap();
    let mut level = 0;
    for (offset, byte) in clean.as_bytes()[open..].iter().enumerate() {
        if *byte == b'{' { level += 1; }
        if *byte == b'}' {
            level -= 1;
            if level == 0 {
                let mut end = open + offset + 1;
                if clean.as_bytes().get(end) == Some(&b'\n') { end += 1; }
                assert!(end < main.end);
                return start..end;
            }
        }
    }
    panic!("unterminated feature branch");
}

const TEXTURE_CONFIG_BINDING: &str = "    uint texture_config = DYNAMIC != 0u ? (misc.w & 0x2307u) | (TEXTURE_CONFIG & 0x400u) : TEXTURE_CONFIG;";

fn assert_texture_config_binding(clean: &str) {
    let main = &clean[function_span(clean, "void", "main")];
    assert_eq!(main.lines().filter(|line| *line == TEXTURE_CONFIG_BINDING).count(), 1,
        "texture_config/source binding");
    let after = &main[main.find(TEXTURE_CONFIG_BINDING).unwrap() + TEXTURE_CONFIG_BINDING.len()..];
    let bytes = after.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_alphabetic() || bytes[at] == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') { at += 1; }
            if &bytes[start..at] == b"texture_config" {
                let before = bytes[..start].iter().rfind(|byte| !byte.is_ascii_whitespace());
                let tail = after[at..].trim_start();
                // Only the existing parenthesized bit reads are admitted.
                // Assignments, shadowing, out arguments and prefix/postfix
                // mutation cannot satisfy this restricted read grammar.
                assert!(before == Some(&b'(') &&
                    ((tail.starts_with('&') && !tail.starts_with("&=") && !tail.starts_with("&&")) ||
                     (tail.starts_with(">>") && !tail.starts_with(">>="))),
                    "texture_config must remain read-only after binding");
            }
        } else { at += 1; }
    }
}

pub(super) fn static_feature_elision(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) -> String {
    if constants[29] != 0 { return source.to_owned(); }
    let source_clean = comment_blanks(source);
    let source_clean = std::str::from_utf8(&source_clean).unwrap();
    for (name, value) in [("DYNAMIC", 0), ("LIGHTING", constants[26]), ("TEXTURE_CONFIG", constants[25])] {
        let declaration = format!("const uint {name} = {value}u;");
        assert_eq!(source_clean.lines().filter(|line| *line == declaration).count(), 1, "constant/source binding: {name}");
    }
    assert_texture_config_binding(source_clean);
    let base = static_helper_elision(constants, source);
    let clean = comment_blanks(&base);
    let clean = std::str::from_utf8(&clean).unwrap();
    let mut ranges = Vec::new();
    let mut removed = Vec::new();
    if constants[26] == 0 {
        ranges.push(constant_false_branch_span(clean, "    if (LIGHTING != 0u) {"));
        removed.extend([("void", "shade"), ("float", "lookup"), ("vec3", "rotate"), ("float", "quantize")]);
    }
    if constants[25] & 0x400 == 0 {
        ranges.push(constant_false_branch_span(clean, "    if ((texture_config & (1u << 10)) != 0u) {"));
        removed.extend([("float", "proctex_lookup"), ("vec4", "proctex_color"), ("float", "proctex_random"),
            ("float", "proctex_shift"), ("float", "proctex_clamp"), ("float", "proctex_combine"), ("vec4", "procedural")]);
    }
    ranges.extend(removed.iter().map(|(kind,name)| function_span(clean, kind, name)));
    ranges.sort_by_key(|range| range.start);
    assert!(ranges.windows(2).all(|pair| pair[0].end <= pair[1].start));
    let mut result = base.clone();
    for range in ranges.into_iter().rev() { result.replace_range(range, ""); }
    let after = comment_blanks(&result);
    for (_, name) in removed { assert!(!has_call_token(&after, name), "{name} still called outside removed closure"); }
    let tail = "    // the color leaves as whole bytes";
    assert_eq!(&base[base.find(tail).unwrap()..], &result[result.find(tail).unwrap()..], "alpha/fog/depth main tail remains byte identical");
    result
}

}


/// the alpha test's compare as raster.frag has it, and as Adreno's compiler
/// takes it: it fails to link the switch where it folds to a constant
/// function with a discard after it. Only Adreno gets the second, so other
/// GPUs keep the programs they have cached.
pub(super) const SWITCH_COMPARE: &str = "    switch (function) {
        case 0u: return false;
        case 1u: return true;
        case 2u: return value == reference;
        case 3u: return value != reference;
        case 4u: return value < reference;
        case 5u: return value <= reference;
        case 6u: return value > reference;
        default: return value >= reference;
    }";
pub(super) const ADRENO_COMPARE: &str = "    if (function < 2u) {
        return function == 1u;
    }
    if (function < 4u) {
        return (value == reference) == (function == 2u);
    }
    if (function < 6u) {
        return value < reference || (function == 5u && value == reference);
    }
    return value > reference || (function == 7u && value == reference);";
pub(super) static ADRENO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) const VERTEX_SOURCE: &str = r#"#version 310 es
layout(location = 0) in vec4 position;
layout(location = 1) in vec4 color;
layout(location = 2) in vec4 texcoords01;
layout(location = 3) in vec4 texcoord2_depth;
layout(location = 4) in vec4 quaternion;
layout(location = 5) in vec4 view;

layout(location = 0) out vec4 out_color;
layout(location = 1) out vec4 out_texcoords01;
layout(location = 2) out vec2 out_texcoord2;
layout(location = 3) out float out_depth_w;
layout(location = 4) out vec4 out_quaternion;
layout(location = 5) out vec4 out_view;
layout(location = 6) out float out_w;

void main() {
    float depth = clamp(texcoord2_depth.z, 0.0, 1.0);
    gl_Position = vec4(position.xy, (depth * 2.0 - 1.0) * position.w, position.w);
    out_color = color;
    out_texcoords01 = texcoords01;
    out_texcoord2 = texcoord2_depth.xy;
    out_depth_w = texcoord2_depth.z * position.w;
    out_quaternion = quaternion;
    out_view = view;
    out_w = position.w;
}
"#;

pub(super) const TRANSFER_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D target;
uniform ivec2 size;
uniform int input_height;
uniform int output_height;
uniform ivec2 scale;
uniform int flip;
uniform int input_format;
uniform int output_format;
uniform int source_row;
uniform int source_column;
uniform int target_row;
uniform int source_height;

uint expand(uint value, uint bits) {
    return (value << (8u - bits)) | (value >> (2u * bits - 8u));
}

uvec4 quantize(uvec4 c, int format) {
    switch (format) {
    case 0:
        return c;
    case 1:
        return uvec4(c.rgb, 255u);
    case 2:
        return uvec4(expand(c.r >> 3, 5u), expand(c.g >> 2, 6u), expand(c.b >> 3, 5u), 255u);
    case 3:
        return uvec4(expand(c.r >> 3, 5u), expand(c.g >> 3, 5u), expand(c.b >> 3, 5u), c.a >= 128u ? 255u : 0u);
    default:
        return (c >> 4) * 17u;
    }
}

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= size.x || at.y >= size.y) {
        return;
    }
    uvec4 sum = uvec4(0u);
    for (int dy = 0; dy < scale.y; dy++) {
        for (int dx = 0; dx < scale.x; dx++) {
            int x = at.x * scale.x + dx;
            int y = at.y * scale.y + dy;
            if (flip != 0) {
                y = input_height - 1 - y;
            }
            int row = source_height - 1 - (source_row + y);
            vec4 texel = imageLoad(source, ivec2(source_column + x, row));
            sum += quantize(uvec4(round(texel * 255.0)), input_format);
        }
    }
    uint count = uint(scale.x * scale.y);
    uvec4 pixel = quantize((sum + count / 2u) / count, output_format);
    imageStore(target, ivec2(at.x, output_height - 1 - (target_row + at.y)), vec4(pixel) / 255.0);
}
"#;

/// a surface's pixels from another's drawn over the same memory in another
/// format or layout, through the bytes memory would hold, as a write back
/// and an upload would make them.
pub(super) const REINTERPRET_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D target;
// width, height, bytes a pixel and format of each, whether each is tiled
uniform ivec4 target_shape;
uniform ivec4 source_shape;
uniform ivec2 tiled;
// the target's first row and how many
uniform ivec2 rows;
// how far into the source's memory the target starts
uniform int delta;

int place(int x, int y, int width, int bpp, int is_tiled) {
    if (is_tiled == 0) {
        return (y * width + x) * bpp;
    }
    int m = (x & 1) | ((y & 1) << 1) | ((x & 2) << 1) | ((y & 2) << 2) | ((x & 4) << 2) | ((y & 4) << 3);
    return (((y >> 3) * (width >> 3) + (x >> 3)) * 64 + m) * bpp;
}

ivec2 pixel(int index, int width, int is_tiled) {
    if (is_tiled == 0) {
        return ivec2(index % width, index / width);
    }
    int m = index & 63;
    int x = (m & 1) | ((m >> 1) & 2) | ((m >> 2) & 4);
    int y = ((m >> 1) & 1) | ((m >> 2) & 2) | ((m >> 3) & 4);
    int tile = index >> 6;
    int across = width >> 3;
    return ivec2(tile % across * 8 + x, tile / across * 8 + y);
}

uint encode(uvec4 c, int format) {
    switch (format) {
    case 0:
        return c.a | (c.b << 8) | (c.g << 16) | (c.r << 24);
    case 1:
        return c.b | (c.g << 8) | (c.r << 16);
    case 2:
        return ((c.r >> 3) << 11) | ((c.g >> 2) << 5) | (c.b >> 3);
    case 3:
        return ((c.r >> 3) << 11) | ((c.g >> 3) << 6) | ((c.b >> 3) << 1) | (c.a >= 128u ? 1u : 0u);
    default:
        return ((c.r >> 4) << 12) | ((c.g >> 4) << 8) | ((c.b >> 4) << 4) | (c.a >> 4);
    }
}

uint expand(uint value, uint bits) {
    return (value << (8u - bits)) | (value >> (2u * bits - 8u));
}

uvec4 decode(uint v, int format) {
    switch (format) {
    case 0:
        return uvec4(v >> 24, (v >> 16) & 255u, (v >> 8) & 255u, v & 255u);
    case 1:
        return uvec4((v >> 16) & 255u, (v >> 8) & 255u, v & 255u, 255u);
    case 2:
        return uvec4(expand(v >> 11, 5u), expand((v >> 5) & 63u, 6u), expand(v & 31u, 5u), 255u);
    case 3:
        return uvec4(expand(v >> 11, 5u), expand((v >> 6) & 31u, 5u), expand((v >> 1) & 31u, 5u), (v & 1u) != 0u ? 255u : 0u);
    default:
        return uvec4((v >> 12) * 17u, ((v >> 8) & 15u) * 17u, ((v >> 4) & 15u) * 17u, (v & 15u) * 17u);
    }
}

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= target_shape.x || at.y >= rows.y) {
        return;
    }
    int y = rows.x + at.y;
    int start = delta + place(at.x, y, target_shape.x, target_shape.z, tiled.x);
    uint value = 0u;
    for (int k = 0; k < target_shape.z; k++) {
        int offset = start + k;
        ivec2 p = pixel(offset / source_shape.z, source_shape.x, tiled.y);
        uvec4 c = uvec4(round(imageLoad(source, ivec2(p.x, source_shape.y - 1 - p.y)) * 255.0));
        value |= ((encode(c, source_shape.w) >> uint(8 * (offset % source_shape.z))) & 255u) << uint(8 * k);
    }
    imageStore(target, ivec2(at.x, target_shape.y - 1 - y), vec4(decode(value, target_shape.w)) / 255.0);
}
"#;

pub(super) const UPRIGHT_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp image2D;
layout(local_size_x = 8, local_size_y = 8) in;
layout(binding = 0, rgba8) uniform readonly image2D source;
layout(binding = 1, rgba8) uniform writeonly image2D upright;
uniform ivec2 size;
// the source drawn at this over 4 times the console's size
uniform int quarters;

void main() {
    ivec2 at = ivec2(gl_GlobalInvocationID.xy);
    if (at.x >= size.x || at.y >= size.y) {
        return;
    }
    ivec2 from = ivec2(size.y - 1 - at.y, size.x - 1 - at.x);
    imageStore(upright, at, imageLoad(source, (from * quarters + quarters / 2) / 4));
}
"#;

pub(super) const PACK_VERTEX_SOURCE: &str = r#"#version 310 es
void main() {
    vec2 corner = vec2(float((gl_VertexID & 1) * 4 - 1), float((gl_VertexID & 2) * 2 - 1));
    gl_Position = vec4(corner, 0.0, 1.0);
}
"#;

pub(super) const PACK_DEPTH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D depth;
layout(location = 0) out vec4 color;
void main() {
    uint value = uint(round(texelFetch(depth, ivec2(gl_FragCoord.xy), 0).r * 16777215.0));
    color = vec4(float(value & 255u), float((value >> 8) & 255u), float(value >> 16), 0.0) / 255.0;
}
"#;

pub(super) const PACK_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(0.0, 0.0, 0.0, float(texelFetch(stencil, ivec2(gl_FragCoord.xy), 0).r) / 255.0);
}
"#;

pub(super) const COPY_COLOR_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D source;
uniform int base;
uniform bool opaque;
uniform vec4 levels;
layout(location = 0) out vec4 color;
void main() {
    vec4 texel = texelFetch(source, ivec2(int(gl_FragCoord.x), max(base - int(gl_FragCoord.y), 0)), 0);
    texel = floor(round(texel * 255.0) * (levels + 1.0) / 256.0) / levels;
    color = opaque ? vec4(texel.rgb, 1.0) : texel;
}
"#;

pub(super) const COPY_DEPTH_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D depth;
uniform int base;
uniform bool stencil;
layout(location = 0) out vec4 color;
void main() {
    uint value = uint(round(texelFetch(depth, ivec2(int(gl_FragCoord.x), base - int(gl_FragCoord.y)), 0).r * 16777215.0));
    vec3 bytes = vec3(float(value >> 16), float((value >> 8) & 255u), float(value & 255u)) / 255.0;
    color = stencil ? vec4(0.0, bytes) : vec4(bytes, 1.0);
}
"#;

pub(super) const COPY_STENCIL_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp usampler2D stencil;
uniform int base;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(float(texelFetch(stencil, ivec2(int(gl_FragCoord.x), base - int(gl_FragCoord.y)), 0).r) / 255.0, 0.0, 0.0, 0.0);
}
"#;

pub(super) const SHADING_BLOCK: &str = "layout(std140, binding = 6) uniform Shading {
    vec4 floats[96];
    ivec4 integers[4];
    uvec4 shading_misc;
    vec4 depth_map;
    vec4 viewport;
};
#define bools shading_misc.x
";

pub(super) fn vertex_es(source: &str) -> Result<String, String> {
    let rules = [
        ("#version 450\n", "#version 310 es\nprecision highp float;\nprecision highp int;\n"),
        ("layout(location = 3) noperspective out float out_depth;", "layout(location = 3) out float out_depth_w;\nlayout(location = 6) out float out_w;"),
        ("gl_Position = vec4(position.xy, -position.z, position.w);", "gl_Position = vec4(position.xy, -2.0 * position.z - position.w, position.w);"),
        (
            "    out_depth = position.z / position.w * depth_map.x + depth_map.y;",
            "    out_depth_w = (position.z / position.w * depth_map.x + depth_map.y) * position.w;\n    out_w = position.w;",
        ),
        ("layout(std430, binding = 7) readonly buffer Inputs {\n    vec4 inputs[];\n};\n", ""),
    ];
    let mut out = source.replace("set = 0, ", "");
    for (from, to) in rules {
        if !out.contains(from) {
            return Err(format!("the translation has no {from:?}"));
        }
        out = out.replace(from, to);
    }
    let start = out.find("layout(std430, binding = 6) readonly buffer Shading {").ok_or("the translation has no Shading block")?;
    let end = start + out[start..].find("};\n").ok_or("the Shading block does not end")? + 3;
    out.replace_range(start..end, SHADING_BLOCK);
    let lines: Vec<String> = out.lines().filter(|line| !line.trim_start().starts_with("uint base = uint(gl_VertexIndex)")).map(str::to_owned).collect();
    out = lines.join("\n") + "\n";
    let mut inputs = std::collections::BTreeSet::new();
    let mut rewritten = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find("inputs[base + ") {
        rewritten.push_str(&rest[..at]);
        let after = &rest[at + "inputs[base + ".len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let slot: usize = digits.parse().map_err(|_| "an input read the translation did not number")?;
        if !after[digits.len()..].starts_with("u]") {
            return Err("an input read the translation did not number".into());
        }
        inputs.insert(slot);
        rewritten.push_str(&format!("in_{slot}"));
        rest = &after[digits.len() + 2..];
    }
    rewritten.push_str(rest);
    if rewritten.contains("gl_VertexIndex") || rewritten.contains("inputs[") {
        return Err("the translation reads its inputs some other way".into());
    }
    let declarations: String = inputs.iter().map(|slot| format!("layout(location = {slot}) in vec4 in_{slot};\n")).collect();
    let header_end = rewritten.find("precision highp int;\n").map(|at| at + "precision highp int;\n".len()).unwrap_or(0);
    rewritten.insert_str(header_end, &declarations);
    Ok(rewritten)
}

/// the generic fragment program's constants, which draws whatever the
/// combination through uniforms.
/// the registers fragment_constants reads, which the manifest keeps whole,
/// so a later build works its own constants out from them.
pub(super) fn key_registers() -> impl Iterator<Item = usize> {
    STAGE_REGISTERS
        .into_iter()
        .flat_map(|base| [base, base + 1, base + 2, base + 4])
        .chain(TEXTURE_UNIT_BASES.map(|base| base + 2))
        .chain([REG_UPDATE_BUFFER, REG_TEXTURE_CONFIG, REG_ALPHA_TEST, REG_COLOR_OPERATION, REG_BLEND_FUNC, REG_LOGIC_OP, REG_DEPTH_COLOR_MASK, REG_COLOR_BUFFER_WRITE])
        .chain(crate::lighting::SHAPE_REGISTERS)
}

/// a draw's fragment program as its constants and its lighting's shape,
/// from its registers, the units enabled, those with their alpha apart and
/// those reading a drawing in place, how its depth goes, and its lighting
/// as the uniform block packs it.
pub(super) fn fragment_constants(r: &[u32], enabled: u32, split: u32, sources: [bool; 3], depth_flags: u32, writes_depth: bool, lighting: Option<&[u32]>) -> ([u32; FRAGMENT_CONSTANTS], Vec<u32>) {
    let mut constants = [0u32; FRAGMENT_CONSTANTS];
    for (i, base) in STAGE_REGISTERS.into_iter().enumerate() {
        constants[i] = r[base] & 0x0FFF_0FFF;
        constants[6 + i] = r[base + 1] & 0x0077_7FFF;
        constants[12 + i] = r[base + 2] & 0x000F_000F;
        constants[18 + i] = r[base + 4] & 0x0003_0003;
    }
    // the buffer updates, and the fog's mode and which way its depth runs
    constants[24] = r[REG_UPDATE_BUFFER] & 0x1_FF07;
    constants[25] = ((r[REG_TEXTURE_CONFIG] & !7) | enabled) & 0x2707 | split << 24;
    for (unit, base) in TEXTURE_UNIT_BASES.into_iter().enumerate() {
        let config = r[base + 2];
        let unbordered = enabled & 1 << unit != 0 && (config >> 8) & 3 != 1 && (config >> 12) & 3 != 1;
        constants[25] |= (sources[unit] as u32) << (16 + unit) | (unbordered as u32) << (20 + unit);
    }
    // unit 0 projected, which divides its coordinates by set 0's w
    constants[25] |= (enabled & 1 != 0 && r[TEXTURE_UNIT_BASES[0] + 2] >> 28 & 7 == 3) as u32 * PROJECTED;
    constants[26] = lighting.is_some() as u32;
    constants[27] = r[REG_ALPHA_TEST] & 0x71 | unused_outputs(r);
    constants[28] = depth_flags | if writes_depth { WRITES_DEPTH } else { 0 };
    (constants, lighting.map_or_else(Vec::new, lighting_shape))
}

pub(super) fn generic_constants(constants: &[u32; FRAGMENT_CONSTANTS]) -> [u32; FRAGMENT_CONSTANTS] {
    let mut generic = [0u32; FRAGMENT_CONSTANTS];
    generic[25] = constants[25] & 0x400;
    generic[26] = constants[26];
    generic[28] = constants[28] & WRITES_DEPTH;
    generic[29] = 1;
    generic
}

pub(super) fn lighting_shape(w: &[u32]) -> Vec<u32> {
    let count = w[1] as usize;
    let mut shape = Vec::with_capacity(29 + count * 2);
    shape.extend_from_slice(&w[..8]);
    for i in 0..7 {
        shape.extend([w[8 + i * 4], w[9 + i * 4], w[10 + i * 4]]);
    }
    for slot in 0..count {
        shape.extend([w[40 + slot * 36 + 32], w[40 + slot * 36 + 33]]);
    }
    shape
}

pub(super) fn lighting_source(shape: &[u32]) -> String {
    let (config, count, bump, shadow) = (shape[0], shape[1] as usize, shape[2], shape[3]);
    let (fresnel_primary, fresnel_secondary, clamp_highlights, needs) = (shape[4] != 0, shape[5] != 0, shape[6] != 0, shape[7]);
    let lookup = |i: usize| (shape[8 + i * 3] != 0, shape[9 + i * 3], shape[10 + i * 3] != 0);
    let mut out = String::from("void shade(vec4 textures[4], out vec4 diffuse_out, out vec4 specular_out) {\n");
    out += "    float length_q = length(in_quaternion);\n";
    out += "    vec4 q = length_q > 0.0 ? in_quaternion / length_q : vec4(0.0, 0.0, 0.0, 1.0);\n";
    out += "    vec4 shadow = vec4(1.0);\n";
    if shadow & 1 != 0 {
        out += &format!("    shadow = textures[{}];\n", (shadow >> 4) & 3);
        if shadow & 0x100 != 0 {
            out += "    shadow = vec4(1.0) - shadow;\n";
        }
    }
    out += "    vec3 surface_normal = vec3(0.0, 0.0, 1.0);\n    vec3 surface_tangent = vec3(1.0, 0.0, 0.0);\n";
    match bump & 3 {
        1 => {
            out += &format!("    surface_normal = textures[{}].xyz * 2.0 - 1.0;\n", (bump >> 4) & 3);
            if bump & 0x100 != 0 {
                out += "    surface_normal.z = sqrt(max(1.0 - dot(surface_normal.xy, surface_normal.xy), 0.0));\n";
            }
        }
        2 => out += &format!("    surface_tangent = textures[{}].xyz * 2.0 - 1.0;\n", (bump >> 4) & 3),
        _ => {}
    }
    out += "    vec3 normal = rotate(q, surface_normal);\n";
    out += if config == 8 { "    vec3 tangent = rotate(q, surface_tangent);\n" } else { "    vec3 tangent = surface_tangent;\n" };
    out += "    vec3 view = in_view.xyz;\n";
    out += if needs & 2 != 0 { "    vec3 norm_view = normalize(view);\n" } else { "    vec3 norm_view = vec3(0.0);\n" };
    out += "    vec4 diffuse_sum = vec4(0.0, 0.0, 0.0, 1.0);\n    vec4 specular_sum = vec4(0.0, 0.0, 0.0, 1.0);\n";
    const TABLES: [&str; 6] = ["DISTRIBUTION0", "DISTRIBUTION1", "FRESNEL", "REFLECT_RED", "REFLECT_GREEN", "REFLECT_BLUE"];
    for slot in 0..count {
        let (flags, number) = (shape[29 + slot * 2], shape[30 + slot * 2]);
        let two_sided = flags & 2 != 0;
        let l = format!("lights[{slot}]");
        out += "    {\n";
        if flags & 1 != 0 {
            out += &format!("        vec3 light_vector = {l}.direction.xyz;\n");
        } else {
            out += &format!("        vec3 light_vector = normalize({l}.position.xyz + view);\n");
        }
        out += "        vec3 half_vector = vec3(0.0);\n        vec3 half_unit = vec3(0.0);\n";
        if needs & 1 != 0 {
            out += "        half_vector = norm_view + light_vector;\n        half_unit = normalize(half_vector);\n";
        }
        out += "        float distance = 1.0;\n";
        if flags & 16 != 0 {
            out += &format!("        {{\n            vec3 offset = -view - {l}.position.xyz;\n");
            out += &format!("            float place = clamp({l}.distance.y * length(offset) + {l}.distance.x, 0.0, 1.0);\n");
            out += "            float entry = clamp(floor(place * 256.0), 0.0, 255.0);\n";
            out += &format!("            distance = lookup(DISTANCE + {number}u, uint(entry), place * 256.0 - entry);\n        }}\n");
        }
        for i in 0..7 {
            let (enabled, input, absolute) = lookup(i);
            if !enabled || (i == 6 && flags & 32 == 0) {
                out += &format!("        float v{i} = 1.0;\n");
                continue;
            }
            let expression = match input {
                0 => "dot(normal, half_unit)".to_owned(),
                1 => "dot(norm_view, half_unit)".to_owned(),
                2 => "dot(normal, norm_view)".to_owned(),
                3 => "dot(light_vector, normal)".to_owned(),
                4 => format!("dot(light_vector, {l}.spot.xyz)"),
                5 if config == 8 => "dot(half_unit - normal * dot(normal, half_unit), tangent)".to_owned(),
                _ => "0.0".to_owned(),
            };
            let table = if i == 6 { format!("SPOTLIGHT + {number}u") } else { TABLES[i].to_owned() };
            out += &format!("        float v{i};\n        {{\n            float result = {expression};\n");
            if absolute {
                out += if two_sided { "            result = abs(result);\n" } else { "            result = max(result, 0.0);\n" };
                out += "            float e = clamp(floor(result * 256.0), 0.0, 255.0);\n";
                out += &format!("            v{i} = uintBitsToFloat(lookups[{i}].w) * lookup({table}, uint(e), result * 256.0 - e);\n");
            } else {
                out += "            float e = clamp(floor(result * 128.0), -128.0, 127.0);\n";
                out += &format!("            v{i} = uintBitsToFloat(lookups[{i}].w) * lookup({table}, uint(int(e)) & 0xFFu, result * 128.0 - e);\n");
            }
            out += "        }\n";
        }
        let green = if lookup(4).0 { "v4" } else { "v3" };
        let blue = if lookup(5).0 { "v5" } else { "v3" };
        out += &format!("        vec3 reflect_color = vec3(v3, {green}, {blue});\n");
        out += &format!("        vec3 specular0 = {l}.specular0.rgb * v0;\n");
        out += &format!("        vec3 specular1 = v1 * reflect_color * {l}.specular1.rgb;\n");
        if slot == count - 1 && lookup(2).0 {
            if fresnel_primary {
                out += "        diffuse_sum.a = v2;\n";
            }
            if fresnel_secondary {
                out += "        specular_sum.a = v2;\n";
            }
        }
        out += "        float facing = dot(light_vector, normal);\n";
        out += if two_sided { "        facing = abs(facing);\n" } else { "        facing = max(facing, 0.0);\n" };
        out += if clamp_highlights { "        float highlights = facing == 0.0 ? 0.0 : 1.0;\n" } else { "        float highlights = 1.0;\n" };
        if flags & 12 != 0 {
            out += "        float length2 = dot(half_vector, half_vector);\n";
            out += "        float factor = length2 == 0.0 ? 0.0 : min(facing / length2, 1.0);\n";
            if flags & 4 != 0 {
                out += "        specular0 *= factor;\n";
            }
            if flags & 8 != 0 {
                out += "        specular1 *= factor;\n";
            }
        }
        let shadowed = flags & 64 != 0 && shadow & 1 != 0;
        let primary = if shadowed && shadow & 0x200 != 0 { "shadow.rgb" } else { "vec3(1.0)" };
        let secondary = if shadowed && shadow & 0x400 != 0 { "shadow.rgb" } else { "vec3(1.0)" };
        out += &format!("        vec3 diffuse = ({l}.diffuse.rgb * facing + {l}.ambient.rgb) * distance * v6;\n");
        out += "        vec3 specular = (specular0 + specular1) * highlights * distance * v6;\n";
        out += &format!("        diffuse_sum.rgb += diffuse * {primary};\n        specular_sum.rgb += specular * {secondary};\n    }}\n");
    }
    if shadow & 0x800 != 0 {
        if fresnel_primary {
            out += "    diffuse_sum.a *= shadow.a;\n";
        }
        if fresnel_secondary {
            out += "    specular_sum.a *= shadow.a;\n";
        }
    }
    out += "    diffuse_sum.rgb += global_ambient.rgb;\n";
    out += "    diffuse_out = vec4(quantize(diffuse_sum.r), quantize(diffuse_sum.g), quantize(diffuse_sum.b), quantize(diffuse_sum.a));\n";
    out += "    specular_out = vec4(quantize(specular_sum.r), quantize(specular_sum.g), quantize(specular_sum.b), quantize(specular_sum.a));\n";
    out += "}\n\n";
    out
}

/// a vertex program multiplying as the host does rather than working out the
/// PICA's 0 times infinity = 0 for every product, a third of what the Mali
/// spent on a battle. what makes infinities, reciprocals, roots, logarithms
/// and powers, is held at the largest finite float instead, so 0 times one
/// of them still comes to 0.
pub(super) fn plain_multiplies(source: &str) -> String {
    const LARGEST: &str = "-3.4028235e38, 3.4028235e38";
    source
        .replace("    return mix(product, mix(mix(0.0, product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("    return mix(product, mix(mix(vec4(0.0), product, isnan(b)), product, isnan(a)), isnan(product));", "    return product;")
        .replace("vec4(1.0 / a.x);", &format!("vec4(clamp(1.0 / a.x, {LARGEST}));"))
        .replace("vec4(1.0 / sqrt(a.x));", &format!("vec4(clamp(1.0 / sqrt(a.x), {LARGEST}));"))
        .replace("vec4(log2(a.x));", &format!("vec4(clamp(log2(a.x), {LARGEST}));"))
        .replace("vec4(exp2(a.x));", &format!("vec4(clamp(exp2(a.x), {LARGEST}));"))
}

/// the six combiner stages of one combination as straight code, each
/// reading only what it uses: Mali's compiler on the R36S builds a program
/// in three quarters of the time it takes over the generic stages' switches.
fn combiner_inputs(operation: u32) -> u32 {
    match operation { 0 => 1, 4 | 8 | 9 => 3, _ => 2 }
}

fn combiner_passes(source: u32, operand: u32, combiner: u32, scale: u32) -> bool {
    source & 0x000F_000F == 0x000F_000F && operand & 0x700F == 0 && combiner & 0x000F_000F == 0 && scale == 0
}

// Changing the template requires an explicit liveness review. Release builds
// conservatively retain all words until that review updates this contract.
const fn fragment_template_fingerprint(bytes: &[u8]) -> u64 {
    let (mut at, mut hash) = (0, 0xCBF2_9CE4_8422_2325u64);
    while at < bytes.len() {
        hash = (hash ^ bytes[at] as u64).wrapping_mul(0x100_0000_01B3);
        at += 1;
    }
    hash
}
const FRAGMENT_LIVENESS_CONTRACT: bool = fragment_template_fingerprint(FRAGMENT_SOURCE.as_bytes()) == 0x33C51D8133871264u64;

#[cfg(test)]
fn assert_generated_static_uniform_contract(constants: &[u32; FRAGMENT_CONSTANTS], source: &str) {
    assert_eq!(constants[29], 0);
    let clean = comment_blanks(source);
    let clean = std::str::from_utf8(&clean).unwrap();
    let admitted = [TEXTURE_CONFIG_BINDING.trim(),
        "uint update = (DYNAMIC != 0u ? misc.x : UPDATE) & 0xFF00u;",
        "uint alpha_test = DYNAMIC != 0u ? misc.z & 0x71u : ALPHA_TEST;",
        "uint fog_mode = DYNAMIC != 0u ? misc.x : UPDATE;"];
    let mut reads = 0;
    for line in clean.lines().filter(|line| line.contains("misc.")) {
        assert!(admitted.contains(&line.trim()), "new static misc read requires liveness review: {line}");
        reads += 1;
    }
    assert_eq!(reads, 4, "generated static misc paths must match the audited DYNAMIC=0 template");
    for line in clean.lines().filter(|line| line.contains("tev[")) {
        assert_eq!(line.trim(), "uvec4 tev[12];", "static emitted source must not read tev registers");
    }
    assert!(!clean.contains("colors[stage]"), "no runtime stage-color lookup in static emitted source");
    let emitted = (0..6).fold(0u8, |mask, color| mask | (u8::from(clean.contains(&format!("colors[{color}]"))) << color));
    assert_eq!(FragmentUniformLiveness::for_constants(constants), FragmentUniformLiveness::StaticTev { stage_colors: emitted });
}

/// Uniform reads of the fragment program actually selected for a draw.
/// Generic fallbacks must retain their register words and every stage color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FragmentUniformLiveness {
    All,
    StaticTev { stage_colors: u8 },
}

impl FragmentUniformLiveness {
    pub(super) fn for_constants(constants: &[u32; FRAGMENT_CONSTANTS]) -> Self {
        Self::for_contract(constants, FRAGMENT_LIVENESS_CONTRACT)
    }

    fn for_contract(constants: &[u32; FRAGMENT_CONSTANTS], template_matches: bool) -> Self {
        if constants[29] != 0 || !template_matches { return Self::All; }
        let mut stage_colors = 0;
        for stage in 0..6 {
            let (source, operand, combiner, scale) = (constants[stage], constants[6 + stage], constants[12 + stage], constants[18 + stage]);
            if combiner_passes(source, operand, combiner, scale) { continue; }
            let (color_op, alpha_op) = ((combiner & 0xF).min(9), ((combiner >> 16) & 0xF).min(9));
            let color = (0..combiner_inputs(color_op)).any(|i| (source >> (i * 4)) & 0xF == 0xE);
            let alpha = color_op != 7 && (0..combiner_inputs(alpha_op)).any(|i| (source >> (16 + i * 4)) & 0xF == 0xE);
            if color || alpha { stage_colors |= 1 << stage; }
        }
        Self::StaticTev { stage_colors }
    }

    pub(super) fn canonicalize(self, words: &mut [u32]) {
        let Self::StaticTev { stage_colors } = self else { return };
        assert_eq!(words.len(), UNIFORM_WORDS);
        // Static TEV reads its shape from constants, never tev[] or misc.
        words[..52].fill(0);
        let colors = UNIFORM_WORDS - COLOR_WORDS - FOG_WORDS;
        for stage in 0..6 {
            if stage_colors & (1 << stage) == 0 {
                words[colors + stage * 4..colors + (stage + 1) * 4].fill(0);
            }
        }
    }
}

pub(super) fn combiner_source(constants: &[u32; FRAGMENT_CONSTANTS]) -> String {
    let input = |source: u32, stage: usize| match source {
        0 => "primary".to_owned(),
        1 => "fragment_primary".to_owned(),
        2 => "fragment_secondary".to_owned(),
        3..=6 => format!("textures[{}]", source - 3),
        0xD => "held".to_owned(),
        0xE => format!("colors[{stage}]"),
        _ => "previous".to_owned(),
    };
    let color = |v: &str, operand: u32| match operand {
        0x1 => format!("vec3(1.0) - {v}.rgb"),
        0x2 => format!("vec3({v}.a)"),
        0x3 => format!("vec3(1.0 - {v}.a)"),
        0x4 => format!("vec3({v}.r)"),
        0x5 => format!("vec3(1.0 - {v}.r)"),
        0x8 => format!("vec3({v}.g)"),
        0x9 => format!("vec3(1.0 - {v}.g)"),
        0xC => format!("vec3({v}.b)"),
        0xD => format!("vec3(1.0 - {v}.b)"),
        _ => format!("{v}.rgb"),
    };
    let alpha = |v: &str, operand: u32| match operand {
        0 => format!("{v}.a"),
        1 => format!("1.0 - {v}.a"),
        2 => format!("{v}.r"),
        3 => format!("1.0 - {v}.r"),
        4 => format!("{v}.g"),
        5 => format!("1.0 - {v}.g"),
        6 => format!("{v}.b"),
        _ => format!("1.0 - {v}.b"),
    };
    // how an operation combines its inputs
    let combine = |op: u32, one: &str, zero: &str, p: &str| match op {
        0 => format!("{p}0"),
        1 => format!("{p}0 * {p}1"),
        2 => format!("min({p}0 + {p}1, {one})"),
        3 => format!("clamp({p}0 + {p}1 - 0.5, 0.0, 1.0)"),
        4 => format!("{p}0 * {p}2 + {p}1 * ({one} - {p}2)"),
        5 => format!("max({p}0 - {p}1, {zero})"),
        6 | 7 if one == "1.0" => format!("{p}0"),
        6 | 7 => format!("vec3(clamp(dot({p}0 * 2.0 - 1.0, {p}1 * 2.0 - 1.0), 0.0, 1.0))"),
        8 => format!("min({p}0 * {p}1 + {p}2, {one})"),
        _ => format!("min({p}0 + {p}1, {one}) * {p}2"),
    };
    let factor = |raw: u32| match raw & 3 {
        1 => " * 2.0",
        2 => " * 4.0",
        _ => "",
    };
    let update = constants[24] & 0xFF00;
    let mut out = String::new();
    for stage in 0..6 {
        let (source, operand, combiner, scale) = (constants[stage], constants[6 + stage], constants[12 + stage], constants[18 + stage]);
        let passes = combiner_passes(source, operand, combiner, scale);
        if !passes {
            let (color_op, alpha_op) = ((combiner & 0xF).min(9), ((combiner >> 16) & 0xF).min(9));
            out += "    {\n";
            for i in 0..combiner_inputs(color_op) {
                out += &format!("        vec3 c{i} = {};\n", color(&input((source >> (i * 4)) & 0xF, stage), (operand >> (i * 4)) & 0xF));
            }
            out += &format!("        vec3 rgb = {};\n", combine(color_op, "vec3(1.0)", "vec3(0.0)", "c"));
            if color_op == 7 {
                out += "        float alpha = rgb.r;\n";
            } else {
                for i in 0..combiner_inputs(alpha_op) {
                    out += &format!("        float a{i} = {};\n", alpha(&input((source >> (16 + i * 4)) & 0xF, stage), (operand >> (12 + i * 4)) & 0x7));
                }
                out += &format!("        float alpha = {};\n", combine(alpha_op, "1.0", "0.0", "a"));
            }
            out += &format!("        previous = clamp(vec4(rgb{}, alpha{}), 0.0, 1.0);\n    }}\n", factor(scale), factor(scale >> 16));
        }
        out += "    held = next_buffer;\n";
        if stage < 4 && update & (0x100 << stage) != 0 {
            out += "    next_buffer.rgb = previous.rgb;\n";
        }
        if stage < 4 && update & (0x1000 << stage) != 0 {
            out += "    next_buffer.a = previous.a;\n";
        }
    }
    out
}

#[cfg(test)]
mod uniform_liveness_tests {
    use super::*;

    #[test]
    fn fragment_liveness_contract_requires_template_review() {
        assert!(FRAGMENT_LIVENESS_CONTRACT, "GLSL template changed: review static misc/tev/color liveness before updating its fingerprint");
        let constants = [0; FRAGMENT_CONSTANTS];
        assert_eq!(FragmentUniformLiveness::for_contract(&constants, false), FragmentUniformLiveness::All,
            "unknown template retains all words in production");
    }

    #[test]
    fn fragment_liveness_matches_full_generated_static_sources() {
        for lighting in [0, 1] {
            for procedural in [0, 0x400] {
                for depth in [false, true] {
                    for operation in 0..16 {
                        let mut constants = [0; FRAGMENT_CONSTANTS];
                        constants[26] = lighting;
                        constants[25] = procedural;
                        for stage in 0..6 {
                            constants[stage] = 0x000E_000E;
                            constants[12 + stage] = operation | operation << 16;
                        }
                        let source = fragment_source(&constants, depth, &[]);
                        assert_generated_static_uniform_contract(&constants, &source);
                    }
                }
            }
        }
    }

    #[test]
    fn fragment_uniform_liveness_matches_emitted_color_reads() {
        for stage in 0..6 {
            for color_op in 0..16 {
                for alpha_op in 0..16 {
                    for input in 0..6 {
                        let mut constants = [0; FRAGMENT_CONSTANTS];
                        constants[..6].fill(0x000F_000F);
                        let shift = if input < 3 { input * 4 } else { 16 + (input - 3) * 4 };
                        constants[stage] = (constants[stage] & !(15 << shift)) | (14 << shift);
                        constants[12 + stage] = color_op | alpha_op << 16;
                        let source = combiner_source(&constants);
                        let emitted = (0..6).fold(0u8, |mask, color| mask | (u8::from(source.contains(&format!("colors[{color}]"))) << color));
                        assert_eq!(FragmentUniformLiveness::for_constants(&constants), FragmentUniformLiveness::StaticTev { stage_colors: emitted },
                            "stage={stage} rgb={color_op} alpha={alpha_op} input={input}");
                    }
                }
            }
        }
    }

    #[test]
    fn fragment_uniform_liveness_preserves_active_bits_and_generic_words() {
        let mut constants = [0; FRAGMENT_CONSTANTS];
        constants[..6].fill(0x000F_000F);
        constants[2] = 0x000E_000E;
        let liveness = FragmentUniformLiveness::for_constants(&constants);
        assert_eq!(liveness, FragmentUniformLiveness::StaticTev { stage_colors: 4 });
        let bits = [0x8000_0000, 0x7F80_0001, 0xFFC0_1234, 0x3F80_0000];
        let original: Vec<u32> = (0..UNIFORM_WORDS).map(|i| bits[i % bits.len()]).collect();
        let mut canonical = original.clone();
        liveness.canonicalize(&mut canonical);
        let colors = UNIFORM_WORDS - COLOR_WORDS - FOG_WORDS;
        for word in 0..UNIFORM_WORDS {
            let dead = word < 52 || ((colors..colors + 24).contains(&word) && (word - colors) / 4 != 2);
            assert_eq!(canonical[word], if dead { 0 } else { original[word] }, "word{word}");
            let mut changed = original.clone();
            changed[word] ^= 1;
            liveness.canonicalize(&mut changed);
            assert_eq!(changed == canonical, dead, "word{word}");
        }
        let once = canonical.clone();
        liveness.canonicalize(&mut canonical);
        assert_eq!(canonical, once);
        for mode in [1, 2, u32::MAX] {
            constants[29] = mode;
            let generic = FragmentUniformLiveness::for_constants(&constants);
            assert_eq!(generic, FragmentUniformLiveness::All);
            let mut words = original.clone();
            generic.canonicalize(&mut words);
            assert_eq!(words, original);
        }
    }
}

pub(super) fn fragment_source(constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32]) -> String {
    let source = fragment_source_unelided(constants, writes_depth, lighting);
    elide_or_original(constants, source)
}

fn fragment_source_unelided(constants: &[u32; FRAGMENT_CONSTANTS], writes_depth: bool, lighting: &[u32]) -> String {
    let mut out = String::from("#version 310 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n");
    if writes_depth {
        out.push_str("#define WRITES_DEPTH\n");
    }
    let mut skipping = false;
    for line in FRAGMENT_SOURCE.lines().skip(1) {
        if !lighting.is_empty() && line.starts_with("void shade(") {
            out += &lighting_source(lighting);
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with("precise vec3 color_operand") {
                skipping = false;
            } else {
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("layout(constant_id = ") {
            let (id, rest) = rest.split_once(')').unwrap_or_default();
            let name = rest.trim().trim_start_matches("const uint ").split(' ').next().unwrap_or_default();
            out += &format!("const uint {name} = {}u;\n", constants[id.parse::<usize>().unwrap_or(0)]);
        } else if line.contains("noperspective in float in_depth") {
            out += "layout(location = 3) in float in_depth_w;\nlayout(location = 6) in float in_w;\n#define in_depth (in_depth_w / in_w)\n";
        } else if line.trim_start().starts_with("combine_stage(") && constants[29] == 0 {
            if line.trim_start().starts_with("combine_stage(0u") {
                out += &combiner_source(constants);
            }
        } else {
            out += &line.replace("set = 0, ", "").replace("precise ", "");
            out.push('\n');
        }
    }
    if crate::fast_with(crate::speedup::HALF) {
        // in fast mode the colors at half precision, which the GPU works
        // twice as fast as full, what places texels, depth and lights kept
        // full
        for (from, to) in [
            ("precision highp float;\nprecision highp int;\nprecision highp sampler2D;", "precision mediump float;\nprecision highp int;\nprecision mediump sampler2D;"),
            ("in vec4 in_texcoords01", "in highp vec4 in_texcoords01"),
            ("in vec2 in_texcoord2", "in highp vec2 in_texcoord2"),
            ("in vec4 in_view", "in highp vec4 in_view"),
            ("in float in_depth_w", "in highp float in_depth_w"),
            ("in float in_w", "in highp float in_w"),
            ("sample_unit(uint unit, vec2 uv)", "sample_unit(uint unit, highp vec2 uv)"),
            ("    vec2 st = ", "    highp vec2 st = "),
            ("    float depth = in_depth", "    highp float depth = in_depth"),
            ("    vec3 view = in_view.xyz", "    highp vec3 view = in_view.xyz"),
            ("vec3 offset = -view", "highp vec3 offset = -view"),
        ] {
            out = out.replace(from, to);
        }
    }
    if ADRENO.load(std::sync::atomic::Ordering::Relaxed) {
        out = out.replace(SWITCH_COMPARE, ADRENO_COMPARE);
    }
    // ZAKURO_UNROLLED_STAND_INS: the generic program with the six stages
    // written out as before the loop, for telling whether the loop is what
    // a driver draws wrong
    static UNROLLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *UNROLLED.get_or_init(|| crate::switch("ZAKURO_UNROLLED_STAND_INS", "the generic program's stages written out")) {
        out = out.replace("    if (DYNAMIC != 0u) {\n        // the generic program, which reads every stage's registers", "    if (false) {\n        // the generic program, which reads every stage's registers");
    }
    out
}


pub(super) const PRESENT_SOURCE: &str = r#"#version 310 es
precision highp float;
precision highp int;
uniform highp sampler2D source;
uniform ivec4 area;
uniform ivec4 screen;
uniform int upright;
uniform int smooth_scale;
layout(location = 0) out vec4 color;
void main() {
    if (smooth_scale != 0) {
        // each pixel of the area blends the screen's pixels it covers, so
        // they come out even when the area is not a whole multiple
        vec2 size = vec2(screen.xy);
        vec2 s = size / vec2(area.zw);
        vec2 pixel = vec2(gl_FragCoord.x - float(area.x), float(area.y + area.w) - gl_FragCoord.y) - 0.5;
        vec2 start = pixel * s;
        // more of the screen's pixels than the area's, as one drawn bigger
        // than shown: each of the area's averages a grid of reads spread
        // over all the screen's pixels it covers
        if (s.x > 1.0 || s.y > 1.0) {
            ivec2 n = ivec2(min(ceil(s), vec2(4.0)));
            vec3 sum = vec3(0.0);
            for (int j = 0; j < n.y; j++) {
                for (int i = 0; i < n.x; i++) {
                    vec2 at = clamp(start + (vec2(float(i), float(j)) + 0.5) * s / vec2(n), vec2(0.5), size - 0.5);
                    vec2 texel = upright != 0 ? at : vec2(float(screen.y) - at.y, float(screen.z - screen.w) - at.x);
                    sum += texture(source, texel / vec2(textureSize(source, 0))).rgb;
                }
            }
            color = vec4(sum / float(n.x * n.y), 1.0);
            return;
        }
        vec2 first = floor(start);
        vec2 past = clamp(((pixel + 1.0) * s - first - 1.0) / s, 0.0, 1.0);
        vec2 at = clamp(mix(first + 0.5 + past, start + 0.5 * s, step(1.0, s)), vec2(0.5), size - 0.5);
        vec2 texel = upright != 0 ? at : vec2(float(screen.y) - at.y, float(screen.z - screen.w) - at.x);
        color = vec4(texture(source, texel / vec2(textureSize(source, 0))).rgb, 1.0);
        return;
    }
    int sx = int(floor((gl_FragCoord.x - float(area.x)) * float(screen.x) / float(area.z)));
    int sy = int(floor((float(area.y + area.w) - gl_FragCoord.y) * float(screen.y) / float(area.w)));
    ivec2 at = upright != 0 ? ivec2(sx, sy) : ivec2(screen.y - 1 - sy, screen.z - 1 - screen.w - sx);
    color = vec4(texelFetch(source, at, 0).rgb, 1.0);
}
"#;

pub(super) const OVERLAY_VERTEX_SOURCE: &str = r#"#version 310 es
precision highp float;
layout(location = 0) in vec2 position;
layout(location = 1) in vec2 uv;
layout(location = 2) in vec4 color;
uniform vec2 size;
out vec2 at;
out vec4 tint;
void main() {
    at = uv;
    tint = color;
    gl_Position = vec4(position.x / size.x * 2.0 - 1.0, 1.0 - position.y / size.y * 2.0, 0.0, 1.0);
}
"#;

pub(super) const OVERLAY_FRAGMENT_SOURCE: &str = r#"#version 310 es
precision mediump float;
uniform sampler2D source;
in vec2 at;
in vec4 tint;
layout(location = 0) out vec4 color;
void main() {
    color = texture(source, at) * tint;
}
"#;

pub(super) const ROTATE_SOURCE: &str = r#"#version 310 es
precision highp float;
uniform highp sampler2D canvas;
uniform vec4 rect;
uniform int turn;
layout(location = 0) out vec4 color;
void main() {
    vec2 p = (gl_FragCoord.xy - rect.xy) / rect.zw;
    if (p.x < 0.0 || p.y < 0.0 || p.x > 1.0 || p.y > 1.0) {
        color = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }
    vec2 uv = turn == 0 ? vec2(1.0 - p.y, p.x) : vec2(p.y, 1.0 - p.x);
    color = vec4(texture(canvas, uv).rgb, 1.0);
}
"#;

/// Exact static-elision source, safety, pixel and depth verification.
/// The legacy module name preserves existing focused test selectors.
#[cfg(test)]
mod selective_tev_tests {
    use super::*;

    fn elision_fixture(lighting: u32, procedural: u32) -> ([u32; FRAGMENT_CONSTANTS], String) {
        let mut constants = [0; FRAGMENT_CONSTANTS];
        constants[26] = lighting;
        constants[25] = procedural;
        let mut source = format!("// fixture\nconst uint DYNAMIC = 0u;\nconst uint LIGHTING = {lighting}u;\nconst uint TEXTURE_CONFIG = {procedural}u;\n");
        for (kind, name) in DEAD_TEV_HELPERS.into_iter().chain([
            ("void", "shade"), ("float", "lookup"), ("vec3", "rotate"), ("float", "quantize"),
            ("float", "proctex_lookup"), ("vec4", "proctex_color"), ("float", "proctex_random"),
            ("float", "proctex_shift"), ("float", "proctex_clamp"), ("float", "proctex_combine"), ("vec4", "procedural"),
        ]) { source.push_str(&format!("{kind} {name}() {{ {{ }} }}\n")); }
        source.push_str(&format!("void main() {{\n{TEXTURE_CONFIG_BINDING}\n    // retained insertion\n    if (LIGHTING != 0u) {{\n        shade();\n    }}\n    if ((texture_config & (1u << 10)) != 0u) {{\n        procedural();\n    }}\n    // the color leaves as whole bytes\n}}\n"));
        (constants, source)
    }

    fn elision_panic_text(error: Box<dyn std::any::Any + Send>) -> String {
        if let Some(text) = error.downcast_ref::<String>() { text.clone() }
        else if let Some(text) = error.downcast_ref::<&str>() { text.to_string() }
        else { "non-string panic".to_string() }
    }

    fn compare_elision_acceptance(constants: &[u32; FRAGMENT_CONSTANTS], source: &str, rejection: Option<&str>) {
        for (old, new) in [
            (static_elision_reference::static_helper_elision as fn(_, _) -> _, static_helper_elision as fn(_, _) -> _),
            (static_elision_reference::static_feature_elision as fn(_, _) -> _, static_feature_elision as fn(_, _) -> _),
        ] {
            let before = std::panic::catch_unwind(|| old(constants, source));
            let after = std::panic::catch_unwind(|| new(constants, source));
            match (before, after) {
                (Ok(before), Ok(after)) => {
                    assert!(rejection.is_none(), "expected rejection family {rejection:?}");
                    assert_eq!(before, after);
                },
                (Err(before), Err(after)) => {
                    let before = elision_panic_text(before);
                    let after = elision_panic_text(after);
                    if let Some(family) = rejection {
                        assert!(before.contains(family) && after.contains(family), "panic family {family}: old={before}; new={after}");
                    }
                }
                _ => panic!("old/new elision acceptance differs"),
            }
        }
    }

    #[test]
    fn static_elision_scan_identifier_and_comment_equivalence() {
        let names = DEAD_TEV_HELPERS.into_iter().map(|(_, name)| name).chain([
            "shade", "lookup", "rotate", "quantize", "proctex_lookup", "proctex_color", "proctex_random",
            "proctex_shift", "proctex_clamp", "proctex_combine", "procedural",
        ]).collect::<Vec<_>>();
        let mut seen = 0;
        for name in &names {
            let bit = call_token_bit(name.as_bytes());
            assert!(bit.is_power_of_two());
            assert_eq!(seen & bit, 0);
            seen |= bit;
            for prefix in ["", "x", "_", "0", "é", "//", "/*"] {
                for suffix in ["()", " (", "\t\r\n\x0c(0)", "\x0b(", "\u{a0}(", "_suffix()", "7()", "", " /*c*/ \n (", "*/()"] {
                    let source = format!("{prefix}{name}{suffix}\n");
                    let clean = std::panic::catch_unwind(|| comment_blanks(&source));
                    if let Ok(clean) = clean {
                        let bits = call_token_bits(&clean);
                        for target in &names {
                            assert_eq!(bits & call_token_bit(target.as_bytes()) != 0, has_call_token(&clean, target), "{source:?}, {target}");
                        }
                    }
                }
            }
        }
        assert_eq!(seen.count_ones(), 20);
    }

    #[test]
    fn static_elision_scan_staged_acceptance_and_bytes() {
        for lighting in [0, 1] {
            for procedural in [0, 0x400] {
                let (constants, raw) = elision_fixture(lighting, procedural);
                assert!(static_feature_elision(&constants, &raw).len() < raw.len());
                compare_elision_acceptance(&constants, &raw, None);
                for insertion in ["// combine_stage() {\n", "/* combine_stage() { } */", "combine_stage_suffix();", "_combine_stage();", "combine_stage7();"] {
                    compare_elision_acceptance(&constants, &raw.replace("    // retained insertion", insertion), None);
                }
                for insertion in ["combine_stage();", "0combine_stage /* c */ \n ();", "écombine_stage();"] {
                    compare_elision_acceptance(&constants, &raw.replace("    // retained insertion", insertion), Some("combine_stage"));
                }
                // Stage one must reject this TEV call even when lighting=0 will
                // remove the entire containing branch during stage two.
                compare_elision_acceptance(&constants, &raw.replace("        shade();", "        shade(); combine_stage();"), Some("combine_stage"));
                for changed in [raw.replace("// fixture", "/* unterminated"), raw.replace("void combine_stage(", "void other_stage("), raw.replace("void combine_stage(", "void combine_stage() {}\nvoid combine_stage("), raw.replace("() { { } }", "() { /* } */ { } }"), raw.replace("\n", "\r\n"), raw.trim_end().to_owned(), raw.replace("    if (LIGHTING != 0u) {", "    if (LIGHTING  != 0u) {")] {
                    compare_elision_acceptance(&constants, &changed, None);
                }
                let mut generic = constants;
                generic[29] = 1;
                for malformed in ["/* unterminated", "void main(", "é// no newline", ""] {
                    assert_eq!(static_feature_elision(&generic, malformed), malformed);
                    compare_elision_acceptance(&generic, malformed, None);
                }
            }
        }
        // Test emitted-byte joins independently of GLSL validity. The post-
        // deletion scanner must inspect real joined output, including comments.
        for (source, range) in [("comXXbine_stage()", 3..5), ("/XX/ combine_stage()\n", 1..3), ("/XX* combine_stage() */", 1..3), ("combine_stage XX ()", 14..16)] {
            let mut old = source.to_owned();
            old.replace_range(range.clone(), "");
            let new = without_ranges(source, &[range]);
            assert_eq!(old, new);
            let clean = comment_blanks(&new);
            assert_eq!(call_token_bits(&clean) & call_token_bit(b"combine_stage") != 0, has_call_token(&clean, "combine_stage"));
        }
    }

    #[test]
    #[ignore = "CPU-only retained raw GLSL corpus; set ZAKURO_ELISION_CORPUS_PLAN"]
    fn static_elision_scan_differential_corpus() {
        // Plan rows: helper|feature TAB absolute retained sources.tsv path.
        // Every source is read directly; no generator, compiler or GL context.
        let plan = std::env::var_os("ZAKURO_ELISION_CORPUS_PLAN").expect("explicit corpus plan required");
        let plan = std::fs::read_to_string(plan).unwrap();
        let mut unique = std::collections::HashSet::new();
        let (mut total, mut input_bytes, mut output_bytes) = (0usize, 0usize, 0usize);
        for entry in plan.lines().filter(|line| !line.is_empty()) {
            let (stage, manifest) = entry.split_once('\t').expect("stage and manifest path");
            assert!(matches!(stage, "helper" | "feature"));
            let manifest = std::path::Path::new(manifest);
            assert!(manifest.is_absolute(), "explicit absolute manifest path");
            let text = std::fs::read_to_string(manifest).unwrap();
            let mut rows = text.lines();
            assert_eq!(rows.next().unwrap(), "case\tflags_hex\treference\tselective\tconstants\tlighting_shape");
            let mut count = 0;
            let previous_unique = unique.len();
            for line in rows {
                let fields = line.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 6);
                let raw_path = manifest.parent().unwrap().join(fields[2]);
                let expected_path = manifest.parent().unwrap().join(fields[3]);
                let raw = std::fs::read_to_string(&raw_path).unwrap();
                // Explicitly reject already-elided inputs instead of comparing
                // two transforms that both consumed the wrong corpus stage.
                for (kind, name) in DEAD_TEV_HELPERS {
                    assert!(raw.contains(&format!("\n{kind} {name}(")), "not a raw input: {}", raw_path.display());
                }
                let values = fields[4].strip_prefix('[').unwrap().strip_suffix(']').unwrap()
                    .split(',').map(|v| v.trim().parse::<u32>().unwrap()).collect::<Vec<_>>();
                let constants: [u32; FRAGMENT_CONSTANTS] = values.try_into().unwrap();
                let (old, new) = if stage == "helper" {
                    (static_elision_reference::static_helper_elision(&constants, &raw), static_helper_elision(&constants, &raw))
                } else {
                    (static_elision_reference::static_feature_elision(&constants, &raw), static_feature_elision(&constants, &raw))
                };
                assert_generated_static_uniform_contract(&constants, &new);
                let expected = std::fs::read(&expected_path).unwrap();
                if old.as_bytes() != new.as_bytes() || new.as_bytes() != expected {
                    let first = old.bytes().zip(new.bytes()).position(|(a,b)| a != b);
                    if let Some(directory) = std::env::var_os("ZAKURO_ELISION_FAILURE_DIR") {
                        let directory = std::path::Path::new(&directory);
                        std::fs::create_dir_all(directory).unwrap();
                        for (name, bytes) in [("old.frag", old.as_bytes()), ("new.frag", new.as_bytes()), ("expected.frag", expected.as_slice())] {
                            use std::io::Write;
                            std::fs::OpenOptions::new().write(true).create_new(true).open(directory.join(name)).unwrap().write_all(bytes).unwrap();
                        }
                    }
                    panic!("corpus mismatch {} {} stage={stage} raw={} old={} new={} expected={} first_old_new={first:?}", manifest.display(), fields[0], raw_path.display(), old.len(), new.len(), expected.len());
                }
                let mut generic = constants;
                generic[29] = 1;
                assert_eq!(static_feature_elision(&generic, &raw), raw);
                assert_eq!(static_elision_reference::static_feature_elision(&generic, &raw), raw);
                input_bytes += raw.len(); output_bytes += new.len();
                unique.insert(raw.into_bytes()); count += 1; total += 1;
            }
            assert!(count > 0);
            eprintln!("retained corpus stage={stage} manifest={} cases={count} new_unique_raw={} repeated_raw={}", manifest.display(), unique.len() - previous_unique, count - (unique.len() - previous_unique));
        }
        eprintln!("exact retained corpus comparisons={total} unique_raw={} repeated_raw={} input_bytes={input_bytes} output_bytes={output_bytes}; generic bypass exact", unique.len(), total - unique.len());
        assert!(total > 0);
    }

    #[test]
    fn production_elision_fallback_retains_bytes_and_warns_once() {
        let (constants, raw) = elision_fixture(0, 0);
        let warned = std::sync::atomic::AtomicBool::new(false);
        let warnings = std::cell::RefCell::new(Vec::new());
        for changed in [raw.replace("void combine_stage(", "void changed_stage("),
            raw.replace("        shade();", "        shade(); combine_stage();"),
            raw.replace(TEXTURE_CONFIG_BINDING, "    uint texture_config = misc.w;"),
            raw.replace("// fixture", "/* unterminated")] {
            assert!(checked_feature_elision(&constants, &changed).is_err());
            let original = changed.clone();
            let output = elide_or_original_with_warning(&constants, changed, &warned, |reason| warnings.borrow_mut().push(reason.to_owned()));
            assert_eq!(output, original, "failed structural proof returns every original byte");
        }
        assert_eq!(warnings.borrow().len(), 1, "only the first rejection emits a warning");
        assert_eq!(elide_or_original_with_warning(&constants, raw.clone(), &warned, |_| panic!("valid source must not warn")),
            static_feature_elision(&constants, &raw));
        let mut generic = constants;
        generic[29] = 1;
        assert_eq!(elide_or_original_with_warning(&generic, "/* malformed".to_owned(), &warned, |_| panic!("generic bypass must not warn")), "/* malformed");
    }

    #[test]
    fn feature_elision_requires_literal_false_constants_and_retains_enabled_features() {
        for lighting in [0, 1] {
            for procedural in [0, 0x400] {
                let mut constants = [0; FRAGMENT_CONSTANTS];
                constants[26] = lighting;
                constants[25] = procedural;
                let raw = fragment_source_unelided(&constants, true, &[]);
                let elided = static_feature_elision(&constants, &raw);
                assert_eq!(has_call_token(&comment_blanks(&elided), "shade"), lighting != 0);
                assert_eq!(has_call_token(&comment_blanks(&elided), "procedural"), procedural != 0);
                if lighting != 0 {
                    let before = comment_blanks(&raw);let after = comment_blanks(&elided);
                    assert_eq!(&raw[function_span(std::str::from_utf8(&before).unwrap(), "void", "shade")],
                        &elided[function_span(std::str::from_utf8(&after).unwrap(), "void", "shade")]);
                }
                constants[29] = 1;
                let generic = fragment_source_unelided(&constants, true, &[]);
                assert_eq!(static_feature_elision(&constants, &generic), generic);
            }
        }
    }

    #[test]
    #[should_panic(expected = "constant/source binding: LIGHTING")]
    fn feature_elision_rejects_mismatched_source_constants() {
        let mut constants = [0; FRAGMENT_CONSTANTS];
        let raw = fragment_source_unelided(&constants, false, &[]);
        constants[26] = 1;
        let _ = static_feature_elision(&constants, &raw);
    }

    #[test]
    #[should_panic(expected = "texture_config/source binding")]
    fn feature_elision_rejects_mismatched_local_texture_binding() {
        let constants = [0; FRAGMENT_CONSTANTS];
        let raw = fragment_source_unelided(&constants, false, &[])
            .replace(TEXTURE_CONFIG_BINDING, "    uint texture_config = misc.w;");
        let _ = static_feature_elision(&constants, &raw);
    }

    #[test]
    fn feature_elision_rejects_texture_config_writes_after_binding() {
        let constants = [0; FRAGMENT_CONSTANTS];
        let raw = fragment_source_unelided(&constants, false, &[]);
        for write in ["texture_config = 0u;", "texture_config |= 0x400u;", "texture_config >>= 1u;",
            "texture_config++;", "++texture_config;", "uint texture_config = 0u;", "mutate(texture_config);"] {
            let changed = raw.replace(TEXTURE_CONFIG_BINDING, &format!("{TEXTURE_CONFIG_BINDING}\n    {write}"));
            assert!(std::panic::catch_unwind(|| static_feature_elision(&constants, &changed)).is_err(),
                "must reject subsequent local write/unknown use: {write}");
        }
    }


    #[test]
    fn static_helper_elision_preserves_main_and_generic_source() {
        let mut constants = [0; FRAGMENT_CONSTANTS];
        let raw = fragment_source_unelided(&constants, false, &[]);
        let elided = static_helper_elision(&constants, &raw);
        let clean = comment_blanks(&raw);
        let clean = std::str::from_utf8(&clean).unwrap();
        let mut removed: Vec<_> = DEAD_TEV_HELPERS.into_iter().map(|(kind, name)| function_span(clean, kind, name)).collect();
        removed.sort_by_key(|range| range.start);
        let (mut expected, mut kept_from) = (String::new(), 0);
        for range in removed {
            expected.push_str(&raw[kept_from..range.start]);
            kept_from = range.end;
        }
        expected.push_str(&raw[kept_from..]);
        assert_eq!(elided, expected, "every byte outside the nine helper definitions must be identical");
        assert_eq!(fragment_source(&constants, false, &[]), static_feature_elision(&constants, &raw), "production wrapper must invoke the proven feature elision");
        assert!(elided.len() < raw.len());
        assert!(elided.contains("void shade("), "feature functions with dead-branch callers are conservatively retained");
        assert!(elided.contains("vec4 procedural("));
        constants[29] = 1;
        let generic = fragment_source_unelided(&constants, false, &[]);
        assert_eq!(fragment_source(&constants, false, &[]), generic, "generic source must remain byte identical");
        assert_eq!(static_helper_elision(&constants, &generic), generic);
        assert!(has_call_token(&comment_blanks("// combine_stage()\nmain(); /* combine_stage ( */"), "main"));
        assert!(!has_call_token(&comment_blanks("// combine_stage()\nmain(); /* combine_stage ( */"), "combine_stage"));
        assert!(has_call_token(&comment_blanks("combine_stage /* a comment */ \n (0u);"), "combine_stage"));
    }

    #[test]
    #[ignore = "explicit source corpus check; requires ZAKURO_TEV_MANIFEST"]
    fn static_elision_production_wrapper_matches_captured_states() {
        let path = std::env::var_os("ZAKURO_TEV_MANIFEST").expect("set local shader manifest path");
        let cases = captured_cases(std::path::Path::new(&path), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        for case in &cases {
            let (constants, lighting, _) = shape(case);
            let depth = case.flags & (1 << 16) != 0;
            let raw = fragment_source_unelided(&constants, depth, &lighting);
            assert_eq!(fragment_source(&constants, depth, &lighting), static_feature_elision(&constants, &raw), "{} production path", case.label);
            let generic = generic_constants(&constants);
            assert_eq!(fragment_source(&generic, depth, &lighting), fragment_source_unelided(&generic, depth, &lighting), "{} generic bypass", case.label);
        }
        eprintln!("production static elision wrapper and generic byte identity: {} captured states", cases.len());
    }

    #[derive(Clone)]
    struct Case { registers: [u32; 0x300], flags: u32, label: String }

    fn rng(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_ne_bytes()).collect()
    }

    fn shape(case: &Case) -> ([u32; FRAGMENT_CONSTANTS], Vec<u32>, Vec<u32>) {
        let mut packed = Vec::new();
        if case.flags & (1 << 17) != 0 {
            crate::lighting::Lighting::read(&case.registers).expect("captured enabled lighting").pack(&mut packed);
        }
        let (constants, lighting) = fragment_constants(&case.registers, case.flags & 7, case.flags >> 4 & 7,
            std::array::from_fn(|i| case.flags & (1 << (8 + i)) != 0), case.flags >> 12 & 3,
            case.flags & (1 << 16) != 0, (case.flags & (1 << 17) != 0).then_some(packed.as_slice()));
        (constants, lighting, packed)
    }


    fn eligible_for(constants: &[u32; FRAGMENT_CONSTANTS], operations: &[u32]) -> bool {
        constants[29] == 0 && constants[12..18].iter().all(|word| operations.contains(&(word & 15)) && operations.contains(&((word >> 16) & 15)))
    }

    fn source_fingerprint(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325u64, |hash, byte| (hash ^ *byte as u64).wrapping_mul(0x100_0000_01B3))
    }

    fn dump_fragment(dir: &std::path::Path, kind: &str, source: &str) -> String {
        let filename = format!("{kind}-{:016x}.frag", source_fingerprint(source.as_bytes()));
        let path = dir.join(&filename);
        match std::fs::read(&path) {
            Ok(prior) => assert_eq!(prior, source.as_bytes(), "source fingerprint collision must not overwrite evidence"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::write(path, source).unwrap(),
            Err(error) => panic!("read dumped source: {error}"),
        }
        filename
    }

    fn captured_cases(path: &std::path::Path, operations: &[u32]) -> Vec<Case> {
        let text = std::fs::read_to_string(path).expect("local shader manifest");
        assert_eq!(text.lines().next(), Some(MANIFEST_HEADER));
        let mut unique = std::collections::HashSet::new();
        let mut cases = Vec::new();
        for (line, record) in text.lines().enumerate().skip(1) {
            let mut fields = record.split_whitespace();
            fields.next().expect("vertex descriptor");
            let flags = u32::from_str_radix(fields.next().expect("flags"), 16).unwrap();
            let mut registers = [0; 0x300];
            for field in fields {
                let (register, value) = field.split_once('=').expect("register=value");
                registers[usize::from_str_radix(register, 16).unwrap()] = u32::from_str_radix(value, 16).unwrap();
            }
            let case = Case { registers, flags, label: format!("manifest-line-{}", line + 1) };
            let (constants, lighting, _) = shape(&case);
            if eligible_for(&constants, operations) && unique.insert((constants, lighting)) { cases.push(case); }
        }
        assert!(!cases.is_empty(), "no eligible captured state is not a differential test");
        cases
    }

    fn synthetic_cases_for(operations: &[u32]) -> Vec<Case> {
        const SOURCES: [u32; 10] = [0, 1, 2, 3, 4, 5, 6, 13, 14, 15];
        const COLOR_OPERANDS: [u32; 10] = [0, 1, 2, 3, 4, 5, 8, 9, 12, 13];
        let mut seed = 0x7D9E_123B;
        (0..80).map(|index| {
            let mut registers = [0; 0x300];
            for (stage, base) in STAGE_REGISTERS.into_iter().enumerate() {
                for arg in 0..3 {
                    // Systematic first argument coverage plus deterministic random
                    // second/third operands, including every complement form.
                    registers[base] |= SOURCES[(index + stage + arg) % SOURCES.len()] << (arg * 4);
                    registers[base] |= SOURCES[(rng(&mut seed) as usize) % SOURCES.len()] << (16 + arg * 4);
                    registers[base + 1] |= COLOR_OPERANDS[(index + stage + arg) % COLOR_OPERANDS.len()] << (arg * 4);
                    registers[base + 1] |= ((index + stage + arg) as u32 & 7) << (12 + arg * 4);
                }
                registers[base + 2] = operations[(index + stage) % operations.len()] | operations[(index / operations.len() + stage) % operations.len()] << 16;
                registers[base + 4] = ((index + stage) as u32 & 3) | (((index / 4 + stage) as u32 & 3) << 16);
            }
            registers[REG_UPDATE_BUFFER] = ((index as u32 * 73) & 255) << 8;
            // Preserve a visible RGBA result regardless of captured color masks.
            registers[REG_COLOR_OPERATION] = 0x100;
            registers[REG_BLEND_FUNC] = 0x0101_0000; // source RGBA factors ONE
            registers[REG_DEPTH_COLOR_MASK] = 0xF00;
            registers[REG_COLOR_BUFFER_WRITE] = 0xF;
            let lit = index & 1 != 0;
            if lit { registers[crate::lighting::REG_ENABLE] = 1; }
            if index & 2 != 0 { registers[REG_TEXTURE_CONFIG] |= 0x400; }
            Case { registers, flags: 7 | (1 << 16) | ((index as u32 & 3) << 12) | ((lit as u32) << 17), label: format!("synthetic-{index}") }
        }).collect()
    }

    const VERTEX: &str = r#"#version 310 es
precision highp float;
layout(std140, binding=8) uniform Primary { vec4 base_color; };
layout(location=0) out vec4 in_color;
layout(location=1) out vec4 in_texcoords01;
layout(location=2) out vec2 in_texcoord2;
layout(location=3) out float in_depth_w;
layout(location=4) out vec4 in_quaternion;
layout(location=5) out vec4 in_view;
layout(location=6) out float in_w;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    float w = 0.75 + float(gl_VertexID) * 0.25;
    gl_Position = vec4((p * 2.0 - 1.0) * w, 0.0, w);
    in_color = base_color + vec4((p.x * 17.0 + p.y * 29.0) / 255.0, (p.y * 31.0 - p.x * 13.0) / 255.0, p.y * 0.0625, -p.x * 0.0625);
    in_texcoords01 = vec4(p, p.yx) * 0.75 - 0.125;
    in_texcoord2 = p * 0.5;
    in_depth_w = (0.1 + p.x * 0.2) * w;
    in_w = w;
    in_quaternion = vec4(p * 0.1, 0.2, 0.95);
    in_view = vec4(p * 0.5 - 0.25, 0.75, 1.0);
}
"#;

    const DEPTH_VERTEX: &str = "#version 310 es\nvoid main() { vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2)); gl_Position=vec4(p*2.0-1.0,0.0,1.0); }\n";
    const DEPTH_FRAGMENT: &str = r#"#version 310 es
precision highp float;
precision highp int;
layout(binding=7) uniform highp sampler2D depth_source;
layout(location=0) out vec4 out_color;
void main() {
    uint bits = floatBitsToUint(texelFetch(depth_source, ivec2(gl_FragCoord.xy), 0).r);
    out_color = vec4(uvec4(bits, bits >> 8u, bits >> 16u, bits >> 24u) & uvec4(255u)) / 255.0;
}
"#;

    fn words(case: &Case, packed: &[u32], variant: usize, seed: &mut u32) -> Vec<u32> {
        let r = &case.registers;
        let mut words = Vec::with_capacity(UNIFORM_WORDS);
        for base in STAGE_REGISTERS { words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], 0, 0, 0]); }
        words.extend([r[REG_UPDATE_BUFFER], 0, r[REG_ALPHA_TEST], (r[REG_TEXTURE_CONFIG] & !7) | (case.flags & 7)]);
        for (unit, base) in TEXTURE_UNIT_BASES.into_iter().enumerate() {
            words.extend([r[base + 2], rng(seed), (case.flags >> (8 + unit)) & 1, (case.flags >> (4 + unit)) & 1]);
        }
        words.extend([case.flags >> 12 & 3, (case.flags >> 17) & 1, 0.75f32.to_bits(), 0.9f32.to_bits()]);
        if packed.is_empty() { words.resize(LIGHTING_BASE + 328, 0); } else { words.extend_from_slice(packed); }
        // Finite nonzero vectors/colors retain the actual captured lighting
        // shape, avoiding zero-light inputs that would hide TEV differences.
        if !packed.is_empty() {
            for offset in [36usize].into_iter().chain((0..8).flat_map(|light| (0..8).map(move |v| 40 + light * 36 + v * 4))) {
                for component in 0..3 {
                    words[LIGHTING_BASE + offset + component] = (0.125 + (rng(seed) & 1023) as f32 / 2048.0).to_bits();
                }
            }
        }
        words.extend([0; PROCTEX_WORDS]);
        for color in 0..7 {
            for channel in 0..4 {
                let value = match variant {
                    0 => [0.0, 1.0, 0.5, 1.0 / 255.0][(color + channel) % 4],
                    1 => ((rng(seed) % 256) as f32 + 0.4999) / 255.0,
                    2 => ((rng(seed) % 256) as f32 - 0.0001).max(0.0) / 255.0,
                    _ => (rng(seed) & 0xFFFF) as f32 / 65535.0,
                };
                words.push(value.min(1.0).to_bits());
            }
        }
        words.extend([(((r[REG_ALPHA_TEST] >> 8) & 255) as f32).to_bits(), 0, 0, 0]);
        words.extend([rng(seed) & 0xFF_FFFF, 0, 0, 0]);
        assert_eq!(words.len(), UNIFORM_WORDS);
        words
    }

    #[test]
    #[ignore = "test-only static helper elision admission; requires local manifest and EGL"]
    fn static_tev_helper_elision_exact_pixels_and_depth() { differential(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15], false); }

    #[test]
    #[ignore = "test-only literal-false feature elision admission; requires local manifest and EGL"]
    fn static_feature_elision_exact_pixels_and_depth() { differential(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15], true); }

    fn differential(operations: &[u32], features: bool) {
        let path = std::env::var_os("ZAKURO_TEV_MANIFEST").expect("set local shader manifest path");
        let mut cases = captured_cases(std::path::Path::new(&path), operations);
        let source_dir = std::env::var_os("ZAKURO_TEST_TEV_SOURCE_DIR").map(std::path::PathBuf::from);
        if let Some(dir) = &source_dir {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("synthetic-fixture.vert"), VERTEX).unwrap();
            let manifest = std::fs::read(&path).unwrap();
            std::fs::write(dir.join("MANIFEST.txt"), format!("TEST-ONLY source dump\nstatic_helper_elision=true\nfeature_elision={features}\nvertex=synthetic-fixture.vert; synthetic fixture, not full-game vertex shader\nmaterial_manifest={}\nmaterial_manifest_fnv1a64={:016x}\nallowed_operations={operations:?}\nfragment_sources=exact GLSL passed to build_program, captured material shapes with synthetic uniforms/textures/tables\n", std::path::Path::new(&path).display(), source_fingerprint(&manifest))).unwrap();
            std::fs::write(dir.join("sources.tsv"), "case\tflags_hex\treference\tselective\tconstants\tlighting_shape\n").unwrap();
        }
        let captured = cases.len();
        let shared_keys = cases.iter().map(|case| {
            let (constants, lighting, _) = shape(case);
            (constants, lighting)
        }).collect::<std::collections::HashSet<_>>().len();
        cases.extend(synthetic_cases_for(operations));
        let gles = Gles::new().expect("actual EGL context required");
        gles.worker.run(move || {
            let gl = GL_CONTEXT.get().unwrap();
            // SAFETY: all GL resources are created, used, read and deleted on
            // this worker's current context; buffer ranges match their storage.
            unsafe {
                let framebuffer = gl.create_framebuffer().unwrap();
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                let color = gl.create_renderbuffer().unwrap();
                gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
                gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, 16, 16);
                gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(color));
                let depth = gl.create_texture().unwrap();
                gl.bind_texture(glow::TEXTURE_2D, Some(depth));
                gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::DEPTH_COMPONENT24, 16, 16);
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::TEXTURE_2D, Some(depth), 0);
                assert_eq!(gl.check_framebuffer_status(glow::FRAMEBUFFER), glow::FRAMEBUFFER_COMPLETE);
                let depth_read_framebuffer = gl.create_framebuffer().unwrap();
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(depth_read_framebuffer));
                let depth_read_color = gl.create_renderbuffer().unwrap();
                gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth_read_color));
                gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, 16, 16);
                gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(depth_read_color));
                assert_eq!(gl.check_framebuffer_status(glow::FRAMEBUFFER), glow::FRAMEBUFFER_COMPLETE);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                let depth_read_program = build_program(gl, DEPTH_VERTEX, DEPTH_FRAGMENT, None).expect("depth bit encoding shader links");
                let vao = gl.create_vertex_array().unwrap();
                gl.bind_vertex_array(Some(vao));
                let uniform = gl.create_buffer().unwrap();
                gl.bind_buffer_base(glow::UNIFORM_BUFFER, 3, Some(uniform));
                let primary_uniform = gl.create_buffer().unwrap();
                gl.bind_buffer_base(glow::UNIFORM_BUFFER, 8, Some(primary_uniform));
                let table = gl.create_buffer().unwrap();
                gl.bind_buffer(glow::SHADER_STORAGE_BUFFER, Some(table));
                let table_values: Vec<u32> = (0..TABLES_SIZE / 4).map(|index| (if index & 1 == 0 { 0.25 + (index % 251) as f32 / 502.0 } else { 0.0001 }).to_bits()).collect();
                gl.buffer_data_u8_slice(glow::SHADER_STORAGE_BUFFER, &bytes(&table_values), glow::STATIC_DRAW);
                gl.bind_buffer_base(glow::SHADER_STORAGE_BUFFER, 4, Some(table));
                let mut textures = Vec::new();
                let mut seed = 0x51CE_8A27;
                for unit in 0..6 {
                    let texture = gl.create_texture().unwrap();
                    gl.active_texture(glow::TEXTURE0 + unit);
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    let data: Vec<u8> = (0..4 * 4 * 4).map(|index| {
                        let x = (index / 4) % 4;
                        let y = index / 16;
                        match unit {
                            1 => [0, 85, 170, 255][x],
                            2 => [0, 85, 170, 255][y],
                            3 => [0, 127, 128, 255][(x + y) % 4],
                            4 => if (x + y + index) & 1 == 0 { 0 } else { 255 },
                            _ => rng(&mut seed) as u8,
                        }
                    }).collect();
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, 4, 4, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&data)));
                    for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] { gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::NEAREST as i32); }
                    for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] { gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32); }
                    textures.push(texture);
                }
                gl.viewport(0, 0, 16, 16);
                for cap in [glow::SCISSOR_TEST, glow::CULL_FACE, glow::BLEND, glow::DITHER, glow::STENCIL_TEST] { gl.disable(cap); }
                gl.enable(glow::DEPTH_TEST);
                gl.depth_func(glow::ALWAYS);
                gl.depth_mask(true);
                gl.clear_depth_f32(1.0);
                gl.color_mask(true, true, true, true);
                let mut observed_depths = std::collections::HashSet::new();
                for case in &cases {
                    let (constants, lighting, packed) = shape(case);
                    let reference = fragment_source_unelided(&constants, case.flags & (1 << 16) != 0, &lighting);
                    let prototype = if features {
                        let production = fragment_source(&constants, case.flags & (1 << 16) != 0, &lighting);
                        assert_eq!(production, static_feature_elision(&constants, &reference), "production source must match the proven feature transform");
                        production
                    } else {
                        static_helper_elision(&constants, &reference)
                    };
                    if let Some(dir) = &source_dir {
                        use std::io::Write as _;
                        let raw = dump_fragment(dir, "reference", &reference);
                        let shared = dump_fragment(dir, if features { "features" } else { "elided" }, &prototype);
                        let mut records = std::fs::OpenOptions::new().append(true).open(dir.join("sources.tsv")).unwrap();
                        writeln!(records, "{}\t{:x}\t{raw}\t{shared}\t{constants:?}\t{lighting:?}", case.label, case.flags).unwrap();
                    }
                    let programs = [build_program(gl, VERTEX, &reference, None).expect("reference links"), build_program(gl, VERTEX, &prototype, None).expect("prototype links")];
                    for variant in 0..12 {
                        let words = words(case, &packed, variant, &mut seed);
                        gl.bind_buffer(glow::UNIFORM_BUFFER, Some(uniform));
                        gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, &bytes(&words), glow::DYNAMIC_DRAW);
                        let primary: [f32; 4] = match variant {
                            0..=3 => {
                                let edge = [0.0, 1.0 / 255.0, 0.49999, 0.812345][variant];
                                [edge, 1.0 - edge, 0.5 + edge * 0.25, 0.75]
                            }
                            4 => [f32::MAX, f32::MIN, f32::MIN_POSITIVE, -0.0],
                            5 => [f32::from_bits(1), -f32::from_bits(1), 0.0, -0.0],
                            6 => [-1.0e20, 1.0e20, -f32::MIN_POSITIVE, 1.0],
                            7 => [f32::from_bits((128.0f32 / 255.0).to_bits() - 1), 128.0 / 255.0, f32::from_bits((128.0f32 / 255.0).to_bits() + 1), 0.5],
                            _ => {
                                let half_byte = (variant as f32 - 8.0 + 127.5) / 255.0;
                                [half_byte, 1.0 - half_byte, half_byte - 1.0 / 65536.0, 0.5]
                            }
                        };
                        gl.bind_buffer(glow::UNIFORM_BUFFER, Some(primary_uniform));
                        gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, &bytes(&primary.map(f32::to_bits)), glow::DYNAMIC_DRAW);
                        for (unit, &texture) in textures.iter().enumerate() {
                            gl.active_texture(glow::TEXTURE0 + unit as u32);
                            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                            let filter = if variant >= 8 { glow::LINEAR } else { glow::NEAREST };
                            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] { gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, filter as i32); }
                        }
                        let mut outputs = Vec::new();
                        for program in programs {
                            bind_program(gl, program);
                            gl.clear_color(0.123, 0.456, 0.789, 0.321);
                            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
                            gl.draw_arrays(glow::TRIANGLES, 0, 3);
                            let mut pixels = vec![0; 16 * 16 * 4];
                            gl.read_pixels(0, 0, 16, 16, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut pixels)));
                            // GLES does not portably expose depth through
                            // ReadPixels. Sample its normalized24 value and
                            // encode its exact float bits into RGBA8 instead.
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(depth_read_framebuffer));
                            gl.disable(glow::DEPTH_TEST);
                            gl.active_texture(glow::TEXTURE0 + 7);
                            gl.bind_texture(glow::TEXTURE_2D, Some(depth));
                            bind_program(gl, depth_read_program);
                            gl.draw_arrays(glow::TRIANGLES, 0, 3);
                            let mut depth_bits = vec![0; 16 * 16 * 4];
                            gl.read_pixels(0, 0, 16, 16, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut depth_bits)));
                            if outputs.is_empty() {
                                observed_depths.extend(depth_bits.chunks_exact(4).map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap())));
                            }
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
                            gl.enable(glow::DEPTH_TEST);
                            outputs.push((pixels, depth_bits));
                        }
                        assert_eq!(gl.get_error(), glow::NO_ERROR, "{} variant {variant}", case.label);
                        if outputs[0] != outputs[1] {
                            if let Some(dir) = std::env::var_os("ZAKURO_TEV_DIFF_OUTPUT") {
                                let dir = std::path::PathBuf::from(dir);
                                std::fs::create_dir_all(&dir).unwrap();
                                for (name, data) in [("reference.rgba", outputs[0].0.as_slice()), ("prototype.rgba", outputs[1].0.as_slice()), ("reference.depth-bits.rgba", outputs[0].1.as_slice()), ("prototype.depth-bits.rgba", outputs[1].1.as_slice()), ("reference.frag", reference.as_bytes()), ("prototype.frag", prototype.as_bytes())] { std::fs::write(dir.join(name), data).unwrap(); }
                                std::fs::write(dir.join("case.txt"), format!("{} variant={variant} operations={operations:?}\nconstants={constants:?}\nlighting={lighting:?}\nwords={words:?}\n", case.label)).unwrap();
                            }
                            let color_mismatches = outputs[0].0.iter().zip(&outputs[1].0).filter(|(a,b)| a != b).count();
                            let depth_mismatches = outputs[0].1.iter().zip(&outputs[1].1).filter(|(a,b)| a != b).count();
                            let max_delta = outputs[0].0.iter().chain(&outputs[0].1).zip(outputs[1].0.iter().chain(&outputs[1].1)).map(|(&a,&b)| a.abs_diff(b)).max().unwrap();
                            panic!("GL differential rejected: static_elision=true, {} variant {variant}: {color_mismatches} color channels and {depth_mismatches} depth encoding bytes differ, maximum delta {max_delta}", case.label);
                        }
                    }
                    for program in programs { delete_program(gl, program); }
                }
                gl.use_program(None);
                for texture in textures { gl.delete_texture(texture); }
                gl.delete_buffer(uniform);
                gl.delete_buffer(primary_uniform);
                gl.delete_buffer(table);
                delete_program(gl, depth_read_program);
                gl.delete_vertex_array(vao);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.delete_framebuffer(framebuffer);
                gl.delete_renderbuffer(color);
                gl.delete_texture(depth);
                gl.delete_framebuffer(depth_read_framebuffer);
                gl.delete_renderbuffer(depth_read_color);
                assert!(observed_depths.iter().any(|bits| ![0.0f32.to_bits(), 0.5f32.to_bits(), 1.0f32.to_bits()].contains(bits)), "depth comparison must observe real nontrivial writes, not only clear/fixed viewport depth");
                eprintln!("GL differential exact: static_elision=true, feature_elision={features}, allowed={operations:?}, separable={}, {captured} captured material states ({shared_keys} keys) + {} synthetic states, twelve finite edge/random variants each including large, tiny, signed-zero, rounding-boundary and bilinear-filtered primary inputs, 16x16 RGBA8 + sampled depth float bits", separable(), cases.len() - captured);
            }
        });
    }
}
