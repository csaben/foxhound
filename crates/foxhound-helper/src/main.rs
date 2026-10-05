//! Platform-neutral entry point for the optional HTTP helper.

#[cfg(windows)]
#[path = "windows_main.rs"]
mod windows_main;

#[cfg(windows)]
#[tokio::main]
async fn main() {
    windows_main::run().await;
}

#[cfg(not(windows))]
fn main() {
    eprintln!("foxhound-helper: the Windows backend is not implemented on this platform yet");
    std::process::exit(78);
}
