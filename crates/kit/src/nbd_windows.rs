//! NBD server management for Windows ephemeral VMs.
//!
//! Deploys bcvk-nbd binary to podman machine, runs as systemd transient
//! unit. No container or gvproxy needed — vsock relay connects directly
//! via VM GUID.

use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use std::process::{Command, Stdio};
use tracing::info;

use crate::vm_helpers::detect_machine_name;

/// NBD server binary (statically linked x86_64 ELF), embedded at compile time.
const NBD_SERVER: &[u8] = include_bytes!("../bcvk-nbd-x86_64");

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sha256_hex(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    let hash1 = h.finish();
    data.len().hash(&mut h);
    let hash2 = h.finish();
    format!("{:016x}{:016x}", hash1, hash2)
}

/// Deploy the NBD server binary to the podman machine (idempotent, hash-checked).
pub(crate) fn deploy_nbd_server(ps: &crate::hyperv::boot_files::PodmanSsh) -> Result<()> {
    use base64::Engine;
    let hash = sha256_hex(NBD_SERVER);

    // Check hash first via ssh_cmd (no stdin) to avoid pipe deadlock.
    // ssh_cmd_stdin sends the full 1.1MB base64 script to stdin;
    // if the remote exits early (hash hit), the write blocks forever
    // because the pipe buffer (64KB) fills up with unread data.
    let existing = ps.ssh_cmd("cat /var/tmp/bcvk/bcvk-nbd.sha256 2>/dev/null || true");
    if let Ok(out) = existing {
        if String::from_utf8_lossy(&out).trim() == hash {
            tracing::debug!("nbd server binary already deployed (hash match)");
            return Ok(());
        }
    }

    info!("deploying nbd server to podman machine...");
    let b64 = base64::engine::general_purpose::STANDARD.encode(NBD_SERVER);
    let script = format!(
        "set -e; \
         mkdir -p /var/tmp/bcvk; \
         printf '%s' '{b64}' | base64 -d > /var/tmp/bcvk/bcvk-nbd; \
         chmod +x /var/tmp/bcvk/bcvk-nbd; \
         chcon -t bin_t /var/tmp/bcvk/bcvk-nbd 2>/dev/null || true; \
         printf '{hash}' > /var/tmp/bcvk/bcvk-nbd.sha256",
        hash = hash,
        b64 = b64,
    );
    ps.ssh_cmd_stdin(&script)?;
    Ok(())
}

/// Start the NBD server as a systemd transient unit (vsock mode).
/// Waits for vsock bind completion via journalctl before returning.
pub(crate) fn start_nbd_server(
    ps: &crate::hyperv::boot_files::PodmanSsh,
    merged_path: &str,
    cmdline: &str,
    ssh_pubkey: &str,
    vsock_port: u32,
    vm_name: &str,
) -> Result<String> {
    let unit_name = format!("bcvk-nbd-{}", vm_name);
    let cmdline_esc = shell_escape(cmdline);
    let mut ssh_args = String::new();
    if !ssh_pubkey.is_empty() {
        ssh_args = format!(" --ssh-pubkey {}", shell_escape(ssh_pubkey));
    }

    let script = format!(
        "set -e\n\
         systemctl stop {u} 2>/dev/null || true\n\
         systemctl reset-failed {u} 2>/dev/null || true\n\
         fallocate -l 512M /dev/shm/.bcvk-mem-reserve 2>/dev/null && rm -f /dev/shm/.bcvk-mem-reserve || true\n\
         systemd-run --unit={u} --service-type=simple --quiet \
           --property=LimitNOFILE=524288 \
           /var/tmp/bcvk/bcvk-nbd --vsock --port {port} \
           --dir {merged} --cmdline {cmdline}{ssh}\n\
         for i in $(seq 1 120); do\n\
           if journalctl -u {u} --no-pager -n 10 2>/dev/null | grep -q 'listening on vsock'; then exit 0; fi\n\
           sleep 1\n\
         done\n\
         echo 'nbd vsock bind timeout (120s)' >&2\n\
         exit 1",
        u = unit_name,
        port = vsock_port,
        merged = merged_path,
        cmdline = cmdline_esc,
        ssh = ssh_args,
    );
    ps.ssh_cmd_stdin(&script)?;

    info!(
        "nbd server started: {} (vsock port {})",
        unit_name, vsock_port
    );
    Ok(unit_name)
}

/// Stop an NBD server systemd unit (best-effort).
pub(crate) fn stop_nbd_server(unit_name: &str) {
    if let Ok(machine) = detect_machine_name() {
        let _ = machine_ssh(
            &machine,
            &format!(
                "systemctl stop {u} 2>/dev/null; systemctl reset-failed {u} 2>/dev/null",
                u = unit_name
            ),
        );
    }
}

/// Sweep orphaned bcvk-nbd systemd units (best-effort).
pub(crate) fn sweep_orphaned_nbd_units() {
    if let Ok(machine) = detect_machine_name() {
        let _ = machine_ssh(
            &machine,
            "for u in $(systemctl list-units 'bcvk-nbd-*' --no-legend --plain | awk '{print $1}'); do \
               systemctl stop \"$u\" 2>/dev/null; \
               systemctl reset-failed \"$u\" 2>/dev/null; \
             done",
        );
    }
}

fn machine_ssh(machine: &str, cmd: &str) -> Result<()> {
    let status = Command::new("podman")
        .args(["machine", "ssh", machine, "--", cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("podman machine ssh")?;
    if !status.success() {
        bail!("machine ssh command failed");
    }
    Ok(())
}
