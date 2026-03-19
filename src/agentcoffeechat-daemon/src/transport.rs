// Encrypted transport layer (QUIC via quinn).
//
// Provides peer-to-peer communication over QUIC with self-signed TLS
// certificates and length-prefixed message framing.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use serde::{Deserialize, Serialize};

/// ALPN protocol identifier for AgentCoffeeChat.
const ALPN_PROTOCOL: &[u8] = b"agentcoffeechat/1.0";

/// Maximum allowed message size (5 KB).
const MAX_MESSAGE_SIZE: u32 = 5 * 1024;

/// Length-prefix size in bytes (u32 big-endian).
const LENGTH_PREFIX_SIZE: usize = 4;

// ---------------------------------------------------------------------------
// Envelope types
// ---------------------------------------------------------------------------

/// Messages sent over QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    ChatOpen {
        peer_name: String,
        fingerprint_prefix: String,
        proof_code: String,
    },
    Chat {
        text: String,
    },
    AskRequest {
        peer_name: String,
        fingerprint_prefix: String,
        proof_code: String,
        question: String,
    },
    AskResponse {
        answer: String,
        duration_ms: u64,
    },
    Error {
        message: String,
    },
    /// Connection request — asks the remote peer for approval.
    ConnectionRequest {
        peer_name: String,
        fingerprint_prefix: String,
    },
    /// Response to a connection request.
    ConnectionResponse {
        approved: bool,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// TransportService
// ---------------------------------------------------------------------------

/// QUIC transport service that can accept and initiate peer connections.
pub struct TransportService {
    endpoint: quinn::Endpoint,
    port: u16,
}

impl TransportService {
    /// Create a new QUIC endpoint bound to the given port.
    ///
    /// A self-signed TLS certificate is generated on the fly using `rcgen`.
    /// The client side is configured to accept any server certificate (proper
    /// verification will be layered on top via the 3-word code exchange).
    pub fn new(port: u16) -> Result<Self> {
        // -- Generate self-signed certificate --
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .context("failed to generate self-signed certificate")?;
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

        // -- Server TLS config --
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], priv_key.into())
            .context("failed to build server TLS config")?;
        server_crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server_crypto)
                .context("failed to create QUIC server config")?,
        ));

        // -- Client TLS config (skip server verification for now) --
        // SECURITY NOTE: TLS certificate verification is intentionally skipped.
        // In AgentCoffeeChat v1, peer identity is verified via the 3-word code
        // exchange (out-of-band verbal confirmation), not via certificate pinning.
        // This is acceptable because:
        // 1. Connections are local-network only (BLE/mDNS/AWDL range)
        // 2. The 3-word code prevents MITM by confirming peer identity verbally
        // 3. All messages are sanitized before transmission regardless
        // TODO(v2): Implement certificate pinning using Ed25519 public keys
        //           exchanged during the 3-word code handshake.
        let mut client_crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();
        client_crypto.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

        let client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client_crypto)
                .context("failed to create QUIC client config")?,
        ));

        // -- Build endpoint (both client and server) --
        let bind_addr: SocketAddr = ([0, 0, 0, 0], port).into();
        let mut endpoint = quinn::Endpoint::server(server_config, bind_addr)
            .context("failed to bind QUIC endpoint")?;
        endpoint.set_default_client_config(client_config);

        let actual_port = endpoint
            .local_addr()
            .context("failed to get local address")?
            .port();

        Ok(Self {
            endpoint,
            port: actual_port,
        })
    }

    /// Accept the next incoming connection.
    ///
    /// Returns `None` when the endpoint has been closed.
    pub async fn accept(&self) -> Option<Result<Connection>> {
        let incoming = self.endpoint.accept().await?;
        Some(
            incoming
                .await
                .map(|conn| Connection { inner: conn })
                .map_err(|e| e.into()),
        )
    }

    /// Connect to a peer at the given address.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Connection> {
        let connecting = self
            .endpoint
            .connect(addr, "localhost")
            .context("failed to start QUIC connection")?;
        let conn = connecting.await.context("QUIC handshake failed")?;
        Ok(Connection { inner: conn })
    }

    /// Return the port this endpoint is bound to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Gracefully shut down the endpoint.
    pub fn close(&self) {
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"shutdown");
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A wrapper around a QUIC connection providing bidirectional streams.
pub struct Connection {
    inner: quinn::Connection,
}

impl Connection {
    /// Open a new bidirectional QUIC stream.
    pub async fn open_stream(
        &self,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner
            .open_bi()
            .await
            .context("failed to open bidirectional stream")
    }

