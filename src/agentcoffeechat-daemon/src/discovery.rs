// Peer discovery via mDNS (mdns-sd for registration, dns-sd CLI for browsing)
// with UDP broadcast fallback.
//
// `DiscoveryService` runs both mechanisms concurrently and delivers de-duplicated
// `DiscoveredPeer` values to the caller through a `tokio::sync::mpsc` channel.
//
// Key insight: the `mdns-sd` Rust crate works reliably for SERVICE REGISTRATION.
// However, its browsing is flaky on macOS. So we register with `mdns-sd` but
// browse using the native `dns-sd` CLI command, which talks directly to
// mDNSResponder and is rock-solid.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// BLE imports (kept for v2 — btleplug code is dead but encode/decode tested)
// ---------------------------------------------------------------------------

#[allow(unused_imports)]
use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
#[allow(unused_imports)]
use btleplug::platform::Manager as BleManager;
#[allow(unused_imports)]
use futures::StreamExt;

// ---------------------------------------------------------------------------
// mdns-sd imports (registration only)
// ---------------------------------------------------------------------------

use mdns_sd::{ServiceDaemon, ServiceInfo};

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

/// Protocol version byte included in BLE advertisement payloads.
const PROTOCOL_VERSION: u8 = 1;

/// How often to re-check the BLE peripheral list (seconds).
#[allow(dead_code)]
const BLE_POLL_INTERVAL_SECS: u64 = 5;

/// UDP broadcast port for fallback discovery.
const BROADCAST_PORT: u16 = 19532;

/// How often to send UDP broadcast beacons (seconds).
const BROADCAST_INTERVAL_SECS: u64 = 5;

/// mDNS service type string used with dns-sd CLI.
const SERVICE_TYPE: &str = "_agentcoffeechat._udp.";

/// mDNS service type for mdns-sd crate (fully qualified).
const SERVICE_TYPE_FULL: &str = "_agentcoffeechat._udp.local.";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How a peer was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySource {
    Ble,
    Mdns,
    Broadcast,
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

/// Manages concurrent mDNS and UDP broadcast discovery.
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
    /// mdns-sd daemon handle (for registration).
    mdns_daemon: Option<ServiceDaemon>,
}

impl DiscoveryService {
    /// Create a new `DiscoveryService` with the given configuration.
    pub fn new(config: DiscoveryConfig) -> Self {
        Self {
            config,
            shutdown_tx: None,
            tasks: Vec::new(),
            mdns_daemon: None,
        }
    }

    /// Start advertising and scanning.
    ///
    /// Returns a receiver that yields [`DiscoveredPeer`] values as they are
    /// found.  Peers discovered through both mDNS and UDP broadcast are
    /// deduplicated by fingerprint prefix.
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

        // -- Start mDNS registration (mdns-sd crate) -----------------------
        if let Err(e) = self.start_mdns_registration() {
            eprintln!("[discovery] mDNS registration failed: {:#}. Continuing without mDNS.", e);
        }

        // -- Start mDNS browsing (dns-sd CLI) ------------------------------
        let browse_tx = raw_tx.clone();
        let own_fp = self.config.fingerprint_prefix.clone();
        let browse_shutdown_rx = shutdown_rx.clone();
        let browse_handle = tokio::spawn(dns_sd_browse_task(
            browse_tx,
            own_fp,
            browse_shutdown_rx,
        ));
        self.tasks.push(browse_handle);

        // -- Start UDP broadcast beacon sender -----------------------------
        let broadcast_config = self.config.clone();
        let broadcast_shutdown_rx = shutdown_rx.clone();
        let broadcast_send_handle = tokio::spawn(udp_broadcast_send_task(
            broadcast_config,
            broadcast_shutdown_rx,
        ));
        self.tasks.push(broadcast_send_handle);

        // -- Start UDP broadcast listener ----------------------------------
        let broadcast_listen_tx = raw_tx.clone();
        let broadcast_listen_fp = self.config.fingerprint_prefix.clone();
        let broadcast_listen_shutdown_rx = shutdown_rx.clone();
        let broadcast_listen_handle = tokio::spawn(udp_broadcast_listen_task(
            broadcast_listen_tx,
            broadcast_listen_fp,
            broadcast_listen_shutdown_rx,
        ));
        self.tasks.push(broadcast_listen_handle);

        // -- Dedup task ----------------------------------------------------
        let dedup_shutdown_rx = shutdown_rx.clone();
        let dedup_handle = tokio::spawn(dedup_task(raw_rx, dedup_tx, dedup_shutdown_rx));
        self.tasks.push(dedup_handle);

