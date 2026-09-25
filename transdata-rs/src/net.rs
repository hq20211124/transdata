
//! Best-effort detection of the IP address a phone would reach on this machine.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub fn display_ip() -> String {
    if let Ok(ip) = outbound_ip() {
        return ip.to_string();
    }
    "127.0.0.1".to_string()
}

/// All non-loopback IPv4 addresses of this machine (e.g. a Windows mobile
/// hotspot adapter shows up as 192.168.137.1 next to the main NIC IP).
/// Handy for telling the phone which address to open when the default-route
/// IP (`display_ip`) is not the one the phone can reach.
pub fn all_ipv4s() -> Vec<String> {
    // This GUI app has no console: spawning a console program (ipconfig)
    // without CREATE_NO_WINDOW makes Windows allocate a visible black cmd
    // window for each run. Pass the flag so no console can ever pop up.
    let out = if cfg!(windows) {
        let mut cmd = std::process::Command::new("ipconfig");
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd.output().ok()
    } else {
        std::process::Command::new("sh").args(["-c", "ifconfig"]).output().ok()
    };
    let Some(out) = out.as_ref() else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ips: Vec<String> = Vec::new();
    for line in text.lines() {
        let ip;
        if line.trim_start().starts_with("IPv4") {
            // Windows: "   IPv4 Address. . . . . . . . . . . : 192.168.137.1"
            ip = line.split(':').last().unwrap_or("").trim().to_string();
        } else {
            // Unix: "inet 192.168.x.x netmask ..."
            let mut t = line.trim().split_whitespace();
            ip = if t.next() == Some("inet") {
                t.next().unwrap_or("").to_string()
            } else {
                String::new()
            };
        }
        let ok = ip.split('.').count() == 4
            && !ip.starts_with("127.")
            && !ip.starts_with("0.");
        if ok && !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    // Hotspot-range addresses first (least likely to collide with the main NIC).
    ips.sort_by_key(|ip| {
        if ip.starts_with("192.168.137.") { 0 } else { 1 }
    });
    ips
}

fn outbound_ip() -> Result<std::net::IpAddr, ()> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|_| ())?;
    let addr = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 80);
    let _ = sock.connect(addr);
    let local: std::net::SocketAddr = sock.local_addr().map_err(|_| ())?;
    Ok(local.ip())
}

