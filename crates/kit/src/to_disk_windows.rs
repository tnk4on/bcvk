//! Install bootc images to VHDX disk using podman machine (Windows).
//!
//! Creates a VHDX disk image with `bootc install to-disk` running inside
//! the podman machine. Supports both Hyper-V and WSL2 backends:
//! - Hyper-V: hot-plugs VHDX via Add-VMHardDiskDrive
//! - WSL: attaches VHDX via `wsl --mount --vhd --bare`

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

    /// Configure logging for `bootc install` by setting the `RUST_LOG` environment variable
    #[clap(long)]
    pub install_log: Option<String>,

    /// Add metadata to the container in key=value form
    #[clap(long = "label")]
    pub label: Vec<String>,

    /// Check if the disk would be regenerated without actually creating it
    #[clap(long)]
    pub dry_run: bool,
}

pub fn run(opts: ToDiskWindowsOpts) -> Result<()> {
    if opts.dry_run {
        if opts.output.exists() {
            println!("Disk image already exists: {}", opts.output);
            println!("Would reuse existing disk (no cache validation on Windows yet)");
        } else {
            println!("Would create new disk image: {}", opts.output);
        }
        return Ok(());
    }

    if opts.output.exists() {
        bail!("output file already exists: {}", opts.output);
    }

    let parent = opts
        .output
        .parent()
        .map(|p| p.as_std_path())
        .unwrap_or(Path::new("."));
    let output_abs = std::fs::canonicalize(parent)?
        .join(opts.output.file_name().unwrap())
        .to_string_lossy()
        .to_string()
        .trim_start_matches(r"\\?\")
        .to_string();

    // Phase 1: Detect podman machine and backend type
    let machine = crate::run_ephemeral_windows::detect_machine_name()?;
    let vmtype = crate::run_ephemeral_windows::detect_podman_vmtype()?;
    let inspect_out = Command::new("podman")
        .args(["machine", "inspect", &machine])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let inspect_json = String::from_utf8_lossy(&inspect_out.stdout);
    if !inspect_json.contains("\"Running\"") && !inspect_json.contains("\"running\"") {
        bail!("no podman machine is running");
    }
    let rootful = crate::run_ephemeral_windows::is_machine_rootful(&machine);
    let run_cmd = if rootful { "sudo podman" } else { "podman" };
    info!(
        "podman machine: {} (rootful={}, type={})",
        machine, rootful, vmtype
    );

    // Phase 2: Generate SSH keypair
    let key_path = format!("{}.key", output_abs);
    let pub_path = format!("{}.key.pub", output_abs);
    info!("generating SSH keypair: {}", key_path);
    if let Err(e) = std::fs::remove_file(&key_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!("failed to remove old key {}: {}", key_path, e);
        }
    }
    if let Err(e) = std::fs::remove_file(&pub_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!("failed to remove old pubkey {}: {}", pub_path, e);
        }
    }
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
            "-NoProfile",
            "-Command",
            &format!(
                "New-VHD -Path '{}' -SizeBytes {} -Dynamic | Out-Null; Write-Host 'OK'",
                output_abs, size_bytes
            ),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !ps_result.status.success() {
        bail!(
            "New-VHD failed: {}",
            String::from_utf8_lossy(&ps_result.stderr).trim()
        );
    }

    // Phase 4: Attach VHDX to podman machine (backend-specific)
    let disk_device = if vmtype == "wsl" {
        attach_vhdx_wsl(&output_abs, &machine)?
    } else {
        attach_vhdx_hyperv(&output_abs, &machine)?
    };
    info!("disk device inside machine: {}", disk_device);

    // Phase 5: Transfer SSH key + run bootc install in podman machine
    info!("installing bootc to disk...");
    let pub_key_b64 = data_encoding::BASE64.encode(pub_key_content.as_bytes());
    let mut install_opts = opts.install.clone();
    if install_opts.filesystem.is_none() {
        install_opts.filesystem = Some("ext4".to_string());
    }
    let bootc_args = install_opts.to_bootc_args().join(" ");

    let install_log_arg = opts
        .install_log
        .as_deref()
        .map(|v| format!("--env=RUST_LOG={}", v))
        .unwrap_or_default();

    let label_args = opts
        .label
        .iter()
        .map(|l| format!("--label={}", l))
        .collect::<Vec<_>>()
        .join(" ");

    let install_script = format!(
        "#!/bin/bash\nset -euo pipefail\n\
         printf '%s' '{b64}' | base64 -d > /dev/shm/bcvk-ssh-key.pub\n\
         {run} run --rm -i --privileged --pid=host --security-opt label=disable \
         -v /dev:/dev -v /dev/shm:/dev/shm \
         -v /var/lib/containers:/var/lib/containers -v /sys:/sys:ro \
         {install_log} {labels} \
         {image} \
         bootc install to-disk --wipe --generic-image --skip-fetch-check \
         --root-ssh-authorized-keys /dev/shm/bcvk-ssh-key.pub \
         {args} {disk}\n\
         rm -f /dev/shm/bcvk-ssh-key.pub\n",
        b64 = pub_key_b64,
        run = run_cmd,
        install_log = install_log_arg,
        labels = label_args,
        image = opts.image,
        args = bootc_args,
        disk = disk_device,
    );

    let script_path = std::env::temp_dir().join("bcvk-todisk-install.sh");
    std::fs::write(&script_path, &install_script)?;

    let install_result = Command::new("podman")
        .args(["machine", "ssh", &machine, "--", "bash", "-s"])
        .stdin(Stdio::from(std::fs::File::open(&script_path)?))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    if let Err(e) = std::fs::remove_file(&script_path) {
        tracing::debug!("failed to remove temp script: {}", e);
    }

    // Phase 6: Detach VHDX from podman machine (always, even on failure)
    if vmtype == "wsl" {
        detach_vhdx_wsl(&output_abs);
    } else {
        detach_vhdx_hyperv(&machine);
    }

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
            if let Err(e) = std::fs::remove_file(&output_abs) {
                tracing::debug!("failed to clean up VHDX: {}", e);
            }
            bail!("installation failed (exit code: {:?})", status.code());
        }
        Err(e) => {
            if let Err(e) = std::fs::remove_file(&output_abs) {
                tracing::debug!("failed to clean up VHDX: {}", e);
            }
            bail!("failed to run install command: {}", e);
        }
    }
}