        Ok(dedup_rx)
    }

    /// Stop advertising and scanning, clean up resources.
    pub async fn stop(&mut self) {
        // Signal all tokio tasks to stop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        // Shutdown the mdns-sd daemon.
        if let Some(daemon) = self.mdns_daemon.take() {
            let _ = daemon.shutdown();
        }

        // Await spawned tokio tasks.
        for handle in self.tasks.drain(..) {
            let _ = handle.await;
        }

        println!("[discovery] Stopped.");
    }

    // -----------------------------------------------------------------------
    // mDNS registration (mdns-sd crate)
    // -----------------------------------------------------------------------

    /// Register our service via mdns-sd (uses mDNSResponder on macOS).
    fn start_mdns_registration(&mut self) -> Result<()> {
        let daemon = ServiceDaemon::new()
            .context("failed to create mdns-sd ServiceDaemon")?;

        let instance_name = format!(
            "{}-{}",
            self.config.display_name,
            &self.config.fingerprint_prefix[..8.min(self.config.fingerprint_prefix.len())]
        );

        let project_hash_hex = self
            .config
            .project_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let properties = [
            ("v", "1"),
            ("fp", self.config.fingerprint_prefix.as_str()),
            ("port", &self.config.quic_port.to_string()),
            ("proj", &project_hash_hex),
        ];

        // Get the hostname for registration.
        let hostname = gethostname_string();
        let host_fqdn = if hostname.ends_with('.') {
            hostname
        } else {
            format!("{}.", hostname)
        };

        let service_info = ServiceInfo::new(
            SERVICE_TYPE_FULL,
            &instance_name,
            &host_fqdn,
            "",  // let mdns-sd pick the IP
            self.config.quic_port,
            &properties[..],
        )
        .context("failed to create ServiceInfo")?;

        daemon
            .register(service_info)
            .context("failed to register mDNS service")?;

        println!(
            "[discovery] mDNS: registered as \"{}\" on port {}",
            instance_name, self.config.quic_port
        );

        self.mdns_daemon = Some(daemon);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // BLE helpers (disabled in v1 — btleplug requires an app bundle on macOS)
    // -----------------------------------------------------------------------

    /// Start BLE scanning (and best-effort advertising via service data).
    #[allow(dead_code)]
    async fn start_ble(
        &mut self,
        _peer_tx: mpsc::Sender<DiscoveredPeer>,
        _shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        // BLE is disabled in v1. Kept as a placeholder for v2.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// dns-sd CLI browsing task
// ---------------------------------------------------------------------------

/// Browse for peers using the native `dns-sd -B` command, then resolve each
/// discovered instance with `dns-sd -L` and optionally `dns-sd -G`.
async fn dns_sd_browse_task(
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    own_fingerprint: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use std::collections::HashSet;

    println!("[discovery] mDNS browse: starting dns-sd -B {}", SERVICE_TYPE);

    let mut child = match Command::new("dns-sd")
        .args(["-B", SERVICE_TYPE])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[discovery] Failed to spawn dns-sd -B: {}. mDNS browsing disabled.", e);
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            eprintln!("[discovery] dns-sd -B has no stdout");
            return;
        }
    };

    let mut reader = BufReader::new(stdout).lines();

    // Track instances we have already resolved so we don't re-resolve on every
    // repeated "Add" line (dns-sd -B can repeat).
    let mut resolved_instances: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            line_result = reader.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        // dns-sd -B output looks like:
                        //   Browsing for _agentcoffeechat._udp
                        //   DATE: ---Tue 18 Mar 2026---
                        //    2:14:05.123  ...DIFFERING FLAGS...  Add        3   4  local.  _agentcoffeechat._udp.  alice-abcdef01
                        // We look for lines containing "Add" and our service type.
                        if !line.contains("Add") {
                            continue;
                        }
                        if !line.contains("_agentcoffeechat._udp.") {
                            continue;
                        }

                        // The instance name is the last whitespace-delimited field.
                        let instance_name = match line.split_whitespace().last() {
                            Some(name) => name.to_string(),
                            None => continue,
                        };

                        if resolved_instances.contains(&instance_name) {
                            continue;
                        }
                        resolved_instances.insert(instance_name.clone());

                        // Resolve the instance in a separate task.
                        let tx = peer_tx.clone();
                        let own_fp = own_fingerprint.clone();
                        tokio::spawn(resolve_dns_sd_instance(instance_name, tx, own_fp));
                    }
                    Ok(None) => {
                        // dns-sd process exited.
                        eprintln!("[discovery] dns-sd -B process exited unexpectedly");
                        break;
                    }
                    Err(e) => {
                        eprintln!("[discovery] dns-sd -B read error: {}", e);
                        break;
                    }
                }
            }
        }
    }
}

