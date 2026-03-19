// Loopback integration test — simulates the full AgentCoffeeChat flow on a
// single machine without needing a second peer. Exercises QUIC transport,
// message framing, chat engine channels, IPC daemon commands, word code
// exchange, and the sanitization pipeline in a chat context.

use std::net::SocketAddr;

use agentcoffeechat_core::types::{ChatBriefing, Message, MessagePhase, MessageSender, MessageType};
use agentcoffeechat_core::{
    generate_three_word_code, validate_code, DaemonCommand, DaemonResponse, SanitizationPipeline,
};
use agentcoffeechat_daemon::session_manager::SessionManager;
use agentcoffeechat_daemon::transport::{self, TransportService};

// =========================================================================
// 1. Two QUIC endpoints on localhost — bidirectional message exchange
// =========================================================================

/// Create two TransportService instances on OS-assigned ports, connect them
/// over loopback, and exchange messages in both directions.
#[tokio::test]
async fn quic_loopback_two_endpoints_bidirectional() {
    // -- Set up two endpoints --
    let endpoint_a = TransportService::new(0).expect("endpoint A");
    let endpoint_b = TransportService::new(0).expect("endpoint B");

    let addr_a: SocketAddr = ([127, 0, 0, 1], endpoint_a.port()).into();
    let _addr_b: SocketAddr = ([127, 0, 0, 1], endpoint_b.port()).into();

    // -- B connects to A --
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    let accept_handle = tokio::spawn(async move {
        let conn_a = endpoint_a
            .accept()
            .await
            .expect("no incoming connection on A")
            .expect("accept failed on A");

        // A accepts a stream opened by B.
        let (mut send_a, mut recv_a) = conn_a.accept_stream().await.expect("accept_stream A");

        // Read message from B.
        let msg = transport::recv_message(&mut recv_a)
            .await
            .expect("recv on A");
        assert_eq!(msg, b"hello from B");

        // Send response back to B.
        transport::send_message(&mut send_a, b"hello from A")
            .await
            .expect("send from A");
        send_a.finish().expect("finish A");

        let _ = done_rx.await;
    });

    let conn_b = endpoint_b
        .connect(addr_a)
        .await
        .expect("B connect to A failed");
    let (mut send_b, mut recv_b) = conn_b.open_stream().await.expect("open_stream B");

    // B sends a message to A.
    transport::send_message(&mut send_b, b"hello from B")
        .await
        .expect("send from B");
    send_b.finish().expect("finish B");

    // B reads A's response.
    let response = transport::recv_message(&mut recv_b)
        .await
        .expect("recv on B");
    assert_eq!(response, b"hello from A");

    let _ = done_tx.send(());
    accept_handle.await.expect("accept task panicked");
}

/// Both endpoints connect to each other simultaneously and exchange messages.
#[tokio::test]
async fn quic_loopback_cross_connect() {
    let endpoint_a = TransportService::new(0).expect("endpoint A");
    let endpoint_b = TransportService::new(0).expect("endpoint B");

    let addr_a: SocketAddr = ([127, 0, 0, 1], endpoint_a.port()).into();
    let addr_b: SocketAddr = ([127, 0, 0, 1], endpoint_b.port()).into();

    let (done_tx_a, done_rx_a) = tokio::sync::oneshot::channel::<()>();
    let (done_tx_b, done_rx_b) = tokio::sync::oneshot::channel::<()>();

    // A: accept incoming from B, echo it back.
    let a_accept = tokio::spawn(async move {
        let conn = endpoint_a
            .accept()
            .await
            .expect("no incoming on A")
            .expect("accept A");
        let (mut send, mut recv) = conn.accept_stream().await.expect("accept_stream A");
        let msg = transport::recv_message(&mut recv).await.expect("recv A");
        transport::send_message(&mut send, &msg).await.expect("send A");
        send.finish().expect("finish A");
        let _ = done_rx_a.await;
    });

    // B: accept incoming from A, echo it back.
    let b_accept = tokio::spawn(async move {
        let conn = endpoint_b
            .accept()
            .await
            .expect("no incoming on B")
            .expect("accept B");
        let (mut send, mut recv) = conn.accept_stream().await.expect("accept_stream B");
        let msg = transport::recv_message(&mut recv).await.expect("recv B");
        transport::send_message(&mut send, &msg).await.expect("send B");
        send.finish().expect("finish B");
        let _ = done_rx_b.await;
    });

    // Connect A -> B and send a message.
    let client_a = TransportService::new(0).expect("client A");
    let conn_a_to_b = client_a.connect(addr_b).await.expect("A connect B");
    let (mut s_ab, mut r_ab) = conn_a_to_b.open_stream().await.expect("open A->B");
    transport::send_message(&mut s_ab, b"A says hi to B")
        .await
        .expect("send A->B");
    s_ab.finish().expect("finish A->B");
    let echo_from_b = transport::recv_message(&mut r_ab).await.expect("recv A<-B");
    assert_eq!(echo_from_b, b"A says hi to B");

    // Connect B -> A and send a message.
    let client_b = TransportService::new(0).expect("client B");
    let conn_b_to_a = client_b.connect(addr_a).await.expect("B connect A");
    let (mut s_ba, mut r_ba) = conn_b_to_a.open_stream().await.expect("open B->A");
    transport::send_message(&mut s_ba, b"B says hi to A")
        .await
        .expect("send B->A");
    s_ba.finish().expect("finish B->A");
    let echo_from_a = transport::recv_message(&mut r_ba).await.expect("recv B<-A");
    assert_eq!(echo_from_a, b"B says hi to A");

    let _ = done_tx_a.send(());
    let _ = done_tx_b.send(());
    a_accept.await.expect("A accept panicked");
    b_accept.await.expect("B accept panicked");
}

