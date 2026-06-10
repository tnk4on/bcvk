//! Rootfs VHDX construction for native mode.
//!
//! Creates a VHDX containing the container image rootfs for direct
//! attachment to a Hyper-V VM.
//!
//! Flow:
//! 1. COM: CreateContainer → Export → rootfs.tar (host file)
//! 2. PowerShell: New-VHD → empty VHDX
//! 3. wsl --mount → mkfs.ext4 → tar xf → umount
//! 4. Cache the VHDX by image digest

use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{debug, info};

use crate::wslc_com::WslcSession;

/// Create (or retrieve from cache) a rootfs VHDX for the given image.
pub fn create_rootfs_vhdx(
    session: &WslcSession,
    image: &str,
    digest_short: &str,
    cache_dir: &Path,
) -> Result<PathBuf> {
    let vhdx_path = cache_dir.join(format!("rootfs-{digest_short}.vhdx"));

    if vhdx_path.exists() {
        info!(path = %vhdx_path.display(), "rootfs VHDX cache hit");
        return Ok(vhdx_path);
    }

    info!(image, digest = digest_short, "building rootfs VHDX (first time)");

    fs::create_dir_all(cache_dir)?;
    let tar_path = cache_dir.join(format!("rootfs-{digest_short}.tar"));
    let vhdx_tmp = cache_dir.join(format!("rootfs-{digest_short}.vhdx.tmp"));

    // Step 1: Export container rootfs to tar
    export_rootfs_tar(session, image, digest_short, &tar_path)?;

    // Step 2: Create empty dynamic VHDX
    create_empty_vhdx(&vhdx_tmp, "10GB")?;

    // Step 3: Mount VHDX via WSL, format ext4, extract tar
    populate_vhdx_from_tar(&vhdx_tmp, &tar_path)?;

    // Step 4: Rename to final path (atomic on same volume)
    fs::rename(&vhdx_tmp, &vhdx_path)
        .context("failed to rename rootfs VHDX to final path")?;

    // Cleanup tar
    let _ = fs::remove_file(&tar_path);

    info!(path = %vhdx_path.display(), "rootfs VHDX ready");
    Ok(vhdx_path)
}

/// Export a container's rootfs as tar via COM Export API.
fn export_rootfs_tar(
    session: &WslcSession,
    image: &str,
    digest_short: &str,
    tar_path: &Path,
) -> Result<()> {
    let container_name = format!("bcvk-export-{digest_short}");
    info!(name = container_name, "creating temporary container for export");

    let container = session.create_container(image, &container_name)?;

    let file = fs::File::create(tar_path)
        .with_context(|| format!("failed to create {}", tar_path.display()))?;

    info!("exporting rootfs via COM IWSLCContainer::Export...");
    let export_result = container.export(&file);

    // Always try to delete the container, even if export failed
    if let Err(e) = container.delete() {
        debug!("failed to delete temporary container: {e}");
    }

    export_result.context("rootfs export failed")?;
    drop(file);

    let size = fs::metadata(tar_path)?.len();
    info!(size_mb = size / (1024 * 1024), "rootfs tar exported");
    Ok(())
}

/// Create an empty dynamic VHDX via PowerShell.
fn create_empty_vhdx(path: &Path, size: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    let script = format!(
        "New-VHD -Path '{}' -SizeBytes {} -Dynamic | Out-Null; Write-Host 'OK'",
        path_str, size
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run New-VHD")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("New-VHD failed: {stderr}");
    }
    debug!(path = %path_str, "created empty VHDX");
    Ok(())
}

/// Mount the VHDX via `wsl --mount`, format as ext4, extract tar, unmount.
fn populate_vhdx_from_tar(vhdx_path: &Path, tar_path: &Path) -> Result<()> {
    let vhdx_str = vhdx_path.to_string_lossy();
    let tar_str = tar_path.to_string_lossy();

    // Convert tar path to WSL-accessible path (/mnt/c/...)
    let wsl_tar_path = windows_to_wsl_path(&tar_str)?;

    // Mount VHDX via wsl --mount --bare
    info!("mounting VHDX via wsl --mount...");
    let mount_output = Command::new("wsl")
        .args(["--mount", "--vhd", &vhdx_str, "--bare"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("wsl --mount failed")?;

    if !mount_output.status.success() {
        let stderr = String::from_utf8_lossy(&mount_output.stderr);
        bail!("wsl --mount failed: {stderr}");
    }

    // Find the block device — wsl --mount attaches as /dev/sdX
    let dev = find_wsl_block_device()?;

    // Format, mount, extract, unmount — all in one WSL command
    let script = format!(
        "set -e && \
         mkfs.ext4 -q -F '{dev}' && \
         mkdir -p /mnt/bcvk-rootfs && \
         mount '{dev}' /mnt/bcvk-rootfs && \
         tar xf '{wsl_tar_path}' -C /mnt/bcvk-rootfs && \
         umount /mnt/bcvk-rootfs"
    );

    info!("formatting ext4 and extracting rootfs...");
    let extract_result = Command::new("wsl")
        .args(["-u", "root", "-e", "bash", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("WSL rootfs extraction failed")?;

    // Always unmount
    let _ = Command::new("wsl")
        .args(["--unmount", &vhdx_str])
        .output();

    if !extract_result.status.success() {
        let stderr = String::from_utf8_lossy(&extract_result.stderr);
        bail!("rootfs extraction failed: {stderr}");
    }

    info!("rootfs VHDX populated successfully");
    Ok(())
}

/// Convert a Windows path (C:\...) to WSL path (/mnt/c/...).
fn windows_to_wsl_path(win_path: &str) -> Result<String> {
    let output = Command::new("wsl")
        .args(["wslpath", "-a", win_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("wslpath failed")?;

    if !output.status.success() {
        // Fallback: manual conversion
        let path = win_path.replace('\\', "/");
        if let Some(rest) = path.strip_prefix("C:") {
            return Ok(format!("/mnt/c{rest}"));
        }
        if let Some(rest) = path.strip_prefix("c:") {
            return Ok(format!("/mnt/c{rest}"));
        }
        bail!("cannot convert path to WSL: {win_path}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Find the block device attached by `wsl --mount --bare`.
fn find_wsl_block_device() -> Result<String> {
    // `wsl --mount --bare` attaches as the last /dev/sd[a-z] device.
    // Use lsblk to find the most recently added disk.
    let output = Command::new("wsl")
        .args([
            "-u", "root", "-e", "sh", "-c",
            "lsblk -dpno NAME,SIZE | tail -1 | awk '{print $1}'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("lsblk failed")?;

    let dev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dev.is_empty() || !dev.starts_with("/dev/") {
        bail!("could not find block device after wsl --mount");
    }
    debug!(dev, "found WSL block device");
    Ok(dev)
}
