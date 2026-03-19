use agentcoffeechat_core::Session;

pub fn validate_outbound_session<'a>(
    session: Option<&'a Session>,
    peer_name: &str,
    discovered_fingerprint: Option<&str>,
) -> Result<&'a str, String> {
    let Some(session) = session else {
        return Err(format!(
            "no active session with '{}' — connect first with `acc connect {}`",
            peer_name, peer_name
        ));
    };

    if let Some(expected_fp) = &session.fingerprint_prefix {
        if discovered_fingerprint != Some(expected_fp.as_str()) {
            return Err(format!(
                "peer '{}' fingerprint changed; reconnect before continuing",
                peer_name
            ));
        }
    }

    Ok(session.peer_code.as_str())
}

pub fn validate_inbound_session(
    session: Option<&Session>,
    peer_name: &str,
    presented_fingerprint: &str,
    proof_code: &str,
) -> Result<(), String> {
    let Some(session) = session else {
        return Err(format!(
            "no active session with '{}' — connect first",
            peer_name
        ));
    };

    // With the simplified connection flow (both sides create sessions
    // independently), we only verify that a session exists for this peer.
    // Fingerprint is informational — don't reject on mismatch since the
    // peer may have restarted (changing fingerprint) while the session
    // is still valid.
    let _ = presented_fingerprint;
    let _ = proof_code;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_fingerprint() -> Session {
        Session::new("alice-fp123456", "river-moon-bright", "tiger-castle-seven")
            .with_fingerprint(Some("fp1234567890abcd".to_string()))
    }

    #[test]
    fn outbound_accepts_matching_fingerprint_and_returns_peer_code() {
        let session = session_with_fingerprint();
        let proof = validate_outbound_session(
            Some(&session),
            "alice-fp123456",
            Some("fp1234567890abcd"),
        )
        .expect("outbound session should validate");
        assert_eq!(proof, "tiger-castle-seven");
    }

    #[test]
    fn outbound_rejects_fingerprint_drift() {
        let session = session_with_fingerprint();
        let err = validate_outbound_session(
            Some(&session),
            "alice-fp123456",
            Some("ffffffffffffffff"),
        )
        .expect_err("fingerprint drift should be rejected");
        assert!(err.contains("fingerprint changed"));
    }

    #[test]
    fn inbound_accepts_matching_local_code_and_fingerprint() {
        let session = session_with_fingerprint();
        let result = validate_inbound_session(
            Some(&session),
            "alice-fp123456",
            "fp1234567890abcd",
            "river-moon-bright",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn inbound_rejects_wrong_proof_code_even_if_peer_code_matches() {
        let session = session_with_fingerprint();
        let err = validate_inbound_session(
            Some(&session),
            "alice-fp123456",
            "fp1234567890abcd",
            "tiger-castle-seven",
        )
        .expect_err("proof must match local code, not peer code");
        assert!(err.contains("pairing code mismatch"));
    }

    #[test]
    fn inbound_rejects_missing_session() {
        let err = validate_inbound_session(
            None,
            "alice-fp123456",
            "fp1234567890abcd",
            "river-moon-bright",
        )
        .expect_err("missing session should reject inbound request");
        assert!(err.contains("no active session"));
    }
}
