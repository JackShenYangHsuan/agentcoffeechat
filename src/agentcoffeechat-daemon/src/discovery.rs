// Peer discovery via BLE (btleplug) and Bonjour/mDNS (mdns-sd).
//
// `DiscoveryService` runs both mechanisms concurrently and delivers de-duplicated
// `DiscoveredPeer` values to the caller through a `tokio::sync::mpsc` channel.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// BLE imports
// ---------------------------------------------------------------------------

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager as BleManager;
use futures::StreamExt;

// ---------------------------------------------------------------------------
// mDNS imports
// ---------------------------------------------------------------------------

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Custom 128-bit service UUID for AgentCoffeeChat BLE advertisements.
/// Generated deterministically from the namespace "agentcoffeechat.dev".
#[allow(dead_code)]
const BLE_SERVICE_UUID: Uuid = Uuid::from_bytes([
    0xAC, 0xC0, 0xFF, 0xEE, // "acc0ffee"
    0xCA, 0xFE,             // "cafe"
    0x40, 0x01,             // version nibble + "001"
    0x80, 0x01,             // variant
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
]);

/// mDNS service type for AgentCoffeeChat.
const MDNS_SERVICE_TYPE: &str = "_agentcoffeechat._udp.local.";

/// Protocol version byte included in BLE advertisement payloads.
const PROTOCOL_VERSION: u8 = 1;

/// How often to re-check the BLE peripheral list (seconds).
#[allow(dead_code)]
const BLE_POLL_INTERVAL_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How a peer was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySource {
    Ble,
    Mdns,
}

/// A peer discovered on the local network or via BLE.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub name: String,
    pub fingerprint_prefix: String,
    pub quic_port: u16,
    pub project_hash: [u8; 4],
    pub address: Option<IpAddr>,
    pub source: DiscoverySource,
    /// BLE signal strength (only present for BLE-discovered peers).
    pub rssi: Option<i16>,
}

/// Configuration supplied by the caller when starting discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Human-readable name advertised to other peers (max 16 bytes).
    pub display_name: String,
    /// First 8 bytes of our identity fingerprint, hex-encoded (16 chars).
    pub fingerprint_prefix: String,
    /// The QUIC port we are listening on.
    pub quic_port: u16,
    /// A 4-byte hash representing the current project.
    pub project_hash: [u8; 4],
}

// ---------------------------------------------------------------------------
// BLE advertisement payload helpers
// ---------------------------------------------------------------------------

/// Encode our advertisement payload into the service-data blob attached to
/// [`BLE_SERVICE_UUID`].
///
/// Layout:
///   [0]       protocol version  (1 byte)
///   [1..9]    fingerprint prefix (8 bytes, raw)
///   [9..11]   QUIC port          (2 bytes, big-endian)
///   [11..15]  project hash       (4 bytes)
///   [15..31]  display name       (up to 16 UTF-8 bytes, zero-padded)
pub fn encode_ble_payload(cfg: &DiscoveryConfig) -> Vec<u8> {
    let mut buf = Vec::with_capacity(31);

    // version
    buf.push(PROTOCOL_VERSION);

    // fingerprint prefix — take the first 16 hex chars, decode to 8 raw bytes.
    let fp_hex: String = cfg
        .fingerprint_prefix
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(16)
        .collect();
    let mut fp_bytes = [0u8; 8];
    for (i, chunk) in fp_hex.as_bytes().chunks(2).enumerate() {
        if i >= 8 {
            break;
        }
        let hex_str = std::str::from_utf8(chunk).unwrap_or("00");
        fp_bytes[i] = u8::from_str_radix(hex_str, 16).unwrap_or(0);
    }
    buf.extend_from_slice(&fp_bytes);

    // QUIC port
    buf.extend_from_slice(&cfg.quic_port.to_be_bytes());

    // project hash
    buf.extend_from_slice(&cfg.project_hash);

    // display name (up to 16 bytes, zero-padded)
    let name_bytes = cfg.display_name.as_bytes();
    let name_len = name_bytes.len().min(16);
    buf.extend_from_slice(&name_bytes[..name_len]);
    for _ in name_len..16 {
        buf.push(0);
    }

    buf
}

