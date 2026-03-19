use std::collections::HashMap;

use agentcoffeechat_core::Session;
use chrono::{DateTime, Utc};

/// A pending incoming connection request awaiting user approval.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub peer_name: String,
    pub fingerprint_prefix: String,
    pub received_at: DateTime<Utc>,
}

/// Manages active chat sessions and pending connection requests.
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    pending: Vec<PendingRequest>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            pending: Vec::new(),
        }
    }

    /// Add a pending connection request (from an incoming peer).
    pub fn add_pending(&mut self, peer_name: &str, fingerprint_prefix: &str) {
        // Don't duplicate if already pending.
        if self.pending.iter().any(|p| p.peer_name == peer_name) {
            return;
        }
        self.pending.push(PendingRequest {
            peer_name: peer_name.to_string(),
            fingerprint_prefix: fingerprint_prefix.to_string(),
            received_at: Utc::now(),
        });
    }

    /// List all pending requests.
    pub fn list_pending(&self) -> &[PendingRequest] {
        &self.pending
    }

    /// Remove and return a pending request by peer name.
    pub fn take_pending(&mut self, peer_name: &str) -> Option<PendingRequest> {
        if let Some(idx) = self.pending.iter().position(|p| p.peer_name == peer_name) {
            Some(self.pending.remove(idx))
        } else {
            None
        }
    }

    /// Return the names of all peers with active sessions.
    pub fn active_peers(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Look up a session by peer name (exact match first, then prefix match).
    pub fn get_session(&self, peer_name: &str) -> Option<&Session> {
        // Exact match first.
        if let Some(s) = self.sessions.get(peer_name) {
            return Some(s);
        }
        // Fuzzy: peer_name might include a suffix like "(2)" or a fingerprint.
        // Try matching by prefix: "jackshen" matches "jackshen-93bd6a8c".
        // Also try the reverse: "jackshen-93bd6a8c" matches stored "jackshen".
        for (key, session) in &self.sessions {
            if key.starts_with(peer_name) || peer_name.starts_with(key) {
                return Some(session);
            }
            // Match by fingerprint if the session has one.
            if let Some(ref fp) = session.fingerprint_prefix {
                if peer_name.contains(&fp[..8.min(fp.len())]) {
                    return Some(session);
                }
            }
        }
        None
    }

    /// Create (or replace) a session for the given peer.
    ///
    /// Sessions are created with a 1-hour expiry by default.
    pub fn create_session(
        &mut self,
        peer_name: &str,
        local_code: &str,
        peer_code: &str,
        fingerprint_prefix: Option<String>,
    ) -> &Session {
        let expires_at = Utc::now() + chrono::Duration::hours(1);
        let session = Session::new(peer_name, local_code, peer_code)
            .with_expiry(expires_at)
            .with_fingerprint(fingerprint_prefix);
        self.sessions.insert(peer_name.to_string(), session);
        self.sessions.get(peer_name).expect("just inserted")
    }

    /// Remove and return the session for the given peer.
    pub fn remove_session(&mut self, peer_name: &str) -> Option<Session> {
        self.sessions.remove(peer_name)
    }

    /// Remove all sessions whose `expires_at` is in the past. Returns the count removed.
    pub fn cleanup_expired(&mut self) -> usize {
        let now = Utc::now();
        let before = self.sessions.len();
        self.sessions.retain(|_, s| match s.expires_at {
            Some(exp) => exp > now,
            None => true,
        });
        before - self.sessions.len()
    }
}
