//! Region-based virtual block device composition.
//! Inspired by the regions pattern in nbdkit's floppy plugin (BSD-3-Clause).

use std::path::PathBuf;
use std::sync::Arc;

/// Memory-mapped file region (Send+Sync safe via immutable read-only access).
pub struct MmapRegion {
    ptr: *const u8,
    len: usize,
    path: PathBuf,
}

impl MmapRegion {
    pub fn new(file: &std::fs::File, len: usize, path: PathBuf) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(MmapRegion {
            ptr: ptr as *const u8,
            len,
            path,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl std::fmt::Debug for MmapRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MmapRegion({}, {} bytes)", self.path.display(), self.len)
    }
}

#[derive(Debug)]
pub enum RegionType {
    Data(Arc<Vec<u8>>),
    File {
        path: PathBuf,
        handle: Arc<std::fs::File>,
    },
    Mmap(Arc<MmapRegion>),
    Zero,
}

#[derive(Debug)]
pub struct Region {
    pub start: u64,
    pub len: u64,
    pub region_type: RegionType,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

pub fn find_region(regions: &[Region], offset: u64) -> Option<&Region> {
    regions
        .binary_search_by(|r| {
            if offset < r.start {
                std::cmp::Ordering::Greater
            } else if offset >= r.end() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
        .map(|i| &regions[i])
}

pub fn pread(regions: &[Region], buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut remaining = buf.len();
    let mut buf_offset = 0;
    let mut disk_offset = offset;

    while remaining > 0 {
        let region = find_region(regions, disk_offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("offset {} outside disk", disk_offset),
            )
        })?;

        let region_offset = disk_offset - region.start;
        let avail = (region.len - region_offset) as usize;
        let len = remaining.min(avail);

        match &region.region_type {
            RegionType::Data(data) => {
                let start = region_offset as usize;
                buf[buf_offset..buf_offset + len].copy_from_slice(&data[start..start + len]);
            }
            RegionType::File { handle, .. } => {
                use std::os::unix::fs::FileExt;
                handle.read_exact_at(&mut buf[buf_offset..buf_offset + len], region_offset)?;
            }
            RegionType::Mmap(mmap) => {
                let start = region_offset as usize;
                let src = &mmap.as_slice()[start..start + len];
                buf[buf_offset..buf_offset + len].copy_from_slice(src);
            }
            RegionType::Zero => {
                buf[buf_offset..buf_offset + len].fill(0);
            }
        }

        remaining -= len;
        buf_offset += len;
        disk_offset += len as u64;
    }

    Ok(())
}
