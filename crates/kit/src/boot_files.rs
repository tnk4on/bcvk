//! Extract boot files and create VHDX for Hyper-V UEFI boot.
//!
//! Uses direct SSH/SCP to podman machine for fast file transfer,
//! with local cache by image digest for instant subsequent runs.

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
#[cfg(target_os = "windows")]
use tracing::info;

#[cfg(target_os = "windows")]
const NBD_VSOCK_BIN: &[u8] = include_bytes!("nbd-vsock.bin");

#[cfg(target_os = "windows")]
const VSOCK_NBD_BRIDGE_BIN: &[u8] = include_bytes!("vsock-nbd-bridge.bin");

#[cfg(target_os = "windows")]
const PASSWORD_HASH: &str =
    "$6$bcvksalt$2g2axTGKGM92b6AvQiSXWoYYU3x6nqdhaMJWfCO6iKn0.fTA6DI5sXk.G86OYvNgXXbrYByeMOIMyLcUUA8/1.";

/// SSH connection info for podman machine.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
pub struct PodmanSsh {
    pub port: u16,
    pub key: String,
    pub rootful: bool,
}

#[cfg(target_os = "windows")]
impl PodmanSsh {
    fn user(&self) -> &str {
        if self.rootful { "root" } else { "core" }
    }

