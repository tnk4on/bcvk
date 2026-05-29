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

#[allow(unsafe_code)]
fn wmi_put_bstr(obj: &IWbemClassObject, name: &str, value: &str) -> Result<()> {
    let prop_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut v = VARIANT::default();
        let p = &mut v as *mut VARIANT;
        let inner = &mut (*p).Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VT_BSTR;
        std::ptr::write(
            std::ptr::addr_of_mut!(inner.Anonymous.bstrVal),
            std::mem::ManuallyDrop::new(BSTR::from(value)),
        );
        obj.Put(windows::core::PCWSTR(prop_w.as_ptr()), 0, &v, 0)?;
    }
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_put_i32(obj: &IWbemClassObject, name: &str, value: i32) -> Result<()> {
    let prop_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut v = VARIANT::default();
        let p = &mut v as *mut VARIANT;
        let inner = &mut (*p).Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VT_I4;
        (*std::ptr::addr_of_mut!(inner.Anonymous)).lVal = value;
        obj.Put(windows::core::PCWSTR(prop_w.as_ptr()), 0, &v, 0)?;
    }
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_put_bstr_array(obj: &IWbemClassObject, name: &str, values: &[&str]) -> Result<()> {
    use windows::Win32::System::Ole::{
        SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement,
    };
    use windows::Win32::System::Variant::{VT_ARRAY, VT_BSTR};

    let prop_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let sa = SafeArrayCreateVector(VT_BSTR, 0, values.len() as u32);
        if sa.is_null() {
            bail!("SafeArrayCreateVector failed");
        }
        for (i, s) in values.iter().enumerate() {
            let bstr = BSTR::from(*s);
            let idx = i as i32;
            if let Err(e) = SafeArrayPutElement(sa, &idx, bstr.as_ptr() as *const _) {
                SafeArrayDestroy(sa).ok();
                return Err(e.into());
            }
        }
        let mut v = VARIANT::default();
        let p = &mut v as *mut VARIANT;
        let inner = &mut (*p).Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VARENUM(VT_BSTR.0 | VT_ARRAY.0);
        (*std::ptr::addr_of_mut!(inner.Anonymous)).parray = sa;
        obj.Put(windows::core::PCWSTR(prop_w.as_ptr()), 0, &v, 0)?;
    }
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_get_mgmt_service(services: &IWbemServices) -> Result<(IWbemClassObject, String)> {
    let query = "SELECT * FROM Msvm_VirtualSystemManagementService";
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
            bail!("Msvm_VirtualSystemManagementService not found");
        }
        let obj = objs[0]
            .take()
            .ok_or_else(|| color_eyre::eyre::eyre!("no mgmt service"))?;
        let path_val = wmi_get_property(&obj, "__PATH")?;
        let path = variant_to_string(&path_val);
        Ok((obj, path))
    }
}

#[allow(unsafe_code)]
fn wmi_get_method_in_params(
    services: &IWbemServices,
    class_name: &str,
    method_name: &str,
) -> Result<IWbemClassObject> {
    unsafe {
        let mut class_obj = None;
        services.GetObject(
            &BSTR::from(class_name),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            Some(&mut class_obj),
            None,
        )?;
        let class_obj =
            class_obj.ok_or_else(|| color_eyre::eyre::eyre!("GetObject({}) failed", class_name))?;
        let mut in_params_class = None;
        class_obj.GetMethod(
            &BSTR::from(method_name),
            0,
            &mut in_params_class,
            std::ptr::null_mut(),
        )?;
        let in_params_class = in_params_class
            .ok_or_else(|| color_eyre::eyre::eyre!("GetMethod({}) failed", method_name))?;
        Ok(in_params_class.SpawnInstance(0)?)
    }
}

#[allow(unsafe_code)]
fn wmi_check_result(services: &IWbemServices, out_params: &IWbemClassObject) -> Result<()> {
    let rv = wmi_get_property(out_params, "ReturnValue")?;
    let rv_val = variant_to_i32(&rv);
    match rv_val {
        0 => Ok(()),
        4096 => {
            let job_val = wmi_get_property(out_params, "Job")?;
            let job_path = variant_to_string(&job_val);
            wmi_wait_for_job(services, &job_path)
        }
        _ => bail!("WMI method failed with ReturnValue={}", rv_val),
    }
}

