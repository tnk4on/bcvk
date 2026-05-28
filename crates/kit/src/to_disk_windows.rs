//! Install bootc images to VHDX disk using Hyper-V ephemeral VMs (Windows).
//!
//! Creates a VHDX disk image with `bootc install to-disk` running inside
//! an ephemeral VM. SSH public key is embedded via `--root-ssh-authorized-keys`.

use std::path::Path;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

/// Options for `to-disk` on Windows.
#[derive(Parser, Debug)]
pub struct ToDiskWindowsOpts {
    /// Container image to install
    pub image: String,

    /// Output VHDX path
    pub output: Utf8PathBuf,

    /// Disk size (e.g. "20G", "10240M")
    #[clap(long, default_value = "20G")]
    pub disk_size: String,

    /// Installation options
    #[clap(flatten)]
    pub install: crate::install_options::InstallOptions,
}

pub fn run(opts: ToDiskWindowsOpts) -> Result<()> {
    if opts.output.exists() {
        bail!("output file already exists: {}", opts.output);
    }

    let parent = opts.output.parent().map(|p| p.as_std_path()).unwrap_or(Path::new("."));
    let output_abs = std::fs::canonicalize(parent)?
        .join(opts.output.file_name().unwrap())
        .to_string_lossy()
        .to_string()
        .trim_start_matches(r"\\?\")
        .to_string();

    // Phase 1: Detect podman machine
    let machine = crate::run_ephemeral_windows::detect_machine_name()?;
    let inspect_out = Command::new("podman")
        .args(["machine", "inspect", &machine])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let inspect_json = String::from_utf8_lossy(&inspect_out.stdout);
    if !inspect_json.contains("\"Running\"") && !inspect_json.contains("\"running\"") {
        bail!("no podman machine is running");
    }
    let rootful =
        inspect_json.contains("\"Rootful\": true") || inspect_json.contains("\"Rootful\":true");
    let run_cmd = if rootful { "sudo podman" } else { "podman" };
    info!("podman machine: {} (rootful={})", machine, rootful);

    // Phase 2: Generate SSH keypair
    let key_path = format!("{}.key", output_abs);
    let pub_path = format!("{}.key.pub", output_abs);
    info!("generating SSH keypair: {}", key_path);
    let _ = std::fs::remove_file(&key_path);
    let _ = std::fs::remove_file(&pub_path);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f", &key_path])
        .status()?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }
    let pub_key_content = std::fs::read_to_string(&pub_path)?.trim().to_string();

    // Phase 3: Create target VHDX
    let size_bytes = parse_size(&opts.disk_size)?;
    info!("creating VHDX: {} ({} bytes)", output_abs, size_bytes);
    let ps_result = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "New-VHD -Path '{}' -SizeBytes {} -Dynamic | Out-Null; Write-Host 'OK'",
                output_abs, size_bytes
            ),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !ps_result.status.success() {
        bail!("New-VHD failed: {}", String::from_utf8_lossy(&ps_result.stderr).trim());
    }

    // Phase 4: Hot-plug VHDX to podman machine
    info!("attaching VHDX to podman machine...");
    let ps_attach = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "Add-VMHardDiskDrive -VMName '{}' -Path '{}' -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 1",
                machine, output_abs
            ),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !ps_attach.status.success() {
        let _ = std::fs::remove_file(&output_abs);
        bail!("Failed to attach VHDX: {}", String::from_utf8_lossy(&ps_attach.stderr).trim());
    }

    // Phase 5: Transfer SSH key + run bootc install in podman machine
    info!("installing bootc to disk...");
    let pub_key_escaped = pub_key_content.replace('\'', "'\\''");
    let mut install_opts = opts.install.clone();
    if install_opts.filesystem.is_none() {
        install_opts.filesystem = Some("ext4".to_string());
    }
    let bootc_args = install_opts.to_bootc_args().join(" ");

    let install_script = format!(
        "echo '{key}' > /dev/shm/bcvk-ssh-key.pub && \
         {run} run --rm -i --privileged --pid=host --security-opt label=disable \
         -v /dev:/dev -v /dev/shm:/dev/shm \
         -v /var/lib/containers:/var/lib/containers -v /sys:/sys:ro \
         {image} \
         bootc install to-disk --wipe --generic-image --skip-fetch-check \
         --root-ssh-authorized-keys /dev/shm/bcvk-ssh-key.pub \
         {args} /dev/sdb && rm -f /dev/shm/bcvk-ssh-key.pub",
        key = pub_key_escaped,
        run = run_cmd,
        image = opts.image,
        args = bootc_args,
    );

    let install_result = Command::new("podman")
        .args(["machine", "ssh", &machine, "--", "/bin/bash", "-c", &install_script])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    // Phase 6: Detach VHDX from podman machine (always, even on failure)
    info!("detaching VHDX from podman machine...");
    let _ = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "Remove-VMHardDiskDrive -VMName '{}' -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 1",
                machine
            ),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match install_result {
        Ok(status) if status.success() => {
            info!("installation completed: {}", output_abs);
            println!("Disk image created: {}", output_abs);
            println!("SSH key: {}", key_path);
            println!("\nTo run:");
            println!("  bcvk vm run --name myvm {}", output_abs);
            Ok(())
        }
        Ok(status) => {
            let _ = std::fs::remove_file(&output_abs);
            bail!("installation failed (exit code: {:?})", status.code());
        }
        Err(e) => {
            let _ = std::fs::remove_file(&output_abs);
            bail!("failed to run install command: {}", e);
        }
    }
}



fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G').or(s.strip_suffix("GB")) {
        Ok(n.trim().parse::<u64>()? * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M').or(s.strip_suffix("MB")) {
        Ok(n.trim().parse::<u64>()? * 1024 * 1024)
    } else {
        Ok(s.parse::<u64>()?)
    }
}
