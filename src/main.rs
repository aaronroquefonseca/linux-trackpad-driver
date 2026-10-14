pub mod apple_parse;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod init;
pub mod lock;

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
