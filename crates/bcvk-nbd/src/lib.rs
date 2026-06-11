//! bcvk-nbd library — ESP/GPT disk image composition.
//!
//! Provides FAT32 ESP and GPT partition table builders using a region-based
//! virtual block device model. Used by bcvk-nbd (NBD server) and bcvk native
//! mode (file-based ESP images).

pub mod fat32;
pub mod gpt;
pub mod initramfs;
pub mod regions;
