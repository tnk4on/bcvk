//! Install bootc images to VHDX disk using podman machine (Windows).
//!
//! Creates a VHDX disk image with `bootc install to-disk` running inside
//! the podman machine. Supports both Hyper-V and WSL2 backends:
//! - Hyper-V: hot-plugs VHDX via Add-VMHardDiskDrive
//! - WSL: attaches VHDX via `wsl --mount --vhd --bare`
//!
//! Caching: base disks are stored by content hash and reused.
//! VM-specific disks use VHDX differencing (backing file) for CoW.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::vm_helpers::{generate_ssh_keypair, remove_file_if_exists};

/// Cache metadata stored alongside base VHDX as `.meta.json`.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMetadata {
    cache_hash: String,
    image_digest: String,
    source_image: String,
    version: u32,
}

/// Options for `to-disk` on Windows.
#[derive(Parser, Debug)]
pub struct ToDiskWindowsOpts {
    /// Container image to install
    pub source_image: String,

    /// Output VHDX path
    pub target_disk: Utf8PathBuf,

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

    /// Use wslc-native mode (no podman machine required)
    #[clap(long)]
    pub native: bool,
}

fn compute_cache_hash(
    image_digest: &str,
    source_image: &str,
    install: &crate::install_options::InstallOptions,
) -> String {
    let inputs = serde_json::json!({
        "image_digest": image_digest,
        "source_image": source_image,
        "filesystem": install.filesystem,
        "root_size": install.root_size,
        "target_transport": install.target_transport,
        "composefs_backend": install.composefs_backend,
        "kernel_args": install.karg,
        "version": 1u32,
    });
    let json = serde_json::to_string(&inputs).expect("serialize cache inputs");
    let hash = Sha256::digest(json.as_bytes());
    format!("sha256:{:x}", hash)
}

fn meta_json_path(vhdx_path: &str) -> PathBuf {
    PathBuf::from(format!("{}.meta.json", vhdx_path))
}