/// Try to decode a BLE advertisement payload produced by [`encode_ble_payload`].
pub fn decode_ble_payload(data: &[u8]) -> Option<DiscoveredPeer> {
    // Minimum: 1 + 8 + 2 + 4 + 1 = 16 bytes (at least 1 byte of name)
    if data.len() < 15 {
        return None;
    }

    let version = data[0];
    if version != PROTOCOL_VERSION {
        return None;
    }

    let fp_bytes = &data[1..9];
    let fingerprint_prefix = fp_bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let quic_port = u16::from_be_bytes([data[9], data[10]]);

    let mut project_hash = [0u8; 4];
    project_hash.copy_from_slice(&data[11..15]);

    // Name: remaining bytes, trim trailing zeros.
    let name_end = data.len().min(31);
    let name_bytes = &data[15..name_end];
    let name = String::from_utf8_lossy(name_bytes)
        .trim_end_matches('\0')
        .to_string();

    Some(DiscoveredPeer {
        name,
        fingerprint_prefix,
        quic_port,
        project_hash,
        address: None,
        source: DiscoverySource::Ble,
        rssi: None,
    })
}

// ---------------------------------------------------------------------------
// DiscoveryService
// ---------------------------------------------------------------------------

/// Manages concurrent BLE and mDNS/Bonjour discovery.
///
/// Call [`start()`](DiscoveryService::start) to begin advertising/scanning, then
/// read [`DiscoveredPeer`] values from the returned channel receiver.  Call
/// [`stop()`](DiscoveryService::stop) to tear everything down gracefully.
pub struct DiscoveryService {
    config: DiscoveryConfig,
    /// Sends a signal to background tasks so they terminate.
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Handles to spawned tasks so we can await their termination.
    tasks: Vec<JoinHandle<()>>,
    /// The mDNS daemon (kept alive so our service registration persists).
    mdns_daemon: Option<ServiceDaemon>,
    /// Fullname of the mDNS service we registered (needed for unregister).
    mdns_fullname: Option<String>,
}

impl DiscoveryService {
    /// Create a new `DiscoveryService` with the given configuration.
    pub fn new(config: DiscoveryConfig) -> Self {
        Self {
            config,
            shutdown_tx: None,
            tasks: Vec::new(),
            mdns_daemon: None,
            mdns_fullname: None,
        }
    }

    /// Start advertising and scanning.
    ///
    /// Returns a receiver that yields [`DiscoveredPeer`] values as they are
    /// found.  Peers discovered through both BLE and mDNS are deduplicated by
    /// fingerprint prefix.
    pub async fn start(&mut self) -> Result<mpsc::Receiver<DiscoveredPeer>> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Channel for discovered peers (before dedup).
        let (raw_tx, raw_rx) = mpsc::channel::<DiscoveredPeer>(128);

        // Channel for deduplicated peers sent to the caller.
        let (dedup_tx, dedup_rx) = mpsc::channel::<DiscoveredPeer>(128);

        // -- Log AWDL status -----------------------------------------------
        let (awdl_available, awdl_desc) = crate::awdl::awdl_status();
        if awdl_available {
            println!("[discovery] AWDL: {} — peers in proximity can connect without shared Wi-Fi", awdl_desc);
        } else {
            println!("[discovery] AWDL: {} — only LAN discovery available", awdl_desc);
        }

        // -- Start mDNS ---------------------------------------------------
        let mdns_result = self.start_mdns(raw_tx.clone(), shutdown_rx.clone());
        if let Err(e) = &mdns_result {
            eprintln!("[discovery] mDNS init failed: {:#}. Continuing without mDNS.", e);
        }

        // NOTE: BLE discovery is disabled in v1. btleplug requires a macOS app
        // bundle with Info.plist containing NSBluetoothAlwaysUsageDescription.
        // Running without a bundle causes a runtime panic. mDNS/Bonjour + AWDL
        // P2P provides equivalent functionality for local peer discovery.
        // TODO(v2): Re-enable BLE when shipping as a .app bundle.

