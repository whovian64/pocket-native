use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Execute the actual C inline helpers and macro against a reference that
/// classifies IEEE exponent/fraction fields, independently of their fast path.
#[test]
fn fpscr_normalization_preserves_ieee_bits_and_control() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("recomp-fpscr-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let scratch = Scratch(dir);
    std::fs::write(scratch.0.join("recomp.h"), recomp3ds::abi::HEADER).unwrap();
    std::fs::write(
        scratch.0.join("test.c"),
        include_str!("fixtures/fpscr_normalization.c"),
    )
    .unwrap();
    let executable = scratch
        .0
        .join(format!("test{}", std::env::consts::EXE_SUFFIX));
    let compiler = std::env::var_os("CC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cc".into());
    let compiled = Command::new(compiler)
        .current_dir(&scratch.0)
        .args([
            "-std=c11",
            "-O2",
            "-ffp-contract=off",
            "-fno-math-errno",
            "test.c",
            "-o",
        ])
        .arg(&executable)
        .arg("-lm")
        .output()
        .expect("the C compiler used by the recompiler must be available");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let result = Command::new(&executable).output().unwrap();
    assert!(
        result.status.success(),
        "C normalization oracle failed:\n{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
