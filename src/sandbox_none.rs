//! Platforms without a command sandbox yet (Windows): `run` is refused, never run unconfined.
use std::path::Path;
use std::time::Duration;

pub fn run(_: &Path, _: &Path, _: &str, _: Duration) -> Result<String, String> {
    Err("running commands is not available on this computer yet".into())
}

pub fn exec_confined(_: &str, _: &str, _: &str) -> i32 {
    eprintln!("refused: no command sandbox on this platform");
    126
}

pub fn inner(_: &str, _: &str, _: &str, _: &str) -> i32 {
    126
}
