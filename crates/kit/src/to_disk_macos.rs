//! Install bootc images to disk on macOS using loopback devices via podman machine.
//!
//! Uses losetup inside podman machine to create loop devices from raw disk files
//! accessible via virtiofs, then runs `bootc install to-disk` targeting the loop device.
//! Base disk caching with APFS clonefile (`cp -c`) provides fast VM creation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;
use color_eyre::eyre::{bail, Context};
use color_eyre::Result;
use tracing::{debug, info};

use crate::install_options::InstallOptions;
use crate::run_ephemeral_macos::{clear_xattr, detect_machine_name, ensure_image_and_get_digest};
use sha2::{Digest, Sha256};

fn remove_file_if_exists(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!("failed to remove {}: {}", path.display(), e);
        }
    }
}

/// Options for `bcvk to-disk` on macOS.
#[derive(Parser, Debug)]
pub struct ToDiskMacosOpts {
    /// Container image to install
    pub source_image: String,
    /// Target disk path (output .raw file)
    pub target_disk: String,
    /// Disk size (e.g. "10G", "5120M", or plain number for bytes)
    #[clap(long, default_value = "10G")]
    pub disk_size: String,
    /// Installation options (filesystem, root-size, etc.)
    #[clap(flatten)]
    pub install: InstallOptions,
}

/// Options for `bcvk run` on macOS (to-disk + vm run).
#[derive(Parser, Debug)]
pub struct RunFromImageOpts {
    /// Container image to run as a persistent VM
    pub image: String,
    /// VM name (auto-generated from image name if not specified)
    #[clap(long)]
    pub name: Option<String>,
    /// Disk size (e.g. "10G", "5120M")
    #[clap(long, default_value = "10G")]
    pub disk_size: String,
    /// Number of vCPUs
    #[clap(long)]
    pub vcpus: Option<u32>,
    /// Memory size (e.g. "4G", "2048M", or plain number for MB)
    #[clap(long, default_value = "4G")]
    pub memory: String,
    /// Installation options (filesystem, root-size, etc.)
    #[clap(flatten)]
    pub install: InstallOptions,
    /// Display VM console in GUI window
    #[clap(long)]
    pub gui: bool,
    /// Replace existing VM with same name
    #[clap(long, short = 'R')]
    pub replace: bool,
}

fn base_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".local/share/bcvk/base")
}

fn vms_dir() -> PathBuf {
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
    // macOS /tmp → /private/tmp symlink; machine内 /tmp は tmpfs なので /private/tmp を使う
    // canonicalize() が /private/tmp に解決するので通常はこの分岐に入らないが念のため
    if resolved.starts_with("/tmp/") {
        format!("/private{}", resolved)
    } else {
        resolved
    }
}

fn parse_disk_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('G').or(s.strip_suffix('g')) {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M').or(s.strip_suffix('m')) {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s.strip_suffix('K').or(s.strip_suffix('k')) {
        (n, 1024u64)
    } else {
        bail!("invalid disk size format: '{}' (use e.g. 10G, 5120M)", s);
    };
    let num: u64 = num_str
        .trim()
        .parse()
        .with_context(|| format!("invalid disk size number: '{}'", num_str))?;
    Ok(num * multiplier)
}

fn create_raw_disk(path: &str, size_bytes: u64) -> Result<()> {
    let file = fs::File::create(path).with_context(|| format!("creating {}", path))?;
    file.set_len(size_bytes)
        .with_context(|| format!("setting size {} on {}", size_bytes, path))?;
    drop(file);
    clear_xattr(Path::new(path));
    Ok(())
}