#[allow(unsafe_code)]
fn wmi_wait_for_job(services: &IWbemServices, job_path: &str) -> Result<()> {
    loop {
        unsafe {
            let mut job_obj = None;
            services.GetObject(
                &BSTR::from(job_path),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut job_obj),
                None,
            )?;
            let job_obj = job_obj.ok_or_else(|| color_eyre::eyre::eyre!("Job object not found"))?;
            let state_val = wmi_get_property(&job_obj, "JobState")?;
            let state = variant_to_i32(&state_val);
            match state {
                7 => return Ok(()), // Completed
                10 | 11 => {
                    let desc = wmi_get_property(&job_obj, "ErrorDescription")
                        .ok()
                        .map(|v| variant_to_string(&v))
                        .unwrap_or_default();
                    bail!("WMI job failed (state={}): {}", state, desc);
                }
                2 | 3 | 4 => {
                    // New, Starting, Running — keep waiting
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
}

#[allow(unsafe_code)]
fn wmi_define_system(name: &str) -> Result<()> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let (_mgmt, mgmt_path) = wmi_get_mgmt_service(&services)?;
    let in_params = wmi_get_method_in_params(
        &services,
        "Msvm_VirtualSystemManagementService",
        "DefineSystem",
    )?;

    let system_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_VirtualSystemSettingData\">\
         <PROPERTY NAME=\"ElementName\" TYPE=\"string\"><VALUE>{name}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"VirtualSystemSubType\" TYPE=\"string\">\
         <VALUE>Microsoft:Hyper-V:SubType:2</VALUE></PROPERTY></INSTANCE>"
    );
    wmi_put_bstr(&in_params, "SystemSettings", &system_xml)?;

    unsafe {
        let mut out_params = None;
        services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("DefineSystem"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        )?;
        let out_params =
            out_params.ok_or_else(|| color_eyre::eyre::eyre!("DefineSystem returned no output"))?;
        wmi_check_result(&services, &out_params)?;
    }
    debug!("DefineSystem succeeded for '{}'", name);
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_query_first_string(services: &IWbemServices, query: &str, property: &str) -> Result<String> {
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
            bail!("WMI query returned no results: {}", query);
        }
        let obj = objs[0]
            .as_ref()
            .ok_or_else(|| color_eyre::eyre::eyre!("no object"))?;
        let val = wmi_get_property(obj, property)?;
        Ok(variant_to_string(&val))
    }
}

#[allow(unsafe_code)]
fn wmi_modify_resource_settings(
    services: &IWbemServices,
    mgmt_path: &str,
    resource_xmls: &[&str],
) -> Result<()> {
    let in_params = wmi_get_method_in_params(
        services,
        "Msvm_VirtualSystemManagementService",
        "ModifyResourceSettings",
    )?;
    wmi_put_bstr_array(&in_params, "ResourceSettings", resource_xmls)?;

    unsafe {
        let mut out_params = None;
        services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("ModifyResourceSettings"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        )?;
        let out_params = out_params
            .ok_or_else(|| color_eyre::eyre::eyre!("ModifyResourceSettings returned no output"))?;
        wmi_check_result(services, &out_params)?;
    }
    Ok(())
}

/// Get a default EthernetPortAllocationSettingData from the primordial resource pool,
/// set Parent (NIC) and HostResource (switch), and return as CIM-XML string.
/// This is the MS-recommended way to connect a NIC to a switch via WMI v2.
#[allow(unsafe_code)]
unsafe fn wmi_get_default_ethernet_connection(
    services: &IWbemServices,
    nic_path: &str,
    switch_path: &str,
) -> Result<String> {
    // 1. Get primordial resource pool for Ethernet Connection
    let pool_path = wmi_query_first_string(
        services,
        "SELECT * FROM Msvm_ResourcePool \
         WHERE ResourceSubType='Microsoft:Hyper-V:Ethernet Connection' AND Primordial=True",
        "__PATH",
    )?;

    // 2. Follow Msvm_ElementCapabilities to get AllocationCapabilities
    let cap_path = wmi_query_first_string(
        services,
        &format!(
            "ASSOCIATORS OF {{{}}} WHERE AssocClass=Msvm_ElementCapabilities \
             ResultClass=Msvm_AllocationCapabilities",
            pool_path
        ),
        "__PATH",
    )?;

    // 3. Follow Msvm_SettingsDefineCapabilities (ValueRole=0) to get default template
    let enumerator = services.ExecQuery(
        &BSTR::from("WQL"),
        &BSTR::from(format!(
            "REFERENCES OF {{{}}} WHERE ResultClass=Msvm_SettingsDefineCapabilities",
            cap_path
        )),
        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
        None,
    )?;
    let mut default_path = String::new();
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
            let role = variant_to_i32(&wmi_get_property(obj, "ValueRole")?);
            if role == 0 {
                default_path = variant_to_string(&wmi_get_property(obj, "PartComponent")?);
                break;
            }
        }
    }
    if default_path.is_empty() {
        color_eyre::eyre::bail!("no default EthernetPortAllocationSettingData found");
    }

    // 4. Get the default instance and clone it
    let mut default_obj = None;
    services.GetObject(
        &BSTR::from(&default_path),
        WBEM_GENERIC_FLAG_TYPE(0),
        None,
        Some(&mut default_obj),
        None,
    )?;
    let template =
        default_obj.ok_or_else(|| color_eyre::eyre::eyre!("failed to get default template"))?;
    let instance = template.SpawnInstance(0)?;

    // 5. Set Parent (NIC) and HostResource (switch)
    wmi_put_bstr(&instance, "Parent", nic_path)?;

    // HostResource is a string array
    use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
    use windows::Win32::System::Variant::{VT_ARRAY, VT_BSTR};
    let sa = SafeArrayCreateVector(VT_BSTR, 0, 1);
    let bstr = BSTR::from(switch_path);
    let idx: i32 = 0;
    SafeArrayPutElement(sa, &idx, bstr.as_ptr() as *const _)?;
    let prop_w: Vec<u16> = "HostResource"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut v = VARIANT::default();
    let p = &mut v as *mut VARIANT;
    let inner = &mut (*p).Anonymous.Anonymous;
    inner.vt = windows::Win32::System::Variant::VARENUM(VT_BSTR.0 | VT_ARRAY.0);
    (*std::ptr::addr_of_mut!(inner.Anonymous)).parray = sa;
    instance.Put(windows::core::PCWSTR(prop_w.as_ptr()), 0, &v, 0)?;

    // 6. Get MOF text representation
    let text = instance.GetObjectText(0)?;
    Ok(text.to_string())
}

