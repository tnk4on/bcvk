//! wslc COM API bindings for Windows-native mode.
//!
//! Provides direct COM access to WSL Containers (wslc) without using the CLI.
//! Interface definitions are derived from refs/WSL/src/windows/service/inc/wslc.idl.
//!
//! Uses manual vtable definitions because the `#[interface]` proc macro in
//! the `windows` crate requires `windows_core` at the crate root, which
//! conflicts with our workspace setup.

#![allow(unsafe_code)]
#![allow(non_snake_case)]
#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

use color_eyre::eyre::Context;
use color_eyre::Result;
use std::ffi::c_void;
use tracing::{debug, info};

use windows::core::{Interface, GUID, HRESULT, PCSTR};
use windows::Win32::System::Com::*;

// ── CLSID / IID ────────────────────────────────────────────────────

const CLSID_WSLC_SESSION_MANAGER: GUID =
    GUID::from_u128(0xa9b7a1b9_0671_405c_95f1_e0612cb4ce8f);

const IID_IWSLC_SESSION_MANAGER: GUID =
    GUID::from_u128(0x82A7ABC8_6B50_43FC_AB96_15FBBE7E8760);

const IID_IWSLC_SESSION: GUID =
    GUID::from_u128(0xEF0661E4_6364_40EA_B433_E2FDF11F3519);

const IID_IWSLC_CONTAINER: GUID =
    GUID::from_u128(0x7577FE8D_DE85_471E_B870_11669986F332);

// ── WSLCHandle (discriminated union for file handle transfer) ──────

#[repr(C)]
#[derive(Clone, Copy)]
struct WSLCHandle {
    handle_type: u32,
    handle: isize, // HANDLE as isize for COM marshalling
}

const WSLC_HANDLE_TYPE_FILE: u32 = 1;

// WSLCSessionFlags
const WSLC_SESSION_FLAGS_PERSISTENT: u32 = 1;
const WSLC_SESSION_FLAGS_OPEN_EXISTING: u32 = 2;

// ── Raw COM pointer wrappers ───────────────────────────────────────

/// Opaque COM pointer wrapper with Release on drop.
#[derive(Debug)]
struct ComPtr {
    ptr: *mut c_void,
}

impl ComPtr {
    fn new(ptr: *mut c_void) -> Self {
        Self { ptr }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// Get the vtable at a given offset (after IUnknown's 3 methods).
    unsafe fn vtable_fn<F>(&self, index: usize) -> F
    where
        F: Copy,
    {
        let vtable = *(self.ptr as *const *const *const c_void);
        let fn_ptr = *vtable.add(index);
        std::mem::transmute_copy(&fn_ptr)
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                // IUnknown::Release is vtable index 2
                let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                    self.vtable_fn(2);
                release(self.ptr);
            }
        }
    }
}

// ── IWSLCSessionManager vtable ─────────────────────────────────────
// vtable layout: IUnknown (0-2) + GetVersion(3) + IsClientVersionSupported(4) + CreateSession(5) + ...

/// IWSLCSessionManager wrapper.
#[derive(Debug)]
pub struct WslcSessionManager {
    com: ComPtr,
}

impl WslcSessionManager {
    /// CreateSession (vtable index 5)
    pub fn create_session(&self, flags: u32) -> Result<WslcSession> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            *const c_void, // Settings
            u32,           // Flags
            *const c_void, // WarningCallback
            *mut *mut c_void, // Session out
        ) -> HRESULT;

        let mut session_ptr: *mut c_void = std::ptr::null_mut();
        unsafe {
            let f: Fn = self.com.vtable_fn(5);
            let hr = f(
                self.com.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
                &mut session_ptr,
            );
            hr.ok().context("CreateSession failed")?;
        }
        if session_ptr.is_null() {
            color_eyre::eyre::bail!("CreateSession returned null");
        }
        let session = WslcSession { com: ComPtr::new(session_ptr) };
        configure_com_impersonation(session_ptr)?;
        Ok(session)
    }
}