    fn ssh_args(&self) -> Vec<String> {
        vec![
            "-p".to_string(), self.port.to_string(),
            "-i".to_string(), self.key.clone(),
            "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(), "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(), "LogLevel=ERROR".to_string(),
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
        let output = Command::new("ssh")
            .args(self.ssh_args())
            .arg(&self.user_host())
            .arg(&full_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        if !output.status.success() {
            bail!("ssh failed: {}", cmd);
        }
        Ok(output.stdout)
    }

    fn scp_to_local(&self, remote_path: &str, local_path: &std::path::Path) -> Result<()> {
        let remote = format!("{}:{}", self.user_host(), remote_path);
        let status = Command::new("scp")
            .args([
                "-P", &self.port.to_string(),
                "-i", &self.key,
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR",
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

/// Ensure image exists locally (pull if needed) and return short digest.
#[cfg(target_os = "windows")]
pub fn ensure_image_and_get_digest(image: &str) -> Result<String> {
    let exists = Command::new("podman")
        .args(["image", "exists", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !exists.success() {
        info!("pulling image {}...", image);
        let pull = Command::new("podman")
            .args(["pull", image])
            .status()?;
        if !pull.success() {
            bail!("failed to pull image: {}", image);
        }
    }
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if digest.is_empty() { bail!("failed to get image digest: {}", image); }
    Ok(digest.trim_start_matches("sha256:").chars().take(16).collect())
}

/// Cache directory for boot files, keyed by short digest.
#[cfg(target_os = "windows")]
fn cache_dir_from_digest(digest_short: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk").join("cache").join(format!("boot-{}", digest_short))
}

/// Fetch boot files via SCP (fast, ~50MB/s) and cache locally.
#[cfg(target_os = "windows")]
fn fetch_boot_files(merged_path: &str, ssh: &PodmanSsh, cache_dir: &PathBuf) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    // Get metadata via SSH (small, fast)
    let meta = ssh.ssh_cmd(&format!(
        "KVER=$(ls {m}/usr/lib/modules/ | head -1); \
         echo KVER=$KVER; \
         GRUB=$(find {m}/usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' 2>/dev/null | head -1); \
         echo GRUB=$GRUB",
        m = merged_path,
    ))?;
    let meta_str = String::from_utf8_lossy(&meta);

    let kver = meta_str.lines()
        .find(|l| l.starts_with("KVER="))
        .map(|l| l.trim_start_matches("KVER=").trim().to_string())
        .unwrap_or_default();
    let grub_path = meta_str.lines()
        .find(|l| l.starts_with("GRUB="))
        .map(|l| l.trim_start_matches("GRUB=").trim().to_string())
        .unwrap_or_default();

    if kver.is_empty() { bail!("kernel version not found"); }
    if grub_path.is_empty() { bail!("GRUB EFI not found"); }
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

    // Fetch vsock kernel modules (decompress to /tmp since overlay is read-only)
    let _ = ssh.ssh_cmd(&format!(
        "xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vsock.ko.xz > /tmp/vsock.ko; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/hv_sock.ko.xz > /tmp/hv_sock.ko",
        m = merged_path, k = kver,
    ));
    let _ = ssh.scp_to_local("/tmp/vsock.ko", &cache_dir.join("vsock.ko"));
    let _ = ssh.scp_to_local("/tmp/hv_sock.ko", &cache_dir.join("hv_sock.ko"));
    info!("vsock modules: SCP complete");

    let kernel = std::fs::read(cache_dir.join("vmlinuz"))?;
    let grub_efi = std::fs::read(cache_dir.join("grubx64.efi"))?;
    let initramfs = std::fs::read(cache_dir.join("initramfs.img"))?;

    info!("kernel: {} bytes, GRUB: {} bytes, initramfs: {} bytes (SCP cached)",
          kernel.len(), grub_efi.len(), initramfs.len());

    Ok((kernel, grub_efi, initramfs))
}



/// Create a VHDX with FAT32 ESP containing GRUB + kernel + initramfs.
/// Returns the path to the VHDX file.
#[cfg(target_os = "windows")]
pub fn create_boot_vhdx(
    digest_short: &str,
    merged_path: &str,
    ssh: &PodmanSsh,
    ssh_pubkey: &str,
    vsock_port: u32,
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
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim(), PASSWORD_HASH)?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }

    // Write initramfs with CPIOs to temp file
    let initramfs_tmp = cache_dir.join("initramfs-final.img");
    std::fs::write(&initramfs_tmp, &initramfs)?;
    info!("final initramfs: {} bytes", initramfs.len());

    // grub.cfg
    let grub_cfg = "set timeout=0\nset default=0\nmenuentry bcvk {\n  \
         linux /boot/vmlinuz root=/dev/nbd0p2 rootfstype=erofs ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}";
    let grub_cfg_path = cache_dir.join("grub.cfg");
    std::fs::write(&grub_cfg_path, grub_cfg)?;

    // Create VHDX via PowerShell
    let vhdx_path = cache_dir.join("esp.vhdx");
    let vhdx_str = vhdx_path.to_string_lossy().to_string();

    let ps_script = format!(
        "Remove-Item '{vhdx}' -Force -ErrorAction SilentlyContinue; \
         New-VHD -Path '{vhdx}' -SizeBytes 256MB -Dynamic | Out-Null; \
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
    );

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

    // Cleanup temp initramfs
    let _ = std::fs::remove_file(&initramfs_tmp);
    let _ = std::fs::remove_file(&grub_cfg_path);

    info!("boot VHDX: {}", vhdx_str);
    Ok(vhdx_str)
}

/// Create CPIO with nbd-vsock binary + vsock modules + systemd service.
#[cfg(target_os = "windows")]
fn create_nbd_vsock_cpio(vsock_port: u32, cache_dir: &std::path::Path) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let mut buf = Vec::new();

    let dirs = [
        "usr", "usr/bin", "usr/lib", "usr/lib/bcvk",
        "usr/lib/systemd", "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir).mode(0o755).set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    let b = NewcBuilder::new("usr/bin/nbd-vsock").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, NBD_VSOCK_BIN.len() as u32);
    w.write_all(NBD_VSOCK_BIN)?;
    w.finish()?;

    // Include decompressed vsock kernel modules in initramfs
    for module_name in &["vsock.ko", "hv_sock.ko"] {
        let module_path = cache_dir.join(module_name);
        if module_path.exists() {
            let module_data = std::fs::read(&module_path)?;
            let cpio_path = format!("usr/lib/bcvk/{}", module_name);
            let b = NewcBuilder::new(&cpio_path).mode(0o644).set_mode_file_type(ModeFileType::Regular);
            let mut w = b.write(&mut buf, module_data.len() as u32);
            w.write_all(&module_data)?;
            w.finish()?;
            info!("included {} ({} bytes) in initramfs", module_name, module_data.len());
        }
    }

    let setup = format!(
        "#!/bin/bash\n\
modprobe nbd max_part=16 2>/dev/null\n\
# Load vsock modules (hv_sock depends on hv_vmbus which loads late)\n\
insmod /usr/lib/bcvk/vsock.ko 2>/dev/kmsg\n\
for i in 1 2 3 4 5 6 7 8 9 10; do\n\
  insmod /usr/lib/bcvk/hv_sock.ko 2>/dev/null && break\n\
  sleep 1\n\
done\n\
\n\
# Copy nbd-vsock to /run so it survives switch-root\n\
cp /usr/bin/nbd-vsock /run/nbd-vsock\n\
\n\
# Install proxy service that persists across switch-root\n\
mkdir -p /run/systemd/system\n\
cat > /run/systemd/system/bcvk-nbd-proxy.service <<'UNIT'\n\
[Unit]\n\
Description=NBD vsock proxy\n\
DefaultDependencies=no\n\
[Service]\n\
Type=simple\n\
ExecStart=/run/nbd-vsock /dev/nbd0 {vsock_port}\n\
Restart=no\n\
KillMode=none\n\
UNIT\n\
\n\
systemctl start bcvk-nbd-proxy.service 2>/dev/kmsg\n\
\n\
# Wait for NBD device\n\
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do\n\
  [ -b /dev/nbd0p2 ] && break\n\
  sleep 1\n\
done\n\
blockdev --rereadpt /dev/nbd0 2>/dev/null\n",
        vsock_port = vsock_port
    );
    let b = NewcBuilder::new("usr/lib/bcvk/setup-nbd.sh").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, setup.len() as u32);
    w.write_all(setup.as_bytes())?;
    w.finish()?;

    let service =
        "[Unit]\n\
         Description=Setup NBD vsock connection\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=sysroot.mount initrd-root-device.target\n\
         After=systemd-udevd.service systemd-modules-load.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         TimeoutStartSec=120\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-nbd.sh\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/bcvk-setup-nbd.service").mode(0o644).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, service.len() as u32);
    w.write_all(service.as_bytes())?;
    w.finish()?;

    let dropin = b"[Unit]\nWants=bcvk-setup-nbd.service\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/initrd-root-device.target.d/bcvk-setup-nbd.conf").mode(0o644).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, dropin.len() as u32);
    w.write_all(dropin)?;
    w.finish()?;

    Ok(cpio::newc::trailer(buf)?)
}

/// Vsock NBD bridge binary for deployment to podman machine.
#[cfg(target_os = "windows")]
pub fn vsock_nbd_bridge_binary() -> &'static [u8] {
    VSOCK_NBD_BRIDGE_BIN
}

#[cfg(target_os = "windows")]
fn append_cpio(initramfs: &mut Vec<u8>, cpio: &[u8]) {
    let aligned = (initramfs.len() + 3) & !3;
    initramfs.resize(aligned, 0);
    initramfs.extend_from_slice(cpio);
}

