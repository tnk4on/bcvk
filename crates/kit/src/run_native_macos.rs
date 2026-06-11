//! Native mode ephemeral VM launch for macOS.
//!
//! Uses apple/container CLI for image management and vfkit with EFI boot
//! (no podman machine, no NBD). Boot assets are read directly from the
//! rootfs.ext4 snapshot using ext4-view (Rust-native EXT4 reader).
//!
//! Boot flow:
//! 1. `container image pull` (macOS native, no Linux VM)
//! 2. Locate rootfs.ext4 snapshot via `container image inspect`
//! 3. Read vmlinuz, initramfs, and GRUB from rootfs.ext4 via ext4-view
//! 4. Build ESP disk image in memory (CPIO concat + FAT32/GPT)
//! 5. Launch vfkit with `--bootloader efi` and two virtio-blk devices

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use bcvk_nbd::{fat32, gpt, regions};
use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use ext4_view::Ext4;
use tracing::{debug, info};

use crate::run_ephemeral_macos::{
    ephemeral_base_dir, expose_port, find_available_ssh_port, find_vfkit, generate_mac,
    start_gvproxy, EphemeralVmMetadata, RunEphemeralOpts, VmCleanup,
};
use crate::vm_helpers::{
    default_vcpus, parse_memory_to_mb, run_ssh_command, run_ssh_interactive, wait_for_ssh,
};