// ── IWSLCSession vtable ────────────────────────────────────────────
// IUnknown (0-2) + GetId(3) + GetState(4) + PullImage(5) + BuildImage(6)
// + LoadImage(7) + ImportImage(8) + SaveImage(9) + SaveImages(10)
// + ListImages(11) + DeleteImage(12) + TagImage(13) + InspectImage(14)
// + PruneImages(15) + CreateContainer(16) + ...

/// IWSLCSession wrapper.
#[derive(Debug)]
pub struct WslcSession {
    com: ComPtr,
}

impl WslcSession {
    /// PullImage (vtable index 5)
    pub fn pull_image(&self, image: &str) -> Result<()> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            PCSTR, // Image
            PCSTR, // RegistryAuth
            *const c_void, // ProgressCallback
            *const c_void, // WarningCallback
        ) -> HRESULT;

        info!(image, "pulling image via wslc COM");
        let image_cstr = std::ffi::CString::new(image)?;
        unsafe {
            let f: Fn = self.com.vtable_fn(5);
            let hr = f(
                self.com.as_ptr(),
                PCSTR::from_raw(image_cstr.as_ptr() as *const u8),
                PCSTR::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
            hr.ok().context("PullImage failed")?;
        }
        Ok(())
    }

    /// InspectImage (vtable index 14)
    ///
    /// IDL: HRESULT InspectImage([in] LPCSTR ImageNameOrId, [out] LPSTR* Output)
    /// Note: Output is LPSTR (char*), not LPWSTR (wchar_t*).
    pub fn inspect_image(&self, image: &str) -> Result<String> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            PCSTR, // ImageNameOrId
            *mut *mut u8, // LPSTR* Output (UTF-8)
        ) -> HRESULT;

        let image_cstr = std::ffi::CString::new(image)?;
        let mut output: *mut u8 = std::ptr::null_mut();
        unsafe {
            let f: Fn = self.com.vtable_fn(14);
            let hr = f(
                self.com.as_ptr(),
                PCSTR::from_raw(image_cstr.as_ptr() as *const u8),
                &mut output,
            );
            hr.ok().context("InspectImage failed")?;
            if output.is_null() {
                color_eyre::eyre::bail!("InspectImage returned null");
            }
            let cstr = std::ffi::CStr::from_ptr(output as *const i8);
            let result = cstr.to_str().context("InspectImage output not valid UTF-8")?.to_string();
            debug!(len = result.len(), first_100 = &result[..100.min(result.len())], "InspectImage raw output");
            // Free COM-allocated memory
            windows::Win32::System::Com::CoTaskMemFree(Some(output as *const c_void));
            Ok(result)
        }
    }

    /// InspectImage and extract the short digest.
    pub fn inspect_image_digest(&self, image: &str) -> Result<String> {
        let json_str = self.inspect_image(image)?;
        let data: serde_json::Value = serde_json::from_str(&json_str)
            .context("failed to parse InspectImage JSON")?;
        // COM returns a single object; CLI wraps it in an array
        let obj = if data.is_array() {
            data.as_array().and_then(|a| a.first()).cloned()
        } else {
            Some(data.clone())
        };
        let id = obj
            .as_ref()
            .and_then(|img| img.get("Id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| color_eyre::eyre::eyre!("no Id in InspectImage output"))?;
        let digest = id.strip_prefix("sha256:").unwrap_or(id);
        let short = &digest[..16.min(digest.len())];
        debug!(digest = short, "image digest");
        Ok(short.to_string())
    }

    /// CreateContainer (vtable index 16)
    pub fn create_container(&self, image: &str, name: &str) -> Result<WslcContainer> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            *const c_void, // WSLCContainerOptions*
            *const c_void, // WarningCallback
            *mut *mut c_void, // Container out
        ) -> HRESULT;

        // Build a minimal WSLCContainerOptions on the stack.
        // The struct is large but mostly zeroed; only Image and Name are set.
        let image_cstr = std::ffi::CString::new(image)?;
        let name_cstr = std::ffi::CString::new(name)?;

        // We allocate the struct as a zeroed byte array to avoid defining every field.
        // The first two fields are Image (PCSTR) and Name (PCSTR).
        const OPTIONS_SIZE: usize = 512; // generous upper bound
        let mut options_buf = vec![0u8; OPTIONS_SIZE];

        // Write Image pointer at offset 0
        let image_ptr = image_cstr.as_ptr() as usize;
        options_buf[..std::mem::size_of::<usize>()].copy_from_slice(&image_ptr.to_ne_bytes());

        // Write Name pointer at offset sizeof(usize)
        let name_ptr = name_cstr.as_ptr() as usize;
        let offset = std::mem::size_of::<usize>();
        options_buf[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&name_ptr.to_ne_bytes());

        let mut container_ptr: *mut c_void = std::ptr::null_mut();
        unsafe {
            let f: Fn = self.com.vtable_fn(16);
            let hr = f(
                self.com.as_ptr(),
                options_buf.as_ptr() as *const c_void,
                std::ptr::null(),
                &mut container_ptr,
            );
            hr.ok().context("CreateContainer failed")?;
        }
        if container_ptr.is_null() {
            color_eyre::eyre::bail!("CreateContainer returned null");
        }
        let container = WslcContainer { com: ComPtr::new(container_ptr) };
        configure_com_impersonation(container_ptr)?;
        Ok(container)
    }
}

