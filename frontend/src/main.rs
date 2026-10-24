//! the pocket-native program, the frontend in src/lib.rs run with this
//! process's arguments.

fn main() {
    pocket_native::run(std::env::args().skip(1).collect());
}
