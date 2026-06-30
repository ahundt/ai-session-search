use std::collections::{BTreeMap, HashSet};

use linkify::{LinkFinder, LinkKind};
use serde::Serialize;
use url::Url;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageRef {
    pub kind: String,
    pub value: String,
    pub normalized_value: Option<String>,
    pub host: Option<String>,
    pub source_tool: Option<String>,
    pub source_field: Option<String>,
    pub confidence: String,
    pub span_start: usize,
    pub span_end: usize,
}

pub fn extract_refs_from_text(text: &str, source_tool: Option<&str>) -> Vec<MessageRef> {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    finder.url_must_have_scheme(false);

    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for link in finder.links(text) {
        let raw = trim_trailing_punctuation(link.as_str());
        if raw.is_empty() {
            continue;
        }
        let span_start = link.start();
        let span_end = span_start + raw.len();
        let (normalized_value, host) = normalize_url(raw);
        let key = (
            raw.to_ascii_lowercase(),
            normalized_value.clone(),
            span_start,
            span_end,
        );
        if !seen.insert(key) {
            continue;
        }
        refs.push(MessageRef {
            kind: "url".to_string(),
            value: raw.to_string(),
            normalized_value,
            host,
            source_tool: source_tool.map(str::to_string),
            source_field: None,
            confidence: "parsed".to_string(),
            span_start,
            span_end,
        });
    }
    refs
}

pub fn ref_summary(refs: &[MessageRef]) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in refs {
        *counts.entry(item.kind.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(kind, count)| {
            if count == 1 {
                kind.to_string()
            } else {
                format!("{count} {kind}s")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalize_url(raw: &str) -> (Option<String>, Option<String>) {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    match Url::parse(&candidate) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => {
            let host = url.host_str().map(str::to_string);
            (Some(url.to_string()), host)
        }
        _ => (None, None),
    }
}

fn trim_trailing_punctuation(raw: &str) -> &str {
    raw.trim_end_matches(['.', ',', ';', ':', ')', ']', '}'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_explicit_and_scheme_less_urls_with_spans() {
        let text = "See https://example.com/paper.pdf, docs.rs/linkify, and www.rust-lang.org).";
        let refs = extract_refs_from_text(text, Some("WebSearch"));
        let values = refs.iter().map(|r| r.value.as_str()).collect::<Vec<_>>();
        assert!(values.contains(&"https://example.com/paper.pdf"));
        assert!(values.contains(&"docs.rs/linkify"));
        assert!(values.contains(&"www.rust-lang.org"));
        assert!(refs
            .iter()
            .all(|r| r.source_tool.as_deref() == Some("WebSearch")));
        assert!(refs.iter().all(|r| r.span_start < r.span_end));
        assert!(refs
            .iter()
            .any(|r| r.host.as_deref() == Some("example.com")));
    }

    #[test]
    fn summarizes_refs_by_kind() {
        let refs = extract_refs_from_text("https://a.test https://b.test", None);
        assert_eq!(ref_summary(&refs), "2 urls");
        assert_eq!(ref_summary(&[]), "");
    }
}
