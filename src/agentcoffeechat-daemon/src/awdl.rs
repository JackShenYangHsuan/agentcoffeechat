// AWDL (Apple Wireless Direct Link) support for AgentCoffeeChat.
//
// AWDL is the protocol behind AirDrop — it creates ad-hoc peer-to-peer Wi-Fi
// links between Apple devices without requiring a shared Wi-Fi network.
//
// Key insight: on macOS, AWDL is automatically activated when Bonjour/mDNS
// services are registered or browsed with peer-to-peer (P2P) flags.  The
// `dns-sd` command with `-includeP2P` triggers this.  Once AWDL is active,
// the `awdl0` interface carries traffic, and our existing QUIC endpoint
// (bound to `0.0.0.0`) naturally handles connections arriving over it.
//
// This module provides:
//   - `is_awdl_available()` — check if the `awdl0` interface exists
//   - `awdl_status()` — parse interface flags/state from `ifconfig awdl0`
//   - `AwdlActivator` — keep AWDL alive by running a background `dns-sd`
//     browse with the P2P flag

use std::process::{Child, Command, Stdio};

/// The mDNS service type we browse/register with P2P to keep AWDL active.
const P2P_BROWSE_SERVICE: &str = "_agentcoffeechat._udp.";

// ---------------------------------------------------------------------------
// Status helpers
// ---------------------------------------------------------------------------