fn read_cache_metadata(vhdx_path: &str) -> Option<CacheMetadata> {
    let path = meta_json_path(vhdx_path);
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn write_cache_metadata(vhdx_path: &str, meta: &CacheMetadata) -> Result<()> {
    let path = meta_json_path(vhdx_path);
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Base disk directory for caching.
pub(crate) fn base_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk")
        .join("base")
}

/// Find or create a cached base disk for the given image + install options.
/// Returns the path to the base VHDX.
pub fn find_or_create_base_disk(
    source_image: &str,
    image_digest: &str,
    install: &crate::install_options::InstallOptions,
    disk_size: &str,
    install_log: &Option<String>,
    labels: &[String],
) -> Result<String> {
    let cache_hash = compute_cache_hash(image_digest, source_image, install);
    let short_hash: String = cache_hash
        .strip_prefix("sha256:")
        .unwrap_or(&cache_hash)
        .chars()
        .take(16)
        .collect();

    let base = base_dir();
    std::fs::create_dir_all(&base)?;
    let base_disk_name = format!("bootc-base-{}.vhdx", short_hash);
    let base_disk_path = base.join(&base_disk_name);
    let base_disk_str = base_disk_path.to_string_lossy().to_string();

    // Check existing cache
    if base_disk_path.exists() {
        if let Some(meta) = read_cache_metadata(&base_disk_str) {
            if meta.cache_hash == cache_hash {
                info!("reusing cached base disk: {}", base_disk_str);
                return Ok(base_disk_str);
            }
            info!("base disk cache hash mismatch, recreating");
        } else {
            info!("base disk has no cache metadata, recreating");
        }
        std::fs::remove_file(&base_disk_path)?;
        remove_file_if_exists(&meta_json_path(&base_disk_str));
    }

    info!("creating base disk: {}", base_disk_str);
    create_base_disk(
        &base_disk_str,
        source_image,
        install,
        disk_size,
        install_log,
        labels,
    )?;

    // Write cache metadata
    write_cache_metadata(
        &base_disk_str,
        &CacheMetadata {
            cache_hash,
            image_digest: image_digest.to_string(),
            source_image: source_image.to_string(),
            version: 1,
        },
    )?;

    Ok(base_disk_str)
}

/// Create a differencing VHDX from a base disk (CoW clone).
pub fn create_differencing_vhdx(base_path: &str, child_path: &str) -> Result<()> {
    info!(
        "creating differencing VHDX: {} (parent: {})",
        child_path, base_path
    );
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "New-VHD -Path '{}' -ParentPath '{}' -Differencing | Out-Null; Write-Host 'OK'",
                child_path, base_path
            ),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        bail!(
            "New-VHD differencing failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn create_base_disk(
    output_path: &str,
    source_image: &str,
    install: &crate::install_options::InstallOptions,
    disk_size: &str,
    install_log: &Option<String>,
    labels: &[String],
) -> Result<()> {
    let machine = crate::vm_helpers::detect_machine_name()?;
    let vmtype = crate::vm_helpers::detect_podman_vmtype()?;
    let rootful = crate::vm_helpers::is_machine_rootful(&machine);
    let run_cmd = if rootful { "sudo podman" } else { "podman" };
    info!(
        "podman machine: {} (rootful={}, type={})",
        machine, rootful, vmtype
    );

    // Generate SSH keypair
    let key_path = PathBuf::from(format!("{}.key", output_path));
    let pub_key_content = generate_ssh_keypair(&key_path)?;

    // Create target VHDX
    let size_bytes = crate::vm_helpers::parse_size(disk_size)?;
    info!("creating VHDX: {} ({} bytes)", output_path, size_bytes);
    let ps_result = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "New-VHD -Path '{}' -SizeBytes {} -Dynamic | Out-Null; Write-Host 'OK'",
                output_path, size_bytes
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

    // Attach VHDX to podman machine (backend-specific)
    let disk_device = if vmtype == "wsl" {
        attach_vhdx_wsl(output_path, &machine)?
    } else {
        attach_vhdx_hyperv(output_path, &machine)?
    };
    info!("disk device inside machine: {}", disk_device);

    // Run bootc install
    info!("installing bootc to disk...");
    let pub_key_b64 = data_encoding::BASE64.encode(pub_key_content.as_bytes());
    let mut install_opts = install.clone();
    if install_opts.filesystem.is_none() {
        install_opts.filesystem = Some("ext4".to_string());
    }
    let bootc_args = install_opts.to_bootc_args().join(" ");

    let install_log_arg = install_log
        .as_deref()
        .map(|v| format!("--env=RUST_LOG={}", v))
        .unwrap_or_default();

    let label_args = labels
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
        image = source_image,
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

    remove_file_if_exists(&script_path);

    // Detach VHDX (always, even on failure)
    if vmtype == "wsl" {
        detach_vhdx_wsl(output_path);
    } else {
        detach_vhdx_hyperv(&machine);
    }

    match install_result {
        Ok(status) if status.success() => {
            info!("installation completed: {}", output_path);
            Ok(())
        }
        Ok(status) => {
            remove_file_if_exists(Path::new(output_path));
            bail!("installation failed (exit code: {:?})", status.code());
        }
        Err(e) => {
            remove_file_if_exists(Path::new(output_path));
            bail!("failed to run install command: {}", e);
        }
    }
}

pub fn run(opts: ToDiskWindowsOpts) -> Result<()> {
    if opts.native {
        return run_native(opts);
    }

    // Get image digest for caching
    let image_digest = crate::vm_helpers::ensure_image_and_get_digest(&opts.source_image)?;

    if opts.dry_run {
        let cache_hash = compute_cache_hash(&image_digest, &opts.source_image, &opts.install);
        let short_hash: String = cache_hash
            .strip_prefix("sha256:")
            .unwrap_or(&cache_hash)
            .chars()
            .take(16)
            .collect();
        let base_path = base_dir().join(format!("bootc-base-{}.vhdx", short_hash));

        if base_path.exists() {
            if let Some(meta) = read_cache_metadata(&base_path.to_string_lossy()) {
                if meta.cache_hash == cache_hash {
                    println!("Would reuse cached base disk: {}", base_path.display());
                    if opts.target_disk.exists() {
                        println!("Output already exists: {}", opts.target_disk);
                    } else {
                        println!(
                            "Would create differencing disk: {} (from base)",
                            opts.target_disk
                        );
                    }
                    return Ok(());
                }
            }
            println!("Would regenerate base disk (hash mismatch)");
        } else {
            println!(
                "Would create new base disk and output: {}",
                opts.target_disk
            );
        }
        return Ok(());
    }

    // Find or create base disk (with caching)
    let base_disk = find_or_create_base_disk(
        &opts.source_image,
        &image_digest,
        &opts.install,
        &opts.disk_size,
        &opts.install_log,
        &opts.label,
    )?;

    // Create output as differencing disk from base
    let output_str = opts.target_disk.as_str();
    if opts.target_disk.exists() {
        bail!("output file already exists: {}", opts.target_disk);
    }
    create_differencing_vhdx(&base_disk, output_str)?;

    // Copy SSH key from base to output
    let base_key = format!("{}.key", base_disk);
    let output_key = format!("{}.key", output_str);
    let output_pub = format!("{}.key.pub", output_str);
    if Path::new(&base_key).exists() {
        std::fs::copy(&base_key, &output_key)?;
        let base_pub = format!("{}.key.pub", base_disk);
        if Path::new(&base_pub).exists() {
            std::fs::copy(&base_pub, &output_pub)?;
        }
    }

    println!("Disk image created: {}", output_str);
    println!("  Base: {}", base_disk);
    println!("SSH key: {}", output_key);
    println!("\nTo run:");
    println!("  bcvk vm run --name myvm {}", output_str);
    Ok(())
}

// --- Disk attach/detach helpers ---

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
        remove_file_if_exists(Path::new(vhdx_path));
        bail!(
            "Failed to attach VHDX: {}",
            String::from_utf8_lossy(&ps_attach.stderr).trim()
        );
    }
    Ok("/dev/sdb".to_string())
}

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

