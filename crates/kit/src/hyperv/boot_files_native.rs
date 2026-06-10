//! Boot file extraction for native mode.
//!
//! Extracts vmlinuz, initramfs, and GRUB EFI from a rootfs VHDX
//! by mounting it via `wsl --mount` and reading directly.
//!
//! Future: replace with ext4-view for zero-copy in-memory reading
//! (same pattern as macOS native).

use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{debug, info};

/// Boot assets extracted from the rootfs.
pub struct BootAssets {
    pub vmlinuz: Vec<u8>,
    pub initramfs: Vec<u8>,
    pub grub_efi: Vec<u8>,
    pub kernel_version: String,
    pub vsock_ko: Option<Vec<u8>>,
    pub hv_sock_ko: Option<Vec<u8>>,
}

/// Fetch boot files from a rootfs VHDX via WSL mount.
pub fn fetch_boot_files_native(
    rootfs_vhdx: &Path,
    cache_dir: &Path,
) -> Result<BootAssets> {
    let vmlinuz_cache = cache_dir.join("vmlinuz");
    let initramfs_cache = cache_dir.join("initramfs.img");
    let grub_cache = cache_dir.join("grubx64.efi");

    // Check cache
    if vmlinuz_cache.exists() && initramfs_cache.exists() && grub_cache.exists() {
        info!("boot files cache hit");
        let kver = read_cached_kernel_version(cache_dir)?;
        return Ok(BootAssets {
            vmlinuz: fs::read(&vmlinuz_cache)?,
            initramfs: fs::read(&initramfs_cache)?,
            grub_efi: fs::read(&grub_cache)?,
            kernel_version: kver,
            vsock_ko: fs::read(cache_dir.join("vsock.ko")).ok(),
            hv_sock_ko: fs::read(cache_dir.join("hv_sock.ko")).ok(),
        });
    }

    fs::create_dir_all(cache_dir)?;
    info!("extracting boot files from rootfs VHDX...");

    let vhdx_str = rootfs_vhdx.to_string_lossy();

    // Mount VHDX read-only
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

    // Find the device
    let dev = find_wsl_block_device()?;

    // Mount read-only and extract
    let mountpoint = "/mnt/bcvk-boot-extract";
    let mount_script = format!(
        "mkdir -p {mountpoint} && mount -o ro '{dev}' {mountpoint}"
    );
    run_wsl_root(&mount_script)?;

    let result = extract_boot_files_from_mount(mountpoint, cache_dir);

    // Always unmount
    let _ = run_wsl_root(&format!("umount {mountpoint} 2>/dev/null"));
    let _ = Command::new("wsl").args(["--unmount", &vhdx_str]).output();

    let assets = result?;
    info!(kernel = assets.kernel_version, "boot files extracted");
    Ok(assets)
}

fn extract_boot_files_from_mount(mountpoint: &str, cache_dir: &Path) -> Result<BootAssets> {
    // Discover kernel version
    let kver_output = run_wsl_root_capture(&format!(
        "ls {mountpoint}/usr/lib/modules/ | head -1"
    ))?;
    let kver = kver_output.trim().to_string();
    if kver.is_empty() {
        bail!("no kernel version found in /usr/lib/modules/");
    }
    info!(kernel_version = kver, "found kernel");

    // Extract vmlinuz
    let vmlinuz_path = format!("{mountpoint}/usr/lib/modules/{kver}/vmlinuz");
    let vmlinuz = wsl_read_file(&vmlinuz_path)?;
    fs::write(cache_dir.join("vmlinuz"), &vmlinuz)?;

    // Extract initramfs — bootc images store it alongside vmlinuz in /usr/lib/modules/<kver>/
    let initramfs_path = format!("{mountpoint}/usr/lib/modules/{kver}/initramfs.img");
    let initramfs = wsl_read_file(&initramfs_path)?;
    fs::write(cache_dir.join("initramfs.img"), &initramfs)?;

    // Find and extract GRUB EFI
    let grub_search = format!(
        "find {mountpoint}/usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' 2>/dev/null | head -1"
    );
    let grub_path = run_wsl_root_capture(&grub_search)?;
    let grub_path = grub_path.trim();
    if grub_path.is_empty() {
        bail!("GRUB EFI not found in rootfs");
    }
    let grub_efi = wsl_read_file(grub_path)?;
    fs::write(cache_dir.join("grubx64.efi"), &grub_efi)?;

    // Extract kernel modules (vsock, hv_sock)
    let modules_dir = format!("{mountpoint}/usr/lib/modules/{kver}/kernel");
    let vsock_ko = wsl_decompress_ko(&modules_dir, "vsock.ko").ok();
    let hv_sock_ko = wsl_decompress_ko(&modules_dir, "hv_sock.ko").ok();

    if let Some(ref data) = vsock_ko {
        fs::write(cache_dir.join("vsock.ko"), data)?;
    }
    if let Some(ref data) = hv_sock_ko {
        fs::write(cache_dir.join("hv_sock.ko"), data)?;
    }

    // Save kernel version
    fs::write(cache_dir.join("kver"), &kver)?;

    Ok(BootAssets {
        vmlinuz,
        initramfs,
        grub_efi,
        kernel_version: kver,
        vsock_ko,
        hv_sock_ko,
    })
}

fn read_cached_kernel_version(cache_dir: &Path) -> Result<String> {
    fs::read_to_string(cache_dir.join("kver"))
        .context("failed to read cached kernel version")
}

fn wsl_read_file(path: &str) -> Result<Vec<u8>> {
    let output = Command::new("wsl")
        .args(["-u", "root", "-e", "cat", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to read {path} via WSL"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to read {path}: {stderr}");
    }
    Ok(output.stdout)
}

fn wsl_decompress_ko(modules_dir: &str, name: &str) -> Result<Vec<u8>> {
    // Try .xz first, then .zst, then uncompressed
    for (ext, cmd) in [("xz", "xzcat"), ("zst", "zstdcat")] {
        let path = format!(
            "$(find {modules_dir} -name '{name}.{ext}' 2>/dev/null | head -1)"
        );
        let script = format!(
            "p={path}; [ -n \"$p\" ] && {cmd} \"$p\" || exit 1"
        );
        if let Ok(data) = run_wsl_root_binary(&script) {
            if !data.is_empty() {
                debug!(module = name, "decompressed from .{ext}");
                return Ok(data);
            }
        }
    }

    // Try uncompressed
    let path = format!(
        "$(find {modules_dir} -name '{name}' 2>/dev/null | head -1)"
    );
    let script = format!("p={path}; [ -n \"$p\" ] && cat \"$p\" || exit 1");
    run_wsl_root_binary(&script)
}

fn run_wsl_root(script: &str) -> Result<()> {
    let output = Command::new("wsl")
        .args(["-u", "root", "-e", "bash", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("WSL command failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("WSL command failed: {stderr}");
    }
    Ok(())
}

fn run_wsl_root_capture(script: &str) -> Result<String> {
    let output = Command::new("wsl")
        .args(["-u", "root", "-e", "bash", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("WSL command failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("WSL command failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_wsl_root_binary(script: &str) -> Result<Vec<u8>> {
    let output = Command::new("wsl")
        .args(["-u", "root", "-e", "bash", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("WSL command failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("WSL command failed: {stderr}");
    }
    Ok(output.stdout)
}

fn find_wsl_block_device() -> Result<String> {
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
