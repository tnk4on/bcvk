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
    )).unwrap_or_default();

    if !existing_ip.contains(host_ip) {
        powershell_ignore_error(&format!(
            "New-NetIPAddress -InterfaceIndex {} -IPAddress {} -PrefixLength {}",
            if_index, host_ip, prefix_len
        ));
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
        "Set-NetFirewallProfile -Profile Domain,Public,Private -Enabled False -ErrorAction SilentlyContinue"
    );
    // Disable NDIS Capture extension (blocks host→VM traffic)
    powershell_ignore_error(&format!(
        "Disable-VMSwitchExtension -VMSwitchName '{}' -Name 'Microsoft NDIS Capture' -ErrorAction SilentlyContinue",
        name
    ));

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

    // Enable Guest Service Interface for hv_sock (vsock) communication
    powershell_ignore_error(&format!(
        "Enable-VMIntegrationService -VMName '{}' -Name 'Guest Service Interface'",
        name
    ));

    // Disable checkpoints (not needed for ephemeral VMs)
    powershell_ignore_error(&format!(
        "Set-VM -Name '{}' -CheckpointType Disabled",
        name
    ));

    // COM1 → named pipe for serial console capture
    powershell_ignore_error(&format!(
        "Set-VMComPort -VMName '{}' -Number 1 -Path '\\\\.\\pipe\\bcvk-serial-{}'",
        name, name
    ));

    info!("created Hyper-V Gen2 VM: {} ({} vCPUs, {}MB)", name, vcpus, memory_mb);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn add_nic(vm_name: &str, switch: &str) -> Result<()> {
    powershell(&format!(
        "Add-VMNetworkAdapter -VMName '{}' -SwitchName '{}'",
        vm_name, switch
    ))?;
    debug!("added NIC on '{}' to VM {}", switch, vm_name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn get_default_switch_ip() -> Result<String> {
    let ip = powershell(
        "Get-NetIPAddress -InterfaceAlias 'vEthernet (Default Switch)' -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -ExpandProperty IPAddress"
    )?;
    if ip.is_empty() {
        bail!("Default Switch IP not found");
    }
    Ok(ip)
}

#[cfg(target_os = "windows")]
pub fn get_physical_ip() -> Result<String> {
    let ip = powershell(
        "(Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -notlike 'vEthernet*' -and $_.InterfaceAlias -ne 'Loopback*' -and $_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1).IPAddress"
    )?;
    if ip.is_empty() {
        bail!("no physical NIC IP found");
    }
    Ok(ip)
}

#[cfg(target_os = "windows")]
pub fn ensure_external_switch(name: &str, nic_name: &str) -> Result<String> {
    let check = powershell(&format!(
        "Get-VMSwitch -Name '{}' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name",
        name
    ))?;
    if check.is_empty() {
        info!("creating External Switch: {} on {}", name, nic_name);
        powershell(&format!(
            "New-VMSwitch -Name '{}' -NetAdapterName '{}' -AllowManagementOS $true",
            name, nic_name
        ))?;
    }
    Ok(name.to_string())
}

#[cfg(target_os = "windows")]
pub fn ensure_iphlpsvc() -> Result<()> {
    powershell_ignore_error("sc.exe config iphlpsvc start= demand");
    powershell_ignore_error("net start iphlpsvc");
    debug!("IP Helper service started");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn setup_nbd_portproxy(listen_ip: &str, listen_port: u16, connect_port: u16) -> Result<()> {
    powershell_ignore_error("netsh interface portproxy reset");
    powershell(&format!(
        "netsh interface portproxy add v4tov4 listenaddress={} listenport={} connectaddress=127.0.0.1 connectport={}",
        listen_ip, listen_port, connect_port
    ))?;
    info!("netsh portproxy: {}:{} → 127.0.0.1:{}", listen_ip, listen_port, connect_port);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn add_pxe_firewall_rules(nbd_port: u16) -> Result<()> {
    powershell_ignore_error(
        "New-NetFirewallRule -DisplayName 'bcvk-pxe-dhcp' -Direction Inbound -Protocol UDP -LocalPort 67,4011 -Action Allow -ErrorAction SilentlyContinue"
    );
    powershell_ignore_error(
        "New-NetFirewallRule -DisplayName 'bcvk-pxe-tftp' -Direction Inbound -Protocol UDP -LocalPort 69 -Action Allow -ErrorAction SilentlyContinue"
    );
    powershell_ignore_error(&format!(
        "New-NetFirewallRule -DisplayName 'bcvk-nbd' -Direction Inbound -Protocol TCP -LocalPort {} -Action Allow -ErrorAction SilentlyContinue",
        nbd_port
    ));
    powershell_ignore_error(
        "Set-NetFirewallProfile -Profile Domain,Public,Private -Enabled False -ErrorAction SilentlyContinue"
    );
    debug!("firewall rules added");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn set_pxe_boot(name: &str) -> Result<()> {
    powershell(&format!(
        "Set-VMFirmware -VMName '{}' -FirstBootDevice (Get-VMNetworkAdapter -VMName '{}' | Select-Object -First 1)",
        name, name
    ))?;
    debug!("set PXE boot for {}", name);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn add_vhdx_boot(name: &str, vhdx_path: &str) -> Result<()> {
    powershell(&format!(
        "Add-VMHardDiskDrive -VMName '{}' -Path '{}' -ControllerType SCSI",
        name, vhdx_path
    ))?;
    powershell(&format!(
        "Set-VMFirmware -VMName '{}' -FirstBootDevice (Get-VMHardDiskDrive -VMName '{}' | Select-Object -First 1)",
        name, name
    ))?;
    debug!("set VHDX boot for {}: {}", name, vhdx_path);
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