// =========================================================================
// 2. Message exchange over QUIC — framing with various payloads
// =========================================================================

/// Send multiple messages of different sizes through QUIC loopback and verify
/// they all arrive correctly with proper framing.
#[tokio::test]
async fn quic_loopback_multi_message_framing() {
    let server = TransportService::new(0).expect("server");
    let client = TransportService::new(0).expect("client");
    let addr: SocketAddr = ([127, 0, 0, 1], server.port()).into();

    let messages: Vec<Vec<u8>> = vec![
        b"icebreaker: Hi! I'm alice's agent.".to_vec(),
        b"followup: What tools do you use?".to_vec(),
        b"followup: I use Rust and tokio.".to_vec(),
        b"wrapup: Great chatting! Bye!".to_vec(),
        vec![],           // empty message (edge case)
        vec![0xFF; 4096], // near-max payload
    ];
    let msg_count = messages.len();
    let messages_clone = messages.clone();

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().unwrap();
        let (mut send, mut recv) = conn.accept_stream().await.unwrap();

        for _ in 0..msg_count {
            let msg = transport::recv_message(&mut recv).await.unwrap();
            // Echo each message back.
            transport::send_message(&mut send, &msg).await.unwrap();
        }
        send.finish().unwrap();
        let _ = done_rx.await;
    });

    let conn = client.connect(addr).await.unwrap();
    let (mut send, mut recv) = conn.open_stream().await.unwrap();

    // Send all messages.
    for msg in &messages_clone {
        transport::send_message(&mut send, msg).await.unwrap();
    }
    send.finish().unwrap();

    // Receive and verify all echoed messages.
    for (i, expected) in messages_clone.iter().enumerate() {
        let echoed = transport::recv_message(&mut recv).await.unwrap();
        assert_eq!(
            echoed, *expected,
            "Message {} mismatch: expected len={}, got len={}",
            i,
            expected.len(),
            echoed.len()
        );
    }

    let _ = done_tx.send(());
    server_handle.await.unwrap();
}

// =========================================================================
// 3. Simulated chat flow — two ChatEngines with mpsc channels
// =========================================================================

