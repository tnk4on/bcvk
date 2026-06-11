//! Install bootc images to disk on macOS.
//!
//! Podman mode: uses losetup inside podman machine to create loop devices from raw disk files
//! accessible via virtiofs, then runs `bootc install to-disk` targeting the loop device.
//!
//! Native mode: boots a temporary vfkit VM from the source image, shares the apple/container
//! content store via virtiofs as an OCI layout, and runs `bootc install to-disk` directly
//! (no podman needed). The OCI blobs are read from the host via virtiofs without copying.
//!
//! Base disk caching with APFS clonefile (`cp -c`) provides fast VM creation in both modes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use tracing::{debug, info};

use crate::install_options::InstallOptions;
use crate::run_ephemeral_macos::{
    clear_xattr, ephemeral_base_dir, expose_port, find_available_ssh_port, find_vfkit,
    generate_mac, start_gvproxy, VmCleanup,
};
use crate::vm_helpers::{
    default_vcpus, detect_machine_name, ensure_image_and_get_digest, generate_ssh_keypair,
    is_machine_rootful, parse_memory_to_mb, parse_size, remove_file_if_exists, run_ssh_command,
    wait_for_ssh,
};
use sha2::{Digest, Sha256};

/// Options for `bcvk to-disk` on macOS.
#[derive(Parser, Debug)]
pub struct ToDiskMacosOpts {
    /// Container image to install
    pub source_image: String,
    /// Target disk path (output .raw file)
    pub target_disk: String,
    /// Disk size (e.g. "10G", "5120M", or plain number for bytes)
    #[clap(long, default_value = "20G")]
    pub disk_size: String,
    /// Installation options (filesystem, root-size, etc.)
    #[clap(flatten)]
    pub install: InstallOptions,
    /// Configure logging for `bootc install` by setting the `RUST_LOG` environment variable
    #[clap(long)]
    pub install_log: Option<String>,
    /// Add metadata to the container in key=value form
    #[clap(long = "label")]
    pub label: Vec<String>,
    /// Check if the disk would be regenerated without actually creating it
    #[clap(long)]
    pub dry_run: bool,
    /// Use native mode (apple/container CLI, no podman machine)
    #[clap(long)]
    pub native: bool,
}

fn base_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".local/share/bcvk/base")
}

/// Directory for persistent VM disk images.
pub fn vms_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".local/share/bcvk/vms")
}

fn resolve_path_in_machine(host_path: &str) -> String {
    let resolved = if let Ok(canonical) = std::fs::canonicalize(host_path) {
        canonical.to_string_lossy().to_string()
    } else {
        host_path.to_string()
    };
    // macOS /tmp is a symlink to /private/tmp; podman machine mounts
    // /private/tmp via virtiofs, so we need the canonical path.
    // canonicalize() normally resolves this, but handle it explicitly.
    if resolved.starts_with("/tmp/") {
        format!("/private{}", resolved)
    } else {
        resolved
    }
}

fn create_raw_disk(path: &str, size_bytes: u64) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("creating {}", path))?;
    file.set_len(size_bytes)
        .with_context(|| format!("setting size {} on {}", size_bytes, path))?;
    drop(file);
    clear_xattr(Path::new(path));
    Ok(())
}

