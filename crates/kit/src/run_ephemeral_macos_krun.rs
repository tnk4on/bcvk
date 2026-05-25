//! Ephemeral VM launch flow for macOS using libkrun + ublk/NBD over vsock.
//!
//! Boot flow:
//! 1. Extract vmlinuz + initramfs + kernel modules from container image (via podman machine)
//! 2. Build initramfs with CPIO appends (ublk-vsock, modules, scripts, SSH keys)
//! 3. Start nbdkit with EROFS plugin in vsock mode (podman machine container)
//! 4. Launch VM via libkrun with vsock port for NBD + virtio-net (gvproxy)
//! 5. Wait for SSH and execute commands

use std::ffi::{c_char, c_int, CString};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::{
    eyre::{bail, eyre, Context},
    Result,
};
use tracing::{debug, info};

use crate::run_ephemeral_macos::{
    default_vcpus, detect_machine_name, ephemeral_base_dir, ensure_image_and_get_digest,
    expose_ssh_port, find_available_ssh_port, generate_mac, is_machine_rootful,
    parse_memory_to_mb, run_ssh_command, run_ssh_interactive, start_gvproxy, wait_for_ssh,
    EphemeralVmMetadata, RunEphemeralOpts,
};

// --- libkrun C API FFI ---

#[link(name = "krun")]
extern "C" {
    fn krun_set_log_level(level: u32) -> i32;
    fn krun_create_ctx() -> i32;
    fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    fn krun_set_kernel(
        ctx_id: u32,
        kernel_path: *const c_char,
        kernel_format: u32,
        initramfs_path: *const c_char,
        cmdline: *const c_char,
    ) -> i32;
    fn krun_add_net_unixgram(
        ctx_id: u32,
        path: *const c_char,
        fd: c_int,
        mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;
    fn krun_start_enter(ctx_id: u32) -> i32;
    fn krun_disable_implicit_console(ctx_id: u32) -> i32;
    fn krun_add_virtio_console_default(
        ctx_id: u32,
        input_fd: c_int,
        output_fd: c_int,
        err_fd: c_int,
    ) -> i32;
    fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        filepath: *const c_char,
        listen: bool,
    ) -> i32;
}

const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
const COMPAT_NET_FEATURES: u32 =
    (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);
const NET_FLAG_VFKIT: u32 = 1 << 0;
const VSOCK_PORT: u32 = 1030;

// --- Boot file extraction ---

struct BootFiles {
    kernel_path: PathBuf,
    initramfs_path: PathBuf,
    cache_dir: PathBuf,
}

/// Decompress vmlinuz (PE+zstd) to ARM64 Image.
/// ARM64 vmlinuz is a PE binary containing a zstd-compressed kernel payload.
/// The zboot header at offset 4 contains payload offset and size.
fn decompress_vmlinuz(vmlinuz_path: &Path, output_path: &Path) -> Result<()> {
    let data = fs::read(vmlinuz_path)?;

    // Check for zboot header: magic "zimg" at offset 4
    if data.len() < 0x14 || &data[4..8] != b"zimg" {
        bail!("vmlinuz does not have zboot header (not PE+zstd?)");
    }

    let payload_offset = u32::from_le_bytes(data[0x08..0x0c].try_into().unwrap()) as usize;
    let payload_size = u32::from_le_bytes(data[0x0c..0x10].try_into().unwrap()) as usize;

    if payload_offset + payload_size > data.len() {
        bail!(
            "zboot payload exceeds file size (offset={}, size={}, file={})",
            payload_offset,
            payload_size,
            data.len()
        );
    }

    let compressed = &data[payload_offset..payload_offset + payload_size];
    let decompressed = zstd::decode_all(compressed)
        .context("zstd decompression of vmlinuz payload failed")?;

    // Verify ARM64 Image magic at offset 0x38
    if decompressed.len() > 0x40 && &decompressed[0x38..0x3c] == b"ARMd" {
        info!(
            "ARM64 Image decompressed: {} → {} bytes",
            data.len(),
            decompressed.len()
        );
    } else {
        tracing::warn!("decompressed kernel does not have ARM64 magic at 0x38");
    }

    fs::write(output_path, &decompressed)?;
    Ok(())
}

