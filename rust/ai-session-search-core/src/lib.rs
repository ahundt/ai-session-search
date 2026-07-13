#![recursion_limit = "256"]

pub mod analysis_pipeline;
pub mod analysis_publication;
pub mod analytics;
mod cli;
pub mod config;
pub mod dates;
pub mod db;
pub mod diagnostics;
mod durable_fs;
pub mod export;
pub mod files;
pub mod indexer;
pub mod inspect;
pub mod mcp_install;
pub mod mcp_server;
pub mod messages;
pub mod migration;
pub mod models;
// Safety guard (plan H8): the provider parse path must never `.unwrap()` on
// non-test code — a single malformed session file would abort the whole reindex.
// Errors there must flow through `minimal_record` (util.rs) instead. Scoped to
// `not(test)` so the providers' test fixtures may still use `.unwrap()` freely.
#[cfg_attr(not(test), warn(clippy::unwrap_used))]
pub mod providers;
pub mod refs;
pub mod render;
pub mod service;
pub mod source;
pub mod sql_query;
pub mod tail;
pub mod trigram;
pub mod trigram_index;
mod tui;
pub mod util;

pub use cli::run_from as run_cli_from;
