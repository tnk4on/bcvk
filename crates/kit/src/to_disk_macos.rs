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
use crate::run_ephemeral_macos::clear_xattr;
use crate::vm_helpers::{
    detect_machine_name, ensure_image_and_get_digest, generate_ssh_keypair, parse_disk_size,
    remove_file_if_exists,
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
    #[clap(long, default_value = "10G")]
    pub disk_size: String,
    /// Installation options (filesystem, root-size, etc.)
    #[clap(flatten)]
    pub install: InstallOptions,
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
    // macOS /tmp → /private/tmp symlink; machine内 /tmp は tmpfs なので /private/tmp を使う
    // canonicalize() が /private/tmp に解決するので通常はこの分岐に入らないが念のため
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

/// Find or create a cached base disk for the given image + install options.
pub fn find_or_create_base_disk(
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