// ── IWSLCContainer vtable ──────────────────────────────────────────
// IUnknown (0-2) + Attach(3) + Stop(4) + Kill(5) + Start(6)
// + Delete(7) + Export(8) + ...

/// IWSLCContainer wrapper.
#[derive(Debug)]
pub struct WslcContainer {
    com: ComPtr,
}

impl WslcContainer {
    /// Stop (vtable index 4)
    pub fn stop(&self) -> Result<()> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            i32, // WSLCSignal (SIGTERM = 15)
            i32, // TimeoutSeconds
        ) -> HRESULT;
        unsafe {
            let f: Fn = self.com.vtable_fn(4);
            let hr = f(self.com.as_ptr(), 15, 10);
            hr.ok().context("Stop failed")?;
        }
        Ok(())
    }

    /// Start (vtable index 5)
    pub fn start(&self) -> Result<()> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            u32,           // WSLCContainerStartFlags
            *const c_void, // WSLCProcessStartOptions*
            *const c_void, // WarningCallback
        ) -> HRESULT;
        unsafe {
            let f: Fn = self.com.vtable_fn(5);
            let hr = f(self.com.as_ptr(), 0, std::ptr::null(), std::ptr::null());
            hr.ok().context("Start failed")?;
        }
        Ok(())
    }

    /// Delete (vtable index 6)
    pub fn delete(&self) -> Result<()> {
        type Fn = unsafe extern "system" fn(*mut c_void, u32) -> HRESULT;
        unsafe {
            let f: Fn = self.com.vtable_fn(6);
            let hr = f(self.com.as_ptr(), 0);
            hr.ok().context("Delete failed")?;
        }
        Ok(())
    }

    /// Export (vtable index 7) — writes rootfs tar to the given file handle.
    pub fn export(&self, file: &std::fs::File) -> Result<()> {
        type Fn = unsafe extern "system" fn(
            *mut c_void,
            WSLCHandle,
        ) -> HRESULT;

        use std::os::windows::io::AsRawHandle;
        let handle = WSLCHandle {
            handle_type: WSLC_HANDLE_TYPE_FILE,
            handle: file.as_raw_handle() as isize,
        };
        unsafe {
            let f: Fn = self.com.vtable_fn(7);
            let hr = f(self.com.as_ptr(), handle);
            hr.ok().context("Export failed")?;
        }
        Ok(())
    }
}

// ── Public entry point ─────────────────────────────────────────────

/// Create an IWSLCSessionManager via COM.
pub fn create_session_manager() -> Result<WslcSessionManager> {
    crate::hyperv::vm::com_init_once();
    let mut ptr: *mut c_void = std::ptr::null_mut();
    unsafe {
        let unknown: windows::core::IUnknown = CoCreateInstance(
            &CLSID_WSLC_SESSION_MANAGER,
            None,
            CLSCTX_LOCAL_SERVER,
        )
        .context("failed to create WSLCSessionManager — is WSL 2.8+ installed?")?;
        // QI for IWSLCSessionManager
        type QiFn = unsafe extern "system" fn(
            *mut c_void, *const GUID, *mut *mut c_void,
        ) -> HRESULT;
        let raw = unknown.as_raw();
        let vtable = *(raw as *const *const *const c_void);
        let qi: QiFn = std::mem::transmute_copy(&*vtable.add(0));
        qi(raw, &IID_IWSLC_SESSION_MANAGER, &mut ptr)
            .ok()
            .context("QI for IWSLCSessionManager failed")?;
    }
    if ptr.is_null() {
        color_eyre::eyre::bail!("WSLCSessionManager QI returned null");
    }
    configure_com_impersonation(ptr)?;
    Ok(WslcSessionManager { com: ComPtr::new(ptr) })
}

