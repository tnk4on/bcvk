//! Ephemeral VM launch flow for macOS using vfkit + SquashFS.
//!
//! Boot flow:
//! 1. Extract kernel + initramfs from container image
//! 2. Create SquashFS rootfs (lz4, cached by digest)
//! 3. Decompress vmlinuz PE+zstd → uncompressed ARM64 Image
//! 4. Append bcvk units CPIO to initramfs (/etc overlay + /var tmpfs + SSH)
//! 5. Launch vfkit with virtio-blk (SquashFS) + virtio-net (gvproxy)
//!
//! Common helpers (gvproxy, SSH, vfkit detection) are pub for reuse by vfkit/ module.

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::{Result, eyre::{bail, eyre, Context}};
use tracing::{info, debug};

// --- Data structures ---

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct EphemeralVmMetadata {
    pub name: String,
    pub image: String,
    pub pid: u32,
    pub gvproxy_pid: u32,
    pub ssh_port: u16,
    pub ssh_key: String,
    pub serial_log: String,
    pub log_path: Option<String>,
    pub created: String,
}

#[allow(dead_code)]
impl EphemeralVmMetadata {
    pub fn vms_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/private/tmp/bcvk/vms")
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::vms_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.name));
        fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn remove(name: &str) {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let _ = fs::remove_file(path);
    }

    pub fn load(name: &str) -> Result<Self> {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let data = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn list_all() -> Result<Vec<Self>> {
        let dir = Self::vms_dir();
        if !dir.exists() { return Ok(Vec::new()); }
        let mut vms = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<Self>(&data) {
                    vms.push(meta);
                }
            }
        }
        Ok(vms)
    }

    pub fn is_alive(&self) -> bool {
        Command::new("kill")
            .args(["-0", &self.pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[derive(clap::Parser, Debug)]
pub struct RunEphemeralOpts {
    /// Container image to boot
    pub image: String,
    #[clap(long, help = "Number of vCPUs")]
    pub vcpus: Option<u32>,
    #[clap(long, default_value = "4G", help = "Memory size (e.g. 4G, 2048M, or plain number for MB)")]
    pub memory: String,
    #[clap(long = "ssh-keygen", short = 'K')]
    pub ssh_keygen: bool,
    #[clap(long)]
    pub execute: Vec<String>,
    #[clap(long, help = "VM name for identification")]
    pub name: Option<String>,
    #[clap(long = "karg", help = "Additional kernel command line arguments")]
    pub kernel_args: Vec<String>,
    /// Display VM console in GUI window
    #[clap(long)]
    pub gui: bool,
    /// Run in background
    #[clap(long, short = 'd')]
    pub detach: bool,
    /// Enable debug mode (reserved for future use)
    #[clap(long)]
    pub debug: bool,
}

fn default_vcpus() -> u32 {
    2
}

pub fn parse_memory_to_mb(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        Ok((n.parse::<f64>()? * 1024.0) as u32)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(n.parse::<f64>()? as u32)
    } else {
        Ok(s.parse::<u32>()?)
    }
}

// --- RAII cleanup guard ---

struct VmCleanup {
    vfkit_pid: u32,
    gvproxy_pid: u32,
    vm_name: String,
}

impl Drop for VmCleanup {
    fn drop(&mut self) {
        tracing::debug!("cleaning up VM processes...");
        if let Err(e) = Command::new("kill").arg(self.vfkit_pid.to_string())
            .stdout(Stdio::null()).stderr(Stdio::null()).status() {
            tracing::warn!("failed to kill vfkit (PID {}): {}", self.vfkit_pid, e);
        }
        if let Err(e) = Command::new("kill").arg(self.gvproxy_pid.to_string())
            .stdout(Stdio::null()).stderr(Stdio::null()).status() {
            tracing::warn!("failed to kill gvproxy (PID {}): {}", self.gvproxy_pid, e);
        }
        EphemeralVmMetadata::remove(&self.vm_name);
    }
}

// --- Main entry point ---

pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    if opts.gui && opts.detach {
        bail!("--gui and --detach cannot be used together (GUI requires foreground process)");
    }

    if opts.detach {
        return run_detached(&opts);
    }

    let vfkit_bin = find_vfkit()?;
    info!(image = %opts.image, "starting ephemeral VM on macOS (vfkit + SquashFS)");

    let cache_base = std::path::PathBuf::from("/private/tmp/bcvk");
    fs::create_dir_all(&cache_base)?;

    let machine = detect_machine_name()?;
    let rootful = is_machine_rootful(&machine);
    debug!("podman machine '{}' ({})", machine, if rootful { "rootful" } else { "rootless" });
    let digest = ensure_image_and_get_digest(&opts.image)?;
    let digest_short = &digest[..16.min(digest.len())];
    info!("image digest: {}...", digest_short);

    let boot_dir = cache_base.join(format!("boot-{}", digest_short));
    fs::create_dir_all(&boot_dir)?;
    let squashfs_path = format!("/private/tmp/bcvk/rootfs-{}.squashfs", digest_short);
    let vmlinuz_path = boot_dir.join("vmlinuz");
    let image_path = boot_dir.join("Image");
    let initramfs_orig = boot_dir.join("initramfs-orig.img");
    let initramfs_path = boot_dir.join("initramfs.img");

    // Step 1+2: kernel extract + SquashFS creation (parallel)
    let step2_handle = if !Path::new(&squashfs_path).exists() {
        let mc = machine.clone();
        let rf = rootful;
        let img = opts.image.clone();
        let sp = squashfs_path.clone();
        Some(std::thread::spawn(move || -> Result<()> {
            info!("creating SquashFS image (lz4)...");
            create_squashfs_image(&mc, rf, &img, &sp)
        }))
    } else {
        info!("using cached SquashFS: {}", squashfs_path);
        None
    };

    if !vmlinuz_path.exists() || !initramfs_orig.exists() {
        info!("extracting kernel and initramfs...");
        extract_kernel(&machine, &opts.image, &boot_dir)?;
        fs::rename(boot_dir.join("initramfs.img"), &initramfs_orig)?;
    }

    // Step 3+4: kernel decompress + CPIO append (parallel after Step 1)
    let step3_handle = if !image_path.exists() {
        let vp = vmlinuz_path.clone();
        let ip = image_path.clone();
        Some(std::thread::spawn(move || -> Result<()> {
            info!("decompressing kernel (vmlinuz → Image)...");
            extract_uncompressed_kernel(&vp, &ip)
        }))
    } else { None };

    fs::copy(&initramfs_orig, &initramfs_path)?;
    {
        let cpio_data = crate::cpio::create_initramfs_units_cpio()
            .map_err(|e| eyre!("failed to create CPIO: {e}"))?;
        let mut f = OpenOptions::new().append(true).open(&initramfs_path)?;
        let sz = f.seek(SeekFrom::End(0))?;
        let pad = sz.next_multiple_of(4) - sz;
        if pad > 0 { f.write_all(&vec![0u8; pad as usize])?; }
        f.write_all(&cpio_data)?;

        let ssh_key_path = cache_base.join("ephemeral-key");
        if opts.ssh_keygen || !opts.execute.is_empty() {
            info!("generating SSH keypair...");
            let _ = fs::remove_file(&ssh_key_path);
            let _ = fs::remove_file(ssh_key_path.with_extension("pub"));
            let status = Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f", &ssh_key_path.to_string_lossy(), "-N", "", "-q"])
                .status()?;
            if !status.success() { bail!("ssh-keygen failed (exit code: {:?})", status.code()); }
            let pubkey = fs::read_to_string(ssh_key_path.with_extension("pub"))?;
            let ssh_cpio = create_ssh_setup_cpio(pubkey.trim())?;
            let pos = f.seek(SeekFrom::End(0))?;
            let pad = pos.next_multiple_of(4) - pos;
            if pad > 0 { f.write_all(&vec![0u8; pad as usize])?; }
            f.write_all(&ssh_cpio)?;
        }
        info!("initramfs prepared");
    }

    if let Some(h) = step3_handle {
        h.join().map_err(|_| eyre!("kernel decompression thread panicked"))??;
    }
    if let Some(h) = step2_handle {
        h.join().map_err(|_| eyre!("squashfs creation thread panicked"))??;
    }

    // 5. gvproxy + vfkit
    let gvproxy_sock = cache_base.join(format!("gvproxy-{}.sock", digest_short));
    let services_sock = cache_base.join(format!("gvproxy-svc-{}.sock", digest_short));
    let gvproxy_sock_str = gvproxy_sock.to_string_lossy().to_string();
    let services_sock_str = services_sock.to_string_lossy().to_string();
    info!("starting gvproxy...");
    let mut gvproxy_child = start_gvproxy(&gvproxy_sock_str, &services_sock_str)?;

    let mut cmdline_parts: Vec<&str> = vec![
        "root=/dev/vda", "ro", "rootfstype=squashfs",
        "console=tty0", "console=hvc0", "loglevel=4",
        "selinux=0", "net.ifnames=0",
        "systemd.journald.storage=volatile",
    ];
    let user_args: Vec<&str> = opts.kernel_args.iter().map(|s| s.as_str()).collect();
    cmdline_parts.extend(&user_args);
    let cmdline = cmdline_parts.join(" ");

    let mac = generate_mac();
    let mac_str = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    let bootloader_arg = format!(
        "linux,kernel={},initrd={},cmdline=\"{}\"",
        image_path.display(), initramfs_path.display(), cmdline
    );

    let vcpus = opts.vcpus.unwrap_or_else(default_vcpus);
    let memory_mb = parse_memory_to_mb(&opts.memory)?;

    let mut vfkit_args = vec![
        "--cpus".to_string(), vcpus.to_string(),
        "--memory".to_string(), memory_mb.to_string(),
        "--bootloader".to_string(), bootloader_arg,
        "--device".to_string(), format!("virtio-blk,path={}", squashfs_path),
        "--device".to_string(), format!("virtio-net,unixSocketPath={},mac={}", gvproxy_sock_str, mac_str),
        "--device".to_string(), "virtio-rng".to_string(),
    ];
    if opts.gui {
        vfkit_args.push("--gui".to_string());
    }

    info!("launching vfkit...");
    let vfkit_log = cache_base.join("vfkit.log");
    let vfkit_log_file = fs::File::create(&vfkit_log)?;
    let mut vfkit_child = Command::new(&vfkit_bin)
        .args(&vfkit_args)
        .stdout(vfkit_log_file.try_clone()?)
        .stderr(vfkit_log_file)
        .spawn()
        .context("failed to start vfkit")?;

    let vm_name = opts.name.clone()
        .unwrap_or_else(|| format!("ephemeral-{}", &digest_short[..8]));
    let ssh_key_path = cache_base.join("ephemeral-key");

    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        pid: vfkit_child.id(),
        gvproxy_pid: gvproxy_child.id(),
        ssh_port: 2222,
        ssh_key: ssh_key_path.to_string_lossy().to_string(),
        serial_log: String::new(),
        log_path: None,
        created: chrono::Utc::now().to_rfc3339(),
    };
    metadata.save()?;

    let _cleanup = VmCleanup {
        vfkit_pid: vfkit_child.id(),
        gvproxy_pid: gvproxy_child.id(),
        vm_name: vm_name.clone(),
    };

    if opts.ssh_keygen || !opts.execute.is_empty() {
        let ssh_port: u16 = 2222;

        info!("setting up SSH port forwarding...");
        for attempt in 0..15u32 {
            match expose_ssh_port(&services_sock_str, "192.168.127.2", ssh_port) {
                Ok(_) => { info!("SSH port {} forwarded", ssh_port); break; }
                Err(e) if attempt < 14 => {
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

        info!("SSH ready: ssh -p {} -i {} root@localhost", ssh_port, ssh_key_path.display());

        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            let status = run_ssh_interactive(ssh_port, &ssh_key_path, "root")?;
            let exit_code = status.code().unwrap_or(1);
            drop(_cleanup);
            std::process::exit(exit_code);
        }
    }

    // No SSH: wait for vfkit to exit (GUI window closed or VM shutdown)
    std::mem::forget(_cleanup);
    let status = vfkit_child.wait()?;
    info!("vfkit exited: {}", status);
    let _ = gvproxy_child.kill();
    EphemeralVmMetadata::remove(&vm_name);
    Ok(())
}

fn run_detached(opts: &RunEphemeralOpts) -> Result<()> {
    let cache_base = std::path::PathBuf::from("/private/tmp/bcvk");
    fs::create_dir_all(&cache_base)?;
    let digest = ensure_image_and_get_digest(&opts.image)?;
    let digest_short = &digest[..16.min(digest.len())];
    let vm_name = opts.name.clone()
        .unwrap_or_else(|| format!("ephemeral-{}", &digest_short[..8]));
    let log_path = cache_base.join(format!("bcvk-{}.log", vm_name));
    let log_file = fs::File::create(&log_path)?;

    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args().skip(1)
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
        ssh_port: 2222,
        ssh_key: cache_base.join("ephemeral-key").to_string_lossy().to_string(),
        serial_log: String::new(),
        log_path: Some(log_path.to_string_lossy().to_string()),
        created: chrono::Utc::now().to_rfc3339(),
    };
    metadata.save()?;
    println!("{}", vm_name);
    Ok(())
}

// --- SSH setup CPIO ---

fn create_ssh_setup_cpio(pubkey: &str) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    let mut buf = Vec::new();

    let script = format!(
        "#!/bin/bash\n\
         mkdir -p /sysroot/var/roothome/.ssh\n\
         chmod 700 /sysroot/var/roothome/.ssh\n\
         echo '{}' > /sysroot/var/roothome/.ssh/authorized_keys\n\
         chmod 600 /sysroot/var/roothome/.ssh/authorized_keys\n\
         chown -R 0:0 /sysroot/var/roothome/.ssh\n",
        pubkey
    );

    let service = "[Unit]\n\
         Description=Setup SSH authorized_keys for root\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=initrd-fs.target\n\
         After=bcvk-var-ephemeral.service\n\
         Requires=bcvk-var-ephemeral.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-ssh.sh\n";

    let dropin = "[Unit]\nWants=bcvk-ssh-setup.service\n";

    let write_entry = |buf: &mut Vec<u8>, path: &str, data: &[u8], executable: bool| -> std::io::Result<()> {
        let mode = if executable { 0o100755 } else { 0o100644 };
        let builder = NewcBuilder::new(path).mode(mode).uid(0).gid(0);
        let mut writer = builder.write(buf, data.len() as u32);
        writer.write_all(data)?;
        writer.finish()?;
        Ok(())
    };

    let write_dir = |buf: &mut Vec<u8>, path: &str| -> std::io::Result<()> {
        NewcBuilder::new(path).mode(0o040755).uid(0).gid(0).write(buf, 0).finish()?;
        Ok(())
    };

    write_dir(&mut buf, "usr/lib/bcvk")?;
    write_entry(&mut buf, "usr/lib/bcvk/setup-ssh.sh", script.as_bytes(), true)?;
    write_entry(&mut buf, "usr/lib/systemd/system/bcvk-ssh-setup.service", service.as_bytes(), false)?;
    write_entry(&mut buf, "usr/lib/systemd/system/initrd-fs.target.d/bcvk-ssh-setup.conf", dropin.as_bytes(), false)?;
    cpio::newc::trailer(&mut buf).map_err(|e| eyre!("cpio trailer: {e}"))?;
    Ok(buf)
}

