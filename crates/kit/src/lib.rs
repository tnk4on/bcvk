//! bcvk library - exposes internal modules for testing

pub mod cpio;
pub mod kernel_cmdline;
pub mod qemu_img;
pub mod ssh_options;
pub mod xml_utils;

// Linux-only modules
#[cfg(target_os = "linux")]
pub mod kernel;

// Windows-only modules (Hyper-V + NBD backend)
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod ephemeral_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod hyperv;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod install_options;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod instancetypes;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod nbd_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod run_ephemeral_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod run_native_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod to_disk_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod vm_helpers;
#[cfg(target_os = "windows")]
#[allow(missing_docs, unsafe_code)]
pub mod wslc_com;
