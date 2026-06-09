//! Native mode ephemeral VM launch for macOS.
//!
//! Uses apple/container CLI for image management and vfkit with direct
//! kernel boot (no podman machine, no NBD, no GRUB).
//!
//! Boot flow:
//! 1. `container image pull` (macOS native, no Linux VM)
//! 2. Locate rootfs.ext4 snapshot via `container image inspect`
//! 3. Extract vmlinuz + initramfs from rootfs.ext4 via `debugfs`
//! 4. Concatenate bcvk CPIO units to initramfs
//! 5. Launch vfkit with `--bootloader linux` (direct kernel boot)
//! 6. rootfs.ext4 attached as virtio-blk device

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use tracing::{debug, info};

use crate::run_ephemeral_macos::{
    ephemeral_base_dir, expose_port, find_available_ssh_port, find_vfkit, generate_mac,
    start_gvproxy, EphemeralVmMetadata, RunEphemeralOpts, VmCleanup,
};
use crate::vm_helpers::{
    default_vcpus, parse_memory_to_mb, run_ssh_command, run_ssh_interactive, wait_for_ssh,
};

const CONTAINER_APP_ROOT: &str = "Library/Application Support/com.apple.container";

/// Run an ephemeral VM using apple/container CLI and vfkit direct kernel boot.
pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    check_prerequisites()?;
    ensure_container_system()?;

    let cache_base = ephemeral_base_dir();
    fs::create_dir_all(&cache_base)?;

    let vfkit_bin = find_vfkit()?;

    info!(image = %opts.image, "starting ephemeral VM (native mode, direct kernel boot)");

    // Pull image and get platform-specific snapshot digest
    let snapshot_digest = pull_image(&opts.image)?;
    let rootfs_path = find_rootfs_snapshot(&snapshot_digest)?;
    info!("rootfs snapshot: {}", rootfs_path.display());

    let digest_short = &snapshot_digest[..16.min(snapshot_digest.len())];
    let vm_name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("native-{}", &digest_short[..8]));

    let kernel_dir = cache_base.join(format!("{}-kernel", vm_name));
    fs::create_dir_all(&kernel_dir)?;

    // Extract kernel and initramfs from rootfs.ext4
    let (vmlinuz, initramfs_orig) = extract_kernel_from_ext4(&rootfs_path, &kernel_dir)?;
    info!("extracted kernel: {}", vmlinuz.display());

    // Generate SSH keypair if needed
    let ssh_key_path = cache_base.join(format!("{}-key", vm_name));
    let mut ssh_pubkey = String::new();
    if opts.ssh_keygen || !opts.execute.is_empty() {
        info!("generating SSH keypair...");
        crate::vm_helpers::remove_file_if_exists(&ssh_key_path);
        crate::vm_helpers::remove_file_if_exists(&ssh_key_path.with_extension("pub"));
        let status = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                &ssh_key_path.to_string_lossy(),
                "-N",
                "",
                "-q",
            ])
            .status()?;
        if !status.success() {
            bail!("ssh-keygen failed");
        }
        ssh_pubkey = fs::read_to_string(ssh_key_path.with_extension("pub"))?
            .trim()
            .to_string();
    }

    // Build combined initramfs
    let combined_initramfs = kernel_dir.join("initramfs-combined.img");
    build_combined_initramfs(&initramfs_orig, &ssh_pubkey, &combined_initramfs)?;
    info!("combined initramfs: {}", combined_initramfs.display());

    // Build kernel cmdline
    let mut cmdline_parts: Vec<&str> = vec![
        "root=/dev/vda",
        "ro",
        "rootfstype=ext4",
        "console=tty0",
        "console=hvc0",
        "loglevel=4",
        "selinux=0",
        "net.ifnames=0",
        "systemd.journald.storage=volatile",
    ];
    let user_args: Vec<&str> = opts.kernel_args.iter().map(|s| s.as_str()).collect();
    cmdline_parts.extend(&user_args);
    let cmdline = cmdline_parts.join(" ");

    // Start gvproxy
    let gvproxy_sock = cache_base.join(format!("{}-gvproxy.sock", vm_name));
    let services_sock = cache_base.join(format!("{}-gvproxy-svc.sock", vm_name));
    let gvproxy_sock_str = gvproxy_sock.to_string_lossy().to_string();
    let services_sock_str = services_sock.to_string_lossy().to_string();
    info!("starting gvproxy...");
    let mut gvproxy_child = start_gvproxy(&gvproxy_sock_str, &services_sock_str)?;

    let mac = generate_mac();
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let vcpus = opts
        .itype
        .map(|t| t.vcpus())
        .or(opts.vcpus)
        .unwrap_or_else(default_vcpus);
    let memory_mb = opts
        .itype
        .map(|t| t.memory_mb())
        .map(Ok)
        .unwrap_or_else(|| parse_memory_to_mb(&opts.memory))?;

    let bootloader_arg = format!(
        "linux,kernel={},initrd={},cmdline={}",
        vmlinuz.display(),
        combined_initramfs.display(),
        cmdline
    );

    let serial_log = cache_base.join(format!("{}-serial.log", vm_name));
    let mut vfkit_args = vec![
        "--cpus".to_string(),
        vcpus.to_string(),
        "--memory".to_string(),
        memory_mb.to_string(),
        "--bootloader".to_string(),
        bootloader_arg,
        "--device".to_string(),
        format!("virtio-blk,path={},readonly", rootfs_path.display()),
        "--device".to_string(),
        format!(
            "virtio-net,unixSocketPath={},mac={}",
            gvproxy_sock_str, mac_str
        ),
        "--device".to_string(),
        "virtio-rng".to_string(),
        "--device".to_string(),
        format!("virtio-serial,logFilePath={}", serial_log.display()),
    ];

    if opts.gui {
        vfkit_args.push("--gui".to_string());
    }

    info!("launching vfkit (direct kernel boot)...");
    let vfkit_log = cache_base.join(format!("{}-vfkit.log", vm_name));
    let vfkit_log_file = fs::File::create(&vfkit_log)?;
    let mut vfkit_child = Command::new(&vfkit_bin)
        .args(&vfkit_args)
        .stdout(vfkit_log_file.try_clone()?)
        .stderr(vfkit_log_file)
        .spawn()
        .context("failed to start vfkit")?;

    let ssh_port = find_available_ssh_port();
    debug!("allocated SSH port: {}", ssh_port);

    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        pid: vfkit_child.id(),
        gvproxy_pid: gvproxy_child.id(),
        ssh_port,
        ssh_key: ssh_key_path.to_string_lossy().to_string(),
        serial_log: serial_log.to_string_lossy().to_string(),
        log_path: None,
        created: chrono::Utc::now().to_rfc3339(),
        nbd_container: None,
        nbd_port: None,
        backend: "native".to_string(),
        rootfs_path: Some(rootfs_path.to_string_lossy().to_string()),
    };
    metadata.save()?;

    let _cleanup = VmCleanup {
        vfkit_pid: vfkit_child.id(),
        gvproxy_pid: gvproxy_child.id(),
        nbd_container: None,
        nbd_port: None,
        image: opts.image.clone(),
        vm_name: vm_name.clone(),
        backend: "native".to_string(),
    };

    if opts.ssh_keygen || !opts.execute.is_empty() {
        info!("setting up SSH port forwarding...");
        for attempt in 0..15u32 {
            match expose_port(&services_sock_str, "192.168.127.2", ssh_port, 22) {
                Ok(_) => {
                    info!("SSH port {} forwarded", ssh_port);
                    break;
                }
                Err(e) if attempt < 14 => {
                    debug!("SSH port forward attempt {}: {}", attempt, e);
                    let backoff = 200 * 2u64.pow(attempt.min(4));
                    std::thread::sleep(Duration::from_millis(backoff));
                }
                Err(e) => bail!("SSH port forward failed: {}", e),
            }
        }

        wait_for_ssh(ssh_port, &ssh_key_path, "root")?;

        if !opts.execute.is_empty() {
            for cmd_str in &opts.execute {
                info!("executing: {}", cmd_str);
                let status = run_ssh_command(ssh_port, &ssh_key_path, "root", cmd_str)?;
                if !status.success() {
                    bail!("command failed: {}", status);
                }
            }
            return Ok(());
        }

        info!(
            "SSH ready: ssh -p {} -i {} root@localhost",
            ssh_port,
            ssh_key_path.display()
        );

        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            let status = run_ssh_interactive(ssh_port, &ssh_key_path, "root")?;
            let exit_code = status.code().unwrap_or(1);
            drop(_cleanup);
            std::process::exit(exit_code);
        }
    }

    // No SSH: wait for vfkit to exit
    std::mem::forget(_cleanup);
    let status = vfkit_child.wait()?;
    info!("vfkit exited: {}", status);
    if let Err(e) = gvproxy_child.kill() {
        debug!("failed to kill gvproxy: {}", e);
    }
    EphemeralVmMetadata::remove(&vm_name);
    // Clean up extracted kernel files
    if let Err(e) = fs::remove_dir_all(&kernel_dir) {
        debug!("failed to clean up kernel dir: {}", e);
    }
    Ok(())
}

