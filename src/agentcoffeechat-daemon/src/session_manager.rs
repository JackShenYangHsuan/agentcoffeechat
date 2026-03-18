use std::collections::HashMap;

use agentcoffeechat_core::Session;
use chrono::Utc;

/// Manages active chat sessions, keyed by peer name.
pub struct SessionManager {
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Return the names of all peers with active sessions.
    pub fn active_peers(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Look up a session by peer name.
    pub fn get_session(&self, peer_name: &str) -> Option<&Session> {
        self.sessions.get(peer_name)
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
