#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> eyre::Result<()> {
    gale::worker::tray::run_from_args()
}

#[cfg(not(windows))]
fn main() {}