/// Resolve a single discovered instance using `dns-sd -L` and optionally
/// `dns-sd -G` to get the IP address.
async fn resolve_dns_sd_instance(
    instance_name: String,
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    own_fingerprint: String,
) {
    use tokio::process::Command;

    // Run dns-sd -L <instance> _agentcoffeechat._udp. local.
    // with a 3-second timeout (dns-sd -L runs forever; we just need the first result).
    let lookup_result = tokio::time::timeout(
        Duration::from_secs(3),
        async {
            let output = Command::new("dns-sd")
                .args(["-L", &instance_name, SERVICE_TYPE, "local."])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
            output
        },
    )
    .await;

    let output = match lookup_result {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            eprintln!("[discovery] dns-sd -L failed for {}: {}", instance_name, e);
            return;
        }
        Err(_) => {
            // Timeout — dns-sd -L runs forever, so we need to handle this differently.
            // Spawn the process and read line by line with a timeout instead.
            if let Some(peer) = resolve_with_line_reader(&instance_name).await {
                if peer.fingerprint_prefix != own_fingerprint {
                    println!(
                        "[discovery] mDNS: found peer \"{}\" fp={} at {:?}",
                        peer.name, peer.fingerprint_prefix, peer.address
                    );
                    let _ = peer_tx.send(peer).await;
                }
            }
            return;
        }
    };

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if let Some(peer) = parse_dns_sd_lookup_output(&stdout_str, &instance_name) {
        if peer.fingerprint_prefix != own_fingerprint {
            println!(
                "[discovery] mDNS: found peer \"{}\" fp={} at {:?}",
                peer.name, peer.fingerprint_prefix, peer.address
            );
            let _ = peer_tx.send(peer).await;
        }
    }
}

/// Resolve an instance by spawning `dns-sd -L` and reading its stdout line by
/// line, stopping after we get the result or a timeout.
async fn resolve_with_line_reader(instance_name: &str) -> Option<DiscoveredPeer> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut child = Command::new("dns-sd")
        .args(["-L", instance_name, SERVICE_TYPE, "local."])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout).lines();
    let mut collected_lines = String::new();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(line))) => {
                collected_lines.push_str(&line);
                collected_lines.push('\n');
                // Once we see "can be reached at", we have enough info.
                if line.contains("can be reached at") || line.contains("port") {
                    // Collect one more line in case TXT records follow.
                    if let Ok(Ok(Some(extra_line))) =
                        tokio::time::timeout(Duration::from_millis(500), reader.next_line()).await
                    {
                        collected_lines.push_str(&extra_line);
                        collected_lines.push('\n');
                    }
                    break;
                }
            }
            Ok(Ok(None)) => break,   // process exited
            Ok(Err(_)) => break,     // read error
            Err(_) => break,         // timeout
        }
    }

    let mut peer = parse_dns_sd_lookup_output(&collected_lines, instance_name);

    // If we didn't get an IP from -L, try dns-sd -G.
    if let Some(ref mut p) = peer {
        if p.address.is_none() {
            if let Some(hostname) = extract_hostname_from_lookup(&collected_lines) {
                if let Some(ip) = resolve_hostname_via_dns_sd(&hostname).await {
                    p.address = Some(ip);
                }
            }
        }
    }

    peer
}