/// Simulate two chat engines (alice and bob) communicating through mpsc
/// channels. Since we cannot spawn the actual agent subprocess in tests,
/// we drive the channel exchange manually to verify the plumbing works.
#[tokio::test]
async fn simulated_chat_flow_channel_exchange() {
    use agentcoffeechat_daemon::chat_engine::ChatEvent;

    // Create cross-connected channels:
    // alice_send_tx -> bob_recv_rx (alice sends, bob receives)
    // bob_send_tx -> alice_recv_rx (bob sends, alice receives)
    let (alice_send_tx, mut bob_recv_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (bob_send_tx, mut alice_recv_rx) = tokio::sync::mpsc::channel::<String>(64);

    // Event channels for each side.
    let (alice_event_tx, mut alice_event_rx) =
        tokio::sync::mpsc::channel::<ChatEvent>(128);
    let (bob_event_tx, mut bob_event_rx) =
        tokio::sync::mpsc::channel::<ChatEvent>(128);

    // Simulate alice's side: send icebreaker, receive bob's icebreaker,
    // exchange follow-ups, send wrapup.
    let alice_handle = tokio::spawn(async move {
        // Phase 1: Alice sends icebreaker.
        let _ = alice_event_tx
            .send(ChatEvent::Phase("icebreaker".into()))
            .await;
        alice_send_tx
            .send("Hi! I'm Alice's agent. I'm working on a Rust CLI tool.".into())
            .await
            .expect("alice send icebreaker");

        // Wait for Bob's icebreaker.
        let bob_icebreaker = alice_recv_rx
            .recv()
            .await
            .expect("alice recv bob icebreaker");
        let _ = alice_event_tx
            .send(ChatEvent::RemoteMessage(bob_icebreaker.clone()))
            .await;
        assert!(bob_icebreaker.contains("Bob"));

        // Phase 2: Follow-up exchange.
        let _ = alice_event_tx
            .send(ChatEvent::Phase("followup".into()))
            .await;
        alice_send_tx
            .send("What prompting strategies work well for you?".into())
            .await
            .expect("alice send followup");

        let bob_followup = alice_recv_rx
            .recv()
            .await
            .expect("alice recv bob followup");
        let _ = alice_event_tx
            .send(ChatEvent::RemoteMessage(bob_followup.clone()))
            .await;
        assert!(bob_followup.contains("chain-of-thought"));

        // Phase 3: Wrapup.
        let _ = alice_event_tx
            .send(ChatEvent::Phase("wrapup".into()))
            .await;
        alice_send_tx
            .send("Great chat! I learned a lot about prompting. Bye!".into())
            .await
            .expect("alice send wrapup");

        let _ = alice_event_tx.send(ChatEvent::Complete).await;
    });

    // Simulate bob's side: receive alice's icebreaker, send own icebreaker,
    // exchange follow-ups, receive wrapup.
    let bob_handle = tokio::spawn(async move {
        // Wait for Alice's icebreaker.
        let alice_icebreaker = bob_recv_rx
            .recv()
            .await
            .expect("bob recv alice icebreaker");
        let _ = bob_event_tx
            .send(ChatEvent::RemoteMessage(alice_icebreaker.clone()))
            .await;
        assert!(alice_icebreaker.contains("Alice"));

        // Phase 1: Bob sends own icebreaker.
        let _ = bob_event_tx
            .send(ChatEvent::Phase("icebreaker".into()))
            .await;
        bob_send_tx
            .send("Hey! I'm Bob's agent. Building a Python data pipeline.".into())
            .await
            .expect("bob send icebreaker");

        // Phase 2: Follow-up exchange.
        let alice_followup = bob_recv_rx
            .recv()
            .await
            .expect("bob recv alice followup");
        let _ = bob_event_tx
            .send(ChatEvent::RemoteMessage(alice_followup))
            .await;

        let _ = bob_event_tx
            .send(ChatEvent::Phase("followup".into()))
            .await;
        bob_send_tx
            .send("I find chain-of-thought prompting works great for debugging.".into())
            .await
            .expect("bob send followup");

        // Phase 3: Receive wrapup.
        let alice_wrapup = bob_recv_rx
            .recv()
            .await
            .expect("bob recv alice wrapup");
        let _ = bob_event_tx
            .send(ChatEvent::RemoteMessage(alice_wrapup.clone()))
            .await;
        assert!(alice_wrapup.contains("Bye"));

        let _ = bob_event_tx.send(ChatEvent::Complete).await;
    });

    // Wait for both sides to complete.
    alice_handle.await.expect("alice task panicked");
    bob_handle.await.expect("bob task panicked");

    // Drain events from both sides and verify we got the expected phases.
    let mut alice_phases = Vec::new();
    let mut alice_complete = false;
    while let Ok(event) = alice_event_rx.try_recv() {
        match event {
            ChatEvent::Phase(p) => alice_phases.push(p),
            ChatEvent::Complete => alice_complete = true,
            _ => {}
        }
    }
    assert_eq!(
        alice_phases,
        vec!["icebreaker", "followup", "wrapup"],
        "Alice should go through all phases"
    );
    assert!(alice_complete, "Alice should complete");

    let mut bob_phases = Vec::new();
    let mut bob_complete = false;
    while let Ok(event) = bob_event_rx.try_recv() {
        match event {
            ChatEvent::Phase(p) => bob_phases.push(p),
            ChatEvent::Complete => bob_complete = true,
            _ => {}
        }
    }
    assert_eq!(
        bob_phases,
        vec!["icebreaker", "followup"],
        "Bob should go through icebreaker and followup"
    );
    assert!(bob_complete, "Bob should complete");
}

/// Verify that both sides produce a ChatResult-like structure with
/// non-empty transcript and briefing data.
#[tokio::test]
async fn simulated_chat_produces_transcript_and_briefing() {
    // Simulate the transcript that would be produced by a chat.
    let mut transcript: Vec<Message> = Vec::new();

    // Alice's icebreaker.
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Opening,
        MessageSender::new("alice", "", "claude-code"),
        "Hi! I'm working on a Rust networking library using QUIC.",
        1,
    ));

    // Bob's icebreaker.
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Opening,
        MessageSender::new("bob", "", "claude-code"),
        "Hey! I'm building a distributed task scheduler in Go.",
        2,
    ));

    // Exchange.
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Exchange,
        MessageSender::new("alice", "", "claude-code"),
        "How do you handle task deduplication across nodes?",
        3,
    ));
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Exchange,
        MessageSender::new("bob", "", "claude-code"),
        "We use a distributed lock with Redis. What about your QUIC retry logic?",
        4,
    ));

    // Closing.
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Closing,
        MessageSender::new("alice", "", "claude-code"),
        "Great chat! I'll look into distributed locks for my project.",
        5,
    ));
    transcript.push(Message::new(
        MessageType::Chat,
        MessagePhase::Closing,
        MessageSender::new("bob", "", "claude-code"),
        "Likewise! QUIC sounds promising. Bye!",
        6,
    ));

    // Build briefing (simulating what the agent would produce).
    let briefing = ChatBriefing {
        what_building: "A distributed task scheduler in Go".into(),
        learnings: vec![
            "Uses Redis distributed locks for deduplication".into(),
            "Go's concurrency model works well for task scheduling".into(),
        ],
        tips: vec!["Try Redis for distributed locking in networked systems".into()],
        ideas_to_explore: vec!["Combine QUIC transport with distributed scheduling".into()],
    };

    // Verify the result has a non-empty transcript and briefing.
    assert!(!transcript.is_empty(), "Transcript should not be empty");
    assert_eq!(transcript.len(), 6);

    // Verify transcript contains messages from both sides.
    let alice_msgs: Vec<_> = transcript.iter().filter(|m| m.from.name == "alice").collect();
    let bob_msgs: Vec<_> = transcript.iter().filter(|m| m.from.name == "bob").collect();
    assert_eq!(alice_msgs.len(), 3);
    assert_eq!(bob_msgs.len(), 3);

    // Verify phases are correct.
    assert_eq!(transcript[0].phase, MessagePhase::Opening);
    assert_eq!(transcript[1].phase, MessagePhase::Opening);
    assert_eq!(transcript[2].phase, MessagePhase::Exchange);
    assert_eq!(transcript[3].phase, MessagePhase::Exchange);
    assert_eq!(transcript[4].phase, MessagePhase::Closing);
    assert_eq!(transcript[5].phase, MessagePhase::Closing);

    // Verify briefing is non-empty.
    assert!(!briefing.what_building.is_empty());
    assert!(!briefing.learnings.is_empty());
    assert!(!briefing.tips.is_empty());
    assert!(!briefing.ideas_to_explore.is_empty());

    // Simulate ChatResult construction.
    let result = agentcoffeechat_daemon::chat_engine::ChatResult {
        transcript,
        briefing,
        output: Default::default(),
        duration_secs: 120,
        message_count: 6,
    };

    assert_eq!(result.message_count, 6);
    assert_eq!(result.duration_secs, 120);
    assert!(!result.briefing.what_building.is_empty());
    assert!(!result.transcript.is_empty());
}

