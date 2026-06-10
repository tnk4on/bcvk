//! Extract boot files and create VHDX for Hyper-V UEFI boot.
//!
//! Uses direct SSH/SCP to podman machine for fast file transfer,
//! with local cache by image digest for instant subsequent runs.

use color_eyre::{eyre::bail, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tracing::info;

/// SSH connection info for podman machine.
#[derive(Clone, Debug)]
pub struct PodmanSsh {
    pub port: u16,
    pub key: String,
    pub rootful: bool,
}

impl PodmanSsh {
    fn user(&self) -> &str {
        if self.rootful {
            "root"
        } else {
            "core"
        }
    }

    fn ssh_args(&self) -> Vec<String> {
        vec![
            "-p".to_string(),
            self.port.to_string(),
            "-i".to_string(),
            self.key.clone(),
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
        ]
    }

    pub fn user_host(&self) -> String {
        format!("{}@127.0.0.1", self.user())
    }

    /// Run a command that needs root access to container storage.
    /// Rootful: runs directly. Rootless: wraps with sudo.
    pub fn ssh_cmd(&self, cmd: &str) -> Result<Vec<u8>> {
        let full_cmd = if self.rootful {
            cmd.to_string()
        } else {
            format!("sudo {}", cmd)
        };
        // Use temp files for stdout/stderr instead of pipes.
        // Rust Command::output() with Stdio::piped() can hang on Windows because
        // ssh.exe inherits pipe handles to background threads/helpers, so the
        // parent-side pipe read never sees EOF after remote command exits.
        let tmp_dir = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stdout_path = tmp_dir.join(format!("bcvk-ssh-{}.out", stamp));
        let stderr_path = tmp_dir.join(format!("bcvk-ssh-{}.err", stamp));
        let stdout_file = std::fs::File::create(&stdout_path)?;
        let stderr_file = std::fs::File::create(&stderr_path)?;
        let status = Command::new("ssh")
            .args(self.ssh_args())
            .arg(&self.user_host())
            .arg(&full_cmd)
            .stdin(Stdio::null())
            .stdout(stdout_file)
            .stderr(stderr_file)
            .status()?;
        let stdout = std::fs::read(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read(&stderr_path).unwrap_or_default();
        for p in [&stdout_path, &stderr_path] {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!("failed to remove temp file {}: {}", p.display(), e);
                }
            }
        }
        if !status.success() {
            let stderr_str = String::from_utf8_lossy(&stderr);
            bail!("ssh failed: {}\nstderr: {}", cmd, stderr_str.trim());
        }
        Ok(stdout)
    }

    /// Run a shell script via stdin (for large scripts that exceed command-line limits).
    pub fn ssh_cmd_stdin(&self, script: &str) -> Result<Vec<u8>> {
        use std::io::Write;
        let remote_cmd = "sudo bash -s".to_string();
        let tmp_dir = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stdout_path = tmp_dir.join(format!("bcvk-ssh-{}.out", stamp));
        let stderr_path = tmp_dir.join(format!("bcvk-ssh-{}.err", stamp));
        let stdout_file = std::fs::File::create(&stdout_path)?;
        let stderr_file = std::fs::File::create(&stderr_path)?;
        let mut child = Command::new("ssh")
            .args(self.ssh_args())
            .arg(&self.user_host())
            .arg(&remote_cmd)
            .stdin(Stdio::piped())
            .stdout(stdout_file)
            .stderr(stderr_file)
            .spawn()?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let status = child.wait()?;
        let stdout = std::fs::read(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read(&stderr_path).unwrap_or_default();
        for p in [&stdout_path, &stderr_path] {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!("failed to remove temp file {}: {}", p.display(), e);
                }
            }
        }
        if !status.success() {
            bail!(
                "ssh script failed:\nstderr: {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        Ok(stdout)
    }

    fn scp_to_local(&self, remote_path: &str, local_path: &std::path::Path) -> Result<()> {
        let remote = format!("{}:{}", self.user_host(), remote_path);
        let status = Command::new("scp")
            .args([
                "-P",
                &self.port.to_string(),
                "-i",
                &self.key,
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
            ])
            .arg(&remote)
            .arg(local_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("scp failed: {} → {}", remote_path, local_path.display());
        }
        Ok(())
    }
}

/// Cache directory for boot files, keyed by short digest.
fn cache_dir_from_digest(digest_short: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk")
        .join("cache")
        .join(format!("boot-{}", digest_short))
}

/// Fetch boot files via SCP (fast, ~50MB/s) and cache locally.
fn fetch_boot_files(
    merged_path: &str,
    ssh: &PodmanSsh,
    cache_dir: &PathBuf,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    // Get metadata via SSH (small, fast)
    let meta = ssh.ssh_cmd(&format!(
        "KVER=$(ls {m}/usr/lib/modules/ | head -1); \
         echo KVER=$KVER; \
         GRUB=$(find {m}/usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' 2>/dev/null | head -1); \
         echo GRUB=$GRUB",
        m = merged_path,
    ))?;
    let meta_str = String::from_utf8_lossy(&meta);

    let kver = meta_str
        .lines()
        .find(|l| l.starts_with("KVER="))
        .map(|l| l.trim_start_matches("KVER=").trim().to_string())
        .unwrap_or_default();
    let grub_path = meta_str
        .lines()
        .find(|l| l.starts_with("GRUB="))
        .map(|l| l.trim_start_matches("GRUB=").trim().to_string())
        .unwrap_or_default();

    if kver.is_empty() {
        bail!("kernel version not found");
    }
    if grub_path.is_empty() {
        bail!("GRUB EFI not found");
    }
    info!("kernel version: {}", kver);

    // SCP files directly to cache directory
    std::fs::create_dir_all(cache_dir)?;

    let vmlinuz_remote = format!("{}/usr/lib/modules/{}/vmlinuz", merged_path, kver);
    let initramfs_remote = format!("{}/usr/lib/modules/{}/initramfs.img", merged_path, kver);

    ssh.scp_to_local(&vmlinuz_remote, &cache_dir.join("vmlinuz"))?;
    info!("kernel: SCP complete");

    ssh.scp_to_local(&grub_path, &cache_dir.join("grubx64.efi"))?;
    info!("GRUB EFI: SCP complete");

    ssh.scp_to_local(&initramfs_remote, &cache_dir.join("initramfs.img"))?;
    info!("initramfs: SCP complete");

    // Fetch kernel modules (decompress to /tmp since overlay is read-only)
    if let Err(e) = ssh.ssh_cmd(&format!(
        "xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vsock.ko.xz > /tmp/vsock.ko; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/hv_sock.ko.xz > /tmp/hv_sock.ko; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/drivers/block/nbd.ko.xz > /tmp/nbd.ko",
        m = merged_path,
        k = kver,
    )) {
        tracing::debug!("failed to decompress kernel modules: {}", e);
    }
    for (remote, local) in [
        ("/tmp/vsock.ko", "vsock.ko"),
        ("/tmp/hv_sock.ko", "hv_sock.ko"),
        ("/tmp/nbd.ko", "nbd.ko"),
    ] {
        if let Err(e) = ssh.scp_to_local(remote, &cache_dir.join(local)) {
            tracing::debug!("failed to fetch {}: {}", remote, e);
        }
    }
    info!("kernel modules (vsock, hv_sock, nbd): SCP complete");

    // Copy nbd-vsock binary from well-known location
    let bcvk_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk");
    for bin_name in &["nbd-vsock"] {
        let src = bcvk_dir.join(bin_name);
        if src.exists() {
            let dest = cache_dir.join(bin_name);
            std::fs::copy(&src, &dest)?;
            info!("copied {} from {}", bin_name, src.display());
        }
    }

    let kernel = std::fs::read(cache_dir.join("vmlinuz"))?;
    let grub_efi = std::fs::read(cache_dir.join("grubx64.efi"))?;
    let initramfs = std::fs::read(cache_dir.join("initramfs.img"))?;

    info!(
        "kernel: {} bytes, GRUB: {} bytes, initramfs: {} bytes (SCP cached)",
        kernel.len(),
        grub_efi.len(),
        initramfs.len()
    );

    Ok((kernel, grub_efi, initramfs))
}

/// Create a VHDX with FAT32 ESP containing GRUB + kernel + initramfs.
/// Returns the path to the VHDX file.
pub fn create_boot_vhdx(
    digest_short: &str,
    merged_path: &str,
    ssh: &PodmanSsh,
    ssh_pubkey: &str,
    vsock_port: u32,
    output_path: &std::path::Path,
) -> Result<String> {
    info!("creating boot VHDX from {}", merged_path);

    let cache_dir = cache_dir_from_digest(digest_short);

    if !(cache_dir.join("vmlinuz").exists() && cache_dir.join("initramfs.img").exists()) {
        info!("boot files cache miss, fetching via SCP...");
        fetch_boot_files(merged_path, ssh, &cache_dir)?;
    } else {
        info!("boot files cache hit: {}", cache_dir.display());
    }

    // Append CPIOs to initramfs (always, SSH key changes per run)
    let base_initramfs = std::fs::read(cache_dir.join("initramfs.img"))?;
    let mut initramfs = base_initramfs;

    let nbd_cpio = create_nbd_vsock_cpio(vsock_port, &cache_dir)?;
    append_cpio(&mut initramfs, &nbd_cpio);

    let overlay_cpio = crate::cpio::create_initramfs_units_cpio()?;
    append_cpio(&mut initramfs, &overlay_cpio);

    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim())?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }

    // Write initramfs with CPIOs to temp file
    let vhdx_stem = output_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default");
    let initramfs_tmp = cache_dir.join(format!("initramfs-{}.img", vhdx_stem));
    std::fs::write(&initramfs_tmp, &initramfs)?;
    info!("final initramfs: {} bytes", initramfs.len());

    // grub.cfg
    let grub_cfg = "set timeout=0\nset default=0\nmenuentry bcvk {\n  \
         linux /boot/vmlinuz root=PARTLABEL=bcvk-root rootfstype=erofs ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}";
    let grub_cfg_path = cache_dir.join("grub.cfg");
    std::fs::write(&grub_cfg_path, grub_cfg)?;

    // Create or update VHDX at output_path (per-VM, avoids cache lock)
    let vhdx_str = output_path.to_string_lossy().to_string();
    let cache_vhdx = cache_dir.join("esp.vhdx");

    // Copy cached VHDX as starting point if available
    if cache_vhdx.exists() && !output_path.exists() {
        if let Err(e) = std::fs::copy(&cache_vhdx, output_path) {
            tracing::debug!("failed to copy cached VHDX: {}", e);
        }
    }

    let ps_script = if output_path.exists() {
        // Cache hit: VHDX exists, only update initramfs (0.8s vs 8s)
        info!("VHDX cache hit, updating initramfs only");
        format!(
            "Mount-VHD -Path '{vhdx}'; \
             $disk = Get-VHD -Path '{vhdx}' | Get-Disk; \
             $part = Get-Partition -DiskNumber $disk.Number | Where-Object {{ $_.Type -eq 'Basic' }}; \
             $d = $part.DriveLetter; \
             if (-not $d) {{ $d = ($part | Add-PartitionAccessPath -AssignDriveLetter -PassThru).DriveLetter }}; \
             Copy-Item '{initramfs}' \"${{d}}:\\boot\\initramfs.img\" -Force; \
             Dismount-VHD -Path '{vhdx}'; \
             Write-Host 'VHDX_OK'",
            vhdx = vhdx_str,
            initramfs = initramfs_tmp.to_string_lossy(),
        )
    } else {
        // Cache miss: create new VHDX from scratch
        info!("VHDX cache miss, creating new VHDX");
        format!(
            "Remove-Item '{vhdx}' -Force -ErrorAction SilentlyContinue; \
             New-VHD -Path '{vhdx}' -SizeBytes 512MB -Dynamic | Out-Null; \
             Mount-VHD -Path '{vhdx}'; \
             $disk = Get-VHD -Path '{vhdx}' | Get-Disk; \
             Initialize-Disk -Number $disk.Number -PartitionStyle GPT -ErrorAction SilentlyContinue; \
             $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter; \
             Format-Volume -Partition $part -FileSystem FAT32 -NewFileSystemLabel ESP -Confirm:$false | Out-Null; \
             $d = $part.DriveLetter; \
             New-Item -Path \"${{d}}:\\EFI\\BOOT\" -ItemType Directory -Force | Out-Null; \
             New-Item -Path \"${{d}}:\\boot\" -ItemType Directory -Force | Out-Null; \
             Copy-Item '{grub_efi}' \"${{d}}:\\EFI\\BOOT\\BOOTX64.EFI\"; \
             Copy-Item '{kernel}' \"${{d}}:\\boot\\vmlinuz\"; \
             Copy-Item '{initramfs}' \"${{d}}:\\boot\\initramfs.img\"; \
             Copy-Item '{grub_cfg}' \"${{d}}:\\EFI\\BOOT\\grub.cfg\"; \
             Dismount-VHD -Path '{vhdx}'; \
             Write-Host 'VHDX_OK'",
            vhdx = vhdx_str,
            grub_efi = cache_dir.join("grubx64.efi").to_string_lossy(),
            kernel = cache_dir.join("vmlinuz").to_string_lossy(),
            initramfs = initramfs_tmp.to_string_lossy(),
            grub_cfg = grub_cfg_path.to_string_lossy(),
        )
    };

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("VHDX_OK") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("VHDX creation failed: {} {}", stderr.trim(), stdout.trim());
    }

    // Save to cache for future reuse (best-effort, ignore lock errors)
    if output_path != cache_vhdx {
        if let Err(e) = std::fs::copy(output_path, &cache_vhdx) {
            tracing::debug!("failed to save VHDX to cache: {}", e);
        }
    }

    // Cleanup temp initramfs
    for p in [&initramfs_tmp, &grub_cfg_path] {
        if let Err(e) = std::fs::remove_file(p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove temp file {}: {}", p.display(), e);
            }
        }
    }

    info!("boot VHDX: {}", vhdx_str);
    Ok(vhdx_str)
}

