//! Hyper-V VM lifecycle management.
//!
//! VM state queries use WMI (root\virtualization\v2) via the MS official
//! `windows` crate. Remaining operations use PowerShell pending migration.

use color_eyre::{eyre::bail, Result};
use std::process::{Command, Stdio};
use std::sync::Once;
use tracing::{debug, info};

use windows::core::BSTR;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::*;

const RPC_C_AUTHN_WINNT: u32 = 10;
const RPC_C_AUTHZ_NONE: u32 = 0;

static COM_INIT: Once = Once::new();

#[allow(unsafe_code)]
fn com_init() {
    COM_INIT.call_once(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        );
    });
}

#[allow(unsafe_code)]
fn wmi_connect(namespace: &str) -> Result<IWbemServices> {
    com_init();
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let services = locator.ConnectServer(
            &BSTR::from(namespace),
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            0,
            &BSTR::new(),
            None,
        )?;
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )?;
        Ok(services)
    }
}

#[allow(unsafe_code)]
fn wmi_get_property(obj: &IWbemClassObject, name: &str) -> Result<VARIANT> {
    let prop_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let prop = windows::core::PCWSTR(prop_w.as_ptr());
    let mut val = VARIANT::default();
    unsafe { obj.Get(prop, 0, &mut val, None, None)? };
    Ok(val)
}

#[allow(unsafe_code)]
fn variant_to_string(val: &VARIANT) -> String {
    unsafe {
        let inner = &val.Anonymous.Anonymous;
        if inner.vt == windows::Win32::System::Variant::VT_BSTR {
            inner.Anonymous.bstrVal.to_string()
        } else {
            String::new()
        }
    }
}

#[allow(unsafe_code)]
fn variant_to_i32(val: &VARIANT) -> i32 {
    unsafe { val.Anonymous.Anonymous.Anonymous.lVal }
}

#[derive(Debug)]
pub struct SwitchInfo {
    pub name: String,
    pub host_ip: String,
}

#[derive(Debug)]
pub struct VmInfo {
    pub name: String,
    pub state: String,
}