fn generate_ssh_keypair(key_path: &Path) -> Result<String> {
    // ssh-keygen creates {key_path} and {key_path}.pub
    let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
    remove_file_if_exists(key_path);
    remove_file_if_exists(&pub_path);
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &key_path.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .status()
        .context("ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }
    let pubkey = fs::read_to_string(&pub_path)
        .with_context(|| format!("reading public key: {}", pub_path.display()))?
        .trim()
        .to_string();
    Ok(pubkey)
}

fn generate_bootc_install_script(
    disk_path_in_machine: &str,
    image: &str,
    install_opts: &InstallOptions,
    ssh_pubkey: &str,
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

    let pubkey_escaped = ssh_pubkey.replace('\'', "'\\''");

    format!(
        r#"set -euo pipefail
LOOP=$(sudo losetup -fP --show {disk_path})
echo "Loop device: $LOOP"
trap 'sudo losetup -d $LOOP 2>/dev/null' EXIT

echo "Running bootc install to-disk..."
podman run --rm --privileged --pid=host --net=none \
  -v /dev:/dev \
  -v /var/lib/containers:/var/lib/containers \
  {image} bootc install to-disk \
  --generic-image --skip-fetch-check --wipe \
  {bootc_args} $LOOP

echo "Injecting SSH key..."
PARTS=$(lsblk -nlo NAME "$LOOP" | tail -n +2)
ROOT_PART=""
for p in $PARTS; do
  LABEL=$(lsblk -nlo PARTLABEL "/dev/$p" 2>/dev/null || true)
  if [ "$LABEL" = "root" ]; then
    ROOT_PART="/dev/$p"
    break
  fi
done
if [ -z "$ROOT_PART" ]; then
  ROOT_PART="/dev/$(echo "$PARTS" | tail -1)"
fi

mkdir -p /tmp/bcvk-mnt
sudo mount "$ROOT_PART" /tmp/bcvk-mnt

# bootc/ostree layout: /root → var/roothome (symlink)
# SSH key goes into ostree/deploy/<osname>/var/roothome/.ssh/
OSNAME=$(ls /tmp/bcvk-mnt/ostree/deploy/ 2>/dev/null | head -1)
if [ -n "$OSNAME" ]; then
  SSH_DIR="/tmp/bcvk-mnt/ostree/deploy/$OSNAME/var/roothome/.ssh"
else
  SSH_DIR="/tmp/bcvk-mnt/root/.ssh"
fi

sudo mkdir -p "$(dirname "$SSH_DIR")"
sudo chmod 700 "$(dirname "$SSH_DIR")"
sudo mkdir -p "$SSH_DIR"
sudo chmod 700 "$SSH_DIR"
echo '{pubkey}' | sudo tee "$SSH_DIR/authorized_keys" > /dev/null
sudo chmod 600 "$SSH_DIR/authorized_keys"
echo "SSH key injected to $SSH_DIR"
sudo umount /tmp/bcvk-mnt

echo "Installation complete!"
"#,
        disk_path = disk_path_in_machine,
        image = image_quoted,
        bootc_args = bootc_args,
        pubkey = pubkey_escaped,
    )
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

fn find_or_create_base_disk(
    source_image: &str,
    image_digest: &str,
    install_options: &InstallOptions,
    disk_size: &str,
    machine: &str,
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

    let size_bytes = parse_disk_size(disk_size)?;
    create_raw_disk(&base_disk_str, size_bytes)?;

    let key_path = PathBuf::from(format!("{}.key", base_disk_path.display()));
    let ssh_pubkey = generate_ssh_keypair(&key_path)?;

    let disk_in_machine = resolve_path_in_machine(&base_disk_str);
    let script =
        generate_bootc_install_script(&disk_in_machine, source_image, install_options, &ssh_pubkey);

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

fn clone_base_disk(base_path: &Path, vm_disk_path: &Path) -> Result<()> {
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
    let machine = detect_machine_name()?;
    let digest = ensure_image_and_get_digest(&opts.source_image)?;
    info!("image digest: {}...", &digest[..16.min(digest.len())]);

    let base_disk_path = find_or_create_base_disk(
        &opts.source_image,
        &digest,
        &opts.install,
        &opts.disk_size,
        &machine,
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

fn sanitize_vm_name(image: &str) -> String {
    image
        .split('/')
        .last()
        .unwrap_or(image)
        .replace(':', "-")
        .replace('.', "-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Execute `bcvk run` on macOS (to-disk + vm run).
pub fn run_from_image(opts: RunFromImageOpts) -> Result<()> {
    let vm_name = opts
        .name
        .clone()
        .unwrap_or_else(|| sanitize_vm_name(&opts.image));

    if vm_name.is_empty() {
        bail!("could not derive VM name from image. Use --name to specify one.");
    }

    // Check if VM already exists
    if let Ok(existing) = crate::vfkit::VmMetadata::load(&vm_name) {
        if opts.replace {
            info!("replacing existing VM '{}'", vm_name);
            if existing.is_alive() {
                if let Err(e) = Command::new("kill")
                    .arg(existing.vfkit_pid.to_string())
                    .status()
                {
                    tracing::warn!("failed to kill vfkit (pid {}): {}", existing.vfkit_pid, e);
                }
                if let Err(e) = Command::new("kill")
                    .arg(existing.gvproxy_pid.to_string())
                    .status()
                {
                    tracing::warn!(
                        "failed to kill gvproxy (pid {}): {}",
                        existing.gvproxy_pid,
                        e
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            crate::vfkit::VmMetadata::remove(&vm_name);
        } else {
            bail!(
                "VM '{}' already exists. Use --replace to overwrite, or --name to choose a different name.",
                vm_name
            );
        }
    }

    let vms_dir = vms_dir();
    fs::create_dir_all(&vms_dir)?;
    let disk_path = vms_dir.join(format!("{}.raw", vm_name));
    let key_path = PathBuf::from(format!("{}.key", disk_path.display()));
    let key_pub_path = PathBuf::from(format!("{}.pub", key_path.display()));

    // Remove old disk if replacing
    if opts.replace {
        remove_file_if_exists(&disk_path);
        remove_file_if_exists(&key_path);
        remove_file_if_exists(&key_pub_path);
    }

    if !disk_path.exists() {
        info!("creating disk image for VM '{}'...", vm_name);
        let machine = detect_machine_name()?;
        let digest = ensure_image_and_get_digest(&opts.image)?;

        let base_disk_path = find_or_create_base_disk(
            &opts.image,
            &digest,
            &opts.install,
            &opts.disk_size,
            &machine,
        )?;

        clone_base_disk(&base_disk_path, &disk_path)?;

        // Copy SSH key from base
        let base_key = PathBuf::from(format!("{}.key", base_disk_path.display()));
        if base_key.exists() {
            fs::copy(&base_key, &key_path).context("copying SSH key")?;
            let base_pub = PathBuf::from(format!("{}.pub", base_key.display()));
            if base_pub.exists() {
                fs::copy(&base_pub, &key_pub_path)?;
            }
        }
    }

    info!("starting VM '{}' from {}...", vm_name, disk_path.display());
    let vm_opts = crate::vfkit::run::VmRunOpts {
        disk: disk_path.to_string_lossy().to_string(),
        name: Some(vm_name),
        vcpus: opts.vcpus,
        memory: opts.memory,
        ssh_key: if key_path.exists() {
            Some(key_path.to_string_lossy().to_string())
        } else {
            None
        },
        ssh_user: "root".to_string(),
        ssh_port: None,
        gui: opts.gui,
    };
    crate::vfkit::run::run(vm_opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_disk_size() {
        assert_eq!(parse_disk_size("10G").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_disk_size("5120M").unwrap(), 5120 * 1024 * 1024);
        assert_eq!(parse_disk_size("1024K").unwrap(), 1024 * 1024);
        assert_eq!(parse_disk_size("1073741824").unwrap(), 1073741824);
        assert!(parse_disk_size("abc").is_err());
        assert!(parse_disk_size("10X").is_err());
    }

    #[test]
    fn test_resolve_path_in_machine() {
        assert_eq!(
            resolve_path_in_machine("/tmp/test.raw"),
            "/private/tmp/test.raw"
        );
    }

    #[test]
    fn test_sanitize_vm_name() {
        assert_eq!(
            sanitize_vm_name("quay.io/fedora/fedora-bootc:latest"),
            "fedora-bootc-latest"
        );
        assert_eq!(
            sanitize_vm_name("centos-bootc:stream10"),
            "centos-bootc-stream10"
        );
        assert_eq!(sanitize_vm_name("simple"), "simple");
    }
}