// --- vfkit kernel decompression ---

fn extract_uncompressed_kernel(vmlinuz_path: &Path, output_path: &Path) -> Result<()> {
    let data = fs::read(vmlinuz_path)?;
    let magic = [0x28u8, 0xb5, 0x2f, 0xfd];
    let pos = data.windows(4).position(|w| w == magic)
        .ok_or_else(|| eyre!("zstd magic not found in vmlinuz"))?;
    info!("zstd payload at offset 0x{:x}", pos);

    let mut kernel = Vec::new();
    let _ = zstd::stream::copy_decode(&data[pos..], &mut kernel);

    if kernel.len() < 0x3c || &kernel[0x38..0x3c] != b"ARMd" {
        bail!("decompressed kernel is not a valid ARM64 Image");
    }
    fs::write(output_path, &kernel)?;
    info!("decompressed kernel: {} bytes (ARM64 Image)", kernel.len());
    Ok(())
}

// --- Shared helpers (pub for vfkit/ module) ---

fn detect_machine_name() -> Result<String> {
    let output = Command::new("podman")
        .args(["machine", "info", "--format", "{{.Host.CurrentMachine}}"])
        .output()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() { bail!("no podman machine is running"); }
    Ok(name)
}

fn ensure_image_and_get_digest(image: &str) -> Result<String> {
    let status = Command::new("podman").args(["image", "exists", image])
        .stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if !status.success() {
        info!("pulling image {}...", image);
        if !Command::new("podman").args(["pull", image]).status()?.success() {
            bail!("failed to pull image: {}", image);
        }
    }
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .output()?;
    let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(digest.trim_start_matches("sha256:").to_string())
}

