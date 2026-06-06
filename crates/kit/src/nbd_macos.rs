//! NBD server management for macOS ephemeral VMs.
//!
//! Replaces nbdkit_macos.rs: uses a statically-linked bcvk-nbd binary
//! instead of nbdkit + C plugin. The binary is deployed to the podman
//! machine via base64/SSH and run inside a thin container for TCP port
//! forwarding (podman -p).

use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::info;

use crate::vm_helpers::detect_machine_name;

/// NBD server binary (statically linked aarch64 ELF), embedded at compile time.
const NBD_SERVER: &[u8] = include_bytes!("../bcvk-nbd-aarch64");

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

/// Get the merged overlay path from podman image mount.
pub(crate) fn get_merged_path(machine: &str, rootful: bool, image: &str) -> Result<String> {
    let output = if rootful {
        Command::new("podman")
            .args([
                "machine", "ssh", machine, "--", "podman", "image", "mount", image,
            ])
            .output()
            .context("podman image mount")?
    } else {
        Command::new("podman")
            .args([
                "machine", "ssh", machine, "--", "podman", "unshare", "podman", "image", "mount",
                image,
            ])
            .output()
            .context("podman image mount")?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("podman image mount failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Deploy the NBD server binary to the podman machine (idempotent, hash-checked).
pub(crate) fn deploy_nbd_server(machine: &str) -> Result<()> {
    use base64::Engine;
    let hash = sha256_hex(NBD_SERVER);
    let b64 = base64::engine::general_purpose::STANDARD.encode(NBD_SERVER);
    let script = format!(
        "set -e; \
         mkdir -p /var/tmp/bcvk; \
         H=/var/tmp/bcvk/bcvk-nbd.sha256; \
         if [ -f \"$H\" ] && [ \"$(cat \"$H\")\" = '{hash}' ]; then exit 0; fi; \
         printf '%s' '{b64}' | base64 -d > /var/tmp/bcvk/bcvk-nbd; \
         chmod +x /var/tmp/bcvk/bcvk-nbd; \
         printf '{hash}' > \"$H\"",
        hash = hash,
        b64 = b64,
    );
    info!("deploying nbd server to podman machine...");
    let mut child = Command::new("podman")
        .args(["machine", "ssh", machine, "--", "bash", "-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("nbd server deploy to podman machine")?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(script.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "nbd server deploy failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Start the NBD server in a thin container (for TCP port forwarding via -p).
#[allow(dead_code)]
pub(crate) fn start_nbd_server(
    machine: &str,
    merged_path: &str,
    cmdline: &str,
    ssh_pubkey: &str,
    nbd_port: u16,
    vm_name: &str,
) -> Result<String> {
    let container_name = format!("bcvk-nbd-{}", vm_name);

    let _ = Command::new("podman")
        .args([
            "machine",
            "ssh",
            machine,
            "--",
            "podman",
            "rm",
            "-f",
            &container_name,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let cmdline_esc = shell_escape(cmdline);
    let mut ssh_args = String::new();
    if !ssh_pubkey.is_empty() {
        ssh_args = format!(" --ssh-pubkey {}", shell_escape(ssh_pubkey));
    }

    let podman_cmd = format!(
        "podman run -d --name {name} --security-opt label=disable \
         -p {port}:10809 \
         -v {merged}:{merged}:ro \
         -v /var/tmp/bcvk/bcvk-nbd:/nbd:ro \
         --entrypoint /nbd \
         quay.io/fedora/fedora-minimal:latest \
         --dir {merged} --port 10809 \
         --cmdline {cmdline}{ssh}",
        name = container_name,
        port = nbd_port,
        merged = merged_path,
        cmdline = cmdline_esc,
        ssh = ssh_args,
    );

    let output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &podman_cmd])
        .output()
        .context("failed to start nbd server")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to start nbd server: {}", stderr.trim());
    }

    info!("waiting for nbd server on port {}...", nbd_port);
    loop {
        if let Ok(mut stream) = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], nbd_port)),
            Duration::from_millis(500),
        ) {
            use std::io::Read;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 8];
            if stream.read_exact(&mut buf).is_ok() && &buf == b"NBDMAGIC" {
                break;
            }
        }
        let ps_output = Command::new("podman")
            .args([
                "machine",
                "ssh",
                machine,
                "--",
                "podman",
                "ps",
                "-a",
                "--filter",
                &format!("name=^{}$", container_name),
                "--format",
                "{{.Status}}",
            ])
            .output();
        if let Ok(out) = &ps_output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains("Exited") {
                let _ = Command::new("podman")
                    .args([
                        "machine",
                        "ssh",
                        machine,
                        "--",
                        "podman",
                        "rm",
                        "-f",
                        &container_name,
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                bail!(
                    "nbd server container '{}' exited before becoming ready on port {}",
                    container_name,
                    nbd_port
                );
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    Ok(container_name)
}

/// Find an available TCP port for NBD in range 10800-10900.
pub fn find_available_nbd_port() -> u16 {
    use rand::Rng;
    let mut rng = rand::rng();
    const PORT_RANGE_START: u16 = 10800;
    const PORT_RANGE_END: u16 = 10900;
    for _ in 0..100 {
        let port = rng.random_range(PORT_RANGE_START..PORT_RANGE_END);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    for port in PORT_RANGE_START..PORT_RANGE_END {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    PORT_RANGE_START
}

/// Stop and remove an NBD server container (best-effort).
pub fn stop_nbd_container(container_name: &str) {
    if let Ok(machine) = detect_machine_name() {
        let _ = Command::new("podman")
            .args([
                "machine",
                "ssh",
                &machine,
                "--",
                "podman",
                "rm",
                "-f",
                container_name,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
