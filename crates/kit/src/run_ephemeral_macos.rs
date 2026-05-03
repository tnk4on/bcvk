//! Ephemeral VM launch flow for macOS using libkrun C API + SquashFS.
//!
//! Boot flow:
//! 1. Extract kernel + initramfs from container image
//! 2. Create SquashFS rootfs (lz4, cached by digest)
//! 3. Append bcvk units CPIO to initramfs (/etc overlay + /var tmpfs + SSH)
//! 4. Launch VM via libkrun with virtio-blk (SquashFS) + virtio-net (gvproxy)
//!
//! vmlinuz PE+zstd is passed directly to libkrun (format=5), no decompression needed.

use std::ffi::{CString, c_int};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::{Result, eyre::{bail, eyre}};
use tracing::info;

// --- libkrun C API FFI ---

#[cfg(target_os = "macos")]
#[link(name = "krun")]
extern "C" {
    fn krun_set_log_level(level: u32) -> i32;
    fn krun_create_ctx() -> i32;
    fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    fn krun_set_kernel(
        ctx_id: u32, kernel_path: *const std::ffi::c_char, kernel_format: u32,
        initramfs_path: *const std::ffi::c_char, cmdline: *const std::ffi::c_char,
    ) -> i32;
    fn krun_add_disk2(
        ctx_id: u32, block_id: *const std::ffi::c_char, disk_path: *const std::ffi::c_char,
        disk_format: u32, read_only: bool,
    ) -> i32;
    fn krun_add_net_unixgram(
        ctx_id: u32, path: *const std::ffi::c_char, fd: c_int,
        mac: *const u8, features: u32, flags: u32,
    ) -> i32;
    fn krun_start_enter(ctx_id: u32) -> i32;
    fn krun_disable_implicit_console(ctx_id: u32) -> i32;
    fn krun_add_virtio_console_default(
        ctx_id: u32, input_fd: c_int, output_fd: c_int, err_fd: c_int,
    ) -> i32;
}

const KRUN_KERNEL_FORMAT_IMAGE_ZSTD: u32 = 5;
const KRUN_DISK_FORMAT_RAW: u32 = 0;
const NET_FLAG_VFKIT: u32 = 1 << 0;
const COMPAT_NET_FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);

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
    #[clap(long, default_value = "2")]
    pub cpus: u32,
    #[clap(long, default_value = "2048")]
    pub memory: u32,
    #[clap(long = "ssh-keygen", short = 'K')]
    pub ssh_keygen: bool,
    #[clap(long)]
    pub execute: Vec<String>,
    #[clap(long)]
    pub name: Option<String>,
    #[clap(long = "kernel-args")]
    pub kernel_args: Vec<String>,
    /// Run in background
    #[clap(long, short = 'd')]
    pub detach: bool,
    /// Enable debug mode (reserved for future use)
    #[clap(long)]
    pub debug: bool,
}

// --- RAII cleanup guard ---

struct VmCleanup {
    gvproxy_pid: u32,
    vm_name: String,
}

impl Drop for VmCleanup {
    fn drop(&mut self) {
        info!("cleaning up VM processes...");
        let _ = Command::new("kill").arg(self.gvproxy_pid.to_string())
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        EphemeralVmMetadata::remove(&self.vm_name);
    }
}

// --- Main entry point ---

pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    if opts.detach {
        return run_detached(&opts);
    }

    info!(image = %opts.image, "starting ephemeral VM on macOS (libkrun + SquashFS)");

    let cache_base = std::path::PathBuf::from("/private/tmp/bcvk");
    fs::create_dir_all(&cache_base)?;

    let machine = detect_machine_name()?;
    let digest = ensure_image_and_get_digest(&opts.image)?;
    let digest_short = &digest[..16.min(digest.len())];
    info!("image digest: {}...", digest_short);

    let boot_dir = cache_base.join(format!("boot-{}", digest_short));
    fs::create_dir_all(&boot_dir)?;
    let squashfs_path = format!("/private/tmp/bcvk/rootfs-{}.squashfs", digest_short);
    let vmlinuz_path = boot_dir.join("vmlinuz");
    let initramfs_orig = boot_dir.join("initramfs-orig.img");
    let initramfs_path = boot_dir.join("initramfs.img");

    // Step 1+2: kernel extract + SquashFS creation (parallel)
    let step2_handle = if !Path::new(&squashfs_path).exists() {
        let mc = machine.clone();
        let img = opts.image.clone();
        let sp = squashfs_path.clone();
        Some(std::thread::spawn(move || -> Result<()> {
            info!("creating SquashFS image (lz4)...");
            create_squashfs_image(&mc, &img, &sp)
        }))
    } else {
        info!("using cached SquashFS: {}", squashfs_path);
        None
    };

    if !vmlinuz_path.exists() || !initramfs_orig.exists() {
        info!("extracting kernel and initramfs...");
        extract_kernel(&opts.image, &boot_dir)?;
        fs::rename(boot_dir.join("initramfs.img"), &initramfs_orig)?;
    }

    // Step 3: CPIO append (no zstd decompression needed — libkrun handles PE+zstd)
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
            if !status.success() { bail!("ssh-keygen failed"); }
            let pubkey = fs::read_to_string(ssh_key_path.with_extension("pub"))?;
            let ssh_cpio = create_ssh_setup_cpio(pubkey.trim())?;
            let pos = f.seek(SeekFrom::End(0))?;
            let pad = pos.next_multiple_of(4) - pos;
            if pad > 0 { f.write_all(&vec![0u8; pad as usize])?; }
            f.write_all(&ssh_cpio)?;
        }
        info!("initramfs prepared");
    }

    if let Some(h) = step2_handle {
        h.join().map_err(|_| eyre!("squashfs creation thread panicked"))??;
    }

    // 4. gvproxy + libkrun
    let gvproxy_sock = cache_base.join(format!("gvproxy-{}.sock", digest_short));
    let services_sock = cache_base.join(format!("gvproxy-svc-{}.sock", digest_short));
    let gvproxy_sock_str = gvproxy_sock.to_string_lossy().to_string();
    let services_sock_str = services_sock.to_string_lossy().to_string();
    info!("starting gvproxy...");
    let mut gvproxy_child = start_gvproxy(&gvproxy_sock_str, &services_sock_str)?;

    let mut cmdline_parts: Vec<&str> = vec![
        "root=/dev/vda", "ro", "rootfstype=squashfs",
        "console=hvc0", "loglevel=4",
        "selinux=0", "net.ifnames=0",
        "systemd.journald.storage=volatile",
    ];
    let user_args: Vec<&str> = opts.kernel_args.iter().map(|s| s.as_str()).collect();
    cmdline_parts.extend(&user_args);
    let cmdline = cmdline_parts.join(" ");

    let mac = generate_mac();
    let vm_name = opts.name.clone()
        .unwrap_or_else(|| format!("ephemeral-{}", &digest_short[..8]));
    let ssh_key_path = cache_base.join("ephemeral-key");
    let serial_log_path = cache_base.join(format!("serial-{}.log", vm_name));

    let vmlinuz_str = vmlinuz_path.to_string_lossy().to_string();
    let initramfs_str = initramfs_path.to_string_lossy().to_string();
    let gvproxy_sock_clone = gvproxy_sock_str.clone();
    let squashfs_clone = squashfs_path.clone();
    let serial_log_clone = serial_log_path.to_string_lossy().to_string();
    let vcpus = opts.cpus as u8;
    let memory = opts.memory;

    info!("launching VM via libkrun...");
    let vm_thread = std::thread::spawn(move || -> Result<()> {
        #[cfg(target_os = "macos")]
        #[allow(unsafe_code)]
        unsafe {
            krun_set_log_level(3);
            let ctx = krun_create_ctx();
            if ctx < 0 { bail!("krun_create_ctx failed: {}", ctx); }
            let ctx = ctx as u32;

            if krun_set_vm_config(ctx, vcpus, memory) < 0 {
                bail!("krun_set_vm_config failed");
            }

            krun_disable_implicit_console(ctx);
            let serial_file = std::fs::File::create(&serial_log_clone)?;
            use std::os::unix::io::AsRawFd;
            krun_add_virtio_console_default(ctx, -1, serial_file.as_raw_fd(), -1);
            info!("serial log: {}", serial_log_clone);
            std::mem::forget(serial_file);

            let kernel_cstr = CString::new(vmlinuz_str)?;
            let initramfs_cstr = CString::new(initramfs_str)?;
            let cmdline_cstr = CString::new(cmdline)?;
            if krun_set_kernel(ctx, kernel_cstr.as_ptr(), KRUN_KERNEL_FORMAT_IMAGE_ZSTD,
                initramfs_cstr.as_ptr(), cmdline_cstr.as_ptr()) < 0 {
                bail!("krun_set_kernel failed");
            }

            let block_id = CString::new("rootfs")?;
            let disk_path = CString::new(squashfs_clone)?;
            if krun_add_disk2(ctx, block_id.as_ptr(), disk_path.as_ptr(),
                KRUN_DISK_FORMAT_RAW, true) < 0 {
                bail!("krun_add_disk2 failed");
            }

            let net_path = CString::new(gvproxy_sock_clone)?;
            if krun_add_net_unixgram(ctx, net_path.as_ptr(), -1,
                mac.as_ptr(), COMPAT_NET_FEATURES, NET_FLAG_VFKIT) < 0 {
                bail!("krun_add_net_unixgram failed");
            }

            info!("starting VM...");
            let ret = krun_start_enter(ctx);
            info!("VM exited: {}", ret);
        }
        Ok(())
    });

    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        pid: std::process::id(),
        gvproxy_pid: gvproxy_child.id(),
        ssh_port: 2222,
        ssh_key: ssh_key_path.to_string_lossy().to_string(),
        serial_log: serial_log_path.to_string_lossy().to_string(),
        log_path: None,
        created: chrono::Utc::now().to_rfc3339(),
    };
    metadata.save()?;

    let _cleanup = VmCleanup {
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
        let _ = run_ssh_interactive(ssh_port, &ssh_key_path, "root");
        return Ok(());
    }

    std::mem::forget(_cleanup);
    vm_thread.join().map_err(|_| eyre!("VM thread panicked"))??;
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

