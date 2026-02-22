pub mod client;
pub mod connect;
pub mod daemon;
pub mod protocol;
pub mod security;
pub mod server;

/// Collect TERM/LANG/COLORTERM from the environment for forwarding to remote sessions.
pub fn collect_env_vars() -> Vec<(String, String)> {
    ["TERM", "LANG", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}
