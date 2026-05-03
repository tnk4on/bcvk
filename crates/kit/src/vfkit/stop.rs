//! vm stop — Stop a running persistent VM.

use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::{Result, eyre::bail};
use tracing::info;
use super::VmMetadata;

pub fn run(name: &str) -> Result<()> {
    let mut meta = VmMetadata::load(name)?;
    if !meta.is_alive() {
        bail!("VM '{}' is not running", name);
    }

    info!("stopping VM '{}'...", name);

    if meta.vfkit_pid > 0 {
        let _ = Command::new("kill").args(["-TERM", &meta.vfkit_pid.to_string()])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        std::thread::sleep(Duration::from_secs(3));
        if meta.is_alive() {
            let _ = Command::new("kill").args(["-KILL", &meta.vfkit_pid.to_string()])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }

    if meta.gvproxy_pid > 0 {
        let _ = Command::new("kill").args(["-KILL", &meta.gvproxy_pid.to_string()])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
    }

    meta.state = "stopped".to_string();
    meta.vfkit_pid = 0;
    meta.gvproxy_pid = 0;
    meta.save()?;

    println!("Stopped '{}'", name);
    Ok(())
}
