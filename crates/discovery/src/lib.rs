//! Host discovery over mDNS / DNS-SD (`_secondscrn._tcp.local.`).
//!
//! The host announces its name, control port and certificate fingerprint.
//! The fingerprint is public information (it is also sent in every TLS
//! handshake); it lets a tablet recognise a paired host even after its IP
//! address changed. Discovery is a convenience only: nothing received over
//! mDNS is trusted, authentication always happens on the TLS channel.
//!
//! On Android the app must hold a `WifiManager.MulticastLock` while browsing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use secondscrn_protocol::{MDNS_SERVICE_TYPE, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mDNS: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// A host seen on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAdvert {
    /// mDNS instance name (unique on the network).
    pub instance: String,
    pub name: String,
    /// Hex SHA-256 certificate fingerprint as announced (untrusted).
    pub fingerprint: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub protocol_version: u32,
}

/// Keeps the host's announcement alive until dropped.
pub struct Announcer {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Announcer {
    pub fn start(
        display_name: &str,
        fingerprint_hex: &str,
        port: u16,
    ) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        // Instance names must be unique; the fingerprint prefix disambiguates
        // two PCs with the same name.
        let instance = format!(
            "{}-{}",
            sanitize(display_name),
            &fingerprint_hex[..8.min(fingerprint_hex.len())]
        );
        let hostname = format!("{}.local.", sanitize(display_name).to_lowercase());
        let props: HashMap<String, String> = [
            ("name".to_string(), display_name.to_string()),
            ("fp".to_string(), fingerprint_hex.to_string()),
            ("v".to_string(), PROTOCOL_VERSION.to_string()),
        ]
        .into();
        let info = ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &hostname, "", port, props)?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        tracing::info!(%instance, port, "announcing host via mDNS");
        Ok(Announcer { daemon, fullname })
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if s.is_empty() { "secondscrn".into() } else { s }
}

/// Parses a resolved service into an advert. Returns `None` for malformed or
/// incompatible announcements.
fn advert_from(info: &mdns_sd::ResolvedService) -> Option<HostAdvert> {
    let fp = info.get_property_val_str("fp")?.to_string();
    if fp.len() != 64 || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let name = info
        .get_property_val_str("name")
        .unwrap_or("Windows PC")
        .to_string();
    let version = info
        .get_property_val_str("v")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut addresses: Vec<IpAddr> = info
        .get_addresses()
        .iter()
        .map(|a| a.to_ip_addr())
        .collect();
    addresses.sort_by_key(|a| (!a.is_ipv4(), *a)); // prefer IPv4
    Some(HostAdvert {
        instance: info.get_fullname().to_string(),
        name,
        fingerprint: fp,
        addresses,
        port: info.get_port(),
        protocol_version: version,
    })
}

#[derive(Debug, Clone)]
pub enum BrowseEvent {
    Found(HostAdvert),
    Lost { instance: String },
}

/// Browses for hosts until dropped. Events are delivered via `recv`.
pub struct Browser {
    daemon: ServiceDaemon,
    rx: mdns_sd::Receiver<ServiceEvent>,
}

impl Browser {
    pub fn start() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
        Ok(Browser { daemon, rx })
    }

    /// Waits up to `timeout` for the next relevant event.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<BrowseEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let ev = self.rx.recv_timeout(remaining).ok()?;
            match ev {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(ad) = advert_from(&info) {
                        return Some(BrowseEvent::Found(ad));
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    return Some(BrowseEvent::Lost { instance: fullname });
                }
                _ => {}
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(MDNS_SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize("Nick's PC"), "Nick-s-PC");
        assert_eq!(sanitize(""), "secondscrn");
        assert_eq!(sanitize(&"x".repeat(100)).len(), 40);
    }

    /// Real multicast round trip. Ignored by default because CI runners and
    /// some firewalls block multicast; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn announce_and_browse() {
        let fp = "ab".repeat(32);
        let _a = Announcer::start("Test Host", &fp, 47999).unwrap();
        let b = Browser::start().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Some(BrowseEvent::Found(ad)) = b.recv_timeout(Duration::from_secs(1))
                && ad.fingerprint == fp
            {
                assert_eq!(ad.port, 47999);
                assert_eq!(ad.name, "Test Host");
                return;
            }
        }
        panic!("host not discovered");
    }
}
