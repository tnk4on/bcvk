//! Native mode ephemeral VM launch for macOS.
//!
//! Uses apple/container CLI for image management and vfkit with EFI boot
//! (no podman machine, no NBD). The ESP disk image is built from components
//! extracted from the rootfs.ext4 snapshot.
//!
//! Boot flow:
//! 1. `container image pull` (macOS native, no Linux VM)
//! 2. Locate rootfs.ext4 snapshot via `container image inspect`
//! 3. Extract vmlinuz, initramfs, and GRUB from rootfs.ext4 via `debugfs`
//! 4. Concatenate bcvk CPIO units to initramfs
//! 5. Build ESP disk image (FAT32 + GPT) using bcvk-nbd fat32/gpt modules
//! 6. Launch vfkit with `--bootloader efi` and two virtio-blk devices:
//!    - ESP disk image (GRUB + vmlinuz + initramfs)
//!    - rootfs.ext4 (readonly)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use bcvk_nbd::{fat32, gpt, regions};
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

/// Run an ephemeral VM using apple/container CLI and vfkit EFI boot.
pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    if opts.detach {
        return run_detached_native(&opts);
    }

    check_prerequisites()?;
    ensure_container_system()?;

    let cache_base = ephemeral_base_dir();
    fs::create_dir_all(&cache_base)?;

    let vfkit_bin = find_vfkit()?;

    info!(image = %opts.image, "starting ephemeral VM (native mode, EFI boot)");

    let snapshot_digest = pull_image(&opts.image)?;
    let rootfs_path = find_rootfs_snapshot(&snapshot_digest)?;
    info!("rootfs snapshot: {}", rootfs_path.display());

    let digest_short = &snapshot_digest[..16.min(snapshot_digest.len())];
    let vm_name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("native-{}", &digest_short[..8]));

    let work_dir = cache_base.join(format!("{}-kernel", vm_name));
    fs::create_dir_all(&work_dir)?;

    // Extract kernel assets from rootfs.ext4
    let (vmlinuz, initramfs_orig, grub_efi) = extract_boot_assets(&rootfs_path, &work_dir)?;
    info!("extracted: vmlinuz, initramfs, grub");

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
    let combined_initramfs = work_dir.join("initramfs-combined.img");
    build_combined_initramfs(&initramfs_orig, &ssh_pubkey, &combined_initramfs)?;

    // Build kernel cmdline — rootfs is the second virtio-blk device (/dev/vdb)
    let mut cmdline_parts: Vec<&str> = vec![
        "root=/dev/vdb",
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

    // Build ESP disk image
    let esp_disk = work_dir.join("esp.img");
    build_esp_disk(
        &vmlinuz,
        &combined_initramfs,
        &grub_efi,
        &cmdline,
        &esp_disk,
    )?;
    info!("ESP disk image: {}", esp_disk.display());

    // EFI variable store
    let efi_var_store = cache_base.join(format!("{}-efi-vars", vm_name));

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

    let bootloader_arg = format!("efi,variable-store={},create", efi_var_store.display());
    let serial_log = cache_base.join(format!("{}-serial.log", vm_name));

    let mut vfkit_args = vec![
        "--cpus".to_string(),
        vcpus.to_string(),
        "--memory".to_string(),
        memory_mb.to_string(),
        "--bootloader".to_string(),
        bootloader_arg,
        // ESP disk (first virtio-blk = /dev/vda)
        "--device".to_string(),
        format!("virtio-blk,path={}", esp_disk.display()),
        // rootfs (second virtio-blk = /dev/vdb, readonly)
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

    info!("launching vfkit (EFI boot)...");
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

    std::mem::forget(_cleanup);
    let status = vfkit_child.wait()?;
    info!("vfkit exited: {}", status);
    if let Err(e) = gvproxy_child.kill() {
        debug!("failed to kill gvproxy: {}", e);
    }
    EphemeralVmMetadata::remove(&vm_name);
    if let Err(e) = fs::remove_dir_all(&work_dir) {
        debug!("failed to clean up work dir: {}", e);
    }
    Ok(())
}

/// Spawn a detached native mode VM by re-invoking bcvk without --detach.
fn run_detached_native(opts: &RunEphemeralOpts) -> Result<()> {
    let cache_base = ephemeral_base_dir();
    fs::create_dir_all(&cache_base)?;

    let vm_name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("native-detach-{}", &opts.image.len().to_string()[..1]));
    let log_path = cache_base.join(format!("bcvk-{}.log", vm_name));
    let log_file = fs::File::create(&log_path)?;

    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach" && a != "-d")
        .collect();
    if !args.contains(&"-K".to_string()) && !args.contains(&"--ssh-keygen".to_string()) {
        args.insert(args.len() - 1, "-K".to_string());
    }
    if opts.name.is_none() {
        args.insert(args.len() - 1, "--name".to_string());
        args.insert(args.len() - 1, vm_name.clone());
    }

    let child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()?;

    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        pid: child.id(),
        gvproxy_pid: 0,
        ssh_port: 0,
        ssh_key: cache_base
            .join(format!("{}-key", vm_name))
            .to_string_lossy()
            .to_string(),
        serial_log: String::new(),
        log_path: Some(log_path.to_string_lossy().to_string()),
        created: chrono::Utc::now().to_rfc3339(),
        nbd_container: None,
        nbd_port: None,
        backend: "native".to_string(),
        rootfs_path: None,
    };
    metadata.save()?;
    println!("{}", vm_name);
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
        if !stderr.contains("already") {
            debug!("container system start stderr: {}", stderr);
        }
    }
    Ok(())
}

