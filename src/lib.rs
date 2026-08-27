pub(crate) mod adapters;
pub(crate) mod bench;
#[cfg(feature = "bench")]
pub mod bench_api;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod db;
pub(crate) mod embedding;
pub(crate) mod export;
pub(crate) mod extension;
pub(crate) mod handoff;
pub(crate) mod import;
pub(crate) mod info;
pub(crate) mod mcp;
pub(crate) mod mcp_host;
pub(crate) mod project_scope;
pub(crate) mod query;
pub(crate) mod repo_identity;
pub(crate) mod semantic;
pub(crate) mod session;
pub(crate) mod session_action;
pub(crate) mod share;
pub(crate) mod share_init;
pub(crate) mod skill_audit;
pub(crate) mod sync;
pub(crate) mod transcript;
pub(crate) mod tui;
pub(crate) mod types;
pub(crate) mod usage;
pub(crate) mod utils;

pub(crate) const PROTOCOL_VERSION: u32 = 2;

#[cfg(test)]
mod integration;

pub fn init() {
    db::schema::register_sqlite_vec();
}

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