/// Parse the output of `dns-sd -L` to extract peer information.
///
/// Example output:
/// ```text
/// Lookup _agentcoffeechat._udp.local. alice-abcdef01
///  DATE: ---Tue 18 Mar 2026---
///  2:14:06.789  alice-abcdef01._agentcoffeechat._udp.local. can be reached at alice-macbook.local.:9443 (interface 4)
///                               fp=abcdef0123456789 port=9443 proj=deadbeef v=1
/// ```
fn parse_dns_sd_lookup_output(output: &str, instance_name: &str) -> Option<DiscoveredPeer> {
    let mut hostname: Option<String> = None;
    let mut port: u16 = 0;
    let mut fp = String::new();
    let mut proj_hex = String::new();
    let mut address: Option<IpAddr> = None;

    for line in output.lines() {
        // Parse the "can be reached at" line for hostname and port.
        if line.contains("can be reached at") {
            // Format: "... can be reached at <hostname>:<port> ..."
            if let Some(at_idx) = line.find("can be reached at ") {
                let after_at = &line[at_idx + "can be reached at ".len()..];
                // The target is "hostname:port" possibly followed by more text.
                let target = after_at.split_whitespace().next().unwrap_or("");
                if let Some(colon_idx) = target.rfind(':') {
                    let h = &target[..colon_idx];
                    let p = &target[colon_idx + 1..];
                    hostname = Some(h.to_string());
                    port = p.parse().unwrap_or(0);
                }
            }
        }

        // Parse TXT record key=value pairs.
        // They appear as space-separated key=value on a line.
        if line.contains("fp=") || line.contains("port=") || line.contains("proj=") {
            for token in line.split_whitespace() {
                if let Some(val) = token.strip_prefix("fp=") {
                    fp = val.to_string();
                } else if let Some(val) = token.strip_prefix("port=") {
                    if port == 0 {
                        port = val.parse().unwrap_or(0);
                    }
                } else if let Some(val) = token.strip_prefix("proj=") {
                    proj_hex = val.to_string();
                }
            }
        }
    }

    if fp.is_empty() {
        return None;
    }

    let mut project_hash = [0u8; 4];
    for (i, chunk) in proj_hex.as_bytes().chunks(2).enumerate() {
        if i >= 4 {
            break;
        }
        let hex_str = std::str::from_utf8(chunk).unwrap_or("00");
        project_hash[i] = u8::from_str_radix(hex_str, 16).unwrap_or(0);
    }

    // Try to resolve the hostname to an IP address.
    if let Some(ref h) = hostname {
        // Try parsing as an IP directly (sometimes dns-sd returns an IP).
        if let Ok(ip) = h.parse::<IpAddr>() {
            address = Some(ip);
        }
    }

    // Use the instance name as the display name, stripping the fingerprint suffix.
    let display_name = instance_name
        .rsplit_once('-')
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| instance_name.to_string());

    Some(DiscoveredPeer {
        name: display_name,
        fingerprint_prefix: fp,
        quic_port: port,
        project_hash,
        address,
        source: DiscoverySource::Mdns,
        rssi: None,
    })
}

/// Extract the hostname from dns-sd -L output.
fn extract_hostname_from_lookup(output: &str) -> Option<String> {
    for line in output.lines() {
        if line.contains("can be reached at") {
            if let Some(at_idx) = line.find("can be reached at ") {
                let after_at = &line[at_idx + "can be reached at ".len()..];
                let target = after_at.split_whitespace().next().unwrap_or("");
                if let Some(colon_idx) = target.rfind(':') {
                    return Some(target[..colon_idx].to_string());
                }
            }
        }
    }
    None
}

/// Resolve a hostname to an IPv4 address using `dns-sd -G v4 <hostname>`.
async fn resolve_hostname_via_dns_sd(hostname: &str) -> Option<IpAddr> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut child = Command::new("dns-sd")
        .args(["-G", "v4", hostname])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout).lines();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(line))) => {
                // dns-sd -G output line containing the address:
                // "... Add ... <hostname> <ip_address>"
                // or "... Rmv ..."
                if line.contains("Add") {
                    // The IP is typically the last token on the line.
                    for token in line.split_whitespace().rev() {
                        if let Ok(ip) = token.parse::<IpAddr>() {
                            return Some(ip);
                        }
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    None
}

// ---------------------------------------------------------------------------
// UDP broadcast discovery
// ---------------------------------------------------------------------------

/// Beacon payload for UDP broadcast discovery.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct BroadcastBeacon {
    svc: String,
    fp: String,
    port: u16,
    proj: String,
    name: String,
}

/// Periodically broadcast a beacon on the LAN.
async fn udp_broadcast_send_task(
    config: DiscoveryConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let project_hash_hex = config
        .project_hash
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let beacon = BroadcastBeacon {
        svc: "agentcoffeechat/1".to_string(),
        fp: config.fingerprint_prefix.clone(),
        port: config.quic_port,
        proj: project_hash_hex,
        name: config.display_name.clone(),
    };

    let beacon_json = match serde_json::to_string(&beacon) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[discovery] Failed to serialize broadcast beacon: {}", e);
            return;
        }
    };

    // Bind a UDP socket for sending broadcasts.
    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[discovery] Failed to bind UDP broadcast sender: {}", e);
            return;
        }
    };

    if let Err(e) = sock.set_broadcast(true) {
        eprintln!("[discovery] Failed to enable broadcast on UDP socket: {}", e);
        return;
    }

    let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), BROADCAST_PORT);

    println!("[discovery] UDP broadcast: sending beacons every {}s on port {}", BROADCAST_INTERVAL_SECS, BROADCAST_PORT);

    let mut interval = tokio::time::interval(Duration::from_secs(BROADCAST_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                if let Err(e) = sock.send_to(beacon_json.as_bytes(), dest).await {
                    // Broadcast may fail on some network configurations; not fatal.
                    eprintln!("[discovery] UDP broadcast send error: {}", e);
                }
            }
        }
    }
}

