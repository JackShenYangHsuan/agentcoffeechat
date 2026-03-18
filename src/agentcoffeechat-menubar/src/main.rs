// AgentCoffeeChat menu bar status icon.
//
// Displays a minimal system tray icon that reflects the current daemon state:
//   - Idle (no peers)        — dim icon
//   - Peer nearby            — active icon
//   - Chatting               — pulsing icon
//   - Error (daemon down)    — warning icon
//
// The menu shows discovered peers, active sessions, and quick actions.

use std::process::Command;
use std::thread;
use std::time::Duration;

use agentcoffeechat_core::{DaemonCommand, IpcClient};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::TrayIconBuilder;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum DaemonState {
    /// Daemon is not running or unreachable.
    Offline,
    /// Daemon is running, no peers discovered.
    Idle,
    /// At least one peer is nearby.
    PeerNearby { count: usize },
    /// An active chat session is in progress.
    Chatting { peer: String },
}

impl DaemonState {
    fn status_text(&self) -> String {
        match self {
            DaemonState::Offline => "AgentCoffeeChat: Offline".into(),
            DaemonState::Idle => "AgentCoffeeChat: Idle".into(),
            DaemonState::PeerNearby { count } => {
                format!("AgentCoffeeChat: {} peer(s) nearby", count)
            }
            DaemonState::Chatting { peer } => {
                format!("AgentCoffeeChat: Chatting with {}", peer)
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Daemon polling
// ---------------------------------------------------------------------------

/// Poll the daemon for current state. Returns the aggregated state.
fn poll_daemon() -> DaemonState {
    let mut client = match IpcClient::new() {
        Ok(c) => c,
        Err(_) => return DaemonState::Offline,
    };

    // Ping to verify daemon is alive.
    match client.send(&DaemonCommand::Ping) {
        Ok(resp) if resp.ok => {}
        _ => return DaemonState::Offline,
    }

    // Check sessions first — if there's an active chat, that takes priority.
    if let Ok(resp) = client.send(&DaemonCommand::ListSessions) {
        if let Some(data) = &resp.data {
            if let Some(arr) = data.as_array() {
                if let Some(first) = arr.first() {
                    let peer = first
                        .get("peer_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    return DaemonState::Chatting { peer };
                }
            }
        }
    }

    // Check peers.
    if let Ok(resp) = client.send(&DaemonCommand::ListPeers) {
        if let Some(data) = &resp.data {
            if let Some(arr) = data.as_array() {
                if !arr.is_empty() {
                    return DaemonState::PeerNearby { count: arr.len() };
                }
            }
        }
    }

    DaemonState::Idle
}

/// Build a context menu with current state information.
fn build_menu(state: &DaemonState) -> (Menu, Option<MenuId>, MenuId, MenuId) {
    let menu = Menu::new();
    let mut start_id = None;

    // Status header.
    let status = MenuItem::new(state.status_text(), false, None);
    let _ = menu.append(&status);
    let _ = menu.append(&PredefinedMenuItem::separator());

    match state {
        DaemonState::Offline => {
            let start_item = MenuItem::new("Start Daemon (acc start)", true, None);
            let _ = menu.append(&start_item);
            start_id = Some(start_item.id().clone());
        }
        DaemonState::PeerNearby { count } => {
            let peers_label = MenuItem::new(
                format!("{} peer(s) discovered", count),
                false,
                None,
            );
            let _ = menu.append(&peers_label);
        }
        DaemonState::Chatting { peer } => {
            let chat_label = MenuItem::new(
                format!("Chatting with {}", peer),
                false,
                None,
            );
            let _ = menu.append(&chat_label);
        }
        DaemonState::Idle => {
            let idle_label = MenuItem::new("No peers nearby", false, None);
            let _ = menu.append(&idle_label);
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    let doctor_item = MenuItem::new("Run Doctor", true, None);
    let _ = menu.append(&doctor_item);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let quit_item = MenuItem::new("Quit", true, None);
    let _ = menu.append(&quit_item);

    (menu, start_id, doctor_item.id().clone(), quit_item.id().clone())
}

// ---------------------------------------------------------------------------
// Icon generation
// ---------------------------------------------------------------------------

/// Generate a simple icon as RGBA pixel data.
///
/// Creates a 22x22 icon with a colored circle indicating state:
/// - Offline: gray
/// - Idle: dim green
/// - PeerNearby: bright green
/// - Chatting: blue
fn generate_icon(state: &DaemonState) -> tray_icon::Icon {
    let size = 22u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    let (r, g, b) = match state {
        DaemonState::Offline => (128, 128, 128),
        DaemonState::Idle => (80, 160, 80),
        DaemonState::PeerNearby { .. } => (40, 200, 80),
        DaemonState::Chatting { .. } => (60, 120, 220),
    };

    let center = size as f64 / 2.0;
    let radius = (size as f64 / 2.0) - 2.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius {
                let idx = ((y * size + x) * 4) as usize;
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255; // full alpha
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, size, size).expect("failed to create icon")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let event_loop = EventLoopBuilder::new().build();

    let initial_state = poll_daemon();
    let icon = generate_icon(&initial_state);
    let (menu, mut start_id, mut doctor_id, mut quit_id) = build_menu(&initial_state);

    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip(initial_state.status_text())
        .build()
        .expect("failed to build tray icon");

    // Spawn a background thread that polls the daemon every 5 seconds
    // and updates the tray icon.
    let (state_tx, state_rx) = std::sync::mpsc::channel::<DaemonState>();

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        let state = poll_daemon();
        if state_tx.send(state).is_err() {
            break;
        }
    });

    let menu_channel = MenuEvent::receiver();
    let mut current_state = initial_state;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + Duration::from_secs(1),
        );

        // Check for menu events.
        if let Ok(menu_event) = menu_channel.try_recv() {
            if start_id.as_ref() == Some(&menu_event.id) {
                let _ = Command::new("agentcoffeechat")
                    .arg("start")
                    .spawn();
            } else if menu_event.id == quit_id {
                *control_flow = ControlFlow::Exit;
            } else if menu_event.id == doctor_id {
                let _ = Command::new("agentcoffeechat")
                    .arg("doctor")
                    .spawn();
            }
        }

        // Check for state updates from the polling thread.
        if let Ok(new_state) = state_rx.try_recv() {
            if new_state != current_state {
                let new_icon = generate_icon(&new_state);
                let (new_menu, new_start_id, new_doctor_id, new_quit_id) = build_menu(&new_state);
                tray.set_icon(Some(new_icon)).ok();
                tray.set_menu(Some(Box::new(new_menu)));
                tray.set_tooltip(Some(new_state.status_text())).ok();
                start_id = new_start_id;
                doctor_id = new_doctor_id;
                quit_id = new_quit_id;
                current_state = new_state;
            }
        }

        // Process event loop; no additional event handling required.
        let _ = event;
    });
}