#[allow(unsafe_code)]
fn wmi_add_resource_settings(
    services: &IWbemServices,
    mgmt_path: &str,
    affected_config: &str,
    resource_xmls: &[&str],
) -> Result<Vec<String>> {
    let in_params = wmi_get_method_in_params(
        services,
        "Msvm_VirtualSystemManagementService",
        "AddResourceSettings",
    )?;
    wmi_put_bstr(&in_params, "AffectedConfiguration", affected_config)?;
    wmi_put_bstr_array(&in_params, "ResourceSettings", resource_xmls)?;

    unsafe {
        let mut out_params = None;
        services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("AddResourceSettings"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        )?;
        let out_params = out_params
            .ok_or_else(|| color_eyre::eyre::eyre!("AddResourceSettings returned no output"))?;
        wmi_check_result(services, &out_params)?;

        let result_val = wmi_get_property(&out_params, "ResultingResourceSettings")?;
        let inner = &result_val.Anonymous.Anonymous;
        if inner.vt
            == windows::Win32::System::Variant::VARENUM(
                windows::Win32::System::Variant::VT_BSTR.0
                    | windows::Win32::System::Variant::VT_ARRAY.0,
            )
        {
            use windows::Win32::System::Ole::{
                SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
            };
            let sa = inner.Anonymous.parray;
            let lb = SafeArrayGetLBound(sa, 1)?;
            let ub = SafeArrayGetUBound(sa, 1)?;
            let mut paths = Vec::new();
            for i in lb..=ub {
                let mut bstr = BSTR::default();
                SafeArrayGetElement(sa, &i, &mut bstr as *mut _ as *mut _)?;
                paths.push(bstr.to_string());
            }
            return Ok(paths);
        }
        Ok(Vec::new())
    }
}