// --- Shared helpers ---

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

fn extract_kernel(image: &str, boot_dir: &Path) -> Result<()> {
    let status = Command::new("podman")
        .args(["run", "--rm", "-v", &format!("{}:/out", boot_dir.display()), image,
            "bash", "-c",
            "KVER=$(ls /usr/lib/modules/ | head -1) && \
             cp /usr/lib/modules/$KVER/vmlinuz /out/vmlinuz && \
             cp /usr/lib/modules/$KVER/initramfs.img /out/initramfs.img"])
        .status()?;
    if !status.success() { bail!("failed to extract kernel"); }
    Ok(())
}

fn create_squashfs_image(machine: &str, image: &str, output_path: &str) -> Result<()> {
    let mount_output = Command::new("podman")
        .args(["machine", "ssh", machine, "--",
            "podman", "image", "mount", image])
        .output()?;
    let merged = String::from_utf8_lossy(&mount_output.stdout).trim().to_string();
    if merged.is_empty() {
        bail!("podman image mount returned empty path");
    }
    info!("container rootfs: {}", merged);

    let status = Command::new("podman")
        .args(["machine", "ssh", machine, "--",
            "mksquashfs", &merged, output_path, "-noappend", "-comp", "lz4", "-b", "1M", "-quiet"])
        .status()?;
    if !status.success() { bail!("mksquashfs failed"); }
    Ok(())
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
        .spawn()?;
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

pub fn wait_for_ssh(port: u16, key_path: &Path, user: &str) -> Result<()> {
    let user_host = format!("{}@localhost", user);
    info!("waiting for SSH on port {} ({}@localhost)...", port, user);
    for i in 0..40u32 {
        let status = Command::new("ssh")
            .args([
                "-p", &port.to_string(),
                "-i", &key_path.to_string_lossy(),
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "IdentitiesOnly=yes",
                "-o", "ConnectTimeout=1",
                "-o", "BatchMode=yes",
                &user_host, "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(s) = status {
            if s.success() {
                info!("SSH connected after {}s", i);
                return Ok(());
            }
        }
        let backoff = if i < 2 { 500 } else if i < 4 { 1000 } else { 2000 };
        std::thread::sleep(Duration::from_millis(backoff));
    }
    bail!("SSH connection timeout");
}

pub fn run_ssh_command(port: u16, key_path: &Path, user: &str, command: &str) -> Result<std::process::ExitStatus> {
    let user_host = format!("{}@localhost", user);
    Command::new("ssh")
        .args([
            "-p", &port.to_string(),
            "-i", &key_path.to_string_lossy(),
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "IdentitiesOnly=yes",
            "-o", "BatchMode=yes",
            &user_host, command,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}

pub fn run_ssh_interactive(port: u16, key_path: &Path, user: &str) -> Result<std::process::ExitStatus> {
    let user_host = format!("{}@localhost", user);
    Command::new("ssh")
        .args([
            "-p", &port.to_string(),
            "-i", &key_path.to_string_lossy(),
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "IdentitiesOnly=yes",
            "-t",
            &user_host,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}
