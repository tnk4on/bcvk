//! Hyper-V VM lifecycle management via PowerShell commands.

use color_eyre::{eyre::bail, Result};
use std::process::{Command, Stdio};
use tracing::{debug, info};

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

pub fn get_vm_state(name: &str) -> Result<String> {
    powershell(&format!(
        "$v = Get-VM -Name '{}' -ErrorAction SilentlyContinue; if ($v) {{ Write-Host $v.State }}; exit 0",
        name
    ))
}

pub fn list_vms(prefix: &str) -> Result<Vec<VmInfo>> {
    let output = powershell(&format!(
        "$vms = Get-VM -Name '{}*' -ErrorAction SilentlyContinue; if ($vms) {{ $vms | ForEach-Object {{ \"$($_.Name)|$($_.State)\" }} }}; exit 0",
        prefix
    ))?;
    let mut vms = Vec::new();
    for line in output.lines() {
        if let Some((name, state)) = line.split_once('|') {
            vms.push(VmInfo {
                name: name.to_string(),
                state: state.to_string(),
            });
        }
    }
    Ok(vms)
}

pub fn is_hyper_v_enabled() -> bool {
    powershell(
        "Write-Host (Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V).State",
    )
    .map(|s| s.contains("Enabled"))
    .unwrap_or(false)
}

pub fn get_vm_guid(vm_name: &str) -> Result<String> {
    powershell(&format!(
        "Write-Host (Get-VM -Name '{}').VMId.Guid",
        vm_name
    ))
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

pub fn register_vsock_service(port: u32) -> Result<()> {
    let guid = format!("{:08X}-FACB-11E6-BD58-64006A7986D3", port);
    powershell_ignore_error(&format!(
        "New-Item -Path 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Virtualization\\GuestCommunicationServices\\{}' -Force | \
         Set-ItemProperty -Name 'ElementName' -Value 'bcvk-nbd'",
        guid
    ));
    debug!("registered vsock service GUID: {}", guid);
    Ok(())
}

pub fn unregister_vsock_service(_port: u32) -> Result<()> {
    // GUID is kept permanently. Deleting it caused re-registration failures
    // because powershell_ignore_error silently swallowed HKLM write errors.
    // The key only permits vsock on one port — no cleanup needed.
    Ok(())
}
