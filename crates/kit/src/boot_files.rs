//! Extract boot files from bootc container image.
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
use tracing::{debug, info};

#[cfg(target_os = "windows")]
use crate::pxe_server::BootFiles;

#[cfg(target_os = "windows")]
const NBD_TCP_BIN: &[u8] = include_bytes!("nbd-vsock.bin");

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

    fn user_host(&self) -> String {
        format!("{}@127.0.0.1", self.user())
    }

    /// Run a command that needs root access to container storage.
    /// Rootful: runs directly. Rootless: wraps with sudo.
    fn ssh_cmd(&self, cmd: &str) -> Result<Vec<u8>> {
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
            .args(self.ssh_args())
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

    /// Mount image and return merged path.
    /// Rootful: podman image mount. Rootless: podman unshare podman image mount.
    pub fn image_mount(&self, image: &str) -> Result<String> {
        let cmd = if self.rootful {
            format!("podman image mount {}", image)
        } else {
            format!("podman unshare podman image mount {}", image)
        };
        let output = self.ssh_cmd(&cmd)?;
        let path = String::from_utf8_lossy(&output).trim().to_string();
        if path.is_empty() {
            bail!("podman image mount returned empty path");
        }
        Ok(path)
    }
}

/// Cache directory for boot files, keyed by image digest.
#[cfg(target_os = "windows")]
fn cache_dir(image: &str) -> Result<Option<PathBuf>> {
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if digest.is_empty() { return Ok(None); }
    let short = digest.trim_start_matches("sha256:").chars().take(16).collect::<String>();
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk").join("cache").join(format!("boot-{}", short));
    Ok(Some(base))
}

/// Extract boot files using direct SSH/SCP to podman machine.
#[cfg(target_os = "windows")]
pub fn extract_boot_files(
    image: &str,
    merged_path: &str,
    ssh: &PodmanSsh,
    ssh_pubkey: &str,
    nbd_host: &str,
    nbd_port: u16,
    client_ip: &str,
    gateway_ip: &str,
) -> Result<BootFiles> {
    info!("extracting boot files from {}", merged_path);

    // Check local cache
    let cache = cache_dir(image)?;
    let (kernel, grub_efi, base_initramfs) = if let Some(ref dir) = cache {
        if dir.join("vmlinuz").exists() && dir.join("initramfs.img").exists() {
            info!("boot files cache hit: {}", dir.display());
            let kernel = std::fs::read(dir.join("vmlinuz"))?;
            let grub_efi = std::fs::read(dir.join("grubx64.efi"))?;
            let initramfs = std::fs::read(dir.join("initramfs.img"))?;
            info!("kernel: {} bytes, GRUB: {} bytes, initramfs: {} bytes (cached)",
                  kernel.len(), grub_efi.len(), initramfs.len());
            (kernel, grub_efi, initramfs)
        } else {
            info!("boot files cache miss, fetching via SCP...");
            let files = fetch_boot_files(merged_path, ssh, dir)?;
            files
        }
    } else {
        info!("no cache available, fetching via SSH...");
        fetch_boot_files_ssh(merged_path, ssh)?
    };

    // Append CPIOs
    let mut initramfs = base_initramfs;

    let nbd_cpio = create_nbd_tcp_cpio(nbd_host, nbd_port, client_ip, gateway_ip)?;
    append_cpio(&mut initramfs, &nbd_cpio);

    let overlay_cpio = crate::cpio::create_windows_overlay_cpio()?;
    append_cpio(&mut initramfs, &overlay_cpio);

    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim(), PASSWORD_HASH)?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }

    info!("final initramfs: {} bytes", initramfs.len());

    let grub_cfg = "set timeout=0\nset default=0\nmenuentry bcvk {\n  \
         linux /boot/vmlinuz root=/dev/nbd0p2 rootfstype=erofs ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 \
         rd.neednet=1 \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}"
        .to_string();
    debug!("grub.cfg:\n{}", grub_cfg);

    if kernel.is_empty() || grub_efi.is_empty() || initramfs.is_empty() {
        bail!("failed to extract one or more boot files");
    }

    Ok(BootFiles { grub_efi, kernel, initramfs, grub_cfg })
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

    let kernel = std::fs::read(cache_dir.join("vmlinuz"))?;
    let grub_efi = std::fs::read(cache_dir.join("grubx64.efi"))?;
    let initramfs = std::fs::read(cache_dir.join("initramfs.img"))?;

    info!("kernel: {} bytes, GRUB: {} bytes, initramfs: {} bytes (SCP cached)",
          kernel.len(), grub_efi.len(), initramfs.len());

    Ok((kernel, grub_efi, initramfs))
}

