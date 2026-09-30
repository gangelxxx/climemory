pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BINARY_PROTOCOL: u32 = 4;
pub const BINARY_PROFILE: &str = "climemory-memory-v1";
pub const BINARY_CAPABILITIES: &[&str] = &["multi_agent_memory", "local_context", "code_search"];