fn ensure_boot_files(
    machine: &str,
    rootful: bool,
    merged_path: &str,
    digest_short: &str,
) -> Result<BootFiles> {
    let cache_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/bcvk/cache")
        .join(format!("boot-{}", digest_short));

    let vmlinuz_path = cache_dir.join("vmlinuz");
    let kernel_path = cache_dir.join("Image");
    let initramfs_path = cache_dir.join("initramfs.img");

    if kernel_path.exists() && initramfs_path.exists() {
        info!("boot files cache hit: {}", cache_dir.display());
        return Ok(BootFiles {
            kernel_path,
            initramfs_path,
            cache_dir,
        });
    }

    fs::create_dir_all(&cache_dir)?;
    info!("extracting boot files via podman machine ssh...");

    let ssh_prefix = if rootful { "" } else { "sudo " };

    // Get kernel version
    let kver_cmd = format!(
        "{}ls {}/usr/lib/modules/ | head -1",
        ssh_prefix, merged_path
    );
    let kver_output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &kver_cmd])
        .output()
        .context("failed to get kernel version")?;
    let kver = String::from_utf8_lossy(&kver_output.stdout)
        .trim()
        .to_string();
    if kver.is_empty() {
        bail!("kernel version not found in {}/usr/lib/modules/", merged_path);
    }
    info!("kernel version: {}", kver);

    // Extract vmlinuz via cat
    let vmlinuz_remote = format!("{}/usr/lib/modules/{}/vmlinuz", merged_path, kver);
    podman_machine_cat(machine, ssh_prefix, &vmlinuz_remote, &vmlinuz_path)?;
    info!("vmlinuz extracted ({} bytes)", fs::metadata(&vmlinuz_path)?.len());

    // Extract initramfs.img
    let initramfs_remote = format!("{}/usr/lib/modules/{}/initramfs.img", merged_path, kver);
    podman_machine_cat(machine, ssh_prefix, &initramfs_remote, &initramfs_path)?;
    info!(
        "initramfs.img extracted ({} bytes)",
        fs::metadata(&initramfs_path)?.len()
    );

    // Extract kernel modules (.ko.xz → decompress to /tmp, then cat)
    let modules_cmd = format!(
        "{p}bash -c '\
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vsock.ko.xz > /tmp/vsock.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko.xz > /tmp/vmw_vsock_virtio_transport_common.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko.xz > /tmp/vmw_vsock_virtio_transport.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/drivers/block/nbd.ko.xz > /tmp/nbd.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/drivers/block/ublk_drv.ko.xz > /tmp/ublk_drv.ko 2>/dev/null; \
         echo OK'",
        p = ssh_prefix,
        m = merged_path,
        k = kver,
    );
    let _ = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &modules_cmd])
        .output();

    for ko in &[
        "vsock.ko",
        "vmw_vsock_virtio_transport_common.ko",
        "vmw_vsock_virtio_transport.ko",
        "nbd.ko",
        "ublk_drv.ko",
    ] {
        let _ = podman_machine_cat(
            machine,
            ssh_prefix,
            &format!("/tmp/{}", ko),
            &cache_dir.join(ko),
        );
    }
    info!("kernel modules extracted");

    // Copy ublk-vsock binary from well-known location if available
    let ublk_vsock_src = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/bcvk/ublk-vsock");
    if ublk_vsock_src.exists() {
        fs::copy(&ublk_vsock_src, cache_dir.join("ublk-vsock"))?;
        info!("copied ublk-vsock from {}", ublk_vsock_src.display());
    }

    // Decompress vmlinuz PE+zstd → ARM64 Image (libkrun aarch64 doesn't support format=5)
    info!("decompressing vmlinuz to ARM64 Image...");
    decompress_vmlinuz(&vmlinuz_path, &kernel_path)?;

    Ok(BootFiles {
        kernel_path,
        initramfs_path,
        cache_dir,
    })
}

fn podman_machine_cat(
    machine: &str,
    ssh_prefix: &str,
    remote_path: &str,
    local_path: &Path,
) -> Result<()> {
    let cmd = format!("{}cat {}", ssh_prefix, remote_path);
    let output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &cmd])
        .output()
        .context(format!("failed to cat {}", remote_path))?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!(
            "failed to extract {}: {}",
            remote_path,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::write(local_path, &output.stdout)?;
    Ok(())
}

// --- Initramfs construction ---

fn build_krun_initramfs(
    boot_files: &BootFiles,
    vsock_port: u32,
    ssh_pubkey: &str,
) -> Result<PathBuf> {
    let mut initramfs = fs::read(&boot_files.initramfs_path)?;

    let block_cpio =
        crate::boot_files_macos::create_krun_block_device_cpio(vsock_port, &boot_files.cache_dir)?;
    crate::boot_files_macos::append_cpio(&mut initramfs, &block_cpio);

    let overlay_cpio = crate::cpio::create_initramfs_units_cpio()
        .map_err(|e| eyre!("failed to create overlay CPIO: {e}"))?;
    crate::boot_files_macos::append_cpio(&mut initramfs, &overlay_cpio);

    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::boot_files_macos::create_macos_ssh_cpio(ssh_pubkey)?;
        crate::boot_files_macos::append_cpio(&mut initramfs, &ssh_cpio);
    }

    let final_path = boot_files.cache_dir.join("initramfs-krun.img");
    fs::write(&final_path, &initramfs)?;
    info!("krun initramfs: {} bytes", initramfs.len());
    Ok(final_path)
}