/// Listen for UDP broadcast beacons from other peers.
async fn udp_broadcast_listen_task(
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    own_fingerprint: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let sock = match tokio::net::UdpSocket::bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), BROADCAST_PORT),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[discovery] Failed to bind UDP broadcast listener on port {}: {}",
                BROADCAST_PORT, e
            );
            return;
        }
    };

    if let Err(e) = sock.set_broadcast(true) {
        eprintln!("[discovery] Failed to enable broadcast on listener socket: {}", e);
        return;
    }

    println!("[discovery] UDP broadcast: listening on port {}", BROADCAST_PORT);

    let mut buf = [0u8; 2048];

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            result = sock.recv_from(&mut buf) => {
                match result {
                    Ok((len, src_addr)) => {
                        let data = &buf[..len];
                        match serde_json::from_slice::<BroadcastBeacon>(data) {
                            Ok(beacon) => {
                                // Validate service identifier.
                                if !beacon.svc.starts_with("agentcoffeechat/") {
                                    continue;
                                }
                                // Skip our own beacons.
                                if beacon.fp == own_fingerprint {
                                    continue;
                                }

                                let mut project_hash = [0u8; 4];
                                for (i, chunk) in beacon.proj.as_bytes().chunks(2).enumerate() {
                                    if i >= 4 {
                                        break;
                                    }
                                    let hex_str = std::str::from_utf8(chunk).unwrap_or("00");
                                    project_hash[i] =
                                        u8::from_str_radix(hex_str, 16).unwrap_or(0);
                                }

                                let peer = DiscoveredPeer {
                                    name: beacon.name,
                                    fingerprint_prefix: beacon.fp,
                                    quic_port: beacon.port,
                                    project_hash,
                                    address: Some(src_addr.ip()),
                                    source: DiscoverySource::Broadcast,
                                    rssi: None,
                                };

                                println!(
                                    "[discovery] UDP broadcast: found peer \"{}\" fp={} at {}",
                                    peer.name, peer.fingerprint_prefix, src_addr
                                );

                                if peer_tx.send(peer).await.is_err() {
                                    break; // receiver dropped
                                }
                            }
                            Err(_) => {
                                // Not a valid beacon; ignore.
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[discovery] UDP broadcast recv error: {}", e);
                    }
                }
            }
        }
    }
}

/// Deduplication task: receives raw peers from mDNS and broadcast and forwards
/// only unique ones (by fingerprint prefix) to the caller.  A peer is
/// re-emitted if we see it again from a *different* source (so the caller
/// learns about mDNS addresses for a broadcast-discovered peer).
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

/// Get the local hostname as a String, falling back to "localhost".
fn gethostname_string() -> String {
    // Use libc::gethostname (libc is already a dependency).
    let mut buf = [0u8; 256];
    let result = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if result == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).to_string()
    } else {
        "localhost".to_string()
    }
}

/// Derive a Hash for DiscoverySource so it can live in a HashSet.
impl std::hash::Hash for DiscoverySource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
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
    fn discovery_source_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DiscoverySource::Ble);
        set.insert(DiscoverySource::Mdns);
        set.insert(DiscoverySource::Broadcast);
        set.insert(DiscoverySource::Ble); // duplicate
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn parse_dns_sd_lookup_output_basic() {
        let output = r#"Lookup _agentcoffeechat._udp.local. alice-abcdef01
 DATE: ---Tue 18 Mar 2026---
 2:14:06.789  alice-abcdef01._agentcoffeechat._udp.local. can be reached at alice-macbook.local.:9443 (interface 4)
                              fp=abcdef0123456789 port=9443 proj=deadbeef v=1
"#;
        let peer = parse_dns_sd_lookup_output(output, "alice-abcdef01").unwrap();
        assert_eq!(peer.name, "alice");
        assert_eq!(peer.fingerprint_prefix, "abcdef0123456789");
        assert_eq!(peer.quic_port, 9443);
        assert_eq!(peer.project_hash, [0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(peer.source, DiscoverySource::Mdns);
    }
}