fn check_prerequisites() -> Result<()> {
    if Command::new("container")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        bail!(
            "container CLI not found. Install from: \
             https://github.com/apple/container/releases"
        );
    }

    let debugfs = debugfs_path();
    if !Path::new(&debugfs).exists() {
        bail!(
            "debugfs not found at {}. Install with: brew install e2fsprogs",
            debugfs
        );
    }

    Ok(())
}

fn debugfs_path() -> String {
    // e2fsprogs is keg-only on Homebrew, not in PATH
    let brew_path = "/opt/homebrew/opt/e2fsprogs/sbin/debugfs";
    if Path::new(brew_path).exists() {
        return brew_path.to_string();
    }
    "debugfs".to_string()
}

fn ensure_container_system() -> Result<()> {
    let output = Command::new("container")
        .args(["system", "start", "--disable-kernel-install"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run container system start")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already running is fine
        if !stderr.contains("already") {
            debug!("container system start stderr: {}", stderr);
        }
    }
    Ok(())
}

/// Pull image and return the platform-specific snapshot digest (without sha256: prefix).
fn pull_image(image: &str) -> Result<String> {
    info!("pulling image: {}", image);
    let status = Command::new("container")
        .args(["image", "pull", "--platform", "linux/arm64", image])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run container image pull")?;

    if !status.success() {
        bail!("container image pull failed");
    }

    // Get the platform-specific digest from image inspect
    let output = Command::new("container")
        .args(["image", "inspect", image])
        .output()
        .context("failed to run container image inspect")?;

    if !output.status.success() {
        bail!("container image inspect failed");
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let data: serde_json::Value =
        serde_json::from_str(&json_str).context("failed to parse image inspect output")?;

    // Find platform-specific descriptor digest in variants
    let digest = data
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|img| img.get("variants"))
        .and_then(|v| v.as_array())
        .and_then(|variants| {
            variants.iter().find_map(|v| {
                v.get("descriptor")
                    .and_then(|d| d.get("digest"))
                    .and_then(|d| d.as_str())
                    .map(|s| s.strip_prefix("sha256:").unwrap_or(s).to_string())
            })
        })
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("could not find platform-specific digest in image inspect")
        })?;

    debug!("platform-specific snapshot digest: {}", digest);
    Ok(digest)
}

