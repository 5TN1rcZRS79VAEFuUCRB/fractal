//! Helper for Fractal+'s screen sharing on Linux: capture the sound of other
//! apps through `PipeWire`. See `linux.rs`.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("This helper is only used on Linux.");
    std::process::ExitCode::FAILURE
}