/// Create CPIO with block device binaries, kernel modules, and systemd services.
fn create_nbd_vsock_cpio(vsock_port: u32, cache_dir: &std::path::Path) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;

    let mut buf = Vec::new();

    let dirs = [
        "usr",
        "usr/bin",
        "usr/lib",
        "usr/lib/bcvk",
        "usr/lib/systemd",
        "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    add_binaries_to_cpio(&mut buf, cache_dir)?;
    add_kernel_modules_to_cpio(&mut buf, cache_dir)?;
    add_shell_scripts_to_cpio(&mut buf, vsock_port)?;
    add_systemd_services_to_cpio(&mut buf)?;

    Ok(cpio::newc::trailer(buf)?)
}

fn add_binaries_to_cpio(buf: &mut Vec<u8>, cache_dir: &std::path::Path) -> Result<()> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let nbd_vsock_path = cache_dir.join("nbd-vsock");
    if !nbd_vsock_path.exists() {
        bail!(
            "nbd-vsock binary not found at {}. Build it with: scripts/windows/build-nbd-vsock.sh",
            nbd_vsock_path.display()
        );
    }
    let nbd_data = std::fs::read(&nbd_vsock_path)?;
    let b = NewcBuilder::new("usr/bin/nbd-vsock")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, nbd_data.len() as u32);
    w.write_all(&nbd_data)?;
    w.finish()?;

    Ok(())
}