    /// Accept an incoming bidirectional QUIC stream from the peer.
    pub async fn accept_stream(
        &self,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner
            .accept_bi()
            .await
            .context("failed to accept bidirectional stream")
    }

    /// Return the remote peer's address.
    pub fn remote_address(&self) -> SocketAddr {
        self.inner.remote_address()
    }
}

// ---------------------------------------------------------------------------
// Message framing helpers
// ---------------------------------------------------------------------------

/// Send a length-prefixed message.
///
/// Wire format: 4-byte big-endian length prefix followed by `msg` bytes.
/// Returns an error if the message exceeds [`MAX_MESSAGE_SIZE`].
pub async fn send_message(
    stream: &mut quinn::SendStream,
    msg: &[u8],
) -> Result<()> {
    let len: u32 = msg
        .len()
        .try_into()
        .ok()
        .filter(|&l| l <= MAX_MESSAGE_SIZE)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "message too large ({} bytes, max {})",
                msg.len(),
                MAX_MESSAGE_SIZE
            )
        })?;

    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("failed to write message length prefix")?;
    stream
        .write_all(msg)
        .await
        .context("failed to write message payload")?;
    Ok(())
}

/// Receive a length-prefixed message.
///
/// Reads a 4-byte big-endian length prefix, validates it against
/// [`MAX_MESSAGE_SIZE`], then reads exactly that many bytes.
pub async fn recv_message(
    stream: &mut quinn::RecvStream,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; LENGTH_PREFIX_SIZE];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("failed to read message length prefix")?;

    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_SIZE {
        bail!(
            "incoming message too large ({} bytes, max {})",
            len,
            MAX_MESSAGE_SIZE
        );
    }

    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .context("failed to read message payload")?;
    Ok(buf)
}

/// Serialize and send a structured wire message.
pub async fn send_wire_message(
    stream: &mut quinn::SendStream,
    msg: &WireMessage,
) -> Result<()> {
    let bytes = serde_json::to_vec(msg).context("failed to serialize WireMessage")?;
    send_message(stream, &bytes).await
}

/// Receive and deserialize a structured wire message.
pub async fn recv_wire_message(
    stream: &mut quinn::RecvStream,
) -> Result<WireMessage> {
    let bytes = recv_message(stream).await?;
    serde_json::from_slice(&bytes).context("failed to deserialize WireMessage")
}

// ---------------------------------------------------------------------------
// TLS: skip server certificate verification (temporary)
// ---------------------------------------------------------------------------