/// Ensure image is available locally and return the platform-specific snapshot digest.
fn pull_image(image: &str) -> Result<String> {
    // Try inspect first (fast path for cached images)
    if let Ok(digest) = inspect_image_digest(image) {
        debug!("image already cached, digest: {}", digest);
        return Ok(digest);
    }

    // Image not found locally — pull it
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

    inspect_image_digest(image)
}

/// Get the platform-specific snapshot digest from a locally cached image.
fn inspect_image_digest(image: &str) -> Result<String> {
    let output = Command::new("container")
        .args(["image", "inspect", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("failed to run container image inspect")?;

    if !output.status.success() {
        bail!("image not found locally");
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let data: serde_json::Value =
        serde_json::from_str(&json_str).context("failed to parse image inspect output")?;

    let digest = data
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|img| img.get("variants"))
        .and_then(|v| v.as_array())
        .and_then(|variants| {
            variants.iter().find_map(|v| {
                v.get("digest")
                    .and_then(|d| d.as_str())
                    .map(|s| s.strip_prefix("sha256:").unwrap_or(s).to_string())
            })
        })
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("could not find platform-specific digest in image inspect")
        })?;

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

/// Extract vmlinuz, initramfs.img, and GRUB EFI binary from an EXT4 rootfs.
///
/// Uses debugfs batch mode (`-f`) to minimize process startup overhead:
/// a single debugfs invocation lists directories and extracts all files.
fn extract_boot_assets(rootfs: &Path, dest: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let debugfs = debugfs_path();
    let vmlinuz_path = dest.join("vmlinuz");
    let initramfs_path = dest.join("initramfs.img");
    let grub_path = dest.join("grub.efi");

    // Phase 1: batch list kernel version + GRUB search dirs (single debugfs call)
    let list_cmds = "ls -p /usr/lib/modules/\nls -p /usr/lib/efi/grub2/\n";
    let output = Command::new(&debugfs)
        .args(["-f", "/dev/stdin", &rootfs.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(list_cmds.as_bytes())?;
            child.wait_with_output()
        })
        .context("failed to run debugfs batch ls")?;

    let ls_output = String::from_utf8_lossy(&output.stdout);
    let mut lines = ls_output.lines();

    // Parse kernel version from first ls output
    let kernel_version = parse_debugfs_ls_names(&mut lines)
        .into_iter()
        .next()
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "no kernel version directory found in /usr/lib/modules/ of {}",
                rootfs.display()
            )
        })?;
    info!("found kernel version: {}", kernel_version);

    // Parse GRUB versioned dirs from second ls output
    let grub_efi_versions = parse_debugfs_ls_names(&mut lines);

    // Phase 2: batch extract kernel + initramfs + GRUB (single debugfs call)
    // Build GRUB dump commands — try bootupd paths first, then versioned /usr/lib/efi/
    let mut dump_cmds = format!(
        "dump /usr/lib/modules/{kv}/vmlinuz {vmlinuz}\n\
         dump /usr/lib/modules/{kv}/initramfs.img {initramfs}\n",
        kv = kernel_version,
        vmlinuz = vmlinuz_path.display(),
        initramfs = initramfs_path.display(),
    );

    // Add GRUB candidates — bootupd (Fedora 42, CentOS, RHEL)
    let bootupd_distros = ["fedora", "centos", "redhat"];
    for distro in &bootupd_distros {
        dump_cmds += &format!(
            "dump /usr/lib/bootupd/updates/EFI/{}/grubaa64.efi {}\n",
            distro,
            grub_path.display()
        );
    }
    // Add versioned paths (Fedora 44+): /usr/lib/efi/grub2/<ver>/EFI/fedora/
    for ver in &grub_efi_versions {
        for distro in &bootupd_distros {
            dump_cmds += &format!(
                "dump /usr/lib/efi/grub2/{}/EFI/{}/grubaa64.efi {}\n",
                ver,
                distro,
                grub_path.display()
            );
        }
    }

    Command::new(&debugfs)
        .args(["-f", "/dev/stdin", &rootfs.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(dump_cmds.as_bytes())?;
            child.wait_with_output()
        })
        .context("failed to run debugfs batch dump")?;

    // Verify extracted files
    if !vmlinuz_path.exists() || fs::metadata(&vmlinuz_path)?.len() == 0 {
        bail!("vmlinuz not found in /usr/lib/modules/{}/", kernel_version);
    }
    if !initramfs_path.exists() || fs::metadata(&initramfs_path)?.len() == 0 {
        bail!(
            "initramfs.img not found in /usr/lib/modules/{}/",
            kernel_version
        );
    }
    if !grub_path.exists() || fs::metadata(&grub_path)?.len() == 0 {
        bail!("grubaa64.efi not found in rootfs (checked bootupd + /usr/lib/efi/)");
    }

    Ok((vmlinuz_path, initramfs_path, grub_path))
}

