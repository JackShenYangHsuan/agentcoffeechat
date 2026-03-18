pub mod doctor;
pub mod identity;
pub mod ipc;
pub mod plugin;
pub mod types;
pub mod wordcode;
pub mod sanitize;

pub use types::*;
pub use wordcode::{generate_three_word_code, validate_code};
pub use sanitize::{SanitizationPipeline, SanitizationStage, SanitizeResult};
pub use ipc::{DaemonCommand, DaemonResponse, IpcClient, socket_path};
pub use plugin::{AiTool, detect_ai_tool, detect_all_ai_tools, install_plugin, install_all_plugins, uninstall_plugin, is_plugin_installed};
pub use doctor::{run_doctor_checks, DoctorCheck, CheckStatus};
pub use identity::{get_or_create_identity, identity_exists_in_keychain, Identity};

/// Crate version, re-exported for CLI use.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