        // -- Dedup task ----------------------------------------------------
        let dedup_shutdown_rx = shutdown_rx.clone();
        let dedup_handle = tokio::spawn(dedup_task(raw_rx, dedup_tx, dedup_shutdown_rx));
        self.tasks.push(dedup_handle);

        Ok(dedup_rx)
    }

    /// Stop advertising and scanning, clean up resources.
    pub async fn stop(&mut self) {
        // Signal all tasks to stop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        // Unregister mDNS service.
        if let (Some(daemon), Some(fullname)) =
            (self.mdns_daemon.take(), self.mdns_fullname.take())
        {
            match daemon.unregister(&fullname) {
                Ok(receiver) => {
                    // Best-effort wait for confirmation.
                    let _ = receiver.recv_timeout(Duration::from_secs(2));
                }
                Err(e) => {
                    eprintln!("[discovery] mDNS unregister error: {:#}", e);
                }
            }
            if let Err(e) = daemon.shutdown() {
                eprintln!("[discovery] mDNS shutdown error: {:#}", e);
            }
        }

        // Await spawned tasks.
        for handle in self.tasks.drain(..) {
            let _ = handle.await;
        }

        println!("[discovery] Stopped.");
    }

    // -----------------------------------------------------------------------
    // mDNS helpers
    // -----------------------------------------------------------------------

    /// Register our service and browse for peers via mDNS / Bonjour.
    fn start_mdns(
        &mut self,
        peer_tx: mpsc::Sender<DiscoveredPeer>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;

        // Build a unique instance name.
        let instance_name = format!(
            "{}-{}",
            self.config.display_name,
            &self.config.fingerprint_prefix[..8.min(self.config.fingerprint_prefix.len())]
        );

        // Get the local hostname for mDNS (fallback to a generated one).
        let hostname = get_local_hostname();

        let properties = [
            ("v", "1"),
            ("fp", self.config.fingerprint_prefix.as_str()),
            ("port", &self.config.quic_port.to_string()),
            (
                "proj",
                &self
                    .config
                    .project_hash
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>(),
            ),
        ];

        // We pass an empty IP string and use enable_addr_auto so the daemon
        // automatically fills in our interface addresses.
        let service_info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &instance_name,
            &hostname,
            "",            // auto-detect addresses
            self.config.quic_port,
            &properties[..],
        )
        .context("failed to build mDNS ServiceInfo")?
        .enable_addr_auto();

        let fullname = service_info.get_fullname().to_string();
        mdns.register(service_info)
            .context("failed to register mDNS service")?;

        println!(
            "[discovery] mDNS: registered as {} ({})",
            instance_name, fullname
        );

        // Browse for other AgentCoffeeChat services.
        let browse_receiver = mdns
            .browse(MDNS_SERVICE_TYPE)
            .context("failed to browse mDNS")?;

        self.mdns_daemon = Some(mdns);
        self.mdns_fullname = Some(fullname.clone());

        // Spawn a task to consume mDNS browse events.
        let own_fingerprint = self.config.fingerprint_prefix.clone();
        let handle = tokio::spawn(mdns_browse_task(
            browse_receiver,
            peer_tx,
            own_fingerprint,
            fullname,
            shutdown_rx,
        ));
        self.tasks.push(handle);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // BLE helpers (disabled in v1 — btleplug requires an app bundle on macOS)
    // -----------------------------------------------------------------------

    /// Start BLE scanning (and best-effort advertising via service data).
    #[allow(dead_code)]
    async fn start_ble(
        &mut self,
        peer_tx: mpsc::Sender<DiscoveredPeer>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let manager = BleManager::new()
            .await
            .context("BLE manager creation failed")?;

        let adapters = manager
            .adapters()
            .await
            .context("failed to list BLE adapters")?;

        let adapter = adapters
            .into_iter()
            .next()
            .context("no BLE adapter found")?;

        let adapter_info = adapter
            .adapter_info()
            .await
            .unwrap_or_else(|_| "unknown".into());
        println!("[discovery] BLE: using adapter {}", adapter_info);

        // Start scanning with a filter for our service UUID.
        adapter
            .start_scan(ScanFilter {
                services: vec![BLE_SERVICE_UUID],
            })
            .await
            .context("BLE start_scan failed")?;

        println!("[discovery] BLE: scanning for service UUID {}", BLE_SERVICE_UUID);

        let own_fingerprint = self.config.fingerprint_prefix.clone();
        let payload = encode_ble_payload(&self.config);

        let handle = tokio::spawn(ble_scan_task(
            adapter,
            peer_tx,
            own_fingerprint,
            payload,
            shutdown_rx,
        ));
        self.tasks.push(handle);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Consume mDNS browse events and forward parsed peers.
async fn mdns_browse_task(
    receiver: mdns_sd::Receiver<ServiceEvent>,
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    own_fingerprint: String,
    own_fullname: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut resolved_by_fullname: HashMap<String, DiscoveredPeer> = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            event = receiver.recv_async() => {
                match event {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        // Skip our own service.
                        if info.get_fullname() == own_fullname {
                            continue;
                        }

                        if let Some(peer) = parse_mdns_service(&info) {
                            // Skip our own fingerprint.
                            if peer.fingerprint_prefix == own_fingerprint {
                                continue;
                            }
                            resolved_by_fullname.insert(info.get_fullname().to_string(), peer.clone());
                            println!(
                                "[discovery] mDNS: found peer \"{}\" fp={} at {:?}",
                                peer.name, peer.fingerprint_prefix, peer.address
                            );
                            if peer_tx.send(peer).await.is_err() {
                                break; // receiver dropped
                            }
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_ty, fullname)) => {
                        println!("[discovery] mDNS: service removed: {}", fullname);
                        if let Some(mut peer) = resolved_by_fullname.remove(&fullname) {
                            peer.quic_port = 0;
                            peer.address = None;
                            if peer_tx.send(peer).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(_) => {
                        // SearchStarted, ServiceFound, SearchStopped — informational
                    }
                    Err(_) => {
                        // Channel closed; daemon was shut down.
                        break;
                    }
                }
            }
        }
    }
}