/// Attach VHDX to Hyper-V podman machine via Add-VMHardDiskDrive.
/// Returns the disk device path inside the machine (/dev/sdb).
fn attach_vhdx_hyperv(vhdx_path: &str, machine: &str) -> Result<String> {
    info!("attaching VHDX to Hyper-V machine...");
    let ps_attach = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "Add-VMHardDiskDrive -VMName '{}' -Path '{}' -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 1",
                machine, vhdx_path
            ),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !ps_attach.status.success() {
        if let Err(e) = std::fs::remove_file(vhdx_path) {
            tracing::debug!("failed to clean up VHDX: {}", e);
        }
        bail!(
            "Failed to attach VHDX: {}",
            String::from_utf8_lossy(&ps_attach.stderr).trim()
        );
    }
    Ok("/dev/sdb".to_string())
}

/// Detach VHDX from Hyper-V podman machine.
fn detach_vhdx_hyperv(machine: &str) {
    info!("detaching VHDX from Hyper-V machine...");
    if let Err(e) = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "Remove-VMHardDiskDrive -VMName '{}' -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 1",
                machine
            ),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        tracing::warn!("failed to detach VHDX: {}", e);
    }
}

/// Attach VHDX to WSL podman machine via `wsl --mount --vhd --bare`.
/// Returns the disk device path inside WSL (detected via lsblk).
fn attach_vhdx_wsl(vhdx_path: &str, machine: &str) -> Result<String> {
    info!("attaching VHDX to WSL machine via wsl --mount...");

    // Snapshot block devices before mount
    let before = list_wsl_block_devices(machine)?;

    let mount_result = Command::new("wsl")
        .args(["--mount", "--vhd", vhdx_path, "--bare"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !mount_result.status.success() {
        if let Err(e) = std::fs::remove_file(vhdx_path) {
            tracing::debug!("failed to clean up VHDX: {}", e);
        }
        bail!("wsl --mount failed (ensure Windows 11 22H2+)");
    }

    // Detect new block device by diffing before/after
    let after = list_wsl_block_devices(machine)?;
    let new_devs: Vec<_> = after.iter().filter(|d| !before.contains(d)).collect();

    if new_devs.is_empty() {
        bail!("wsl --mount succeeded but no new block device detected");
    }
    let dev = format!("/dev/{}", new_devs[0]);
    info!("WSL: detected new block device: {}", dev);
    Ok(dev)
}

/// Detach VHDX from WSL.
fn detach_vhdx_wsl(vhdx_path: &str) {
    info!("detaching VHDX from WSL...");
    let _ = Command::new("wsl")
        .args(["--unmount", vhdx_path])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// List block device names (e.g. ["sda", "sdb", "sdc"]) inside WSL machine.
fn list_wsl_block_devices(machine: &str) -> Result<Vec<String>> {
    let output = Command::new("podman")
        .args([
            "machine", "ssh", machine, "--", "lsblk", "-dn", "-o", "NAME",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let names = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(names)
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