/// Open the default wslc session (create if needed).
pub fn open_default_session() -> Result<WslcSession> {
    let manager = create_session_manager()?;
    manager.create_session(WSLC_SESSION_FLAGS_OPEN_EXISTING | WSLC_SESSION_FLAGS_PERSISTENT)
}

// ── Internal ───────────────────────────────────────────────────────

/// Set EOAC_DYNAMIC_CLOAKING on the proxy blanket.
/// Mirrors ConfigureForCOMImpersonation (WslSecurity.cpp:92).
fn configure_com_impersonation(proxy: *mut c_void) -> Result<()> {
    unsafe {
        let mut client_security: *mut c_void = std::ptr::null_mut();
        let iid_client_security =
            GUID::from_u128(0x0000013Du128 << 96 | 0x0000_0000_0000_0000_C000_0000_0000_0046);
        // QI for IClientSecurity
        type QiFn = unsafe extern "system" fn(
            *mut c_void, *const GUID, *mut *mut c_void,
        ) -> HRESULT;
        let qi: QiFn = {
            let vtable = *(proxy as *const *const *const c_void);
            std::mem::transmute_copy(&*vtable.add(0))
        };
        qi(proxy, &iid_client_security, &mut client_security)
            .ok()
            .context("QI for IClientSecurity failed")?;

        if client_security.is_null() {
            color_eyre::eyre::bail!("IClientSecurity is null");
        }

        // IClientSecurity::QueryBlanket is vtable index 3
        type QueryBlanketFn = unsafe extern "system" fn(
            *mut c_void, *mut c_void,
            *mut u32, *mut u32, *mut *mut u16,
            *mut u32, *mut u32, *mut *mut c_void, *mut u32,
        ) -> HRESULT;
        let query_blanket: QueryBlanketFn = {
            let vtable = *(client_security as *const *const *const c_void);
            std::mem::transmute_copy(&*vtable.add(3))
        };

        let mut authn_svc = 0u32;
        let mut authz_svc = 0u32;
        let mut authn_lvl = 0u32;
        let mut capabilities = 0u32;
        query_blanket(
            client_security, proxy,
            &mut authn_svc, &mut authz_svc, std::ptr::null_mut(),
            &mut authn_lvl, std::ptr::null_mut(), std::ptr::null_mut(),
            &mut capabilities,
        )
        .ok()
        .context("QueryBlanket failed")?;

        // Clear EOAC_STATIC_CLOAKING (0x20), set EOAC_DYNAMIC_CLOAKING (0x40)
        capabilities &= !0x20;
        capabilities |= 0x40;

        // IClientSecurity::SetBlanket is vtable index 4
        type SetBlanketFn = unsafe extern "system" fn(
            *mut c_void, *mut c_void,
            u32, u32, *mut u16,
            u32, u32, *mut c_void, u32,
        ) -> HRESULT;
        let set_blanket: SetBlanketFn = {
            let vtable = *(client_security as *const *const *const c_void);
            std::mem::transmute_copy(&*vtable.add(4))
        };

        const RPC_C_IMP_LEVEL_IMPERSONATE: u32 = 3;
        set_blanket(
            client_security, proxy,
            authn_svc, authz_svc, std::ptr::null_mut(),
            authn_lvl, RPC_C_IMP_LEVEL_IMPERSONATE,
            std::ptr::null_mut(), capabilities,
        )
        .ok()
        .context("SetBlanket failed")?;

        // Release IClientSecurity
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = {
            let vtable = *(client_security as *const *const *const c_void);
            std::mem::transmute_copy(&*vtable.add(2))
        };
        release(client_security);
    }
    Ok(())
}
