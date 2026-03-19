use std::collections::HashMap;

use agentcoffeechat_core::Session;
use chrono::Utc;

/// Manages active chat sessions.
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

    /// Look up a session by peer name (exact match first, then unique prefix match).
    pub fn get_session(&self, peer_name: &str) -> Option<&Session> {
        // Exact match first.
        if let Some(s) = self.sessions.get(peer_name) {
            return Some(s);
        }
        // Fuzzy: peer_name might include a suffix like "(2)" or a fingerprint.
        // Try matching by prefix: "jackshen" matches "jackshen-93bd6a8c".
        // Also try the reverse: "jackshen-93bd6a8c" matches stored "jackshen".
        // Only return a result if exactly ONE session matches (avoid ambiguity).
        let mut matches: Vec<&str> = Vec::new();
        for (key, session) in &self.sessions {
            if key.starts_with(peer_name) || peer_name.starts_with(key) {
                matches.push(key);
                continue;
            }
            // Match by fingerprint if the session has one.
            if let Some(ref fp) = session.fingerprint_prefix {
                if peer_name.contains(&fp[..8.min(fp.len())]) {
                    matches.push(key);
                }
            }
        }
        if matches.len() == 1 {
            self.sessions.get(matches[0])
        } else {
            None
        }
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
    ///
    /// Uses the same lookup logic as `get_session` (exact match first, then
    /// unique prefix/fingerprint match) so that callers can remove sessions
    /// with the same names they use to look them up.
    pub fn remove_session(&mut self, peer_name: &str) -> Option<Session> {
        // Exact match first.
        if self.sessions.contains_key(peer_name) {
            return self.sessions.remove(peer_name);
        }
        // Fuzzy: find the matching key using the same algorithm as get_session.
        let mut matches: Vec<String> = Vec::new();
        for (key, session) in &self.sessions {
            if key.starts_with(peer_name) || peer_name.starts_with(key) {
                matches.push(key.clone());
                continue;
            }
            if let Some(ref fp) = session.fingerprint_prefix {
                if peer_name.contains(&fp[..8.min(fp.len())]) {
                    matches.push(key.clone());
                }
            }
        }
        if matches.len() == 1 {
            self.sessions.remove(&matches[0])
        } else {
            None
        }
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