fn extract_kernel(machine: &str, image: &str, boot_dir: &Path) -> Result<()> {
    let boot_dir_str = boot_dir.to_string_lossy();

    // Get kernel version first
    let kver_output = Command::new("podman")
        .args(["machine", "ssh", machine,
            "podman", "run", "--rm", image, "ls", "/usr/lib/modules/"])
        .output()
        .context("detecting kernel version")?;
    let kver = String::from_utf8_lossy(&kver_output.stdout)
        .lines().next().unwrap_or("").trim().to_string();
    if kver.is_empty() || !kver_output.status.success() {
        bail!("No kernel found in image '{}'.\n\
               Checked: /usr/lib/modules/<version>/vmlinuz + initramfs.img\n\
               This image may not be a bootable container (bootc) image.", image);
    }
    info!("kernel version: {}", kver);

    // Extract vmlinuz via podman run cat > file (works with both rootful and rootless)
    let vmlinuz_src = format!("/usr/lib/modules/{}/vmlinuz", kver);
    let initramfs_src = format!("/usr/lib/modules/{}/initramfs.img", kver);

    let output = Command::new("podman")
        .args(["machine", "ssh", machine, &format!(
            "podman run --rm {} cat {} > {}/vmlinuz", image, vmlinuz_src, boot_dir_str)])
        .output()
        .context("extracting vmlinuz")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to extract vmlinuz: {}", stderr.trim());
    }

    let output = Command::new("podman")
        .args(["machine", "ssh", machine, &format!(
            "podman run --rm {} cat {} > {}/initramfs.img", image, initramfs_src, boot_dir_str)])
        .output()
        .context("extracting initramfs")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("No kernel found in image '{}'.\n\
               Checked: /usr/lib/modules/<version>/vmlinuz + initramfs.img\n\
               This image may not be a bootable container (bootc) image.\n\
               {}", image, stderr.trim());
    }
    Ok(())
}

