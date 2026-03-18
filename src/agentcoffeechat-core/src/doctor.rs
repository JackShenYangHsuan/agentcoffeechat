use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::ipc::{socket_path, DaemonCommand, IpcClient};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agentcoffeechat")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Try to connect to the daemon and send a Ping.  Returns the client if
/// successful.
fn try_daemon_ping() -> Option<IpcClient> {
    let mut client = IpcClient::new().ok()?;
    let resp = client.send(&DaemonCommand::Ping).ok()?;
    if resp.ok {
        Some(client)
    } else {
        None
    }
}

/// Try to get daemon status.  Requires a connected client.
fn try_daemon_status(client: &mut IpcClient) -> Option<serde_json::Value> {
    let resp = client.send(&DaemonCommand::GetStatus).ok()?;
    if resp.ok {
        resp.data
    } else {
        None
    }
}

/// Try to get active session count from the daemon.
fn try_session_count(client: &mut IpcClient) -> Option<usize> {
    let resp = client.send(&DaemonCommand::ListSessions).ok()?;
    if resp.ok {
        resp.data
            .as_ref()
            .and_then(|d| d.as_array())
            .map(|a| a.len())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The 10 checks
// ---------------------------------------------------------------------------

fn check_keychain() -> DoctorCheck {
    if crate::identity::identity_exists_in_keychain() {
        match crate::identity::get_or_create_identity() {
            Ok(identity) => DoctorCheck {
                name: "Keychain".into(),
                status: CheckStatus::Pass,
                message: format!(
                    "Ed25519 identity found in Keychain (fingerprint: {})",
                    identity.fingerprint
                ),
            },
            Err(e) => DoctorCheck {
                name: "Keychain".into(),
                status: CheckStatus::Fail,
                message: format!("Ed25519 key exists but could not load identity: {}", e),
            },
        }
    } else {
        DoctorCheck {
            name: "Keychain".into(),
            status: CheckStatus::Fail,
            message: "Ed25519 identity not found in macOS Keychain (run the daemon to generate one)".into(),
        }
    }
}

fn check_config() -> DoctorCheck {
    let path = config_path();
    if !path.exists() {
        return DoctorCheck {
            name: "Config".into(),
            status: CheckStatus::Fail,
            message: "Config file does not exist".into(),
        };
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(_) => DoctorCheck {
                name: "Config".into(),
                status: CheckStatus::Pass,
                message: "Config file is valid JSON".into(),
            },
            Err(e) => DoctorCheck {
                name: "Config".into(),
                status: CheckStatus::Fail,
                message: format!("Config file is not valid JSON: {}", e),
            },
        },
        Err(e) => DoctorCheck {
            name: "Config".into(),
            status: CheckStatus::Fail,
            message: format!("Cannot read config file: {}", e),
        },
    }
}

fn check_daemon() -> (DoctorCheck, Option<IpcClient>) {
    match try_daemon_ping() {
        Some(client) => (
            DoctorCheck {
                name: "Daemon".into(),
                status: CheckStatus::Pass,
                message: "Daemon is running and responding to Ping".into(),
            },
            Some(client),
        ),
        None => (
            DoctorCheck {
                name: "Daemon".into(),
                status: CheckStatus::Fail,
                message: "Daemon is not running or not responding".into(),
            },
            None,
        ),
    }
}

fn check_unix_socket() -> DoctorCheck {
    let sock = socket_path();
    if sock.exists() {
        DoctorCheck {
            name: "Unix socket".into(),
            status: CheckStatus::Pass,
            message: format!("Socket file exists at {}", sock.display()),
        }
    } else {
        DoctorCheck {
            name: "Unix socket".into(),
            status: CheckStatus::Fail,
            message: format!("Socket file not found at {}", sock.display()),
        }
    }
}

fn check_ble() -> DoctorCheck {
    match Command::new("/usr/sbin/system_profiler")
        .arg("SPBluetoothDataType")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => DoctorCheck {
            name: "BLE".into(),
            status: CheckStatus::Pass,
            message: "Bluetooth hardware detected".into(),
        },
        _ => DoctorCheck {
            name: "BLE".into(),
            status: CheckStatus::Warning,
            message: "Could not verify Bluetooth availability".into(),
        },
    }
}

fn check_awdl() -> DoctorCheck {
    // Check if the awdl0 interface exists by running `ifconfig awdl0`.
    let output = Command::new("ifconfig")
        .arg("awdl0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let is_up = stdout.contains("UP");
            if is_up {
                DoctorCheck {
                    name: "AWDL".into(),
                    status: CheckStatus::Pass,
                    message: "AWDL interface (awdl0) is UP — P2P connectivity available".into(),
                }
            } else {
                DoctorCheck {
                    name: "AWDL".into(),
                    status: CheckStatus::Pass,
                    message: "AWDL interface (awdl0) exists but is idle (activates on P2P use)"
                        .into(),
                }
            }
        }
        _ => DoctorCheck {
            name: "AWDL".into(),
            status: CheckStatus::Warning,
            message: "AWDL interface (awdl0) not found — P2P (AirDrop-style) not available".into(),
        },
    }
}

