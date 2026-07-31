use std::path::PathBuf;

use ai_session_search::config::{Config, IndexRefresh};
use ai_session_search::models::{MessageSearchMode, SearchField};
use ai_session_search::service::SessionSearch;
use ai_session_search::{MessageQuery, MessageSearchRequest, MessageTarget, RequestedExtent};

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
        .unwrap_or_else(|| "literal".into())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let field: SearchField = args
        .next()
        .unwrap_or_else(|| "content".into())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let mut config = Config::default();
    config.index.db_path = Some(database.to_string_lossy().into_owned());
    config.index.refresh = IndexRefresh::ExistingOnly;
    config.performance.threads = 1;
    let app = SessionSearch::open(config)?;
    let query = match mode {
        MessageSearchMode::Literal => MessageQuery::literal(query)?,
        MessageSearchMode::Regex => MessageQuery::regex(query)?,
        MessageSearchMode::Fuzzy => MessageQuery::fuzzy(query)?,
    };
    let target = match field {
        SearchField::Content => MessageTarget::content(),
        SearchField::ToolName => MessageTarget::tool_name(),
        SearchField::ToolArgument => MessageTarget::tool_argument("/cmd")?,
    };
    let request = MessageSearchRequest::builder(query, target)
        .extent(RequestedExtent::page(Some(10), 0)?)
        .build()?;
    println!(
        "{}",
        serde_json::to_string(&app.messages().search(request)?)?
    );
    Ok(())
}
