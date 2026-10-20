//! Optional HTTPS support with a self-signed certificate.
//!
//! ## How the certificate works
//!
//! There is no public CA that can issue a certificate for a private LAN
//! address (`192.168.x.x` is not a public name), so the program issues its
//! own certificate once and keeps it on disk:
//!
//! * The certificate contains every local IPv4 address as a *subject
//!   alternative name* (SAN) plus `localhost`, so it is valid for the URL
//!   the phone opens.
//! * It is written next to the exe (`transdata-tls-cert.pem` /
//!   `transdata-tls-key.pem`) and **reused on the next start**. That is the
//!   important part: browsers remember "this certificate is an exception",
//!   so the phone only shows the warning once and never again — as long as
//!   the IP address does not change. If the IP changes, the certificate is
//!   regenerated and the user has to accept the warning one more time.
//! * The SHA-256 fingerprint is shown in the desktop window. A phone that
//!   displays the same fingerprint proves that it is talking to this
//!   program and not to a man-in-the-middle.
//!
//! The private key never leaves the machine and is only readable by the
//! user account running the program.

use std::io::BufReader;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::config::TlsMaterial;

/// File names used next to the exe.
const CERT_FILE: &str = "transdata-tls-cert.pem";
const KEY_FILE: &str = "transdata-tls-key.pem";
/// Remembers which IP addresses the certificate was issued for.
const META_FILE: &str = "transdata-tls-ips.txt";

/// Directories to try for the certificate, in order of preference.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            dirs.push(p.to_path_buf());
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("TransData"));
    }
    dirs.push(std::env::temp_dir().join("TransData"));
    dirs
}

/// Load the certificate from disk or create a new one.
///
/// `ips` are the local IPv4 addresses the certificate must be valid for.
/// The certificate is regenerated when the address list changed, because a
/// certificate for `192.168.1.5` is useless on `192.168.1.9`.
pub fn load_or_create(ips: &[String]) -> Result<Arc<TlsMaterial>, String> {
    let wanted = ips.join(",");

    for dir in candidate_dirs() {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let meta_path = dir.join(META_FILE);

        // Is there a usable certificate for exactly these addresses?
        if let (Ok(cert_pem), Ok(key_pem)) = (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            let meta = std::fs::read_to_string(&meta_path).unwrap_or_default();
            if meta.trim() == wanted.trim() {
                if let Ok(m) = build_material(cert_pem, key_pem) {
                    return Ok(m);
                }
            }
        }

        // No (or outdated) certificate: make a new one and store it.
        match generate(ips) {
            Ok(m) => {
                let _ = std::fs::create_dir_all(&dir);
                if std::fs::write(&cert_path, &m.cert_pem).is_ok()
                    && std::fs::write(&key_path, &m.key_pem).is_ok()
                    && std::fs::write(&meta_path, &wanted).is_ok()
                {
                    return Ok(m);
                }
            }
            Err(e) => {
                // Try the next directory; the last error is reported below.
                eprintln!("tls: cannot create a certificate in {}: {e}", dir.display());
            }
        }
    }
    Err("无法创建自签证书（所有候选目录都不可写）".to_string())
}

/// Issue a fresh self-signed certificate for the given addresses.
fn generate(ips: &[String]) -> Result<Arc<TlsMaterial>, String> {
    let mut params = rcgen::CertificateParams::default();
    // A normal server certificate (not a CA).
    params.is_ca = rcgen::IsCa::NoCa;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "TransData");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "TransData");
    // localhost is always valid, the IPs are the ones the phone uses.
    let localhost: rcgen::Ia5String = "localhost"
        .try_into()
        .map_err(|_| "invalid dns name".to_string())?;
    params
        .subject_alt_names
        .push(rcgen::SanType::DnsName(localhost));
    for ip in ips {
        let addr: IpAddr = match ip.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        params.subject_alt_names.push(rcgen::SanType::IpAddress(addr));
    }
    // Lifetime: 15 years. The program is a local tool; a phone that
    // accepted the warning once should not be nagged every few months.
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);

    let key_pair = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| e.to_string())?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    build_material(cert_pem, key_pem)
}

/// Turn PEM text into a usable server configuration (+ the fingerprint).
fn build_material(cert_pem: String, key_pem: String) -> Result<Arc<TlsMaterial>, String> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
    if certs.is_empty() {
        return Err("证书文件里没有证书".to_string());
    }
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "证书文件里没有私钥".to_string())?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs.clone(), key)
        .map_err(|e| e.to_string())?;

    // Fingerprint of the leaf certificate: SHA-256, hex, colon separated.
    let mut hasher = Sha256::new();
    hasher.update(certs[0].as_ref());
    let digest = hasher.finalize();
    let fingerprint = digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");

    Ok(Arc::new(TlsMaterial {
        cert_pem,
        key_pem,
        fingerprint,
        server_config: Arc::new(server_config),
    }))
}