/// Parse a resolved mDNS `ServiceInfo` into a `DiscoveredPeer`.
fn parse_mdns_service(info: &ServiceInfo) -> Option<DiscoveredPeer> {
    let fp = info.get_property_val_str("fp").unwrap_or("");
    if fp.is_empty() {
        return None;
    }

    let port_str = info.get_property_val_str("port").unwrap_or("0");
    let quic_port: u16 = port_str.parse().unwrap_or(0);

    let proj_hex = info.get_property_val_str("proj").unwrap_or("00000000");
    let mut project_hash = [0u8; 4];
    for (i, chunk) in proj_hex.as_bytes().chunks(2).enumerate() {
        if i >= 4 {
            break;
        }
        let hex_str = std::str::from_utf8(chunk).unwrap_or("00");
        project_hash[i] = u8::from_str_radix(hex_str, 16).unwrap_or(0);
    }

    // Prefer an IPv4 address since the QUIC transport binds to 0.0.0.0.
    // Fall back to IPv6 if no IPv4 address is available.
    let addresses = info.get_addresses();
    let address = addresses
        .iter()
        .copied()
        .find(|a| a.is_ipv4())
        .or_else(|| addresses.iter().copied().next());

    // Derive a display name from the fullname: strip the service suffix.
    let fullname = info.get_fullname();
    let name = fullname
        .strip_suffix(&format!(".{}", MDNS_SERVICE_TYPE))
        .or_else(|| fullname.strip_suffix(MDNS_SERVICE_TYPE))
        .unwrap_or(fullname)
        .to_string();

    Some(DiscoveredPeer {
        name,
        fingerprint_prefix: fp.to_string(),
        quic_port,
        project_hash,
        address,
        source: DiscoverySource::Mdns,
        rssi: None,
    })
}