pub(crate) const CONTAINER_APP_ROOT: &str = "Library/Application Support/com.apple.container";

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

    // Read boot assets directly from rootfs.ext4 via ext4-view (zero-copy)
    let (vmlinuz_data, initramfs_data, grub_data) = read_boot_assets(&rootfs_path)?;
    info!("read boot assets from ext4 (in-memory)");

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

    // Build combined initramfs in memory
    let combined_initramfs_data = build_combined_initramfs_mem(&initramfs_data, &ssh_pubkey)?;

    // Build ESP disk image in memory and write to file
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

    let esp_disk = work_dir.join("esp.img");
    build_esp_disk_mem(
        &vmlinuz_data,
        &combined_initramfs_data,
        &grub_data,
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

pub(crate) fn check_prerequisites() -> Result<()> {
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

    Ok(())
}

pub(crate) fn ensure_container_system() -> Result<()> {
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
pub(crate) fn pull_image(image: &str) -> Result<String> {
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
pub(crate) fn inspect_image_digest(image: &str) -> Result<String> {
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
pub(crate) fn find_rootfs_snapshot(digest: &str) -> Result<PathBuf> {
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

/// Read vmlinuz, initramfs.img, and GRUB EFI binary from an EXT4 rootfs.
///
/// Uses ext4-view for Rust-native EXT4 reading — no external processes.
pub(crate) fn read_boot_assets(rootfs: &Path) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let file = fs::File::open(rootfs).context("failed to open rootfs.ext4")?;
    let ext4 = Ext4::load(Box::new(file)).context("failed to load ext4 filesystem")?;

    // Find kernel version directory
    let modules_path = ext4_view::Path::new("/usr/lib/modules");
    let kernel_version = ext4
        .read_dir(modules_path)
        .context("failed to read /usr/lib/modules/")?
        .filter_map(|e| e.ok())
        .find(|e| {
            let name = e.file_name().as_str().unwrap_or("");
            !name.is_empty() && name != "." && name != ".."
        })
        .map(|e| e.file_name().as_str().unwrap_or("").to_string())
        .ok_or_else(|| color_eyre::eyre::eyre!("no kernel version found in /usr/lib/modules/"))?;

    info!("found kernel version: {}", kernel_version);

    // Read vmlinuz
    let vmlinuz_path_str = format!("/usr/lib/modules/{}/vmlinuz", kernel_version);
    let vmlinuz = ext4
        .read(ext4_view::Path::new(&vmlinuz_path_str))
        .context("failed to read vmlinuz from ext4")?;

    // Read initramfs
    let initramfs_path_str = format!("/usr/lib/modules/{}/initramfs.img", kernel_version);
    let initramfs = ext4
        .read(ext4_view::Path::new(&initramfs_path_str))
        .context("failed to read initramfs.img from ext4")?;

    // Find GRUB EFI binary
    let grub = find_grub_in_ext4(&ext4)?;

    Ok((vmlinuz, initramfs, grub))
}

/// Search for grubaa64.efi in the EXT4 filesystem.
fn find_grub_in_ext4(ext4: &Ext4) -> Result<Vec<u8>> {
    // Try bootupd paths first (Fedora 42, CentOS, RHEL)
    let bootupd_distros = ["fedora", "centos", "redhat"];
    for distro in &bootupd_distros {
        let path = format!("/usr/lib/bootupd/updates/EFI/{}/grubaa64.efi", distro);
        if let Ok(data) = ext4.read(ext4_view::Path::new(&path)) {
            info!("found GRUB at {}", path);
            return Ok(data);
        }
    }

    // Fallback: versioned paths (Fedora 44+) /usr/lib/efi/grub2/<ver>/EFI/<distro>/
    let grub2_dir = ext4_view::Path::new("/usr/lib/efi/grub2");
    if let Ok(entries) = ext4.read_dir(grub2_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().as_str().unwrap_or("").to_string();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            for distro in &bootupd_distros {
                let path = format!("/usr/lib/efi/grub2/{}/EFI/{}/grubaa64.efi", name, distro);
                if let Ok(data) = ext4.read(ext4_view::Path::new(&path)) {
                    info!("found GRUB at {}", path);
                    return Ok(data);
                }
            }
        }
    }

    bail!("grubaa64.efi not found in rootfs (checked bootupd + /usr/lib/efi/)")
}

/// Build combined initramfs in memory by concatenating with bcvk CPIO archives.
pub(crate) fn build_combined_initramfs_mem(initramfs: &[u8], ssh_pubkey: &str) -> Result<Vec<u8>> {
    let mut out = initramfs.to_vec();

    let padding = out.len().next_multiple_of(4) - out.len();
    out.extend(vec![0u8; padding]);

    out.extend(crate::cpio::create_native_initramfs_units_cpio()?);

    if !ssh_pubkey.is_empty() {
        let aligned = out.len().next_multiple_of(4);
        out.resize(aligned, 0);
        out.extend(crate::cpio::create_ssh_cpio(ssh_pubkey)?);
    }

    Ok(out)
}

/// Build ESP disk image from in-memory boot assets (zero intermediate files).
pub(crate) fn build_esp_disk_mem(
    vmlinuz: &[u8],
    initramfs: &[u8],
    grub_efi: &[u8],
    cmdline: &str,
    output: &Path,
) -> Result<()> {
    let grub_cfg = format!(
        "set timeout=0\nset default=0\nmenuentry \"bcvk\" {{\n  linux /boot/vmlinuz {}\n  initrd /boot/initrd.img\n}}\n",
        cmdline
    );

    // Write boot assets to temp files for fat32::build_esp_regions (expects file paths)
    let tmp_dir = output.parent().unwrap_or(Path::new("/tmp"));
    let tmp_vmlinuz = tmp_dir.join(".esp-vmlinuz");
    let tmp_initramfs = tmp_dir.join(".esp-initramfs");
    let tmp_grub = tmp_dir.join(".esp-grub");
    fs::write(&tmp_vmlinuz, vmlinuz)?;
    fs::write(&tmp_initramfs, initramfs)?;
    fs::write(&tmp_grub, grub_efi)?;

    let initrd_parts = vec![(
        fat32::FileDataRegion::FromFile {
            path: tmp_initramfs.clone(),
            len: initramfs.len() as u64,
        },
        initramfs.len() as u64,
    )];

    let (esp_regions, esp_size) = fat32::build_esp_regions(
        &tmp_grub,
        grub_efi.len() as u64,
        grub_cfg.as_bytes(),
        &tmp_vmlinuz,
        vmlinuz.len() as u64,
        initrd_parts,
        initramfs.len() as u64,
    );

    let esp_regions = regions::consolidate_regions(esp_regions);
    let disk = gpt::build_gpt_disk(esp_regions, esp_size, Vec::new(), 0)?;

    let mut file_data = vec![0u8; disk.total_size as usize];
    regions::pread(&disk.regions, &mut file_data, 0)?;
    fs::write(output, &file_data).context("failed to write ESP disk image")?;

    for tmp in [&tmp_vmlinuz, &tmp_initramfs, &tmp_grub] {
        if let Err(e) = fs::remove_file(tmp) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!("failed to remove {}: {}", tmp.display(), e);
            }
        }
    }

    Ok(())
}