/// Find the rootfs.ext4 snapshot file for the given digest.
fn find_rootfs_snapshot(digest: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| color_eyre::eyre::eyre!("cannot find home dir"))?;
    let snapshot_path = home
        .join(CONTAINER_APP_ROOT)
        .join("snapshots")
        .join(digest)
        .join("snapshot");

    if !snapshot_path.exists() {
        bail!(
            "rootfs snapshot not found at {}. Run 'container image pull' first.",
            snapshot_path.display()
        );
    }

    Ok(snapshot_path)
}

/// Extract vmlinuz and initramfs.img from an EXT4 rootfs using debugfs.
fn extract_kernel_from_ext4(rootfs: &Path, dest: &Path) -> Result<(PathBuf, PathBuf)> {
    let debugfs = debugfs_path();

    // Find kernel version directory
    let output = Command::new(&debugfs)
        .args(["-R", "ls -p /usr/lib/modules/", &rootfs.to_string_lossy()])
        .output()
        .context("failed to run debugfs ls")?;

    let ls_output = String::from_utf8_lossy(&output.stdout);
    let kernel_version = ls_output
        .lines()
        .filter_map(|line| {
            // debugfs -p format: /inode/type/perms/uid/gid/name/size/
            let parts: Vec<&str> = line.split('/').collect();
            if parts.len() >= 7 {
                let name = parts[5].trim();
                if !name.is_empty() && name != "." && name != ".." {
                    return Some(name.to_string());
                }
            }
            None
        })
        .next()
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "no kernel version directory found in /usr/lib/modules/ of {}",
                rootfs.display()
            )
        })?;

    info!("found kernel version: {}", kernel_version);

    // Extract vmlinuz
    let vmlinuz_path = dest.join("vmlinuz");
    let dump_cmd = format!(
        "dump /usr/lib/modules/{}/vmlinuz {}",
        kernel_version,
        vmlinuz_path.display()
    );
    let status = Command::new(&debugfs)
        .args(["-R", &dump_cmd, &rootfs.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to extract vmlinuz")?;
    if !vmlinuz_path.exists() || fs::metadata(&vmlinuz_path)?.len() == 0 {
        bail!("vmlinuz extraction failed (debugfs exit: {})", status);
    }

    // Extract initramfs.img
    let initramfs_path = dest.join("initramfs.img");
    let dump_cmd = format!(
        "dump /usr/lib/modules/{}/initramfs.img {}",
        kernel_version,
        initramfs_path.display()
    );
    let status = Command::new(&debugfs)
        .args(["-R", &dump_cmd, &rootfs.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to extract initramfs")?;
    if !initramfs_path.exists() || fs::metadata(&initramfs_path)?.len() == 0 {
        bail!("initramfs extraction failed (debugfs exit: {})", status);
    }

    Ok((vmlinuz_path, initramfs_path))
}

/// Build a combined initramfs by concatenating the original with bcvk CPIO archives.
fn build_combined_initramfs(original: &Path, ssh_pubkey: &str, dest: &Path) -> Result<()> {
    let mut out = fs::read(original).context("failed to read original initramfs")?;

    // Align to 4 bytes
    let padding = out.len().next_multiple_of(4) - out.len();
    out.extend(vec![0u8; padding]);

    // Append native mode units CPIO (without sysroot.mount)
    out.extend(crate::cpio::create_native_initramfs_units_cpio()?);

    // Append SSH setup CPIO if pubkey provided
    if !ssh_pubkey.is_empty() {
        let aligned = out.len().next_multiple_of(4);
        out.resize(aligned, 0);
        out.extend(crate::cpio::create_ssh_cpio(ssh_pubkey)?);
    }

    fs::write(dest, &out).context("failed to write combined initramfs")?;
    Ok(())
}