fn attach_vhdx_wsl(vhdx_path: &str, machine: &str) -> Result<String> {
    info!("attaching VHDX to WSL machine via wsl --mount...");
    let before = list_wsl_block_devices(machine)?;
    let mount_result = Command::new("wsl")
        .args(["--mount", "--vhd", vhdx_path, "--bare"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !mount_result.status.success() {
        remove_file_if_exists(Path::new(vhdx_path));
        bail!("wsl --mount failed (ensure Windows 11 22H2+)");
    }
    let after = list_wsl_block_devices(machine)?;
    let new_devs: Vec<_> = after.iter().filter(|d| !before.contains(d)).collect();
    if new_devs.is_empty() {
        bail!("wsl --mount succeeded but no new block device detected");
    }
    let dev = format!("/dev/{}", new_devs[0]);
    info!("WSL: detected new block device: {}", dev);
    Ok(dev)
}

fn detach_vhdx_wsl(vhdx_path: &str) {
    info!("detaching VHDX from WSL...");
    if let Err(e) = Command::new("wsl")
        .args(["--unmount", vhdx_path])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        tracing::debug!("wsl --unmount failed (non-fatal): {}", e);
    }
}

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

/// Native mode to-disk: uses wslc COM Export instead of podman machine.
///
/// Creates a bootable VHDX by exporting the container rootfs via
/// IWSLCContainer::Export and writing it to an ext4 VHDX.
fn run_native(opts: ToDiskWindowsOpts) -> Result<()> {
    info!(image = %opts.image, output = %opts.output, "to-disk (native mode)");

    let session = crate::wslc_com::open_default_session()?;
    crate::wslc_com::pull_image_auto_auth(&session, &opts.image)?;
    let digest_short = session.inspect_image_digest(&opts.image)?;

    let cache_dir = base_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let output_path = std::path::PathBuf::from(opts.output.as_str());

    // Use rootfs_native to create the VHDX (with caching)
    let rootfs_vhdx = crate::hyperv::rootfs_native::create_rootfs_vhdx(
        &session,
        &opts.image,
        &digest_short,
        &cache_dir,
    )?;

    // Copy (or differencing-clone) to the output path
    if output_path != rootfs_vhdx {
        std::fs::copy(&rootfs_vhdx, &output_path)
            .with_context(|| format!("failed to copy VHDX to {}", output_path.display()))?;
    }

    // Generate SSH key alongside the disk (same convention as podman mode)
    let key_path = output_path.with_extension("vhdx.key");
    let _pubkey = crate::vm_helpers::generate_ssh_keypair(&key_path)?;

    info!(output = %output_path.display(), "to-disk (native) complete");
    Ok(())
}
