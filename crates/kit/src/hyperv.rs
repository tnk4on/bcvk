//! Hyper-V VM lifecycle management via PowerShell commands.


#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
#[cfg(target_os = "windows")]
use tracing::{debug, info};

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct SwitchInfo {
    pub name: String,
    pub host_ip: String,
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct VmInfo {
    pub name: String,
    pub state: String,
}

#[cfg(target_os = "windows")]
fn powershell(script: &str) -> Result<String> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("PowerShell failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "windows")]
fn powershell_ignore_error(script: &str) {
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(target_os = "windows")]
pub fn ensure_internal_switch(name: &str, host_ip: &str, prefix_len: u8) -> Result<SwitchInfo> {
    let check = powershell(&format!(
        "Get-VMSwitch -Name '{}' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name",
        name
    ))?;

    if check.is_empty() {
        info!("creating Internal Switch: {}", name);
        powershell(&format!("New-VMSwitch -Name '{}' -SwitchType Internal", name))?;
    } else {
        debug!("switch {} already exists", name);
    }

    let if_index = powershell(&format!(
        "(Get-NetAdapter -Name 'vEthernet ({})').ifIndex",
        name
    ))?;

    let existing_ip = powershell(&format!(
        "Get-NetIPAddress -InterfaceIndex {} -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -ExpandProperty IPAddress",
        if_index
    ))?;

    if !existing_ip.contains(host_ip) {
        powershell(&format!(
            "New-NetIPAddress -InterfaceIndex {} -IPAddress {} -PrefixLength {}",
            if_index, host_ip, prefix_len
        ))?;
        info!("assigned {} to switch {}", host_ip, name);
    }

    let nat_name = format!("{}-nat", name);
    let subnet = format!("{}/{}", host_ip.rsplit_once('.').map(|(base, _)| format!("{}.0", base)).unwrap_or_default(), prefix_len);
    powershell_ignore_error(&format!(
        "New-NetNat -Name '{}' -InternalIPInterfaceAddressPrefix '{}' -ErrorAction SilentlyContinue",
        nat_name, subnet
    ));

    // Firewall rules for DHCP + TFTP
    powershell_ignore_error(&format!(
        "New-NetFirewallRule -DisplayName 'bcvk-pxe-dhcp' -Direction Inbound -Protocol UDP -LocalPort 67 -Action Allow -ErrorAction SilentlyContinue"
    ));
    powershell_ignore_error(
        "New-NetFirewallRule -DisplayName 'bcvk-pxe-tftp' -Direction Inbound -Protocol UDP -LocalPort 69 -Action Allow -ErrorAction SilentlyContinue"
    );
    // Disable firewall for the Internal Switch profile
    powershell_ignore_error(
        "Set-NetFirewallProfile -Profile Private -Enabled False -ErrorAction SilentlyContinue"
    );

    Ok(SwitchInfo {
        name: name.to_string(),
        host_ip: host_ip.to_string(),
    })
}

#[cfg(target_os = "windows")]
pub fn create_gen2_vm(name: &str, memory_mb: u32, vcpus: u32, switch: &str) -> Result<()> {
    powershell_ignore_error(&format!(
        "Stop-VM -Name '{}' -Force -ErrorAction SilentlyContinue; Remove-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        name, name
    ));

    let memory_bytes = (memory_mb as u64) * 1024 * 1024;
    powershell(&format!(
        "New-VM -Name '{}' -Generation 2 -MemoryStartupBytes {} -NoVHD -SwitchName '{}'",
        name, memory_bytes, switch
    ))?;

    powershell(&format!(
        "Set-VMProcessor -VMName '{}' -Count {}",
        name, vcpus
    ))?;

    powershell(&format!(
        "Set-VMFirmware -VMName '{}' -EnableSecureBoot Off",
        name
    ))?;

    info!("created Hyper-V Gen2 VM: {} ({} vCPUs, {}MB)", name, vcpus, memory_mb);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn set_pxe_boot(name: &str) -> Result<()> {
    powershell(&format!(
        "Set-VMFirmware -VMName '{}' -FirstBootDevice (Get-VMNetworkAdapter -VMName '{}')",
        name, name
    ))?;
    debug!("set PXE boot for {}", name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn start_vm(name: &str) -> Result<()> {
    powershell(&format!("Start-VM -Name '{}'", name))?;
    info!("started VM: {}", name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn stop_vm(name: &str) -> Result<()> {
    powershell_ignore_error(&format!(
        "Stop-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        name
    ));
    debug!("stopped VM: {}", name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn remove_vm(name: &str) -> Result<()> {
    stop_vm(name)?;
    powershell_ignore_error(&format!(
        "Remove-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        name
    ));
    debug!("removed VM: {}", name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn get_vm_state(name: &str) -> Result<String> {
    powershell(&format!(
        "Write-Host (Get-VM -Name '{}' -ErrorAction SilentlyContinue).State",
        name
    ))
}

#[cfg(target_os = "windows")]
pub fn get_vm_ip(name: &str) -> Result<Option<String>> {
    let ips = powershell(&format!(
        "(Get-VMNetworkAdapter -VMName '{}').IPAddresses | Select-Object -First 1",
        name
    ))?;
    if ips.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ips))
    }
}

#[cfg(target_os = "windows")]
pub fn list_vms(prefix: &str) -> Result<Vec<VmInfo>> {
    let output = powershell(&format!(
        "Get-VM -Name '{}*' -ErrorAction SilentlyContinue | ForEach-Object {{ \"$($_.Name)|$($_.State)\" }}",
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

#[cfg(target_os = "windows")]
pub fn is_hyper_v_enabled() -> bool {
    powershell("Write-Host (Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V).State")
        .map(|s| s.contains("Enabled"))
        .unwrap_or(false)
}