fn is_machine_rootful(machine: &str) -> bool {
    Command::new("podman")
        .args(["machine", "ssh", machine, "id", "-u"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn create_squashfs_image(machine: &str, rootful: bool, image: &str, output_path: &str) -> Result<()> {
    let script = if rootful {
        format!(
            "MERGED=$(podman image mount {}) && \
             mksquashfs $MERGED {} -noappend -comp lz4 -b 1M -quiet",
            image, output_path
        )
    } else {
        info!("rootless mode: using podman unshare for SquashFS creation");
        format!(
            "podman unshare sh -c 'MERGED=$(podman image mount {}) && \
             mksquashfs $MERGED {} -noappend -comp lz4 -b 1M -quiet'",
            image, output_path
        )
    };

    let output = Command::new("podman")
        .args(["machine", "ssh", machine, &script])
        .output().context("running mksquashfs")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("mksquashfs failed: {}", stderr.trim());
    }
    Ok(())
}

pub fn find_vfkit() -> Result<String> {
    if let Ok(path) = which::which("vfkit") {
        return Ok(path.to_string_lossy().to_string());
    }
    let podman_path = "/opt/podman/bin/vfkit";
    if Path::new(podman_path).exists() {
        return Ok(podman_path.to_string());
    }
    bail!("vfkit not found. Install: brew install vfkit")
}

pub fn generate_mac() -> [u8; 6] {
    [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]
}

pub fn start_gvproxy(gvproxy_sock: &str, services_sock: &str) -> Result<std::process::Child> {
    let _ = fs::remove_file(gvproxy_sock);
    let _ = fs::remove_file(services_sock);
    let child = Command::new("gvproxy")
        .args([
            "-listen-vfkit", &format!("unixgram://{}", gvproxy_sock),
            "-ssh-port", "-1",
            "-services", &format!("unix://{}", services_sock),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start gvproxy. Ensure gvproxy is installed (included in Podman)")?;
    for _ in 0..50 {
        if Path::new(gvproxy_sock).exists() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !Path::new(gvproxy_sock).exists() {
        bail!("gvproxy socket did not appear");
    }
    Ok(child)
}

pub fn expose_ssh_port(services_sock: &str, vm_ip: &str, host_port: u16) -> Result<()> {
    let body = format!(
        r#"{{"local":":{}","remote":"{}:22","protocol":"tcp"}}"#,
        host_port, vm_ip
    );
    let mut stream = UnixStream::connect(services_sock)?;
    let request = format!(
        "POST /services/forwarder/expose HTTP/1.1\r\nHost: unix\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())?;
    std::io::Write::flush(&mut stream)?;
    let mut response = vec![0u8; 1024];
    let _ = std::io::Read::read(&mut stream, &mut response);
    let response_str = String::from_utf8_lossy(&response);
    if !response_str.contains("200") {
        bail!("gvproxy expose failed: {}", response_str.trim_end_matches('\0'));
    }
    Ok(())
}

const SSH_TIMEOUT: Duration = Duration::from_secs(240);

pub fn wait_for_ssh(port: u16, key_path: &Path, user: &str) -> Result<()> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    info!("waiting for SSH on port {} ({}@localhost)...", port, user);
    let start = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        if start.elapsed() > SSH_TIMEOUT {
            bail!("SSH connection timeout ({}s)", SSH_TIMEOUT.as_secs());
        }
        let mut cmd = Command::new("ssh");
        cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
        ssh_opts.apply_to_command(&mut cmd);
        cmd.args(["-o", "BatchMode=yes", &user_host, "true"]);
        let status = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
        if let Ok(s) = status {
            if s.success() {
                info!("SSH connected after {}s", start.elapsed().as_secs());
                return Ok(());
            }
        }
        let backoff = if attempt < 2 { 500 } else if attempt < 4 { 1000 } else { 2000 };
        std::thread::sleep(Duration::from_millis(backoff));
        attempt += 1;
    }
}

pub fn run_ssh_command(port: u16, key_path: &Path, user: &str, command: &str) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-o", "BatchMode=yes", &user_host, command]);
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}

pub fn run_ssh_interactive(port: u16, key_path: &Path, user: &str) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-t", &user_host]);
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}