fn powershell(script: &str) -> Result<String> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "PowerShell failed ({}): stderr={} stdout={}",
            script.chars().take(80).collect::<String>(),
            stderr.trim(),
            stdout.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn powershell_ignore_error(script: &str) {
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn ensure_internal_switch(name: &str, host_ip: &str, prefix_len: u8) -> Result<SwitchInfo> {
    let subnet = format!(
        "{}/{}",
        host_ip
            .rsplit_once('.')
            .map(|(base, _)| format!("{}.0", base))
            .unwrap_or_default(),
        prefix_len
    );
    let script = format!(
        "$ErrorActionPreference = 'SilentlyContinue'; \
         $sw = Get-VMSwitch -Name '{name}' -EA SilentlyContinue; \
         if (-not $sw) {{ New-VMSwitch -Name '{name}' -SwitchType Internal | Out-Null }}; \
         $idx = (Get-NetAdapter -Name 'vEthernet ({name})').ifIndex; \
         $existing = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv4 -EA SilentlyContinue; \
         if (-not ($existing | Where-Object {{ $_.IPAddress -eq '{ip}' }})) {{ \
             New-NetIPAddress -InterfaceIndex $idx -IPAddress {ip} -PrefixLength {pl} -EA SilentlyContinue | Out-Null \
         }}; \
         New-NetNat -Name '{name}-nat' -InternalIPInterfaceAddressPrefix '{subnet}' -EA SilentlyContinue | Out-Null; \
         New-NetFirewallRule -DisplayName 'bcvk-dhcp' -Direction Inbound -Protocol UDP -LocalPort 67 -Action Allow -EA SilentlyContinue | Out-Null; \
         Set-NetFirewallProfile -Profile Domain,Public,Private -Enabled False -EA SilentlyContinue; \
         Disable-VMSwitchExtension -VMSwitchName '{name}' -Name 'Microsoft NDIS Capture' -EA SilentlyContinue; \
         Write-Host 'OK'",
        name = name, ip = host_ip, pl = prefix_len, subnet = subnet,
    );
    powershell(&script)?;
    Ok(SwitchInfo {
        name: name.to_string(),
        host_ip: host_ip.to_string(),
    })
}

pub fn remove_internal_switch(name: &str) {
    powershell_ignore_error(&format!(
        "Remove-NetNat -Name '{name}-nat' -Confirm:$false -EA SilentlyContinue; \
         Remove-VMSwitch -Name '{name}' -Force -EA SilentlyContinue",
        name = name,
    ));
    debug!("removed switch: {}", name);
}

pub fn create_gen2_vm(name: &str, memory_mb: u32, vcpus: u32, switch: &str) -> Result<()> {
    let memory_bytes = (memory_mb as u64) * 1024 * 1024;
    let script = format!(
        "Stop-VM -Name '{name}' -Force -EA SilentlyContinue; \
         Remove-VM -Name '{name}' -Force -EA SilentlyContinue; \
         New-VM -Name '{name}' -Generation 2 -MemoryStartupBytes {mem} -NoVHD -SwitchName '{sw}' | Out-Null; \
         Set-VMProcessor -VMName '{name}' -Count {cpu}; \
         Set-VMFirmware -VMName '{name}' -EnableSecureBoot Off; \
         Enable-VMIntegrationService -VMName '{name}' -Name 'Guest Service Interface' -EA SilentlyContinue; \
         Set-VM -Name '{name}' -CheckpointType Disabled; \
         Set-VMComPort -VMName '{name}' -Number 1 -Path '\\\\.\\pipe\\bcvk-serial-{name}'; \
         Write-Host 'OK'",
        name = name, mem = memory_bytes, cpu = vcpus, sw = switch,
    );
    powershell(&script)?;
    info!(
        "created Hyper-V Gen2 VM: {} ({} vCPUs, {}MB)",
        name, vcpus, memory_mb
    );
    Ok(())
}

/// Attach VHDX, set boot device, start VM, return VM GUID — all in 1 PowerShell call.
pub fn attach_and_start_vm(name: &str, vhdx_path: &str) -> Result<String> {
    let script = format!(
        "Add-VMHardDiskDrive -VMName '{name}' -Path '{vhdx}' -ControllerType SCSI; \
         Set-VMFirmware -VMName '{name}' -FirstBootDevice (Get-VMHardDiskDrive -VMName '{name}' | Select-Object -First 1); \
         Start-VM -Name '{name}'; \
         (Get-VM -Name '{name}').Id.ToString()",
        name = name, vhdx = vhdx_path,
    );
    let guid = powershell(&script)?;
    info!("started VM: {} (GUID: {})", name, guid.trim());
    Ok(guid.trim().to_string())
}

pub fn attach_vhdx(name: &str, vhdx_path: &str) -> Result<()> {
    powershell(&format!(
        "Add-VMHardDiskDrive -VMName '{name}' -Path '{vhdx}' -ControllerType SCSI; \
         Set-VMFirmware -VMName '{name}' -FirstBootDevice \
         (Get-VMHardDiskDrive -VMName '{name}' | Select-Object -First 1)",
        name = name,
        vhdx = vhdx_path,
    ))?;
    debug!("attached VHDX to VM {}: {}", name, vhdx_path);
    Ok(())
}

pub fn stop_vm(name: &str) -> Result<()> {
    powershell_ignore_error(&format!(
        "Stop-VM -Name '{}' -TurnOff -Force -ErrorAction SilentlyContinue",
        name
    ));
    debug!("stopped VM: {}", name);
    Ok(())
}

pub fn start_vm(name: &str) -> Result<()> {
    powershell(&format!("Start-VM -Name '{}'", name))?;
    debug!("started VM: {}", name);
    Ok(())
}

pub fn remove_vm(name: &str) -> Result<()> {
    stop_vm(name)?;
    powershell_ignore_error(&format!(
        "Remove-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        name
    ));
    debug!("removed VM: {}", name);
    Ok(())
}

#[allow(unsafe_code)]
pub fn get_vm_state(name: &str) -> Result<String> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let query = format!(
        "SELECT EnabledState FROM Msvm_ComputerSystem WHERE ElementName='{}'",
        name
    );
    unsafe {
        let enumerator = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;
        let mut objs = [None; 1];
        let mut returned = 0u32;
        if enumerator
            .Next(WBEM_INFINITE, &mut objs, &mut returned)
            .is_err()
            || returned == 0
        {
            return Ok(String::new());
        }
        if let Some(ref obj) = objs[0] {
            let val = wmi_get_property(obj, "EnabledState")?;
            let state: i32 = variant_to_i32(&val);
            return Ok(match state {
                2 => "Running".to_string(),
                3 => "Off".to_string(),
                6 => "Saved".to_string(),
                9 => "Paused".to_string(),
                _ => format!("Unknown({})", state),
            });
        }
    }
    Ok(String::new())
}

