//! Native mode ephemeral VM launch for macOS.
//!
//! Uses apple/container CLI for image management and vfkit with direct
//! kernel boot (no podman machine, no NBD, no GRUB).

use color_eyre::Result;

use crate::run_ephemeral_macos::RunEphemeralOpts;

/// Run an ephemeral VM using apple/container CLI and vfkit direct kernel boot.
pub fn run(_opts: RunEphemeralOpts) -> Result<()> {
    todo!("native mode not yet implemented")
}