fn generate_bootc_install_script(
    disk_path_in_machine: &str,
    image: &str,
    install_opts: &InstallOptions,
    ssh_pubkey: &str,
    rootful: bool,
    install_log: &Option<String>,
    labels: &[String],
) -> String {
    let bootc_args = install_opts
        .to_bootc_args()
        .iter()
        .map(|a| {
            shlex::try_quote(a)
                .unwrap_or(std::borrow::Cow::Borrowed(a))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ");

    let image_quoted = shlex::try_quote(image)
        .unwrap_or(std::borrow::Cow::Borrowed(image))
        .to_string();

    use base64::Engine;
    let pub_key_b64 = base64::engine::general_purpose::STANDARD.encode(ssh_pubkey);

    let sudo = if rootful { "" } else { "sudo " };

    let rust_log_line = if let Some(ref level) = install_log {
        format!(
            "export RUST_LOG={}\n",
            shlex::try_quote(level).unwrap_or(std::borrow::Cow::Borrowed(level))
        )
    } else {
        String::new()
    };

    let label_args = labels
        .iter()
        .map(|l| {
            format!(
                "--label {}",
                shlex::try_quote(l).unwrap_or(std::borrow::Cow::Borrowed(l))
            )
        })
        .collect::<Vec<_>>()
        .join(" \\\n  ");
    let label_line = if label_args.is_empty() {
        String::new()
    } else {
        format!("  {} \\\n", label_args)
    };

    include_str!("../scripts/bootc-install-to-disk.sh")
        .replace("@@RUST_LOG@@", &rust_log_line)
        .replace("@@SUDO@@", sudo)
        .replace("@@DISK_PATH@@", disk_path_in_machine)
        .replace("@@SSH_PUBKEY_B64@@", &pub_key_b64)
        .replace("@@LABEL_LINE@@", &label_line)
        .replace("@@IMAGE@@", &image_quoted)
        .replace("@@BOOTC_ARGS@@", &bootc_args)
}

const CACHE_HASH_XATTR: &str = "user.bcvk.cache_hash";

fn compute_cache_hash(
    image_digest: &str,
    source_image: &str,
    install_opts: &InstallOptions,
) -> String {
    let bootc_args = install_opts.to_bootc_args().join(",");
    let input = format!("{}|{}|{}", image_digest, source_image, bootc_args);
    let hash = Sha256::digest(input.as_bytes());
    format!("sha256:{:x}", hash)
}

fn read_xattr(path: &Path, name: &str) -> Option<String> {
    let output = Command::new("xattr")
        .args(["-p", name, &path.to_string_lossy()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn write_xattr(path: &Path, name: &str, value: &str) -> Result<()> {
    let status = Command::new("xattr")
        .args(["-w", name, value, &path.to_string_lossy()])
        .status()
        .with_context(|| format!("writing xattr {} on {}", name, path.display()))?;
    if !status.success() {
        bail!("xattr -w failed for {} on {}", name, path.display());
    }
    Ok(())
}

/// Find or create a cached base disk for the given image + install options.
pub fn find_or_create_base_disk(
    source_image: &str,
    image_digest: &str,
    install_options: &InstallOptions,
    disk_size: &str,
    machine: &str,
    install_log: &Option<String>,
    labels: &[String],
) -> Result<PathBuf> {
    let cache_hash = compute_cache_hash(image_digest, source_image, install_options);
    let short_hash = cache_hash
        .strip_prefix("sha256:")
        .unwrap_or(&cache_hash)
        .chars()
        .take(16)
        .collect::<String>();

    let base_dir = base_dir();
    fs::create_dir_all(&base_dir)?;
    let base_disk_name = format!("bootc-base-{}.raw", short_hash);
    let base_disk_path = base_dir.join(&base_disk_name);

    if base_disk_path.exists() {
        debug!("checking existing base disk: {:?}", base_disk_path);
        if let Some(stored_hash) = read_xattr(&base_disk_path, CACHE_HASH_XATTR) {
            if stored_hash == cache_hash {
                info!("reusing cached base disk: {:?}", base_disk_path);
                return Ok(base_disk_path);
            }
            info!("base disk cache hash mismatch, recreating");
        } else {
            info!("base disk has no cache hash, recreating");
        }
        fs::remove_file(&base_disk_path)?;
    }

    info!("creating base disk: {:?}", base_disk_path);
    let base_disk_str = base_disk_path.to_string_lossy().to_string();

    let size_bytes = parse_size(disk_size)?;
    create_raw_disk(&base_disk_str, size_bytes)?;

    let key_path = PathBuf::from(format!("{}.key", base_disk_path.display()));
    let ssh_pubkey = generate_ssh_keypair(&key_path)?;

    let disk_in_machine = resolve_path_in_machine(&base_disk_str);
    let rootful = is_machine_rootful(machine);
    let script = generate_bootc_install_script(
        &disk_in_machine,
        source_image,
        install_options,
        &ssh_pubkey,
        rootful,
        install_log,
        labels,
    );

    info!("running bootc install to-disk in podman machine...");
    let mut child = Command::new("podman")
        .args(["machine", "ssh", machine, "--", "bash", "-s"])
        .stdin(Stdio::piped())
        .spawn()
        .context("podman machine ssh")?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(script.as_bytes())?;
    }
    let status = child.wait()?;

    if !status.success() {
        remove_file_if_exists(&base_disk_path);
        remove_file_if_exists(&key_path);
        remove_file_if_exists(&PathBuf::from(format!("{}.pub", key_path.display())));
        bail!("bootc install to-disk failed");
    }

    write_xattr(&base_disk_path, CACHE_HASH_XATTR, &cache_hash)?;

    Ok(base_disk_path)
}

/// Clone a base disk to create a VM-specific disk via APFS clonefile (`cp -c`).
pub fn clone_base_disk(base_path: &Path, vm_disk_path: &Path) -> Result<()> {
    if let Some(parent) = vm_disk_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("cp")
        .args([
            "-c",
            &base_path.to_string_lossy(),
            &vm_disk_path.to_string_lossy(),
        ])
        .status()
        .context("cp -c (APFS clonefile)")?;
    if !status.success() {
        bail!(
            "APFS clonefile failed: {} -> {}",
            base_path.display(),
            vm_disk_path.display()
        );
    }
    clear_xattr(vm_disk_path);
    Ok(())
}

/// Execute `bcvk to-disk` on macOS.
pub fn run(opts: ToDiskMacosOpts) -> Result<()> {
    if opts.native {
        return run_native(opts);
    }
    let machine = detect_machine_name()?;
    let digest = ensure_image_and_get_digest(&opts.source_image)?;
    info!("image digest: {}...", &digest[..16.min(digest.len())]);

    let cache_hash = compute_cache_hash(&digest, &opts.source_image, &opts.install);
    let short_hash: String = cache_hash
        .strip_prefix("sha256:")
        .unwrap_or(&cache_hash)
        .chars()
        .take(16)
        .collect();
    let base_disk_path = base_dir().join(format!("bootc-base-{}.raw", short_hash));

    if opts.dry_run {
        if base_disk_path.exists() {
            if let Some(stored) = read_xattr(&base_disk_path, CACHE_HASH_XATTR) {
                if stored == cache_hash {
                    println!("Would reuse cached base disk: {}", base_disk_path.display());
                    if Path::new(&opts.target_disk).exists() {
                        println!("Output already exists: {}", opts.target_disk);
                    } else {
                        println!("Would create disk: {} (from base)", opts.target_disk);
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

    let base_disk_path = find_or_create_base_disk(
        &opts.source_image,
        &digest,
        &opts.install,
        &opts.disk_size,
        &machine,
        &opts.install_log,
        &opts.label,
    )?;

    // Copy base disk to target via APFS clonefile
    let target = Path::new(&opts.target_disk);
    clone_base_disk(&base_disk_path, target)?;

    // Copy SSH key ({base}.raw.key → {target}.key)
    let base_key = PathBuf::from(format!("{}.key", base_disk_path.display()));
    let target_key = PathBuf::from(format!("{}.key", target.display()));
    if base_key.exists() {
        fs::copy(&base_key, &target_key).context("copying SSH key")?;
        let base_pub = PathBuf::from(format!("{}.pub", base_key.display()));
        let target_pub = PathBuf::from(format!("{}.pub", target_key.display()));
        if base_pub.exists() {
            fs::copy(&base_pub, &target_pub).context("copying SSH pubkey")?;
        }
    }

    println!("Disk image created: {}", opts.target_disk);
    println!("SSH key: {}", target_key.display());
    println!(
        "\nTo boot:  bcvk vm run --ssh-key {} {}",
        target_key.display(),
        opts.target_disk
    );
    Ok(())
}

/// Execute `bcvk to-disk --native` on macOS (no podman machine).
fn run_native(opts: ToDiskMacosOpts) -> Result<()> {
    crate::run_native_macos::check_prerequisites()?;
    crate::run_native_macos::ensure_container_system()?;
    let digest = crate::run_native_macos::pull_image(&opts.source_image)?;
    info!("image digest: {}...", &digest[..16.min(digest.len())]);

    let cache_hash = compute_cache_hash(&digest, &opts.source_image, &opts.install);
    let short_hash: String = cache_hash
        .strip_prefix("sha256:")
        .unwrap_or(&cache_hash)
        .chars()
        .take(16)
        .collect();
    let base_disk_path = base_dir().join(format!("bootc-base-{}.raw", short_hash));

    if opts.dry_run {
        if base_disk_path.exists() {
            if let Some(stored) = read_xattr(&base_disk_path, CACHE_HASH_XATTR) {
                if stored == cache_hash {
                    println!("Would reuse cached base disk: {}", base_disk_path.display());
                    if Path::new(&opts.target_disk).exists() {
                        println!("Output already exists: {}", opts.target_disk);
                    } else {
                        println!("Would create disk: {} (from base)", opts.target_disk);
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

    let base_disk_path = find_or_create_base_disk_native(
        &opts.source_image,
        &digest,
        &opts.install,
        &opts.disk_size,
        &opts.install_log,
    )?;

    let target = Path::new(&opts.target_disk);
    clone_base_disk(&base_disk_path, target)?;

    let base_key = PathBuf::from(format!("{}.key", base_disk_path.display()));
    let target_key = PathBuf::from(format!("{}.key", target.display()));
    if base_key.exists() {
        fs::copy(&base_key, &target_key).context("copying SSH key")?;
        let base_pub = PathBuf::from(format!("{}.pub", base_key.display()));
        let target_pub = PathBuf::from(format!("{}.pub", target_key.display()));
        if base_pub.exists() {
            fs::copy(&base_pub, &target_pub).context("copying SSH pubkey")?;
        }
    }

    println!("Disk image created: {}", opts.target_disk);
    println!("SSH key: {}", target_key.display());
    println!(
        "\nTo boot:  bcvk vm run --ssh-key {} {}",
        target_key.display(),
        opts.target_disk
    );
    Ok(())
}

/// Find or create a cached base disk using native mode (no podman machine).
pub fn find_or_create_base_disk_native(
    source_image: &str,
    image_digest: &str,
    install_options: &InstallOptions,
    disk_size: &str,
    install_log: &Option<String>,
) -> Result<PathBuf> {
    let cache_hash = compute_cache_hash(image_digest, source_image, install_options);
    let short_hash = cache_hash
        .strip_prefix("sha256:")
        .unwrap_or(&cache_hash)
        .chars()
        .take(16)
        .collect::<String>();

    let base_dir = base_dir();
    fs::create_dir_all(&base_dir)?;
    let base_disk_name = format!("bootc-base-{}.raw", short_hash);
    let base_disk_path = base_dir.join(&base_disk_name);

    if base_disk_path.exists() {
        debug!("checking existing base disk: {:?}", base_disk_path);
        if let Some(stored_hash) = read_xattr(&base_disk_path, CACHE_HASH_XATTR) {
            if stored_hash == cache_hash {
                info!("reusing cached base disk: {:?}", base_disk_path);
                return Ok(base_disk_path);
            }
            info!("base disk cache hash mismatch, recreating");
        } else {
            info!("base disk has no cache hash, recreating");
        }
        fs::remove_file(&base_disk_path)?;
    }

    info!("creating base disk (native mode): {:?}", base_disk_path);
    let base_disk_str = base_disk_path.to_string_lossy().to_string();

    let size_bytes = parse_size(disk_size)?;
    create_raw_disk(&base_disk_str, size_bytes)?;

    let key_path = PathBuf::from(format!("{}.key", base_disk_path.display()));
    let ssh_pubkey = generate_ssh_keypair(&key_path)?;

    if let Err(e) = run_bootc_install_in_native_vm(
        source_image,
        image_digest,
        &base_disk_path,
        &ssh_pubkey,
        install_options,
        install_log,
    ) {
        remove_file_if_exists(&base_disk_path);
        remove_file_if_exists(&key_path);
        remove_file_if_exists(&PathBuf::from(format!("{}.pub", key_path.display())));
        return Err(e);
    }

    write_xattr(&base_disk_path, CACHE_HASH_XATTR, &cache_hash)?;

    Ok(base_disk_path)
}

/// Boot a temporary native VM and run `bootc install to-disk` via virtiofs OCI layout.
fn run_bootc_install_in_native_vm(
    source_image: &str,
    snapshot_digest: &str,
    base_disk_path: &Path,
    ssh_pubkey: &str,
    install_options: &InstallOptions,
    install_log: &Option<String>,
) -> Result<()> {
    let rootfs_path = crate::run_native_macos::find_rootfs_snapshot(snapshot_digest)?;
    info!("rootfs snapshot: {}", rootfs_path.display());

    let content_store = dirs::home_dir()
        .expect("cannot determine home directory")
        .join(crate::run_native_macos::CONTAINER_APP_ROOT)
        .join("content");

    let manifest_path = content_store.join("blobs/sha256").join(snapshot_digest);
    let manifest_size = fs::metadata(&manifest_path)
        .with_context(|| {
            format!(
                "manifest not found in content store: {}",
                manifest_path.display()
            )
        })?
        .len();

    let (vmlinuz_data, initramfs_data, grub_data) =
        crate::run_native_macos::read_boot_assets(&rootfs_path)?;
    info!("read boot assets from ext4");

    let combined_initramfs =
        crate::run_native_macos::build_combined_initramfs_mem(&initramfs_data, ssh_pubkey)?;

    let work_dir = ephemeral_base_dir().join(format!("todisk-install-{}", &snapshot_digest[..8]));
    fs::create_dir_all(&work_dir)?;

    let cmdline = "root=/dev/vdb ro rootfstype=ext4 console=tty0 console=hvc0 \
                   loglevel=4 selinux=0 net.ifnames=0 systemd.journald.storage=volatile";
    let esp_disk = work_dir.join("esp.img");
    crate::run_native_macos::build_esp_disk_mem(
        &vmlinuz_data,
        &combined_initramfs,
        &grub_data,
        cmdline,
        &esp_disk,
    )?;

    let efi_var_store = work_dir.join("efi-vars");
    let serial_log = work_dir.join("serial.log");
    let gvproxy_sock = work_dir.join("gvproxy.sock");
    let services_sock = work_dir.join("gvproxy-svc.sock");
    let gvproxy_sock_str = gvproxy_sock.to_string_lossy().to_string();
    let services_sock_str = services_sock.to_string_lossy().to_string();

    info!("starting gvproxy for install VM...");
    let gvproxy_child = start_gvproxy(&gvproxy_sock_str, &services_sock_str)?;

    let mac = generate_mac();
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let vcpus = default_vcpus();
    let memory_mb = parse_memory_to_mb("4G")?;

    let vfkit_bin = find_vfkit()?;
    let content_store_str = content_store.to_string_lossy().to_string();
    let bootloader_arg = format!("efi,variable-store={},create", efi_var_store.display());
    let vfkit_args = vec![
        "--cpus".to_string(),
        vcpus.to_string(),
        "--memory".to_string(),
        memory_mb.to_string(),
        "--bootloader".to_string(),
        bootloader_arg,
        "--device".to_string(),
        format!("virtio-blk,path={}", esp_disk.display()),
        "--device".to_string(),
        format!("virtio-blk,path={},readonly", rootfs_path.display()),
        "--device".to_string(),
        format!("virtio-blk,path={}", base_disk_path.display()),
        "--device".to_string(),
        format!("virtio-fs,sharedDir={},mountTag=content", content_store_str),
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

    info!("launching install VM (virtiofs + OCI layout)...");
    let vfkit_child = Command::new(&vfkit_bin)
        .args(&vfkit_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start vfkit for install VM")?;

    let _cleanup = VmCleanup {
        vfkit_pid: vfkit_child.id(),
        gvproxy_pid: gvproxy_child.id(),
        nbd_container: None,
        nbd_port: None,
        image: source_image.to_string(),
        vm_name: format!("todisk-install-{}", &snapshot_digest[..8]),
        backend: "native".to_string(),
    };

    let ssh_port = find_available_ssh_port();
    info!("setting up SSH port forwarding (port {})...", ssh_port);
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

    let ssh_key_path = PathBuf::from(format!("{}.key", base_disk_path.display()));
    wait_for_ssh(ssh_port, &ssh_key_path, "root")?;

    let index_json = format!(
        r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:{digest}","size":{size},"platform":{{"architecture":"arm64","os":"linux"}}}}]}}"#,
        digest = snapshot_digest,
        size = manifest_size,
    );

    let script =
        generate_native_bootc_install_script(&index_json, install_options, ssh_pubkey, install_log);

    info!("running bootc install to-disk via virtiofs OCI layout...");
    let status = run_ssh_command(ssh_port, &ssh_key_path, "root", &script)?;

    if !status.success() {
        bail!("bootc install to-disk failed in native VM");
    }

    info!("bootc install completed, shutting down install VM...");
    drop(_cleanup);

    if let Err(e) = fs::remove_dir_all(&work_dir) {
        debug!("failed to clean up work dir: {}", e);
    }

    Ok(())
}

fn generate_native_bootc_install_script(
    index_json: &str,
    install_opts: &InstallOptions,
    ssh_pubkey: &str,
    install_log: &Option<String>,
) -> String {
    let bootc_args = install_opts
        .to_bootc_args()
        .iter()
        .map(|a| {
            shlex::try_quote(a)
                .unwrap_or(std::borrow::Cow::Borrowed(a))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ");

    use base64::Engine;
    let pub_key_b64 = base64::engine::general_purpose::STANDARD.encode(ssh_pubkey);

    let rust_log_line = if let Some(ref level) = install_log {
        format!(
            "export RUST_LOG={}\n",
            shlex::try_quote(level).unwrap_or(std::borrow::Cow::Borrowed(level))
        )
    } else {
        String::new()
    };

    format!(
        r#"set -euo pipefail
{rust_log}
printf '%s' '{b64}' | base64 -d > /dev/shm/bcvk-ssh-key.pub

echo "Mounting OCI content store via virtiofs..."
mkdir -p /mnt/content
mount -t virtiofs content /mnt/content

echo "Building OCI layout..."
mkdir -p /tmp/oci/blobs
ln -s /mnt/content/blobs/sha256 /tmp/oci/blobs/sha256
printf '%s' '{{"imageLayoutVersion":"1.0.0"}}' > /tmp/oci/oci-layout
cat > /tmp/oci/index.json << 'OCIEOF'
{index_json}
OCIEOF

echo "Running bootc install to-disk..."
bootc install to-disk \
  --source-imgref oci:/tmp/oci \
  --generic-image --skip-fetch-check --wipe \
  --root-ssh-authorized-keys /dev/shm/bcvk-ssh-key.pub \
  {bootc_args} /dev/vdc

rm -f /dev/shm/bcvk-ssh-key.pub
echo "Installation complete!"
"#,
        rust_log = rust_log_line,
        b64 = pub_key_b64,
        index_json = index_json,
        bootc_args = bootc_args,
    )
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_path_in_machine() {
        assert_eq!(
            resolve_path_in_machine("/tmp/test.raw"),
            "/private/tmp/test.raw"
        );
    }
}