fn check_bonjour() -> DoctorCheck {
    match Command::new("which")
        .arg("dns-sd")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => DoctorCheck {
            name: "Bonjour".into(),
            status: CheckStatus::Pass,
            message: "dns-sd command found".into(),
        },
        _ => DoctorCheck {
            name: "Bonjour".into(),
            status: CheckStatus::Fail,
            message: "dns-sd command not found (Bonjour may not be available)".into(),
        },
    }
}

fn check_quic(client: &mut Option<IpcClient>) -> DoctorCheck {
    if let Some(ref mut c) = client {
        match try_daemon_status(c) {
            Some(data) => {
                let quic_port = data.get("quic_port").and_then(|v| v.as_u64()).unwrap_or(0);
                if quic_port > 0 {
                    DoctorCheck {
                        name: "QUIC".into(),
                        status: CheckStatus::Pass,
                        message: format!("Daemon reports QUIC listener on port {}", quic_port),
                    }
                } else {
                    DoctorCheck {
                        name: "QUIC".into(),
                        status: CheckStatus::Warning,
                        message: "Daemon is running but QUIC transport is not active".into(),
                    }
                }
            }
            None => DoctorCheck {
                name: "QUIC".into(),
                status: CheckStatus::Warning,
                message: "Could not retrieve daemon status".into(),
            },
        }
    } else {
        DoctorCheck {
            name: "QUIC".into(),
            status: CheckStatus::Warning,
            message: "Daemon not running; cannot check QUIC transport".into(),
        }
    }
}

fn check_sessions(client: &mut Option<IpcClient>) -> DoctorCheck {
    if let Some(ref mut c) = client {
        match try_session_count(c) {
            Some(count) => DoctorCheck {
                name: "Sessions".into(),
                status: CheckStatus::Pass,
                message: format!("{} active session(s)", count),
            },
            None => DoctorCheck {
                name: "Sessions".into(),
                status: CheckStatus::Warning,
                message: "Could not retrieve session count".into(),
            },
        }
    } else {
        DoctorCheck {
            name: "Sessions".into(),
            status: CheckStatus::Warning,
            message: "Daemon not running; cannot check sessions".into(),
        }
    }
}

fn check_disk_space() -> DoctorCheck {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    // Check if the directory is writable by creating and removing a temp file.
    let test_path = home.join(".agentcoffeechat/.doctor_check_tmp");
    // Ensure the parent directory exists
    let parent = test_path.parent().unwrap_or(std::path::Path::new("."));
    let _ = std::fs::create_dir_all(parent);

    match std::fs::write(&test_path, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test_path);
            DoctorCheck {
                name: "Disk space".into(),
                status: CheckStatus::Pass,
                message: format!("Home directory {} is writable", home.display()),
            }
        }
        Err(e) => {
            DoctorCheck {
                name: "Disk space".into(),
                status: CheckStatus::Fail,
                message: format!("Home directory is not writable: {}", e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all 10 doctor checks and return the results.
pub fn run_doctor_checks() -> Vec<DoctorCheck> {
    let mut results = Vec::with_capacity(10);

    // 1. Keychain
    results.push(check_keychain());

    // 2. Config
    results.push(check_config());

    // 3. Daemon (also captures the client for later checks)
    let (daemon_check, mut client) = check_daemon();
    results.push(daemon_check);

    // 4. Unix socket
    results.push(check_unix_socket());

    // 5. BLE
    results.push(check_ble());

    // 6. AWDL
    results.push(check_awdl());

    // 7. Bonjour
    results.push(check_bonjour());

    // 8. QUIC
    results.push(check_quic(&mut client));

    // 9. Sessions
    results.push(check_sessions(&mut client));

    // 10. Disk space
    results.push(check_disk_space());

    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_doctor_checks_returns_10_checks() {
        let checks = run_doctor_checks();
        assert_eq!(checks.len(), 10, "Expected 10 doctor checks");
    }

    #[test]
    fn check_names_are_unique() {
        let checks = run_doctor_checks();
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "Check names should be unique");
    }

    #[test]
    fn awdl_check_returns_valid_status() {
        let check = check_awdl();
        // On macOS with Wi-Fi: Pass (awdl0 exists).
        // On Linux/CI: Warning (awdl0 not found).
        assert!(
            check.status == CheckStatus::Pass || check.status == CheckStatus::Warning,
            "AWDL check should be Pass or Warning, got: {:?}",
            check.status
        );
        assert!(check.message.contains("AWDL") || check.message.contains("awdl0"));
    }

    #[test]
    fn check_status_serialization() {
        let check = DoctorCheck {
            name: "Test".into(),
            status: CheckStatus::Pass,
            message: "all good".into(),
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("\"pass\""), "CheckStatus::Pass should serialize to \"pass\"");

        let check2 = DoctorCheck {
            name: "Test2".into(),
            status: CheckStatus::Fail,
            message: "bad".into(),
        };
        let json2 = serde_json::to_string(&check2).unwrap();
        assert!(json2.contains("\"fail\""), "CheckStatus::Fail should serialize to \"fail\"");
    }

    #[test]
    fn disk_space_check_works() {
        let check = check_disk_space();
        // On any reasonable system the home dir should be writable
        assert_eq!(check.status, CheckStatus::Pass);
    }
}