fn add_kernel_modules_to_cpio(buf: &mut Vec<u8>, cache_dir: &std::path::Path) -> Result<()> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    for module_name in &["nbd.ko", "vsock.ko", "hv_sock.ko"] {
        let module_path = cache_dir.join(module_name);
        if module_path.exists() {
            let module_data = std::fs::read(&module_path)?;
            let cpio_path = format!("usr/lib/bcvk/{}", module_name);
            let b = NewcBuilder::new(&cpio_path)
                .mode(0o644)
                .set_mode_file_type(ModeFileType::Regular);
            let mut w = b.write(&mut *buf, module_data.len() as u32);
            w.write_all(&module_data)?;
            w.finish()?;
            info!(
                "included {} ({} bytes) in initramfs",
                module_name,
                module_data.len()
            );
        }
    }
    Ok(())
}

fn add_shell_scripts_to_cpio(buf: &mut Vec<u8>, vsock_port: u32) -> Result<()> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let setup_modules = "\
#!/bin/bash\n\
modprobe hv_vmbus 2>/dev/null\n\
insmod /usr/lib/bcvk/vsock.ko 2>/dev/kmsg\n\
n=0; while [ $n -lt 10 ]; do insmod /usr/lib/bcvk/hv_sock.ko 2>/dev/null && break; sleep 1; n=$((n+1)); done\n";
    let b = NewcBuilder::new("usr/lib/bcvk/setup-modules.sh")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, setup_modules.len() as u32);
    w.write_all(setup_modules.as_bytes())?;
    w.finish()?;

    let block_device_script = format!(
        "\
#!/bin/bash\n\
VSOCK_PORT={vsock_port}\n\
echo 'bcvk: using NBD block device' > /dev/kmsg\n\
insmod /usr/lib/bcvk/nbd.ko max_part=16 2>/dev/kmsg\n\
exec /usr/bin/nbd-vsock /dev/nbd0 \"$VSOCK_PORT\" 1 2>/dev/kmsg\n",
        vsock_port = vsock_port
    );
    let b = NewcBuilder::new("usr/lib/bcvk/block-device-setup.sh")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, block_device_script.len() as u32);
    w.write_all(block_device_script.as_bytes())?;
    w.finish()?;

    let post_script = "\
#!/bin/bash\n\
sleep 1\n\
DEV=nbd0\n\
blockdev --rereadpt /dev/$DEV 2>/dev/null\n\
echo 65536 > /sys/block/$DEV/queue/read_ahead_kb 2>/dev/null\n";
    let b = NewcBuilder::new("usr/lib/bcvk/block-device-post.sh")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, post_script.len() as u32);
    w.write_all(post_script.as_bytes())?;
    w.finish()?;

    Ok(())
}