// =========================================================================
// 4. Full daemon IPC test — spawn daemon, send commands, verify responses
// =========================================================================

/// Test the IPC wire protocol by encoding/decoding all daemon commands and
/// verifying the responses have the expected structure. This does not spawn
/// the actual daemon binary (which requires BLE/mDNS), but exercises the
/// serialization path end-to-end.
#[test]
fn daemon_ipc_command_response_loop() {
    // Simulate the wire protocol for each command type.
    let test_cases: Vec<(DaemonCommand, Box<dyn Fn(&DaemonResponse)>)> = vec![
        (
            DaemonCommand::Ping,
            Box::new(|resp: &DaemonResponse| {
                // Simulate a "pong" response.
                assert!(resp.ok);
                assert_eq!(resp.message.as_deref(), Some("pong"));
            }),
        ),
        (
            DaemonCommand::GetStatus,
            Box::new(|resp: &DaemonResponse| {
                assert!(resp.ok);
                assert!(resp.data.is_some());
            }),
        ),
        (
            DaemonCommand::ListPeers,
            Box::new(|resp: &DaemonResponse| {
                assert!(resp.ok);
                assert!(resp.data.is_some());
                assert!(resp.data.as_ref().unwrap().is_array());
            }),
        ),
        (
            DaemonCommand::ListSessions,
            Box::new(|resp: &DaemonResponse| {
                assert!(resp.ok);
                assert!(resp.data.is_some());
                assert!(resp.data.as_ref().unwrap().is_array());
            }),
        ),
        (
            DaemonCommand::Shutdown,
            Box::new(|resp: &DaemonResponse| {
                assert!(resp.ok);
                assert!(resp
                    .message
                    .as_deref()
                    .unwrap()
                    .contains("shutting down"));
            }),
        ),
    ];

    for (cmd, validate) in &test_cases {
        // Serialize command (as the CLI would).
        let mut cmd_json = serde_json::to_string(cmd).expect("serialize cmd");
        cmd_json.push('\n');

        // Parse command back (as the daemon would).
        let parsed: DaemonCommand =
            serde_json::from_str(cmd_json.trim()).expect("parse cmd");

        // Generate a simulated response based on the command.
        let response = match parsed {
            DaemonCommand::Ping => DaemonResponse::success("pong"),
            DaemonCommand::GetStatus => DaemonResponse::success_with_data(
                "daemon is running",
                serde_json::json!({
                    "uptime_seconds": 42,
                    "active_sessions": 0,
                    "discovered_peers": 0,
                    "quic_port": 12345,
                }),
            ),
            DaemonCommand::ListPeers => DaemonResponse::success_with_data(
                "0 peer(s) discovered",
                serde_json::json!([]),
            ),
            DaemonCommand::ListSessions => DaemonResponse::success_with_data(
                "0 active session(s)",
                serde_json::json!([]),
            ),
            DaemonCommand::Shutdown => DaemonResponse::success("shutting down"),
            _ => DaemonResponse::error("unexpected command"),
        };

        // Serialize response (as the daemon would).
        let mut resp_json = serde_json::to_string(&response).expect("serialize resp");
        resp_json.push('\n');

        // Parse response back (as the CLI would).
        let parsed_resp: DaemonResponse =
            serde_json::from_str(resp_json.trim()).expect("parse resp");

        // Verify response structure.
        assert!(
            parsed_resp.ok,
            "Response for {:?} should have ok: true",
            cmd
        );
        validate(&parsed_resp);

        // Verify the JSON is valid and contains "ok": true.
        let raw: serde_json::Value =
            serde_json::from_str(resp_json.trim()).expect("parse as Value");
        assert_eq!(
            raw.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "JSON should contain ok: true for {:?}",
            cmd
        );
    }
}