// --- RAII cleanup ---

struct KrunVmCleanup {
    gvproxy_pid: u32,
    nbd_container: Option<String>,
    image: String,
    vm_name: String,
}

impl Drop for KrunVmCleanup {
    fn drop(&mut self) {
        if let Some(ref name) = self.nbd_container {
            crate::nbdkit_macos::stop_nbdkit_container(name);
        }
        if let Err(e) = rustix::process::kill_process(
            rustix::process::Pid::from_raw(self.gvproxy_pid as i32).unwrap(),
            rustix::process::Signal::TERM,
        ) {
            tracing::warn!("failed to kill gvproxy (PID {}): {}", self.gvproxy_pid, e);
        }
        if let Ok(machine) = detect_machine_name() {
            let _ = Command::new("podman")
                .args([
                    "machine", "ssh", &machine, "--", "podman", "image", "umount", &self.image,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        EphemeralVmMetadata::remove(&self.vm_name);
    }
}

// --- Main entry point ---

/// Run an ephemeral VM using libkrun with ublk/NBD over vsock.
pub fn run_krun(opts: RunEphemeralOpts) -> Result<()> {
    if opts.detach {
        bail!("--detach is not yet supported with krun backend");
    }
    if opts.gui {
        bail!("--gui is not supported with krun backend (libkrun is headless)");
    }

    info!(image = %opts.image, "starting ephemeral VM on macOS (krun + ublk/NBD vsock)");

    let cache_base = ephemeral_base_dir();
    fs::create_dir_all(&cache_base)?;

    let machine = detect_machine_name()?;
    let rootful = is_machine_rootful(&machine);
    debug!(
        "podman machine '{}' ({})",
        machine,
        if rootful { "rootful" } else { "rootless" }
    );

    let digest = ensure_image_and_get_digest(&opts.image)?;
    let digest_short = &digest[..16.min(digest.len())];
    info!("image digest: {}...", digest_short);

    let vm_name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("ephemeral-{}", &digest_short[..8]));
    let ssh_key_path = cache_base.join(format!("{}-key", vm_name));

    // Generate SSH keypair
    let mut ssh_pubkey = String::new();
    if opts.ssh_keygen || !opts.execute.is_empty() {
        info!("generating SSH keypair...");
        let _ = fs::remove_file(&ssh_key_path);
        let _ = fs::remove_file(ssh_key_path.with_extension("pub"));
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

    // Get container image merged overlay path
    let merged_path = crate::nbdkit_macos::get_merged_path(&machine, rootful, &opts.image)?;
    info!("overlay merged: {}", merged_path);

    // Extract boot files (vmlinuz, initramfs, kernel modules) — cached by digest
    let boot_files = ensure_boot_files(&machine, rootful, &merged_path, digest_short)?;

    let cmdline = build_cmdline(&opts.kernel_args);

    // Build initramfs with CPIO appends
    let final_initramfs = build_krun_initramfs(&boot_files, VSOCK_PORT, &ssh_pubkey)?;

    // Start nbdkit in vsock mode
    let nbd_container = crate::nbdkit_macos::start_nbdkit_vsock(
        &machine,
        &merged_path,
        &cmdline,
        &ssh_pubkey,
        VSOCK_PORT,
        &vm_name,
    )?;
    info!("nbdkit vsock ready on port {}", VSOCK_PORT);

    // Start gvproxy
    let gvproxy_sock = cache_base.join(format!("{}-gvproxy.sock", vm_name));
    let services_sock = cache_base.join(format!("{}-gvproxy-svc.sock", vm_name));
    let gvproxy_sock_str = gvproxy_sock.to_string_lossy().to_string();
    let services_sock_str = services_sock.to_string_lossy().to_string();
    info!("starting gvproxy...");
    let mut _gvproxy_child = start_gvproxy(&gvproxy_sock_str, &services_sock_str)?;

    let mac = generate_mac();
    let vcpus = opts.vcpus.unwrap_or_else(default_vcpus);
    let memory_mb = parse_memory_to_mb(&opts.memory)?;

    // vsock socket path for krun_add_vsock_port2
    let vsock_sock = cache_base.join(format!("{}-vsock-nbd.sock", vm_name));
    let vsock_sock_str = vsock_sock.to_string_lossy().to_string();

    // Serial console log
    let serial_log = cache_base.join(format!("{}-serial.log", vm_name));
    let serial_file = fs::File::create(&serial_log)?;

    // Save metadata
    let ssh_port = find_available_ssh_port();
    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        pid: std::process::id(),
        gvproxy_pid: _gvproxy_child.id(),
        ssh_port,
        ssh_key: ssh_key_path.to_string_lossy().to_string(),
        serial_log: serial_log.to_string_lossy().to_string(),
        log_path: None,
        created: chrono::Utc::now().to_rfc3339(),
        nbd_container: Some(nbd_container.clone()),
        nbd_port: None,
    };
    metadata.save()?;

    let _cleanup = KrunVmCleanup {
        gvproxy_pid: _gvproxy_child.id(),
        nbd_container: Some(nbd_container),
        image: opts.image.clone(),
        vm_name: vm_name.clone(),
    };

    // Prepare FFI strings
    let vmlinuz_c = CString::new(boot_files.kernel_path.to_string_lossy().as_ref())?;
    let initramfs_c = CString::new(final_initramfs.to_string_lossy().as_ref())?;
    let cmdline_c = CString::new(cmdline.as_str())?;
    let gvproxy_c = CString::new(gvproxy_sock_str.as_str())?;
    let vsock_c = CString::new(vsock_sock_str.as_str())?;

    let serial_fd = serial_file.as_raw_fd();

    // Launch krun VM in separate thread (krun_start_enter blocks)
    info!("launching VM via libkrun...");
    let krun_handle = std::thread::spawn(move || -> Result<()> {
        #[allow(unsafe_code)]
        unsafe {
            krun_set_log_level(3);
            let ctx = krun_create_ctx();
            if ctx < 0 {
                bail!("krun_create_ctx failed: {}", ctx);
            }
            let ctx = ctx as u32;

            if krun_set_vm_config(ctx, vcpus as u8, memory_mb) < 0 {
                bail!("krun_set_vm_config failed");
            }
            if krun_disable_implicit_console(ctx) < 0 {
                bail!("krun_disable_implicit_console failed");
            }
            if krun_add_virtio_console_default(ctx, -1, serial_fd, -1) < 0 {
                bail!("krun_add_virtio_console_default failed");
            }
            if krun_set_kernel(
                ctx,
                vmlinuz_c.as_ptr(),
                KRUN_KERNEL_FORMAT_RAW,
                initramfs_c.as_ptr(),
                cmdline_c.as_ptr(),
            ) < 0
            {
                bail!("krun_set_kernel failed");
            }
            if krun_add_vsock_port2(ctx, VSOCK_PORT, vsock_c.as_ptr(), false) < 0 {
                bail!("krun_add_vsock_port2 failed");
            }
            if krun_add_net_unixgram(
                ctx,
                gvproxy_c.as_ptr(),
                -1,
                mac.as_ptr(),
                COMPAT_NET_FEATURES,
                NET_FLAG_VFKIT,
            ) < 0
            {
                bail!("krun_add_net_unixgram failed");
            }

            let ret = krun_start_enter(ctx);
            info!("krun VM exited: {}", ret);
            if ret < 0 {
                bail!("krun_start_enter failed: {}", ret);
            }
        }
        Ok(())
    });

    // SSH port forwarding + connectivity
    if opts.ssh_keygen || !opts.execute.is_empty() {
        debug!("allocated SSH port: {}", ssh_port);
        info!("setting up SSH port forwarding...");
        for attempt in 0..15u32 {
            match expose_ssh_port(&services_sock_str, "192.168.127.2", ssh_port) {
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

    // Wait for krun VM thread to finish
    std::mem::forget(_cleanup);
    match krun_handle.join() {
        Ok(Ok(())) => info!("krun VM exited normally"),
        Ok(Err(e)) => tracing::warn!("krun VM error: {}", e),
        Err(_) => tracing::warn!("krun VM thread panicked"),
    }
    Ok(())
}

fn build_cmdline(user_args: &[String]) -> String {
    let mut parts: Vec<&str> = vec![
        "root=PARTLABEL=bcvk-root",
        "rootfstype=erofs",
        "ro",
        "console=hvc0",
        "loglevel=7",
        "selinux=0",
        "net.ifnames=0",
        "systemd.journald.storage=volatile",
    ];
    let user: Vec<&str> = user_args.iter().map(|s| s.as_str()).collect();
    parts.extend(&user);
    parts.join(" ")
}