/// Parse directory names from debugfs `ls -p` output lines.
fn parse_debugfs_ls_names(lines: &mut std::str::Lines<'_>) -> Vec<String> {
    let mut names = Vec::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let parts: Vec<&str> = line.split('/').collect();
        if parts.len() >= 7 {
            let name = parts[5].trim();
            if !name.is_empty() && name != "." && name != ".." {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Build ESP disk image (GPT with FAT32 ESP partition) from boot assets.
fn build_esp_disk(
    vmlinuz: &Path,
    initramfs: &Path,
    grub_efi: &Path,
    cmdline: &str,
    output: &Path,
) -> Result<()> {
    let kernel_size = fs::metadata(vmlinuz)?.len();
    let initrd_size = fs::metadata(initramfs)?.len();
    let grub_size = fs::metadata(grub_efi)?.len();

    let grub_cfg = format!(
        "set timeout=0\nset default=0\nmenuentry \"bcvk\" {{\n  linux /boot/vmlinuz {}\n  initrd /boot/initrd.img\n}}\n",
        cmdline
    );

    let initrd_parts = vec![(
        fat32::FileDataRegion::FromFile {
            path: initramfs.to_path_buf(),
            len: initrd_size,
        },
        initrd_size,
    )];

    let (esp_regions, esp_size) = fat32::build_esp_regions(
        grub_efi,
        grub_size,
        grub_cfg.as_bytes(),
        vmlinuz,
        kernel_size,
        initrd_parts,
        initrd_size,
    );

    // Build GPT with ESP only (no EROFS partition — rootfs is a separate virtio-blk)
    let esp_regions = regions::consolidate_regions(esp_regions);

    // Create a minimal GPT with just the ESP
    let disk = gpt::build_gpt_disk(
        esp_regions,
        esp_size,
        Vec::new(), // no second partition
        0,
    )?;

    // Write regions to file
    let mut file_data = vec![0u8; disk.total_size as usize];
    regions::pread(&disk.regions, &mut file_data, 0)?;
    fs::write(output, &file_data).context("failed to write ESP disk image")?;

    Ok(())
}

/// Build a combined initramfs by concatenating the original with bcvk CPIO archives.
fn build_combined_initramfs(original: &Path, ssh_pubkey: &str, dest: &Path) -> Result<()> {
    let mut out = fs::read(original).context("failed to read original initramfs")?;

    let padding = out.len().next_multiple_of(4) - out.len();
    out.extend(vec![0u8; padding]);

    out.extend(crate::cpio::create_native_initramfs_units_cpio()?);

    if !ssh_pubkey.is_empty() {
        let aligned = out.len().next_multiple_of(4);
        out.resize(aligned, 0);
        out.extend(crate::cpio::create_ssh_cpio(ssh_pubkey)?);
    }

    fs::write(dest, &out).context("failed to write combined initramfs")?;
    Ok(())
}