#[allow(unsafe_code)]
pub fn list_vms(prefix: &str) -> Result<Vec<VmInfo>> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let query = format!(
        "SELECT ElementName, EnabledState FROM Msvm_ComputerSystem WHERE ElementName LIKE '{}%'",
        prefix
    );
    let mut vms = Vec::new();
    unsafe {
        let enumerator = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;
        loop {
            let mut objs = [None; 1];
            let mut returned = 0u32;
            if enumerator
                .Next(WBEM_INFINITE, &mut objs, &mut returned)
                .is_err()
                || returned == 0
            {
                break;
            }
            if let Some(ref obj) = objs[0] {
                let name_v = wmi_get_property(obj, "ElementName")?;
                let state_v = wmi_get_property(obj, "EnabledState")?;
                let name = variant_to_string(&name_v);
                let state_i: i32 = variant_to_i32(&state_v);
                let state = match state_i {
                    2 => "Running",
                    3 => "Off",
                    6 => "Saved",
                    9 => "Paused",
                    _ => "Unknown",
                };
                vms.push(VmInfo {
                    name,
                    state: state.to_string(),
                });
            }
        }
    }
    Ok(vms)
}

pub fn is_hyper_v_enabled() -> bool {
    wmi_connect("root\\virtualization\\v2").is_ok()
}

#[allow(unsafe_code)]
pub fn get_vm_guid(vm_name: &str) -> Result<String> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let query = format!(
        "SELECT Name FROM Msvm_ComputerSystem WHERE ElementName='{}'",
        vm_name
    );
    unsafe {
        let enumerator = services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;
        let mut objs = [None; 1];
        let mut returned = 0u32;
        if enumerator
            .Next(WBEM_INFINITE, &mut objs, &mut returned)
            .is_err()
            || returned == 0
        {
            bail!("VM '{}' not found", vm_name);
        }
        if let Some(ref obj) = objs[0] {
            let val = wmi_get_property(obj, "Name")?;
            return Ok(variant_to_string(&val));
        }
    }
    bail!("VM '{}' not found", vm_name)
}

pub fn get_wsl_vm_guid(_machine_name: &str) -> Result<String> {
    // WSL2 VMs are HCS utility VMs, invisible to Get-VM.
    // Use hcsdiag to find the WSL VM GUID.
    let output = Command::new("hcsdiag")
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains("WSL") && i > 0 {
            let guid = lines[i - 1].trim().to_string();
            if guid.len() == 36 && guid.contains('-') {
                return Ok(guid);
            }
        }
    }
    bail!("could not find WSL2 VM via hcsdiag. Ensure podman machine (WSL2) is running.");
}

#[allow(unsafe_code)]
pub fn register_vsock_service(port: u32) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;

    let guid = format!("{:08X}-FACB-11E6-BD58-64006A7986D3", port);
    let key_path = format!(
        "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Virtualization\\GuestCommunicationServices\\{}",
        guid
    );
    let key_path_w: Vec<u16> = key_path.encode_utf16().chain(std::iter::once(0)).collect();
    let value_name: Vec<u16> = "ElementName"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_data: Vec<u16> = "bcvk-nbd"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut hkey = HKEY::default();
        let rc = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_path_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if rc.is_err() {
            debug!("registry write failed (may need admin): {}", guid);
            return Ok(());
        }
        let data_bytes: &[u8] =
            std::slice::from_raw_parts(value_data.as_ptr() as *const u8, value_data.len() * 2);
        let _ = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(data_bytes),
        );
        let _ = RegCloseKey(hkey);
    }
    debug!("registered vsock service GUID: {}", guid);
    Ok(())
}

pub fn unregister_vsock_service(_port: u32) -> Result<()> {
    // GUID is kept permanently. Deleting it caused re-registration failures
    // because powershell_ignore_error silently swallowed HKLM write errors.
    // The key only permits vsock on one port — no cleanup needed.
    Ok(())
}
