//! the kit packed by kit/build-release.sh, carried in the program when
//! POCKET_NATIVE_KIT_PACK names it, nothing otherwise.

fn main() {
    println!("cargo:rerun-if-env-changed=POCKET_NATIVE_KIT_PACK");
    let out = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("kit.pack");
    match std::env::var_os("POCKET_NATIVE_KIT_PACK") {
        Some(pack) => {
            println!("cargo:rerun-if-changed={}", pack.to_string_lossy());
            std::fs::copy(&pack, &out).expect("the kit's pack");
        }
        None => std::fs::write(&out, []).unwrap(),
    }
}