fn add_systemd_services_to_cpio(buf: &mut Vec<u8>) -> Result<()> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let svc_modules = "[Unit]\n\
         Description=Load bcvk kernel modules\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=bcvk-block-device.service\n\
         After=systemd-udevd.service systemd-modules-load.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         TimeoutStartSec=30\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-modules.sh\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/bcvk-setup-modules.service")
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, svc_modules.len() as u32);
    w.write_all(svc_modules.as_bytes())?;
    w.finish()?;

    let svc_block = "[Unit]\n\
         Description=bcvk block device setup\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=sysroot.mount initrd-root-device.target\n\
         After=bcvk-setup-modules.service\n\
         Requires=bcvk-setup-modules.service\n\
         \n\
         [Service]\n\
         Type=notify\n\
         NotifyAccess=all\n\
         KillMode=none\n\
         SurviveFinalKillSignal=yes\n\
         TimeoutStartSec=120\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/block-device-setup.sh\n\
         ExecStartPost=/usr/bin/bash /usr/lib/bcvk/block-device-post.sh\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/bcvk-block-device.service")
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, svc_block.len() as u32);
    w.write_all(svc_block.as_bytes())?;
    w.finish()?;

    let dropin = b"[Unit]\nWants=bcvk-block-device.service\n";
    let b =
        NewcBuilder::new("usr/lib/systemd/system/initrd-root-device.target.d/bcvk-setup-nbd.conf")
            .mode(0o644)
            .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, dropin.len() as u32);
    w.write_all(dropin)?;
    w.finish()?;

    Ok(())
}