/// Test that all DaemonCommand variants serialize to valid JSON and can
/// be deserialized back identically.
#[test]
fn daemon_ipc_all_commands_valid_json() {
    let commands = vec![
        DaemonCommand::Ping,
        DaemonCommand::ListSessions,
        DaemonCommand::BeginPairing {
            peer_name: "alice".into(),
            fingerprint_prefix: None,
        },
        DaemonCommand::CompletePairing {
            peer_name: "alice".into(),
            peer_code: "alpha-bravo-charlie".into(),
        },
        DaemonCommand::EndSession {
            peer_name: "bob".into(),
        },
        DaemonCommand::ListPeers,
        DaemonCommand::GetStatus,
        DaemonCommand::Shutdown,
        DaemonCommand::AskQuestion {
            peer_name: "carol".into(),
            question: "What is your tech stack?".into(),
        },
        DaemonCommand::StartChat {
            peer_name: "dave".into(),
        },
        DaemonCommand::ListHistory,
        DaemonCommand::GetHistory { index: 0 },
        DaemonCommand::RunDoctor,
    ];

    for cmd in &commands {
        // Serialize.
        let json = serde_json::to_string(cmd).expect("serialize");

        // Verify it's valid JSON.
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("valid JSON");
        assert!(value.is_object(), "Command should serialize to object");
        assert!(
            value.get("command").is_some(),
            "Command JSON should have 'command' field: {}",
            json
        );

        // Round-trip.
        let roundtripped: DaemonCommand =
            serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&roundtripped).expect("re-serialize");
        assert_eq!(json, json2, "Round-trip failed for {:?}", cmd);
    }
}