/// Check if the AWDL interface (`awdl0`) exists on this Mac.
///
/// Returns `true` on macOS machines with Wi-Fi hardware (almost all of them).
/// Returns `false` on Linux, VMs without Wi-Fi, or if the interface was
/// removed by MDM policy.
pub fn is_awdl_available() -> bool {
    Command::new("ifconfig")
        .arg("awdl0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get AWDL interface status details.
///
/// Returns `(available, description)` where `available` is `true` when the
/// interface exists and `description` contains human-readable status text
/// parsed from `ifconfig awdl0`.
pub fn awdl_status() -> (bool, String) {
    let output = match Command::new("ifconfig")
        .arg("awdl0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return (false, format!("failed to run ifconfig: {}", e));
        }
    };

    if !output.status.success() {
        return (false, "awdl0 interface not found".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Extract the flags line, e.g.  "awdl0: flags=8943<UP,BROADCAST,...> mtu 1484"
    let flags_summary = stdout
        .lines()
        .find(|l| l.contains("flags="))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "awdl0 present (no flags line)".to_string());

    let is_up = stdout.contains("UP");

    let description = if is_up {
        format!("awdl0 is UP — {}", flags_summary)
    } else {
        format!("awdl0 exists but is DOWN — {}", flags_summary)
    };

    (true, description)
}

// ---------------------------------------------------------------------------
// P2pServiceRegistration
// ---------------------------------------------------------------------------

/// Registers our service with the system mDNS responder using the `dns-sd -R`
/// command with the `-includeP2P` flag.
///
/// Unlike the pure-Rust `mdns-sd` crate used in `discovery.rs`, this goes
/// through macOS's native mDNSResponder, which knows about the AWDL interface
/// and will advertise the service over peer-to-peer links.  This makes us
/// discoverable by nearby peers even when they are NOT on the same Wi-Fi
/// network.
///
/// The registration process runs until killed.  Drop or call
/// [`unregister()`](P2pServiceRegistration::unregister) to stop advertising.
pub struct P2pServiceRegistration {
    child: Option<Child>,
}

impl P2pServiceRegistration {
    pub fn new() -> Self {
        Self { child: None }
    }

    /// Register the service via `dns-sd -R` with `-includeP2P`.
    ///
    /// Spawns:
    /// ```text
    /// dns-sd -R <instance_name> _agentcoffeechat._udp. . <port> \
    ///     v=1 fp=<fingerprint_prefix> port=<quic_port> proj=<project_hash_hex> \
    ///     -includeP2P
    /// ```
    ///
    /// Returns `Ok(true)` if registration was started, `Ok(false)` if AWDL is
    /// not available, and `Err` on spawn failure.
    pub fn register(
        &mut self,
        instance_name: &str,
        quic_port: u16,
        fingerprint_prefix: &str,
        project_hash: &[u8; 4],
    ) -> Result<bool, std::io::Error> {
        // Don't double-register.
        if self.child.is_some() {
            return Ok(true);
        }

        // Check if AWDL is even possible on this machine.
        if !is_awdl_available() {
            return Ok(false);
        }

        let project_hash_hex = project_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let child = Command::new("dns-sd")
            .arg("-R")
            .arg(instance_name)
            .arg(P2P_BROWSE_SERVICE)
            .arg(".")
            .arg(quic_port.to_string())
            .arg("v=1")
            .arg(format!("fp={}", fingerprint_prefix))
            .arg(format!("port={}", quic_port))
            .arg(format!("proj={}", project_hash_hex))
            .arg("-includeP2P")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        println!(
            "[awdl] P2P service registered as '{}' on port {} (pid {})",
            instance_name,
            quic_port,
            child.id()
        );

        self.child = Some(child);
        Ok(true)
    }

    /// Stop the P2P service registration by killing the `dns-sd -R` process.
    pub fn unregister(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            match child.kill() {
                Ok(()) => {
                    // Reap the child to avoid zombies.
                    let _ = child.wait();
                    println!("[awdl] P2P service unregistered (killed pid {})", pid);
                }
                Err(e) => {
                    eprintln!(
                        "[awdl] Failed to kill dns-sd registration pid {}: {}",
                        pid, e
                    );
                }
            }
        }
    }

    /// Returns `true` if a registration is currently active.
    pub fn is_registered(&self) -> bool {
        self.child.is_some()
    }
}

impl Default for P2pServiceRegistration {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for P2pServiceRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

// ---------------------------------------------------------------------------
// AwdlActivator
// ---------------------------------------------------------------------------

/// Keeps the AWDL interface active by running a background `dns-sd` browse
/// with the peer-to-peer flag.
///
/// On macOS, AWDL powers on automatically when a Bonjour P2P browse or
/// registration is active, and powers off shortly after all P2P users stop.
/// By keeping a `dns-sd -B ... -includeP2P` process alive, we ensure the
/// `awdl0` interface stays up for the lifetime of the daemon.
///
/// Drop or call [`deactivate()`](AwdlActivator::deactivate) to release AWDL.
pub struct AwdlActivator {
    child: Option<Child>,
    service_registration: Option<P2pServiceRegistration>,
}

impl AwdlActivator {
    pub fn new() -> Self {
        Self {
            child: None,
            service_registration: None,
        }
    }

    /// Start AWDL activation.
    ///
    /// Spawns a background `dns-sd -B _agentcoffeechat._udp. -includeP2P`
    /// process.  The `-B` flag browses for services, and `-includeP2P` tells
    /// the mDNS responder to also use peer-to-peer interfaces (AWDL).  This
    /// side-effect activates the `awdl0` interface.
    ///
    /// Returns `Ok(true)` if activation was started, `Ok(false)` if AWDL is
    /// not available (non-macOS or no Wi-Fi), and `Err` on spawn failure.
    pub fn activate(&mut self) -> Result<bool, std::io::Error> {
        // Don't double-activate.
        if self.child.is_some() {
            return Ok(true);
        }

        // Check if AWDL is even possible on this machine.
        if !is_awdl_available() {
            return Ok(false);
        }

        // Spawn `dns-sd -B _agentcoffeechat._udp. -includeP2P`
        //
        // This process will run until killed and prints browse results to
        // stdout.  We discard its output — its only purpose is to keep the
        // AWDL interface powered on.
        let child = Command::new("dns-sd")
            .arg("-B")
            .arg(P2P_BROWSE_SERVICE)
            .arg("-includeP2P")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        println!(
            "[awdl] Activated AWDL via dns-sd P2P browse (pid {})",
            child.id()
        );

        self.child = Some(child);
        Ok(true)
    }

    /// Register the service via `dns-sd -R` with `-includeP2P` so peers can
    /// discover us over AWDL even without a shared Wi-Fi network.
    ///
    /// This should be called after [`activate()`](AwdlActivator::activate).
    /// Returns `Ok(true)` if registration succeeded, `Ok(false)` if AWDL is
    /// not available, and `Err` on spawn failure.
    pub fn register_service(
        &mut self,
        instance_name: &str,
        quic_port: u16,
        fingerprint_prefix: &str,
        project_hash: &[u8; 4],
    ) -> Result<bool, std::io::Error> {
        let mut reg = P2pServiceRegistration::new();
        let result = reg.register(instance_name, quic_port, fingerprint_prefix, project_hash)?;
        if result {
            self.service_registration = Some(reg);
        }
        Ok(result)
    }

    /// Stop AWDL activation by killing the background `dns-sd` process
    /// and unregistering any P2P service.
    ///
    /// After this call, macOS may power down the `awdl0` interface if no
    /// other P2P users remain (e.g. AirDrop, Handoff).
    pub fn deactivate(&mut self) {
        // Unregister P2P service first.
        if let Some(ref mut reg) = self.service_registration {
            reg.unregister();
        }
        self.service_registration = None;

        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            match child.kill() {
                Ok(()) => {
                    // Reap the child to avoid zombies.
                    let _ = child.wait();
                    println!("[awdl] Deactivated AWDL (killed pid {})", pid);
                }
                Err(e) => {
                    eprintln!("[awdl] Failed to kill dns-sd pid {}: {}", pid, e);
                }
            }
        }
    }

    /// Returns `true` if the activator is currently running.
    pub fn is_active(&self) -> bool {
        self.child.is_some()
    }
}

impl Default for AwdlActivator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AwdlActivator {
    fn drop(&mut self) {
        self.deactivate();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awdl_available_returns_bool() {
        // Just verify it doesn't panic — the actual result depends on the
        // machine (true on macOS with Wi-Fi, false elsewhere).
        let _ = is_awdl_available();
    }

    #[test]
    fn awdl_status_returns_tuple() {
        let (avail, desc) = awdl_status();
        assert!(!desc.is_empty());
        // On macOS with Wi-Fi, avail should be true.
        // On CI/Linux, avail will be false.
        if avail {
            assert!(desc.contains("awdl0"));
        }
    }

    #[test]
    fn activator_default_is_inactive() {
        let activator = AwdlActivator::new();
        assert!(!activator.is_active());
    }

    #[test]
    fn activator_deactivate_when_inactive_is_noop() {
        let mut activator = AwdlActivator::new();
        // Should not panic.
        activator.deactivate();
        assert!(!activator.is_active());
    }

    #[test]
    fn p2p_registration_default_is_inactive() {
        let reg = P2pServiceRegistration::new();
        assert!(!reg.is_registered());
    }

    #[test]
    fn p2p_registration_unregister_when_inactive_is_noop() {
        let mut reg = P2pServiceRegistration::new();
        // Should not panic.
        reg.unregister();
        assert!(!reg.is_registered());
    }
}