/// Fallback: fetch via SSH cat (when no cache dir available).
#[cfg(target_os = "windows")]
fn fetch_boot_files_ssh(merged_path: &str, ssh: &PodmanSsh) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
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

    let kernel = ssh.ssh_cmd(&format!("cat {}/usr/lib/modules/{}/vmlinuz", merged_path, kver))?;
    let grub_efi = ssh.ssh_cmd(&format!("cat {}", grub_path))?;
    let initramfs = ssh.ssh_cmd(&format!("cat {}/usr/lib/modules/{}/initramfs.img", merged_path, kver))?;

    Ok((kernel, grub_efi, initramfs))
}

/// Create CPIO with nbd-tcp binary + NM profile + systemd service.
#[cfg(target_os = "windows")]
fn create_nbd_tcp_cpio(host: &str, port: u16, client_ip: &str, gateway_ip: &str) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let mut buf = Vec::new();

    let dirs = [
        "usr", "usr/bin", "usr/lib", "usr/lib/bcvk",
        "usr/lib/systemd", "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
        "etc", "etc/NetworkManager", "etc/NetworkManager/system-connections",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir).mode(0o755).set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    let b = NewcBuilder::new("usr/bin/nbd-tcp").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, NBD_TCP_BIN.len() as u32);
    w.write_all(NBD_TCP_BIN)?;
    w.finish()?;

    let nm_conn = format!(
        "[connection]\n\
         id=bcvk-nbd\n\
         type=ethernet\n\
         interface-name=eth0\n\
         autoconnect=true\n\
         \n\
         [ipv4]\n\
         method=manual\n\
         addresses={client}/24\n\
         gateway={gw}\n\
         \n\
         [ipv6]\n\
         method=disabled\n",
        client = client_ip, gw = gateway_ip
    );
    let b = NewcBuilder::new("etc/NetworkManager/system-connections/bcvk-nbd.nmconnection")
        .mode(0o600).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, nm_conn.len() as u32);
    w.write_all(nm_conn.as_bytes())?;
    w.finish()?;

    let setup = format!(
        "#!/bin/bash\n\
modprobe nbd max_part=16 2>/dev/null\n\
systemctl start nm-initrd.service 2>/dev/null\n\
\n\
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do\n\
  /usr/bin/nbd-tcp /dev/nbd0 {host} {port} 2>/dev/kmsg && break\n\
  sleep 2\n\
done\n\
sleep 1\n\
blockdev --rereadpt /dev/nbd0 2>/dev/null\n",
        host = host, port = port
    );
    let b = NewcBuilder::new("usr/lib/bcvk/setup-nbd.sh").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, setup.len() as u32);
    w.write_all(setup.as_bytes())?;
    w.finish()?;

    let service =
        "[Unit]\n\
         Description=Setup NBD TCP connection\n\
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

#[cfg(target_os = "windows")]
fn append_cpio(initramfs: &mut Vec<u8>, cpio: &[u8]) {
    let aligned = (initramfs.len() + 3) & !3;
    initramfs.resize(aligned, 0);
    initramfs.extend_from_slice(cpio);
}