/// BLE scanning task: periodically polls discovered peripherals and looks for
/// our custom service data.
#[allow(dead_code)]
async fn ble_scan_task(
    adapter: btleplug::platform::Adapter,
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    own_fingerprint: String,
    _our_payload: Vec<u8>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // Try to get an event stream for real-time discovery, falling back to
    // periodic polling if that fails.
    let events_result = adapter.events().await;

    match events_result {
        Ok(mut event_stream) => {
            // Event-driven scanning.
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    event = event_stream.next() => {
                        match event {
                            Some(CentralEvent::DeviceDiscovered(id))
                            | Some(CentralEvent::DeviceUpdated(id)) => {
                                if let Ok(peripheral) = adapter.peripheral(&id).await {
                                    if let Ok(Some(props)) = peripheral.properties().await {
                                        if let Some(peer) = extract_ble_peer(&props, &own_fingerprint) {
                                            println!(
                                                "[discovery] BLE: found peer \"{}\" fp={} rssi={:?}",
                                                peer.name, peer.fingerprint_prefix, peer.rssi
                                            );
                                            if peer_tx.send(peer).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            Some(CentralEvent::ServiceDataAdvertisement { id, service_data }) => {
                                if let Some(data) = service_data.get(&BLE_SERVICE_UUID) {
                                    if let Some(mut peer) = decode_ble_payload(data) {
                                        if peer.fingerprint_prefix == own_fingerprint {
                                            continue;
                                        }
                                        // Try to get RSSI from the peripheral.
                                        if let Ok(periph) = adapter.peripheral(&id).await {
                                            if let Ok(Some(props)) = periph.properties().await {
                                                peer.rssi = props.rssi;
                                            }
                                        }
                                        println!(
                                            "[discovery] BLE (svc data): found peer \"{}\" fp={} rssi={:?}",
                                            peer.name, peer.fingerprint_prefix, peer.rssi
                                        );
                                        if peer_tx.send(peer).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            None => break, // stream ended
                            _ => {}
                        }
                    }
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[discovery] BLE event stream unavailable ({:#}), falling back to polling",
                e
            );
            // Polling-based fallback.
            let mut interval =
                tokio::time::interval(Duration::from_secs(BLE_POLL_INTERVAL_SECS));
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        if let Ok(peripherals) = adapter.peripherals().await {
                            for periph in peripherals {
                                if let Ok(Some(props)) = periph.properties().await {
                                    if let Some(peer) = extract_ble_peer(&props, &own_fingerprint) {
                                        println!(
                                            "[discovery] BLE (poll): found peer \"{}\" fp={}",
                                            peer.name, peer.fingerprint_prefix
                                        );
                                        if peer_tx.send(peer).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Best-effort: stop the scan when we exit.
    let _ = adapter.stop_scan().await;
}

/// Try to extract a [`DiscoveredPeer`] from BLE peripheral properties.
///
/// Checks both `service_data` (our custom payload) and the advertised service
/// list for [`BLE_SERVICE_UUID`].
#[allow(dead_code)]
fn extract_ble_peer(
    props: &btleplug::api::PeripheralProperties,
    own_fingerprint: &str,
) -> Option<DiscoveredPeer> {
    // First try service_data (our primary mechanism).
    if let Some(data) = props.service_data.get(&BLE_SERVICE_UUID) {
        if let Some(mut peer) = decode_ble_payload(data) {
            if peer.fingerprint_prefix == own_fingerprint {
                return None;
            }
            peer.rssi = props.rssi;
            return Some(peer);
        }
    }

    // If the device advertises our service UUID but has no service data, we
    // cannot decode a full peer struct.  Skip it.
    None
}

/// Deduplication task: receives raw peers from both BLE and mDNS and forwards
/// only unique ones (by fingerprint prefix) to the caller.  A peer is
/// re-emitted if we see it again from a *different* source (so the caller
/// learns about mDNS addresses for a BLE-discovered peer).
async fn dedup_task(
    mut raw_rx: mpsc::Receiver<DiscoveredPeer>,
    dedup_tx: mpsc::Sender<DiscoveredPeer>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // Map fingerprint_prefix -> set of sources we have already forwarded.
    let mut seen: HashMap<String, std::collections::HashSet<DiscoverySource>> = HashMap::new();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            peer_opt = raw_rx.recv() => {
                match peer_opt {
                    Some(peer) => {
                        let is_removal = peer.source == DiscoverySource::Mdns
                            && peer.quic_port == 0
                            && peer.address.is_none();
                        if is_removal {
                            seen.remove(&peer.fingerprint_prefix);
                            if dedup_tx.send(peer).await.is_err() {
                                break;
                            }
                            continue;
                        }

                        let sources = seen
                            .entry(peer.fingerprint_prefix.clone())
                            .or_default();

                        let is_new_source = sources.insert(peer.source.clone());
                        // Forward if new source, or if we now have an IPv4
                        // address (previous discovery may have only had IPv6).
                        let has_ipv4 = peer.address.map_or(false, |a| a.is_ipv4());
                        if is_new_source || has_ipv4 {
                            if dedup_tx.send(peer).await.is_err() {
                                break; // caller dropped receiver
                            }
                        }
                    }
                    None => break, // all senders dropped
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Derive a Hash for DiscoverySource so it can live in a HashSet.
impl std::hash::Hash for DiscoverySource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}

/// Return a hostname suitable for mDNS registration.
fn get_local_hostname() -> String {
    // On macOS, gethostname(2) typically returns "MyMac.local" or "MyMac".
    // We need a hostname ending in ".local." for mDNS.
    let raw = std::env::var("HOSTNAME")
        .or_else(|_| {
            // Fallback: read via libc.
            let mut buf = [0u8; 256];
            let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
            if rc == 0 {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                Ok(String::from_utf8_lossy(&buf[..len]).to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        })
        .unwrap_or_else(|_| "agentcoffeechat-host".to_string());

    let raw = raw.trim().to_string();

    if raw.ends_with(".local.") {
        raw
    } else if raw.ends_with(".local") {
        format!("{}.", raw)
    } else {
        format!("{}.local.", raw)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ble_payload_roundtrip() {
        let cfg = DiscoveryConfig {
            display_name: "alice".to_string(),
            fingerprint_prefix: "abcdef0123456789".to_string(),
            quic_port: 9443,
            project_hash: [0xDE, 0xAD, 0xBE, 0xEF],
        };

        let encoded = encode_ble_payload(&cfg);
        assert_eq!(encoded.len(), 31); // 1 + 8 + 2 + 4 + 16

        let peer = decode_ble_payload(&encoded).expect("decode should succeed");
        assert_eq!(peer.name, "alice");
        assert_eq!(peer.fingerprint_prefix, "abcdef0123456789");
        assert_eq!(peer.quic_port, 9443);
        assert_eq!(peer.project_hash, [0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(peer.source, DiscoverySource::Ble);
        assert!(peer.rssi.is_none());
    }

    #[test]
    fn ble_payload_short_name() {
        let cfg = DiscoveryConfig {
            display_name: "b".to_string(),
            fingerprint_prefix: "0000000000000000".to_string(),
            quic_port: 443,
            project_hash: [0, 0, 0, 0],
        };

        let encoded = encode_ble_payload(&cfg);
        let peer = decode_ble_payload(&encoded).unwrap();
        assert_eq!(peer.name, "b");
    }

    #[test]
    fn ble_payload_rejects_wrong_version() {
        let mut data = vec![0u8; 31];
        data[0] = 99; // wrong version
        assert!(decode_ble_payload(&data).is_none());
    }

    #[test]
    fn ble_payload_rejects_too_short() {
        assert!(decode_ble_payload(&[]).is_none());
        assert!(decode_ble_payload(&[1; 10]).is_none());
    }

    #[test]
    fn hostname_normalization() {
        // Already correct
        let h = get_local_hostname();
        assert!(h.ends_with(".local."), "hostname should end with .local., got: {}", h);
    }

    #[test]
    fn discovery_source_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DiscoverySource::Ble);
        set.insert(DiscoverySource::Mdns);
        set.insert(DiscoverySource::Ble); // duplicate
        assert_eq!(set.len(), 2);
    }
}