fn append_cpio(initramfs: &mut Vec<u8>, cpio: &[u8]) {
    let aligned = (initramfs.len() + 3) & !3;
    initramfs.resize(aligned, 0);
    initramfs.extend_from_slice(cpio);
}

/// Create a boot VHDX for native mode from pre-extracted boot assets.
///
/// Uses a custom kernel cmdline (root=/dev/sdb rootfstype=ext4) instead
/// of the NBD/EROFS cmdline, and skips NBD-related CPIO content.
pub fn create_boot_vhdx_native(
    assets: &super::boot_files_native::BootAssets,
    ssh_pubkey: &str,
    output_path: &std::path::Path,
) -> Result<String> {
    use std::fs;

    let cache_dir = output_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(cache_dir)?;

    // Write boot assets to cache dir (needed by PowerShell script)
    let vmlinuz_path = cache_dir.join("vmlinuz");
    let grub_path = cache_dir.join("grubx64.efi");
    fs::write(&vmlinuz_path, &assets.vmlinuz)?;
    fs::write(&grub_path, &assets.grub_efi)?;

    // Build initramfs: base + overlay units + SSH key (no NBD CPIO)
    let mut initramfs = assets.initramfs.clone();

    let overlay_cpio = crate::cpio::create_initramfs_units_cpio()?;
    append_cpio(&mut initramfs, &overlay_cpio);

    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim())?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }

    // Add vsock/hv_sock kernel modules (without nbd.ko)
    if assets.vsock_ko.is_some() || assets.hv_sock_ko.is_some() {
        let modules_cpio =
            create_native_modules_cpio(assets.vsock_ko.as_deref(), assets.hv_sock_ko.as_deref())?;
        append_cpio(&mut initramfs, &modules_cpio);
    }

    let initramfs_path = cache_dir.join("initramfs-native.img");
    fs::write(&initramfs_path, &initramfs)?;
    info!("native initramfs: {} bytes", initramfs.len());

    // grub.cfg with native kernel cmdline
    let grub_cfg = "set timeout=0\nset default=0\nmenuentry bcvk {\n  \
         linux /boot/vmlinuz root=/dev/sdb rootfstype=ext4 ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}";
    let grub_cfg_path = cache_dir.join("grub-native.cfg");
    fs::write(&grub_cfg_path, grub_cfg)?;

    // Create VHDX via PowerShell
    let vhdx_str = output_path.to_string_lossy().to_string();
    let ps_script = format!(
        "Remove-Item '{vhdx}' -Force -ErrorAction SilentlyContinue; \
         New-VHD -Path '{vhdx}' -SizeBytes 512MB -Dynamic | Out-Null; \
         Mount-VHD -Path '{vhdx}'; \
         $disk = Get-VHD -Path '{vhdx}' | Get-Disk; \
         Initialize-Disk -Number $disk.Number -PartitionStyle GPT -ErrorAction SilentlyContinue; \
         $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter; \
         Format-Volume -Partition $part -FileSystem FAT32 -NewFileSystemLabel ESP -Confirm:$false | Out-Null; \
         $d = $part.DriveLetter; \
         New-Item -Path \"${{d}}:\\EFI\\BOOT\" -ItemType Directory -Force | Out-Null; \
         New-Item -Path \"${{d}}:\\boot\" -ItemType Directory -Force | Out-Null; \
         Copy-Item '{grub_efi}' \"${{d}}:\\EFI\\BOOT\\BOOTX64.EFI\"; \
         Copy-Item '{kernel}' \"${{d}}:\\boot\\vmlinuz\"; \
         Copy-Item '{initramfs}' \"${{d}}:\\boot\\initramfs.img\"; \
         Copy-Item '{grub_cfg}' \"${{d}}:\\EFI\\BOOT\\grub.cfg\"; \
         Dismount-VHD -Path '{vhdx}'; \
         Write-Host 'VHDX_OK'",
        vhdx = vhdx_str,
        grub_efi = grub_path.to_string_lossy(),
        kernel = vmlinuz_path.to_string_lossy(),
        initramfs = initramfs_path.to_string_lossy(),
        grub_cfg = grub_cfg_path.to_string_lossy(),
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("VHDX_OK") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "native VHDX creation failed: {} {}",
            stderr.trim(),
            stdout.trim()
        );
    }

    // Cleanup temp files
    let _ = fs::remove_file(&initramfs_path);
    let _ = fs::remove_file(&grub_cfg_path);

    info!("native boot VHDX: {}", vhdx_str);
    Ok(vhdx_str)
}