/// A certificate verifier that accepts any server certificate.
///
/// This is intentionally insecure and exists only as a placeholder until
/// proper verification is implemented via the 3-word code exchange.
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[test]
    fn ask_request_wire_message_roundtrip_preserves_proof_code() {
        let msg = WireMessage::AskRequest {
            peer_name: "alice-fp123456".to_string(),
            fingerprint_prefix: "fp1234567890abcd".to_string(),
            proof_code: "river-moon-bright".to_string(),
            question: "How does auth work?".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize ask request");
        let roundtrip: WireMessage = serde_json::from_str(&json).expect("deserialize ask request");
        assert_eq!(roundtrip, msg);
    }

    #[test]
    fn chat_open_wire_message_roundtrip_preserves_proof_code() {
        let msg = WireMessage::ChatOpen {
            peer_name: "bob-abcd1234".to_string(),
            fingerprint_prefix: "abcd1234efef5678".to_string(),
            proof_code: "tiger-castle-seven".to_string(),
        };

        let json = serde_json::to_string(&msg).expect("serialize chat open");
        let roundtrip: WireMessage = serde_json::from_str(&json).expect("deserialize chat open");
        assert_eq!(roundtrip, msg);
    }

    /// Helper: set up a server and client TransportService, return the
    /// server, client, and the server's listen address.
    fn setup() -> (TransportService, TransportService, SocketAddr) {
        let server = TransportService::new(0).expect("failed to create server");
        let port = server.port();
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let client = TransportService::new(0).expect("failed to create client");
        (server, client, addr)
    }

    /// Round-trip test: encode a message with send_message, decode with
    /// recv_message, and verify they match.
    #[tokio::test]
    async fn message_framing_roundtrip() {
        let (server, client, server_addr) = setup();
        let original_msg = b"hello, agentcoffeechat!";

        // Channel to keep the server alive until the client has finished reading.
        let (done_tx, done_rx) = oneshot::channel::<()>();

        let server_handle = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("no incoming connection")
                .expect("accept failed");
            let (mut send, mut recv) = conn
                .accept_stream()
                .await
                .expect("failed to accept stream");
            let received = recv_message(&mut recv).await.expect("recv failed");
            send_message(&mut send, &received)
                .await
                .expect("send failed");
            send.finish().expect("finish failed");
            // Keep the connection and endpoint alive until the client signals it is done.
            let _ = done_rx.await;
        });

        let conn = client
            .connect(server_addr)
            .await
            .expect("connect failed");
        let (mut send, mut recv) = conn.open_stream().await.expect("open_stream failed");

        send_message(&mut send, original_msg)
            .await
            .expect("send failed");
        send.finish().expect("finish failed");

        let echoed = recv_message(&mut recv).await.expect("recv failed");
        assert_eq!(echoed, original_msg);

        // Signal the server that we are done reading.
        let _ = done_tx.send(());
        server_handle.await.expect("server task panicked");
    }

    /// Verify that the framing layer rejects messages larger than 5 KB.
    #[tokio::test]
    async fn message_too_large_is_rejected() {
        let (server, client, server_addr) = setup();

        let server_handle = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("no incoming connection")
                .expect("accept failed");
            let (_send, mut recv) = conn
                .accept_stream()
                .await
                .expect("failed to accept stream");
            let result = recv_message(&mut recv).await;
            assert!(
                result.is_err(),
                "expected error for oversized message, got Ok"
            );
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("too large"),
                "unexpected error: {}",
                err_msg
            );
        });

        let conn = client
            .connect(server_addr)
            .await
            .expect("connect failed");
        let (mut send, _recv) = conn.open_stream().await.expect("open_stream failed");

        // Write a length prefix claiming 10 KB, but don't bother sending payload.
        let fake_len: u32 = 10 * 1024;
        send.write_all(&fake_len.to_be_bytes())
            .await
            .expect("write length prefix");
        send.finish().expect("finish failed");

        server_handle.await.expect("server task panicked");
    }

    /// Verify that an empty message round-trips correctly.
    #[tokio::test]
    async fn empty_message_roundtrip() {
        let (server, client, server_addr) = setup();
        let (done_tx, done_rx) = oneshot::channel::<()>();

        let server_handle = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("no incoming connection")
                .expect("accept failed");
            let (mut send, mut recv) = conn
                .accept_stream()
                .await
                .expect("failed to accept stream");
            let received = recv_message(&mut recv).await.expect("recv failed");
            assert!(received.is_empty(), "expected empty message");
            send_message(&mut send, &received)
                .await
                .expect("send failed");
            send.finish().expect("finish failed");
            let _ = done_rx.await;
        });

        let conn = client
            .connect(server_addr)
            .await
            .expect("connect failed");
        let (mut send, mut recv) = conn.open_stream().await.expect("open_stream failed");

        send_message(&mut send, b"").await.expect("send failed");
        send.finish().expect("finish failed");

        let echoed = recv_message(&mut recv).await.expect("recv failed");
        assert!(echoed.is_empty());

        let _ = done_tx.send(());
        server_handle.await.expect("server task panicked");
    }

    /// Verify that exactly 5 KB (the limit) is accepted.
    #[tokio::test]
    async fn message_at_max_size_is_accepted() {
        let (server, client, server_addr) = setup();
        let (done_tx, done_rx) = oneshot::channel::<()>();

        let payload = vec![0xABu8; MAX_MESSAGE_SIZE as usize];
        let payload_clone = payload.clone();

        let server_handle = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("no incoming connection")
                .expect("accept failed");
            let (mut send, mut recv) = conn
                .accept_stream()
                .await
                .expect("failed to accept stream");
            let received = recv_message(&mut recv).await.expect("recv failed");
            send_message(&mut send, &received)
                .await
                .expect("send failed");
            send.finish().expect("finish failed");
            let _ = done_rx.await;
        });

        let conn = client
            .connect(server_addr)
            .await
            .expect("connect failed");
        let (mut send, mut recv) = conn.open_stream().await.expect("open_stream failed");

        send_message(&mut send, &payload)
            .await
            .expect("send 5KB message failed");
        send.finish().expect("finish failed");

        let echoed = recv_message(&mut recv).await.expect("recv failed");
        assert_eq!(echoed, payload_clone);

        let _ = done_tx.send(());
        server_handle.await.expect("server task panicked");
    }
}
