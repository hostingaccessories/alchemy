//! Alchemy desktop shell: see the `alchemy_gui` library.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(e) = alchemy_gui::run() {
        eprintln!("alchemy-gui: {e}");
        std::process::exit(1);
    }
}