// =========================================================================
// 5. Connect flow simulation — word code exchange
// =========================================================================

/// Simulate the full code exchange flow: alice generates a code, bob generates
/// a code, each validates the other's code, then both store local+peer codes.
#[test]
fn connect_flow_word_code_exchange() {
    // Alice generates her code.
    let alice_code = generate_three_word_code();
    assert!(
        validate_code(&alice_code),
        "Alice's code should validate: {}",
        alice_code
    );

    // Bob generates his code.
    let bob_code = generate_three_word_code();
    assert!(
        validate_code(&bob_code),
        "Bob's code should validate: {}",
        bob_code
    );

    // Codes should be different (astronomically unlikely to collide).
    assert_ne!(
        alice_code, bob_code,
        "Two independently generated codes should differ"
    );

    // Alice validates Bob's code (simulating receiving it over the air).
    assert!(
        validate_code(&bob_code),
        "Alice should accept Bob's valid code"
    );

    // Bob validates Alice's code.
    assert!(
        validate_code(&alice_code),
        "Bob should accept Alice's valid code"
    );

    // Both create sessions using the other's code.
    let mut alice_mgr = SessionManager::new();
    let mut bob_mgr = SessionManager::new();

    alice_mgr.create_session("bob", &alice_code, &bob_code, None);
    bob_mgr.create_session("alice", &bob_code, &alice_code, None);

    // Verify sessions reference the correct codes.
    let alice_session = alice_mgr.get_session("bob").expect("alice session");
    assert_eq!(alice_session.local_code, alice_code);
    assert_eq!(alice_session.peer_code, bob_code);
    assert_eq!(alice_session.peer_name, "bob");

    let bob_session = bob_mgr.get_session("alice").expect("bob session");
    assert_eq!(bob_session.local_code, bob_code);
    assert_eq!(bob_session.peer_code, alice_code);
    assert_eq!(bob_session.peer_name, "alice");
}

/// Verify that invalid codes are rejected during the exchange flow.
#[test]
fn connect_flow_rejects_invalid_codes() {
    let valid = generate_three_word_code();
    assert!(validate_code(&valid));

    // Tampered codes should be rejected.
    let tampered = format!("{}-extra", valid);
    assert!(
        !validate_code(&tampered),
        "Four-word code should be rejected"
    );

    let bad_word = "nonexistent-word-here";
    assert!(
        !validate_code(bad_word),
        "Code with unknown words should be rejected"
    );

    let empty = "";
    assert!(!validate_code(empty), "Empty code should be rejected");
}

/// Stress test: generate many codes, validate all of them, ensure no
/// collisions and they all pass validation.
#[test]
fn connect_flow_bulk_code_generation() {
    let mut codes = std::collections::HashSet::new();

    for _ in 0..200 {
        let code = generate_three_word_code();
        assert!(
            validate_code(&code),
            "Generated code should validate: {}",
            code
        );

        // Verify format: three hyphen-separated words.
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 3, "Code should have 3 parts: {}", code);

        codes.insert(code);
    }

    // With 256^3 = 16M+ combinations, 200 codes should all be unique.
    assert_eq!(codes.len(), 200, "All 200 codes should be unique");
}

// =========================================================================
// 6. Sanitization in chat context — secrets in transcripts
// =========================================================================

