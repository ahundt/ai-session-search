use std::path::PathBuf;

use ai_session_search::config::{Config, IndexRefresh};
use ai_session_search::models::{MessageFilters, MessageSearchMode, SearchField};
use ai_session_search::service::SessionSearch;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let database = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("missing database"))?,
    );
    let query = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing query"))?;
    let mode: MessageSearchMode = args
        .next()
        .unwrap_or_else(|| "exact".into())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let field: SearchField = args
        .next()
        .unwrap_or_else(|| "content".into())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let argument_path = (field == SearchField::ToolArgument).then(|| "/cmd".to_string());
    let mut config = Config::default();
    config.index.db_path = Some(database.to_string_lossy().into_owned());
    config.index.refresh = IndexRefresh::ExistingOnly;
    config.performance.threads = 1;
    let app = SessionSearch::open(config)?;
    let filters = MessageFilters {
        match_mode: mode,
        field: Some(field),
        argument_path,
        limit: 10,
        ..Default::default()
    };
    println!(
        "{}",
        serde_json::to_string(&app.messages().search_legacy(&query, &filters)?)?
    );
    Ok(())
}