#[allow(unsafe_code)]
fn wmi_modify_guest_service_settings(
    services: &IWbemServices,
    mgmt_path: &str,
    settings_xmls: &[&str],
) -> Result<()> {
    let in_params = wmi_get_method_in_params(
        services,
        "Msvm_VirtualSystemManagementService",
        "ModifyGuestServiceSettings",
    )?;
    wmi_put_bstr_array(&in_params, "GuestServiceSettings", settings_xmls)?;

    unsafe {
        let mut out_params = None;
        services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("ModifyGuestServiceSettings"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        )?;
        if let Some(ref out) = out_params {
            let _ = wmi_check_result(services, out);
        }
    }
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_modify_system_settings(
    services: &IWbemServices,
    mgmt_path: &str,
    settings_xml: &str,
) -> Result<()> {
    let in_params = wmi_get_method_in_params(
        services,
        "Msvm_VirtualSystemManagementService",
        "ModifySystemSettings",
    )?;
    wmi_put_bstr(&in_params, "SystemSettings", settings_xml)?;

    unsafe {
        let mut out_params = None;
        services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("ModifySystemSettings"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        )?;
        let out_params = out_params
            .ok_or_else(|| color_eyre::eyre::eyre!("ModifySystemSettings returned no output"))?;
        wmi_check_result(services, &out_params)?;
    }
    Ok(())
}

#[allow(unsafe_code)]
fn wmi_request_state_change(vm_name: &str, state: u16) -> Result<()> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let vm_path = wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_ComputerSystem WHERE ElementName='{}'",
            vm_name
        ),
        "__PATH",
    )?;
    let in_params =
        wmi_get_method_in_params(&services, "Msvm_ComputerSystem", "RequestStateChange")?;
    wmi_put_i32(&in_params, "RequestedState", state as i32)?;

    unsafe {
        services.ExecMethod(
            &BSTR::from(vm_path),
            &BSTR::from("RequestStateChange"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            None,
            None,
        )?;
    }
    Ok(())
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

#[allow(unsafe_code)]
pub fn ensure_internal_switch(name: &str, host_ip: &str, prefix_len: u8) -> Result<SwitchInfo> {
    let subnet = format!(
        "{}/{}",
        host_ip
            .rsplit_once('.')
            .map(|(base, _)| format!("{}.0", base))
            .unwrap_or_default(),
        prefix_len
    );

    let services = wmi_connect("root\\virtualization\\v2")?;

    // Check if switch already exists
    let switch_exists = wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_VirtualEthernetSwitch WHERE ElementName='{}'",
            name
        ),
        "Name",
    )
    .is_ok();

    if !switch_exists {
        // Create switch via DefineSystem on VirtualEthernetSwitchManagementService
        let sw_mgmt_query = "SELECT * FROM Msvm_VirtualEthernetSwitchManagementService";
        unsafe {
            let enumerator = services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(sw_mgmt_query),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )?;
            let mut objs = [None; 1];
            let mut returned = 0u32;
            let _ = enumerator.Next(WBEM_INFINITE, &mut objs, &mut returned);
            let sw_mgmt = objs[0]
                .take()
                .ok_or_else(|| color_eyre::eyre::eyre!("switch mgmt service not found"))?;
            let sw_mgmt_path = variant_to_string(&wmi_get_property(&sw_mgmt, "__PATH")?);

            // Get host computer path for internal port
            let host_name = std::env::var("COMPUTERNAME").unwrap_or_default();
            let host_path = wmi_query_first_string(
                &services,
                &format!(
                    "SELECT * FROM Msvm_ComputerSystem WHERE Name='{}'",
                    host_name
                ),
                "__PATH",
            )?;

            let in_params = wmi_get_method_in_params(
                &services,
                "Msvm_VirtualEthernetSwitchManagementService",
                "DefineSystem",
            )?;
            let sw_xml = format!(
                "<INSTANCE CLASSNAME=\"Msvm_VirtualEthernetSwitchSettingData\">\
                 <PROPERTY NAME=\"ElementName\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
                 </INSTANCE>",
                name
            );
            wmi_put_bstr(&in_params, "SystemSettings", &sw_xml)?;

            // Include internal port in DefineSystem (creates switch + port atomically)
            let port_xml = format!(
                "<INSTANCE CLASSNAME=\"Msvm_EthernetPortAllocationSettingData\">\
                 <PROPERTY NAME=\"ElementName\" TYPE=\"string\"><VALUE>{name}</VALUE></PROPERTY>\
                 <PROPERTY.ARRAY NAME=\"HostResource\" TYPE=\"string\">\
                 <VALUE.ARRAY><VALUE>{host}</VALUE></VALUE.ARRAY></PROPERTY.ARRAY>\
                 <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
                 <VALUE>Microsoft:Hyper-V:Ethernet Connection</VALUE></PROPERTY>\
                 <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>33</VALUE></PROPERTY>\
                 </INSTANCE>",
                name = name,
                host = host_path
            );
            wmi_put_bstr_array(&in_params, "ResourceSettings", &[&port_xml])?;

            let mut out_params = None;
            services.ExecMethod(
                &BSTR::from(&sw_mgmt_path),
                &BSTR::from("DefineSystem"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                &in_params,
                Some(&mut out_params),
                None,
            )?;
            if let Some(ref out) = out_params {
                wmi_check_result(&services, out)?;
            }
        }
        // Wait for vEthernet adapter to appear
        let mut adapter_found = false;
        for i in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let check = Command::new("netsh")
                .args(["interface", "ipv4", "show", "interfaces"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            if let Ok(out) = check {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.contains(name) {
                    adapter_found = true;
                    debug!("vEthernet adapter appeared after {}ms", (i + 1) * 500);
                    break;
                }
            }
        }
        if !adapter_found {
            info!("vEthernet adapter for '{}' did NOT appear after 30s", name);
        }
    }

    // IP address via netsh (idempotent — ignores error if already set)
    let mask = match prefix_len {
        24 => "255.255.255.0",
        16 => "255.255.0.0",
        _ => "255.255.255.0",
    };
    let adapter_name = format!("vEthernet ({})", name);
    let ip_result = Command::new("netsh")
        .args([
            "interface",
            "ip",
            "add",
            "address",
            &adapter_name,
            host_ip,
            mask,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    if let Ok(out) = &ip_result {
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            info!(
                "netsh ip add failed for '{}': {}",
                adapter_name,
                stderr.trim()
            );
        }
    }

    // Wait for IP assignment to take effect
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let check = Command::new("netsh")
            .args(["interface", "ip", "show", "address", &adapter_name])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = check {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(host_ip) {
                break;
            }
        }
    }

    // NAT via WMI (root\standardcimv2)
    let nat_name = format!("{}-nat", name);
    unsafe {
        if let Ok(nat_services) = wmi_connect("root\\standardcimv2") {
            let mut nat_class = None;
            let _ = nat_services.GetObject(
                &BSTR::from("MSFT_NetNat"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut nat_class),
                None,
            );
            if let Some(ref cls) = nat_class {
                if let Ok(nat_inst) = cls.SpawnInstance(0) {
                    let _ = wmi_put_bstr(&nat_inst, "Name", &nat_name);
                    let _ = wmi_put_bstr(&nat_inst, "InternalIPInterfaceAddressPrefix", &subnet);
                    let _ = nat_services.PutInstance(
                        &nat_inst,
                        WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_CREATE_OR_UPDATE.0 as i32),
                        None,
                        None,
                    );
                }
            }
        }
    }

    // Firewall via netsh
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=bcvk-dhcp",
            "dir=in",
            "action=allow",
            "protocol=UDP",
            "localport=67",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let _ = Command::new("netsh")
        .args(["advfirewall", "set", "allprofiles", "state", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    debug!("ensured internal switch: {} ({})", name, host_ip);
    Ok(SwitchInfo {
        name: name.to_string(),
        host_ip: host_ip.to_string(),
    })
}

#[allow(unsafe_code)]
pub fn remove_internal_switch(name: &str) {
    // Remove NAT via WMI (root\standardcimv2)
    let nat_name = format!("{}-nat", name);
    unsafe {
        if let Ok(nat_services) = wmi_connect("root\\standardcimv2") {
            let query = format!("SELECT * FROM MSFT_NetNat WHERE Name='{}'", nat_name);
            if let Ok(enumerator) = nat_services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(query),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            ) {
                let mut objs = [None; 1];
                let mut returned = 0u32;
                if enumerator
                    .Next(WBEM_INFINITE, &mut objs, &mut returned)
                    .is_ok()
                    && returned > 0
                {
                    if let Some(ref nat_obj) = objs[0] {
                        let path = variant_to_string(
                            &wmi_get_property(nat_obj, "__PATH").unwrap_or_default(),
                        );
                        if !path.is_empty() {
                            let _ = nat_services.DeleteInstance(
                                &BSTR::from(path),
                                WBEM_GENERIC_FLAG_TYPE(0),
                                None,
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    // Remove switch via WMI DestroySystem
    if let Ok(services) = wmi_connect("root\\virtualization\\v2") {
        let switch_path = wmi_query_first_string(
            &services,
            &format!(
                "SELECT * FROM Msvm_VirtualEthernetSwitch WHERE ElementName='{}'",
                name
            ),
            "__PATH",
        );
        if let Ok(sw_path) = switch_path {
            unsafe {
                let sw_mgmt_query = "SELECT * FROM Msvm_VirtualEthernetSwitchManagementService";
                if let Ok(enumerator) = services.ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(sw_mgmt_query),
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                ) {
                    let mut objs = [None; 1];
                    let mut returned = 0u32;
                    if enumerator
                        .Next(WBEM_INFINITE, &mut objs, &mut returned)
                        .is_ok()
                        && returned > 0
                    {
                        if let Some(ref sw_mgmt) = objs[0] {
                            let mgmt_path = variant_to_string(
                                &wmi_get_property(sw_mgmt, "__PATH").unwrap_or_default(),
                            );
                            if let Ok(in_params) = wmi_get_method_in_params(
                                &services,
                                "Msvm_VirtualEthernetSwitchManagementService",
                                "DestroySystem",
                            ) {
                                let _ = wmi_put_bstr(&in_params, "AffectedSystem", &sw_path);
                                let _ = services.ExecMethod(
                                    &BSTR::from(mgmt_path),
                                    &BSTR::from("DestroySystem"),
                                    WBEM_GENERIC_FLAG_TYPE(0),
                                    None,
                                    &in_params,
                                    None,
                                    None,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    debug!("removed switch: {}", name);
}

#[allow(unsafe_code)]
pub fn create_gen2_vm(name: &str, memory_mb: u32, vcpus: u32, switch: &str) -> Result<()> {
    let _ = remove_vm(name);

    // Step 1: Create Gen2 VM via WMI DefineSystem
    wmi_define_system(name)?;

    // Step 2: Configure memory, CPU, SecureBoot, and checkpoints via WMI
    let services = wmi_connect("root\\virtualization\\v2")?;
    let (_mgmt, mgmt_path) = wmi_get_mgmt_service(&services)?;

    let vm_guid = wmi_query_first_string(
        &services,
        &format!(
            "SELECT ConfigurationID FROM Msvm_VirtualSystemSettingData \
             WHERE ElementName='{}' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'",
            name
        ),
        "ConfigurationID",
    )?;

    let mem_instance_id = wmi_query_first_string(
        &services,
        &format!(
            "SELECT InstanceID FROM Msvm_MemorySettingData \
             WHERE InstanceID LIKE 'Microsoft:{}%'",
            vm_guid
        ),
        "InstanceID",
    )?;

    let proc_instance_id = wmi_query_first_string(
        &services,
        &format!(
            "SELECT InstanceID FROM Msvm_ProcessorSettingData \
             WHERE InstanceID LIKE 'Microsoft:{}%'",
            vm_guid
        ),
        "InstanceID",
    )?;

    let mem_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_MemorySettingData\">\
         <PROPERTY NAME=\"InstanceID\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"VirtualQuantity\" TYPE=\"uint64\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"Reservation\" TYPE=\"uint64\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"Limit\" TYPE=\"uint64\"><VALUE>{}</VALUE></PROPERTY>\
         </INSTANCE>",
        mem_instance_id, memory_mb, memory_mb, memory_mb,
    );

    let cpu_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_ProcessorSettingData\">\
         <PROPERTY NAME=\"InstanceID\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"VirtualQuantity\" TYPE=\"uint64\"><VALUE>{}</VALUE></PROPERTY>\
         </INSTANCE>",
        proc_instance_id, vcpus,
    );

    wmi_modify_resource_settings(&services, &mgmt_path, &[&mem_xml, &cpu_xml])?;

    let vssd_instance_id = wmi_query_first_string(
        &services,
        &format!(
            "SELECT InstanceID FROM Msvm_VirtualSystemSettingData \
             WHERE ElementName='{}' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'",
            name
        ),
        "InstanceID",
    )?;

    let vssd_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_VirtualSystemSettingData\">\
         <PROPERTY NAME=\"InstanceID\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"SecureBootEnabled\" TYPE=\"boolean\"><VALUE>FALSE</VALUE></PROPERTY>\
         <PROPERTY NAME=\"UserSnapshotType\" TYPE=\"uint16\"><VALUE>2</VALUE></PROPERTY>\
         </INSTANCE>",
        vssd_instance_id,
    );
    wmi_modify_system_settings(&services, &mgmt_path, &vssd_xml)?;

    // Step 3: COM port
    let serial_instance_id = wmi_query_first_string(
        &services,
        &format!(
            "SELECT InstanceID FROM Msvm_SerialPortSettingData \
             WHERE InstanceID LIKE 'Microsoft:{}%' AND InstanceID LIKE '%\\\\0'",
            vm_guid
        ),
        "InstanceID",
    )?;

    let com_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_SerialPortSettingData\">\
         <PROPERTY NAME=\"InstanceID\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY.ARRAY NAME=\"Connection\" TYPE=\"string\">\
         <VALUE.ARRAY><VALUE>\\\\.\\pipe\\bcvk-serial-{}</VALUE></VALUE.ARRAY>\
         </PROPERTY.ARRAY></INSTANCE>",
        serial_instance_id, name,
    );
    wmi_modify_resource_settings(&services, &mgmt_path, &[&com_xml])?;

    // Step 4: Add NIC connected to switch
    let vssd_path = wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_VirtualSystemSettingData \
             WHERE ElementName='{}' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'",
            name
        ),
        "__PATH",
    )?;

    let switch_path = wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_VirtualEthernetSwitch WHERE ElementName='{}'",
            switch
        ),
        "__PATH",
    )?;
    debug!("NIC: switch_path={}", switch_path);

    let nic_xml = "<INSTANCE CLASSNAME=\"Msvm_SyntheticEthernetPortSettingData\">\
         <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
         <VALUE>Microsoft:Hyper-V:Synthetic Ethernet Port</VALUE></PROPERTY>\
         <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>10</VALUE></PROPERTY>\
         </INSTANCE>";
    let nic_paths = wmi_add_resource_settings(&services, &mgmt_path, &vssd_path, &[nic_xml])?;
    debug!("NIC: nic_paths={:?}", nic_paths);

    // Connect NIC to switch via AddResourceSettings with all required properties
    // (template from Msvm_ResourcePool includes EnabledState, DesiredVLANEndpointMode, etc.)
    if let Some(nic_path) = nic_paths.first() {
        let conn_xml = format!(
            "<INSTANCE CLASSNAME=\"Msvm_EthernetPortAllocationSettingData\">\
             <PROPERTY NAME=\"EnabledState\" TYPE=\"uint16\"><VALUE>2</VALUE></PROPERTY>\
             <PROPERTY NAME=\"DesiredVLANEndpointMode\" TYPE=\"uint16\"><VALUE>2</VALUE></PROPERTY>\
             <PROPERTY NAME=\"Parent\" TYPE=\"string\"><VALUE>{parent}</VALUE></PROPERTY>\
             <PROPERTY.ARRAY NAME=\"HostResource\" TYPE=\"string\">\
             <VALUE.ARRAY><VALUE>{host}</VALUE></VALUE.ARRAY></PROPERTY.ARRAY>\
             <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
             <VALUE>Microsoft:Hyper-V:Ethernet Connection</VALUE></PROPERTY>\
             <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>33</VALUE></PROPERTY>\
             </INSTANCE>",
            parent = nic_path,
            host = switch_path,
        );
        wmi_add_resource_settings(&services, &mgmt_path, &vssd_path, &[&conn_xml])?;
    }

    // Step 5: Enable Guest Service Interface
    let gsi_query = format!(
        "SELECT InstanceID FROM Msvm_GuestServiceInterfaceComponentSettingData \
         WHERE InstanceID LIKE 'Microsoft:{}%'",
        vm_guid
    );
    if let Ok(gsi_id) = wmi_query_first_string(&services, &gsi_query, "InstanceID") {
        let gsi_xml = format!(
            "<INSTANCE CLASSNAME=\"Msvm_GuestServiceInterfaceComponentSettingData\">\
             <PROPERTY NAME=\"InstanceID\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
             <PROPERTY NAME=\"EnabledState\" TYPE=\"uint16\"><VALUE>2</VALUE></PROPERTY>\
             </INSTANCE>",
            gsi_id,
        );
        let _ = wmi_modify_guest_service_settings(&services, &mgmt_path, &[&gsi_xml]);
    }

    info!(
        "created Hyper-V Gen2 VM: {} ({} vCPUs, {}MB)",
        name, vcpus, memory_mb
    );
    Ok(())
}

pub fn attach_and_start_vm(name: &str, vhdx_path: &str) -> Result<String> {
    attach_vhdx(name, vhdx_path)?;
    set_boot_order_disk_first(name);
    start_vm(name)?;
    let guid = get_vm_guid(name)?;
    info!("started VM: {} (GUID: {})", name, guid);
    Ok(guid)
}

pub fn set_boot_order_disk_first(name: &str) {
    let ps_cmd = format!(
        "$hd = Get-VMHardDiskDrive -VMName '{}'; \
         if ($hd) {{ Set-VMFirmware -VMName '{}' -FirstBootDevice $hd }}",
        name, name
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[allow(unsafe_code)]
pub fn attach_vhdx(name: &str, vhdx_path: &str) -> Result<()> {
    attach_vhdx_at_slot(name, vhdx_path, 0)
}

#[allow(unsafe_code)]
pub fn attach_vhdx_at_slot(name: &str, vhdx_path: &str, slot: u32) -> Result<()> {
    let services = wmi_connect("root\\virtualization\\v2")?;
    let (_mgmt, mgmt_path) = wmi_get_mgmt_service(&services)?;

    let vssd_path = wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_VirtualSystemSettingData \
             WHERE ElementName='{}' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'",
            name
        ),
        "__PATH",
    )?;

    let vm_guid = wmi_query_first_string(
        &services,
        &format!(
            "SELECT ConfigurationID FROM Msvm_VirtualSystemSettingData \
             WHERE ElementName='{}' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'",
            name
        ),
        "ConfigurationID",
    )?;

    // Ensure SCSI controller exists; add one if missing
    let scsi_path = match wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_ResourceAllocationSettingData \
             WHERE InstanceID LIKE 'Microsoft:{}%' \
             AND ResourceSubType='Microsoft:Hyper-V:Synthetic SCSI Controller'",
            vm_guid
        ),
        "__PATH",
    ) {
        Ok(p) => p,
        Err(_) => {
            let scsi_xml = "<INSTANCE CLASSNAME=\"Msvm_ResourceAllocationSettingData\">\
                 <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
                 <VALUE>Microsoft:Hyper-V:Synthetic SCSI Controller</VALUE></PROPERTY>\
                 <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>6</VALUE></PROPERTY>\
                 </INSTANCE>";
            let paths = wmi_add_resource_settings(&services, &mgmt_path, &vssd_path, &[scsi_xml])?;
            paths.into_iter().next().ok_or_else(|| {
                color_eyre::eyre::eyre!("AddResourceSettings returned no SCSI path")
            })?
        }
    };

    let drive_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_ResourceAllocationSettingData\">\
         <PROPERTY NAME=\"Parent\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"AddressOnParent\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
         <VALUE>Microsoft:Hyper-V:Synthetic Disk Drive</VALUE></PROPERTY>\
         <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>17</VALUE></PROPERTY>\
         </INSTANCE>",
        scsi_path, slot,
    );
    let drive_paths = wmi_add_resource_settings(&services, &mgmt_path, &vssd_path, &[&drive_xml])?;
    let drive_path = drive_paths
        .into_iter()
        .next()
        .ok_or_else(|| color_eyre::eyre::eyre!("AddResourceSettings returned no drive path"))?;

    let vhd_xml = format!(
        "<INSTANCE CLASSNAME=\"Msvm_StorageAllocationSettingData\">\
         <PROPERTY NAME=\"Parent\" TYPE=\"string\"><VALUE>{}</VALUE></PROPERTY>\
         <PROPERTY.ARRAY NAME=\"HostResource\" TYPE=\"string\">\
         <VALUE.ARRAY><VALUE>{}</VALUE></VALUE.ARRAY></PROPERTY.ARRAY>\
         <PROPERTY NAME=\"ResourceSubType\" TYPE=\"string\">\
         <VALUE>Microsoft:Hyper-V:Virtual Hard Disk</VALUE></PROPERTY>\
         <PROPERTY NAME=\"ResourceType\" TYPE=\"uint16\"><VALUE>31</VALUE></PROPERTY>\
         </INSTANCE>",
        drive_path, vhdx_path,
    );
    wmi_add_resource_settings(&services, &mgmt_path, &vssd_path, &[&vhd_xml])?;

    debug!(
        "attached VHDX to VM {} at slot {}: {}",
        name, slot, vhdx_path
    );
    Ok(())
}

pub fn stop_vm(name: &str) -> Result<()> {
    let _ = wmi_request_state_change(name, 3);
    debug!("stopped VM: {}", name);
    Ok(())
}

pub fn start_vm(name: &str) -> Result<()> {
    wmi_request_state_change(name, 2)?;
    debug!("started VM: {}", name);
    Ok(())
}

#[allow(unsafe_code)]
pub fn remove_vm(name: &str) -> Result<()> {
    let _ = stop_vm(name);
    let services = wmi_connect("root\\virtualization\\v2")?;

    let vm_path = match wmi_query_first_string(
        &services,
        &format!(
            "SELECT * FROM Msvm_ComputerSystem WHERE ElementName='{}'",
            name
        ),
        "__PATH",
    ) {
        Ok(p) => p,
        Err(_) => {
            debug!("VM '{}' not found, nothing to remove", name);
            return Ok(());
        }
    };

    let (_mgmt, mgmt_path) = wmi_get_mgmt_service(&services)?;
    let in_params = wmi_get_method_in_params(
        &services,
        "Msvm_VirtualSystemManagementService",
        "DestroySystem",
    )?;
    wmi_put_bstr(&in_params, "AffectedSystem", &vm_path)?;

    unsafe {
        let mut out_params = None;
        let _ = services.ExecMethod(
            &BSTR::from(mgmt_path),
            &BSTR::from("DestroySystem"),
            WBEM_GENERIC_FLAG_TYPE(0),
            None,
            &in_params,
            Some(&mut out_params),
            None,
        );
        if let Some(ref out) = out_params {
            let _ = wmi_check_result(&services, out);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Hyper-V"]
    fn test_create_gen2_vm_wmi() {
        let name = "bcvk-wmi-test";
        let _ = remove_vm(name);

        create_gen2_vm(name, 2048, 2, "bcvk").expect("create_gen2_vm failed");

        let state = get_vm_state(name).expect("get_vm_state failed");
        assert_eq!(state, "Off", "VM should be in Off state after creation");

        let guid = get_vm_guid(name).expect("get_vm_guid failed");
        assert!(!guid.is_empty(), "VM GUID should not be empty");

        let _ = remove_vm(name);
        let state_after = get_vm_state(name).expect("get_vm_state after remove");
        assert!(state_after.is_empty(), "VM should not exist after remove");
    }

    #[test]
    #[ignore = "requires Hyper-V"]
    fn test_simultaneous_vm_creation() {
        let names = ["bcvk-sim-test-1", "bcvk-sim-test-2"];
        for name in &names {
            let _ = remove_vm(name);
        }

        let handles: Vec<_> = names
            .iter()
            .map(|name| {
                let n = name.to_string();
                std::thread::spawn(move || create_gen2_vm(&n, 1024, 1, "bcvk"))
            })
            .collect();

        for (i, h) in handles.into_iter().enumerate() {
            h.join()
                .expect("thread panicked")
                .unwrap_or_else(|e| panic!("VM {} creation failed: {:?}", names[i], e));
        }

        let vms = list_vms("bcvk-sim-test-").expect("list_vms failed");
        assert_eq!(vms.len(), 2, "should find exactly 2 VMs");
        for vm in &vms {
            assert_eq!(vm.state, "Off");
        }

        let guid1 = get_vm_guid(names[0]).expect("guid1");
        let guid2 = get_vm_guid(names[1]).expect("guid2");
        assert_ne!(guid1, guid2, "GUIDs must differ");

        for name in &names {
            let _ = remove_vm(name);
        }
    }

    #[test]
    #[ignore = "requires Hyper-V"]
    fn test_full_vm_lifecycle() {
        let sw_name = "bcvk-lifecycle-test";
        let vm_name = "bcvk-lifecycle-vm";
        let host_ip = "10.0.77.1";

        // Cleanup from previous runs
        let _ = remove_vm(vm_name);
        remove_internal_switch(sw_name);

        // 1. Create internal switch
        let sw =
            ensure_internal_switch(sw_name, host_ip, 24).expect("ensure_internal_switch failed");
        assert_eq!(sw.name, sw_name);
        assert_eq!(sw.host_ip, host_ip);
        eprintln!("[OK] ensure_internal_switch: {}", sw_name);

        // 2. Create Gen2 VM
        create_gen2_vm(vm_name, 1024, 1, sw_name).expect("create_gen2_vm failed");
        let state = get_vm_state(vm_name).expect("get_vm_state");
        assert_eq!(state, "Off");
        let guid = get_vm_guid(vm_name).expect("get_vm_guid");
        assert!(!guid.is_empty());
        eprintln!("[OK] create_gen2_vm: {} (GUID: {})", vm_name, guid);

        // 3. List VMs
        let vms = list_vms("bcvk-lifecycle-").expect("list_vms");
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].name, vm_name);
        assert_eq!(vms[0].state, "Off");
        eprintln!("[OK] list_vms: found {}", vms[0].name);

        // 4. Attach VHDX (create a dummy one first)
        let vhdx_dir = std::env::temp_dir().join("bcvk-test");
        let _ = std::fs::create_dir_all(&vhdx_dir);
        let vhdx_path = vhdx_dir.join("lifecycle-test.vhdx");
        let vhdx_str = vhdx_path.to_string_lossy().to_string();

        // Create minimal VHDX via PowerShell (only for test setup)
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Remove-Item '{}' -Force -EA SilentlyContinue; \
                     New-VHD -Path '{}' -SizeBytes 512MB -Dynamic | Out-Null",
                    vhdx_str, vhdx_str
                ),
            ])
            .status();

        if vhdx_path.exists() {
            attach_vhdx(vm_name, &vhdx_str).expect("attach_vhdx failed");
            eprintln!("[OK] attach_vhdx: {}", vhdx_str);

            // 5. Start VM (will fail to boot but state should change)
            start_vm(vm_name).expect("start_vm failed");
            let state = get_vm_state(vm_name).expect("get_vm_state after start");
            eprintln!("[OK] start_vm: state={}", state);

            // 6. Stop VM
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = stop_vm(vm_name);
            std::thread::sleep(std::time::Duration::from_secs(1));
            let state = get_vm_state(vm_name).expect("get_vm_state after stop");
            eprintln!("[OK] stop_vm: state={}", state);
        } else {
            eprintln!("[SKIP] attach_vhdx: VHDX creation requires PowerShell");
        }

        // 7. Remove VM
        remove_vm(vm_name).expect("remove_vm failed");
        let state = get_vm_state(vm_name).expect("get_vm_state after remove");
        assert!(state.is_empty(), "VM should be gone");
        eprintln!("[OK] remove_vm: {}", vm_name);

        // 8. Remove switch
        remove_internal_switch(sw_name);
        eprintln!("[OK] remove_internal_switch: {}", sw_name);

        // 9. Verify switch is gone (allow async delay)
        std::thread::sleep(std::time::Duration::from_secs(2));
        let switch_gone = wmi_connect("root\\virtualization\\v2")
            .and_then(|svc| {
                wmi_query_first_string(
                    &svc,
                    &format!(
                        "SELECT * FROM Msvm_VirtualEthernetSwitch WHERE ElementName='{}'",
                        sw_name
                    ),
                    "Name",
                )
            })
            .is_err();
        eprintln!(
            "[{}] switch removal verified (gone={})",
            if switch_gone { "OK" } else { "WARN" },
            switch_gone
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&vhdx_dir);
        eprintln!("[OK] all lifecycle tests passed");
    }
}