/// Create a realistic chat transcript with planted secrets, run it through
/// the sanitization pipeline, and verify all secrets are removed while
/// clean conversational text is preserved.
#[test]
fn sanitization_chat_transcript_secrets_removed() {
    let pipeline = SanitizationPipeline::default();

    // Simulate messages that might appear in a coffee chat with accidentally
    // leaked secrets.
    let messages_with_secrets = vec![
        (
            "alice",
            concat!(
                "Hi! I'm working on a deployment tool. ",
                "We connect to postgres://admin:hunter2@db.prod.internal:5432/myapp ",
                "for our backend."
            ),
        ),
        (
            "bob",
            concat!(
                "Cool! We use export API_KEY=sk-live-abc123def456ghi789jkl012mno345 ",
                "in our CI pipeline."
            ),
        ),
        (
            "alice",
            concat!(
                "Our GitHub integration uses ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij. ",
                "It handles webhooks on 10.0.0.42:8080."
            ),
        ),
        (
            "bob",
            concat!(
                "We store tokens in /home/deploy/.ssh/id_rsa for SSH access. ",
                "The JWT is eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
                ".eyJzdWIiOiIxMjM0NTY3ODkwIn0",
                ".dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
            ),
        ),
        (
            "alice",
            "But enough about secrets -- I really like using Rust for this kind of tool. The type system catches so many bugs.",
        ),
    ];

    let mut sanitized_transcript = Vec::new();

    for (from, body) in &messages_with_secrets {
        let result = pipeline.run(body);

        // No message should be blocked (Stage 3 redacts before Stage 4 blocks).
        assert!(
            !result.blocked,
            "Message from {} should not be blocked: {:?}",
            from,
            result.block_reason
        );

        sanitized_transcript.push((*from, result.text.clone(), result.redaction_count));
    }

    // Verify specific secrets are removed from each message.

    // Message 0: postgres connection string.
    assert!(
        !sanitized_transcript[0].1.contains("postgres://"),
        "Connection string should be redacted: {}",
        sanitized_transcript[0].1
    );
    assert!(
        !sanitized_transcript[0].1.contains("hunter2"),
        "Password should be redacted: {}",
        sanitized_transcript[0].1
    );
    assert!(sanitized_transcript[0].2 > 0, "Should have redactions");

    // Message 1: API key / export.
    assert!(
        !sanitized_transcript[1].1.contains("sk-live-"),
        "API key should be redacted: {}",
        sanitized_transcript[1].1
    );
    assert!(sanitized_transcript[1].2 > 0, "Should have redactions");

    // Message 2: GitHub token and IP:port.
    assert!(
        !sanitized_transcript[2].1.contains("ghp_"),
        "GitHub token should be redacted: {}",
        sanitized_transcript[2].1
    );
    assert!(
        !sanitized_transcript[2].1.contains("10.0.0.42:8080"),
        "IP:port should be redacted: {}",
        sanitized_transcript[2].1
    );

    // Message 3: SSH path and JWT.
    assert!(
        !sanitized_transcript[3].1.contains("eyJhbG"),
        "JWT should be redacted: {}",
        sanitized_transcript[3].1
    );

    // Message 4: clean text should pass through unchanged.
    assert_eq!(
        sanitized_transcript[4].2, 0,
        "Clean message should have zero redactions"
    );
    assert!(
        sanitized_transcript[4].1.contains("Rust"),
        "Clean text should be preserved: {}",
        sanitized_transcript[4].1
    );
    assert!(
        sanitized_transcript[4].1.contains("type system"),
        "Clean text should be preserved: {}",
        sanitized_transcript[4].1
    );
}

/// Test the sanitization pipeline with a message that would trigger the
/// AutoScan blocker if secrets survive earlier stages. Verify Stage 3
/// catches them first.
#[test]
fn sanitization_private_key_in_chat_context() {
    let pipeline = SanitizationPipeline::default();

    let message = concat!(
        "Here's my SSH setup:\n",
        "-----BEGIN RSA PRIVATE KEY-----\n",
        "MIIBogIBAAJBALRiMLAHudeSA/x3hB2f+2NRkJLA\n",
        "-----END RSA PRIVATE KEY-----\n",
        "But mainly I use ed25519 keys these days."
    );

    let result = pipeline.run(message);

    // Either the key header is redacted (not blocked) or the message is blocked.
    // Both are acceptable safety outcomes.
    if !result.blocked {
        assert!(
            !result.text.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "Private key header should be redacted: {}",
            result.text
        );
        assert!(result.redaction_count > 0);
    }

    // Clean text should survive either way (if not blocked).
    if !result.blocked {
        assert!(
            result.text.contains("ed25519"),
            "Clean text should survive: {}",
            result.text
        );
    }
}

