//! Wi-Fi through the native WLAN API (no locale-dependent netsh parsing),
//! and the internet probe.

use crate::win::{from_wide, wide};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WiFi::*;

#[derive(Clone)]
pub struct Connection {
    pub profile: String,
    pub ssid: String,
    pub signal: u32,
}

struct Client(HANDLE);

impl Client {
    fn open() -> Option<Self> {
        let (mut ver, mut h) = (0u32, null_mut());
        (unsafe { WlanOpenHandle(2, null(), &mut ver, &mut h) } == 0).then_some(Client(h))
    }

    fn interfaces(&self) -> Vec<GUID> {
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
        if unsafe { WlanEnumInterfaces(self.0, null(), &mut list) } != 0 {
            return vec![];
        }
        let out = unsafe {
            let n = (*list).dwNumberOfItems as usize;
            let first = (*list).InterfaceInfo.as_ptr();
            (0..n).map(|i| (*first.add(i)).InterfaceGuid).collect()
        };
        unsafe { WlanFreeMemory(list.cast()) };
        out
    }

    fn connection(&self, iface: &GUID) -> Option<Connection> {
        let (mut size, mut data, mut kind) = (0u32, null_mut(), 0);
        if unsafe { WlanQueryInterface(self.0, iface, wlan_intf_opcode_current_connection, null(), &mut size, &mut data, &mut kind) } != 0 {
            return None;
        }
        let conn = unsafe {
            let a = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let ssid = &a.wlanAssociationAttributes.dot11Ssid;
            let len = (ssid.uSSIDLength as usize).min(32);
            (a.isState == wlan_interface_state_connected).then(|| Connection {
                profile: from_wide(&a.strProfileName),
                ssid: String::from_utf8_lossy(&ssid.ucSSID[..len]).into_owned(),
                signal: a.wlanAssociationAttributes.wlanSignalQuality,
            })
        };
        unsafe { WlanFreeMemory(data) };
        conn
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { WlanCloseHandle(self.0, null()) };
    }
}

/// The Wi-Fi connection right now, if any.
pub fn current() -> Option<Connection> {
    let c = Client::open()?;
    c.interfaces().iter().find_map(|i| c.connection(i))
}

/// "auto" or "manual" from the saved profile, if it exists.
pub fn profile_mode(profile: &str) -> Option<String> {
    let c = Client::open()?;
    let name = wide(profile);
    c.interfaces().iter().find_map(|iface| {
        let (mut xml, mut flags, mut access): (PWSTR, u32, u32) = (null_mut(), 0, 0);
        if unsafe { WlanGetProfile(c.0, iface, name.as_ptr(), null(), &mut xml, &mut flags, &mut access) } != 0 {
            return None;
        }
        let text = unsafe {
            let len = (0..).take_while(|&i| *xml.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(xml, len))
        };
        unsafe { WlanFreeMemory(xml.cast()) };
        let start = text.find("<connectionMode>")? + "<connectionMode>".len();
        let end = text[start..].find('<')? + start;
        Some(text[start..end].to_string())
    })
}

/// Connects to a saved profile on every Wi-Fi interface that has it.
pub fn reconnect(profile: &str) -> bool {
    let Some(c) = Client::open() else { return false };
    let name = wide(profile);
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: name.as_ptr(),
        pDot11Ssid: null_mut(),
        pDesiredBssidList: null_mut(),
        dot11BssType: dot11_BSS_type_any,
        dwFlags: 0,
    };
    c.interfaces().iter().any(|i| unsafe { WlanConnect(c.0, i, &params, null()) } == 0)
}

/// Same HTTP check Windows uses for "No internet". Ping is NOT used:
/// phone hotspots and proxies often block it (true on this machine).
pub fn internet_ok() -> bool {
    probe("www.msftconnecttest.com", "/connecttest.txt", |r| r.contains("Microsoft Connect Test"))
        // the status line only: a captive portal page could say "Content-Length: 204"
        || probe("connectivitycheck.gstatic.com", "/generate_204", |r| r.split(' ').nth(1) == Some("204"))
}

fn probe(host: &str, path: &str, ok: impl Fn(&str) -> bool) -> bool {
    let timeout = Duration::from_secs(5);
    let Some(addr) = (host, 80).to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else { return false };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: WorkingMode\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = s.take(4096).read_to_end(&mut buf);
    ok(&String::from_utf8_lossy(&buf))
}
