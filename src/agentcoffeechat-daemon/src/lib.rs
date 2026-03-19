// Library facade — re-exports internal modules so integration tests and
// other crates can access them.  The binary entry point remains in main.rs.

pub mod ask_engine;
pub mod awdl;
pub mod chat_engine;
pub mod chat_history;
pub mod discovery;
pub mod session_manager;
pub mod transport;
