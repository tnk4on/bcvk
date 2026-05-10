mod dir_walk;
mod erofs;
mod gpt;
mod regions;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Mutex;

use regions::Region;

static PLUGIN_STATE: Mutex<Option<PluginState>> = Mutex::new(None);

struct PluginState {
    dir: PathBuf,
    esp_path: Option<PathBuf>,
    regions: Vec<Region>,
    total_size: u64,
}

// --- nbdkit C FFI ---

extern "C" {
    fn nbdkit_error(fmt: *const c_char, ...);
}

fn log_error(msg: &str) {
    let c = CString::new(msg).unwrap_or_default();
    unsafe { nbdkit_error(b"%s\0".as_ptr() as *const c_char, c.as_ptr()) };
}

// --- Plugin callbacks ---

#[no_mangle]
pub extern "C" fn plugin_config(key: *const c_char, value: *const c_char) -> c_int {
    let key = unsafe { CStr::from_ptr(key) }.to_str().unwrap_or("");
    let value = unsafe { CStr::from_ptr(value) }.to_str().unwrap_or("");

    let mut state = PLUGIN_STATE.lock().unwrap();
    let state = state.get_or_insert_with(|| PluginState {
        dir: PathBuf::new(),
        esp_path: None,
        regions: Vec::new(),
        total_size: 0,
    });

    match key {
        "dir" => state.dir = PathBuf::from(value),
        "esp" => state.esp_path = Some(PathBuf::from(value)),
        _ => {
            log_error(&format!("unknown parameter: {}", key));
            return -1;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn plugin_config_complete() -> c_int {
    let state = PLUGIN_STATE.lock().unwrap();
    let state = match state.as_ref() {
        Some(s) => s,
        None => {
            log_error("dir parameter is required");
            return -1;
        }
    };

    if state.dir.as_os_str().is_empty() {
        log_error("dir parameter is required");
        return -1;
    }

    0
}

#[no_mangle]
pub extern "C" fn plugin_get_ready() -> c_int {
    let mut state_guard = PLUGIN_STATE.lock().unwrap();
    let state = match state_guard.as_mut() {
        Some(s) => s,
        None => return -1,
    };

    // Walk directory
    let walk = match dir_walk::walk_directory(&state.dir) {
        Ok(w) => w,
        Err(e) => {
            log_error(&format!("failed to walk directory: {}", e));
            return -1;
        }
    };

    // Build EROFS layout
    let erofs_layout = match erofs::build_erofs(&walk) {
        Ok(l) => l,
        Err(e) => {
            log_error(&format!("failed to build EROFS: {}", e));
            return -1;
        }
    };

    // Build regions
    let erofs_regions = erofs::build_erofs_regions(&erofs_layout, &walk.files);

    if let Some(esp_path) = &state.esp_path {
        // GPT + ESP + EROFS disk
        let esp_size = match std::fs::metadata(esp_path) {
            Ok(m) => m.len(),
            Err(e) => {
                log_error(&format!("failed to read ESP: {}", e));
                return -1;
            }
        };

        match gpt::build_gpt_disk(esp_path, esp_size, erofs_regions, erofs_layout.total_size) {
            Ok(disk) => {
                state.regions = disk.regions;
                state.total_size = disk.total_size;
            }
            Err(e) => {
                log_error(&format!("failed to build GPT disk: {}", e));
                return -1;
            }
        }
    } else {
        // EROFS only (no GPT)
        state.total_size = erofs_layout.total_size;
        state.regions = erofs_regions;
    }

    0
}

#[no_mangle]
pub extern "C" fn plugin_open(_readonly: c_int) -> *mut c_void {
    // Return non-null handle (we use global state)
    1 as *mut c_void
}

#[no_mangle]
pub extern "C" fn plugin_close(_handle: *mut c_void) {}

#[no_mangle]
pub extern "C" fn plugin_get_size(_handle: *mut c_void) -> i64 {
    let state = PLUGIN_STATE.lock().unwrap();
    state.as_ref().map(|s| s.total_size as i64).unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn plugin_can_multi_conn(_handle: *mut c_void) -> c_int {
    1 // safe: read-only, deterministic
}

#[no_mangle]
pub extern "C" fn plugin_pread(
    _handle: *mut c_void,
    buf: *mut c_void,
    count: u32,
    offset: u64,
    _flags: u32,
) -> c_int {
    let state = PLUGIN_STATE.lock().unwrap();
    let state = match state.as_ref() {
        Some(s) => s,
        None => return -1,
    };

    let buf = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count as usize) };

    match regions::pread(&state.regions, buf, offset) {
        Ok(()) => 0,
        Err(e) => {
            log_error(&format!("pread error at offset {}: {}", offset, e));
            -1
        }
    }
}

// --- Plugin registration ---

#[repr(C)]
pub struct NbdkitPlugin {
    _struct_size: u64,
    _api_version: c_int,
    _min_thread_model: c_int,
    name: *const c_char,
    longname: *const c_char,
    version: *const c_char,
    description: *const c_char,
    load: Option<extern "C" fn()>,
    unload: Option<extern "C" fn()>,
    config: Option<extern "C" fn(*const c_char, *const c_char) -> c_int>,
    config_complete: Option<extern "C" fn() -> c_int>,
    config_help: *const c_char,
    open: Option<extern "C" fn(c_int) -> *mut c_void>,
    close: Option<extern "C" fn(*mut c_void)>,
    get_size: Option<extern "C" fn(*mut c_void) -> i64>,
    can_write: Option<extern "C" fn(*mut c_void) -> c_int>,
    can_flush: Option<extern "C" fn(*mut c_void) -> c_int>,
    is_rotational: Option<extern "C" fn(*mut c_void) -> c_int>,
    can_trim: Option<extern "C" fn(*mut c_void) -> c_int>,
    pread: Option<extern "C" fn(*mut c_void, *mut c_void, u32, u64, u32) -> c_int>,
    pwrite: Option<extern "C" fn(*mut c_void, *const c_void, u32, u64, u32) -> c_int>,
    flush: Option<extern "C" fn(*mut c_void, u32) -> c_int>,
    trim: Option<extern "C" fn(*mut c_void, u32, u64, u32) -> c_int>,
    zero: Option<extern "C" fn(*mut c_void, u32, u64, u32) -> c_int>,
    magic_config_key: *const c_char,
    can_multi_conn: Option<extern "C" fn(*mut c_void) -> c_int>,
    // ... more fields can be added but must be NULL
    can_extents: Option<extern "C" fn(*mut c_void) -> c_int>,
    extents: Option<extern "C" fn(*mut c_void, u32, u64, u32, *mut c_void) -> c_int>,
    can_cache: Option<extern "C" fn(*mut c_void) -> c_int>,
    cache: Option<extern "C" fn(*mut c_void, u32, u64, u32) -> c_int>,
    thread_model: Option<extern "C" fn() -> c_int>,
    can_fua: Option<extern "C" fn(*mut c_void) -> c_int>,
    get_ready: Option<extern "C" fn() -> c_int>,
    // after_fork, cleanup, preconnect not needed
}

unsafe impl Sync for NbdkitPlugin {}

static PLUGIN_NAME: &[u8] = b"erofs\0";
static PLUGIN_LONGNAME: &[u8] = b"nbdkit EROFS plugin\0";
static PLUGIN_VERSION: &[u8] = b"0.1.0\0";
static PLUGIN_DESCRIPTION: &[u8] = b"Create virtual EROFS disk from directory\0";
static PLUGIN_CONFIG_HELP: &[u8] = b"dir=<DIRECTORY>  (required) The directory to serve\nesp=<FILE>       Optional ESP image for EFI boot\0";
static PLUGIN_MAGIC_KEY: &[u8] = b"dir\0";

static PLUGIN: NbdkitPlugin = NbdkitPlugin {
    _struct_size: std::mem::size_of::<NbdkitPlugin>() as u64,
    _api_version: 2,
    _min_thread_model: 0,
    name: PLUGIN_NAME.as_ptr() as *const c_char,
    longname: PLUGIN_LONGNAME.as_ptr() as *const c_char,
    version: PLUGIN_VERSION.as_ptr() as *const c_char,
    description: PLUGIN_DESCRIPTION.as_ptr() as *const c_char,
    load: None,
    unload: None,
    config: Some(plugin_config),
    config_complete: Some(plugin_config_complete),
    config_help: PLUGIN_CONFIG_HELP.as_ptr() as *const c_char,
    open: Some(plugin_open),
    close: Some(plugin_close),
    get_size: Some(plugin_get_size),
    can_write: None,
    can_flush: None,
    is_rotational: None,
    can_trim: None,
    pread: Some(plugin_pread),
    pwrite: None,
    flush: None,
    trim: None,
    zero: None,
    magic_config_key: PLUGIN_MAGIC_KEY.as_ptr() as *const c_char,
    can_multi_conn: Some(plugin_can_multi_conn),
    can_extents: None,
    extents: None,
    can_cache: None,
    cache: None,
    thread_model: None,
    can_fua: None,
    get_ready: Some(plugin_get_ready),
};

#[no_mangle]
pub extern "C" fn plugin_init() -> *const NbdkitPlugin {
    &PLUGIN
}
