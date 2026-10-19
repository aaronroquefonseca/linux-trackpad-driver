pub mod apple_parse;
pub mod build_identity;
pub mod cli;
pub mod clicks;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod doctor;
pub mod evdev;
pub mod evdev_generic;
pub mod hotplug;
#[cfg(test)]
pub mod hw_mock;
pub mod init;
pub mod lock;
pub mod proxy;
pub mod pump;
pub mod resume;
#[cfg(test)]
pub mod test_env;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match cli::dispatch(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ltd: {e:#}");
            ExitCode::from(1)
        }
    }
}