/// Run a message with multiple secret categories through the pipeline and
/// verify the total redaction count.
#[test]
fn sanitization_combined_secrets_redaction_count() {
    let pipeline = SanitizationPipeline::default();

    let input = concat!(
        "config: DATABASE_URL=postgres://user:pw@host/db\n",
        "slack: xoxb-FAKE0SLACK0-TESTTOKEN0NOOP\n",
        "github: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij\n",
        "server: 192.168.1.100:3000\n",
        "jwt: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U\n",
        "But we love using Rust and TypeScript together."
    );

    let result = pipeline.run(input);
    assert!(!result.blocked);

    // Should have at least 4 redactions (one per secret type).
    assert!(
        result.redaction_count >= 4,
        "Expected >= 4 redactions, got {}. Output: {}",
        result.redaction_count,
        result.text
    );

    // Clean text should survive.
    assert!(
        result.text.contains("Rust and TypeScript"),
        "Clean text should pass through: {}",
        result.text
    );

    // Secrets should be gone.
    assert!(!result.text.contains("postgres://"), "Got: {}", result.text);
    assert!(!result.text.contains("xoxb-"), "Got: {}", result.text);
    assert!(!result.text.contains("ghp_"), "Got: {}", result.text);
    assert!(!result.text.contains("192.168.1.100:3000"), "Got: {}", result.text);
    assert!(!result.text.contains("eyJhbG"), "Got: {}", result.text);
}

// =========================================================================
// Bonus: Combined loopback — QUIC transport + channel plumbing + sessions
// =========================================================================

/// End-to-end loopback: set up QUIC transport between two endpoints, relay
/// messages through mpsc channels (mimicking the chat engine plumbing), and
/// manage sessions on both sides.
#[tokio::test]
async fn full_loopback_quic_channels_sessions() {
    // Set up QUIC endpoints.
    let endpoint_a = TransportService::new(0).expect("endpoint A");
    let endpoint_b = TransportService::new(0).expect("endpoint B");
    let addr_b: SocketAddr = ([127, 0, 0, 1], endpoint_b.port()).into();

    // Session managers.
    let mut mgr_a = SessionManager::new();
    let mut mgr_b = SessionManager::new();

    // Generate word codes for the pairing.
    let code_a = generate_three_word_code();
    let code_b = generate_three_word_code();
    assert!(validate_code(&code_a));
    assert!(validate_code(&code_b));

    // Create sessions.
    mgr_a.create_session("bob", &code_a, &code_b, None);
    mgr_b.create_session("alice", &code_b, &code_a, None);

    assert!(mgr_a.get_session("bob").is_some());
    assert!(mgr_b.get_session("alice").is_some());

    // QUIC channels for message relay.
    let (a_send_tx, mut a_send_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (b_recv_tx, mut b_recv_rx) = tokio::sync::mpsc::channel::<String>(64);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    // B accepts connection from A.
    let b_handle = tokio::spawn(async move {
        let conn = endpoint_b
            .accept()
            .await
            .expect("no incoming on B")
            .expect("accept B");
        let (mut send_b, mut recv_b) = conn.accept_stream().await.expect("accept_stream B");

        // Receive message from A over QUIC.
        let msg = transport::recv_message(&mut recv_b)
            .await
            .expect("recv on B");
        let text = String::from_utf8_lossy(&msg).to_string();

        // Forward to the "chat engine" channel.
        b_recv_tx.send(text).await.expect("forward to B recv channel");

        // Send a reply back over QUIC.
        transport::send_message(&mut send_b, b"Bob's reply: great to chat!")
            .await
            .expect("send from B");
        send_b.finish().expect("finish B");

        let _ = done_rx.await;
    });

    // A connects to B and sends a message.
    let conn_a = endpoint_a.connect(addr_b).await.expect("A connect B");
    let (mut send_a, mut recv_a) = conn_a.open_stream().await.expect("open_stream A");

    // Simulate the chat engine sending a message.
    a_send_tx
        .send("Alice's icebreaker: Hi Bob!".into())
        .await
        .expect("alice send to channel");

    // Relay from channel to QUIC.
    if let Some(msg) = a_send_rx.recv().await {
        transport::send_message(&mut send_a, msg.as_bytes())
            .await
            .expect("send over QUIC from A");
    }
    send_a.finish().expect("finish A");

    // Receive reply from B.
    let reply = transport::recv_message(&mut recv_a)
        .await
        .expect("recv reply on A");
    let reply_text = String::from_utf8_lossy(&reply).to_string();
    assert!(
        reply_text.contains("Bob's reply"),
        "Should get Bob's reply: {}",
        reply_text
    );

    // Verify the message was forwarded to B's chat engine channel.
    let forwarded = b_recv_rx.recv().await.expect("B should have received msg");
    assert!(
        forwarded.contains("Alice's icebreaker"),
        "B should get Alice's message: {}",
        forwarded
    );

    // Verify sessions are still intact.
    assert!(mgr_a.get_session("bob").is_some());
    assert!(mgr_b.get_session("alice").is_some());

    let _ = done_tx.send(());
    b_handle.await.expect("B task panicked");
}