/// Create CPIO with vsock/hv_sock kernel modules and a setup script.
/// No NBD modules or nbd-vsock binary — native mode doesn't use NBD.
fn create_native_modules_cpio(
    vsock_ko: Option<&[u8]>,
    hv_sock_ko: Option<&[u8]>,
) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;

    let mut buf = Vec::new();

    let dirs = [
        "usr",
        "usr/lib",
        "usr/lib/bcvk",
        "usr/lib/systemd",
        "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    // Kernel modules
    if let Some(data) = vsock_ko {
        let b = NewcBuilder::new("usr/lib/bcvk/vsock.ko").mode(0o644);
        b.write(&mut buf, data.len() as u32).finish()?;
        buf.extend_from_slice(data);
        let padding = (4 - (data.len() % 4)) % 4;
        buf.extend(std::iter::repeat(0).take(padding));
    }
    if let Some(data) = hv_sock_ko {
        let b = NewcBuilder::new("usr/lib/bcvk/hv_sock.ko").mode(0o644);
        b.write(&mut buf, data.len() as u32).finish()?;
        buf.extend_from_slice(data);
        let padding = (4 - (data.len() % 4)) % 4;
        buf.extend(std::iter::repeat(0).take(padding));
    }

    // Setup script to load modules
    let setup = b"#!/bin/sh\n\
        for ko in /usr/lib/bcvk/*.ko; do\n\
            [ -f \"$ko\" ] && insmod \"$ko\" 2>/dev/null\n\
        done\n";
    let b = NewcBuilder::new("usr/lib/bcvk/setup-modules.sh").mode(0o755);
    b.write(&mut buf, setup.len() as u32).finish()?;
    buf.extend_from_slice(setup);
    let padding = (4 - (setup.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0).take(padding));

    // systemd service to run setup
    let svc = b"[Unit]\n\
        Description=Load bcvk kernel modules\n\
        DefaultDependencies=no\n\
        Before=initrd-root-device.target\n\
        [Service]\n\
        Type=oneshot\n\
        ExecStart=/usr/lib/bcvk/setup-modules.sh\n\
        [Install]\n\
        WantedBy=initrd-root-device.target\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/bcvk-modules.service").mode(0o644);
    b.write(&mut buf, svc.len() as u32).finish()?;
    buf.extend_from_slice(svc);
    let padding = (4 - (svc.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0).take(padding));

    // Symlink for WantedBy
    let target = b"../bcvk-modules.service";
    let b =
        NewcBuilder::new("usr/lib/systemd/system/initrd-root-device.target.d/bcvk-modules.service")
            .mode(0o644);
    b.write(&mut buf, target.len() as u32).finish()?;
    buf.extend_from_slice(target);
    let padding = (4 - (target.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0).take(padding));

    // Trailer
    let b = NewcBuilder::new("TRAILER!!!").mode(0);
    b.write(&mut buf, 0).finish()?;

    Ok(buf)
}
