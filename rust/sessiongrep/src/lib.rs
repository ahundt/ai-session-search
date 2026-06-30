pub mod analytics;
pub mod config;
pub mod dates;
pub mod db;
pub mod files;
pub mod indexer;
pub mod mcp_install;
pub mod messages;
pub mod minhash;
pub mod models;
// Safety guard (plan H8): the provider parse path must never `.unwrap()` on
// non-test code — a single malformed session file would abort the whole reindex.
// Errors there must flow through `minimal_record` (util.rs) instead. Scoped to
// `not(test)` so the providers' test fixtures may still use `.unwrap()` freely.
#[cfg_attr(not(test), warn(clippy::unwrap_used))]
pub mod providers;
pub mod refs;
pub mod render;
pub mod tail;
pub mod trigram;
pub mod trigram_index;
pub mod util;
