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
        if is_probable_local_file_ref(raw) {
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

fn is_probable_local_file_ref(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("www.") {
        return false;
    }
    if lower.contains('/') {
        return is_probable_local_path_ref(&lower);
    }
    let bare = strip_line_suffix(&lower);
    if matches!(bare, "docs.rs") {
        return false;
    }
    let Some((stem, ext)) = bare.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && is_common_file_extension(ext)
}

fn is_probable_local_path_ref(raw: &str) -> bool {
    let first = raw.split('/').find(|part| !part.is_empty()).unwrap_or("");
    if first.contains('.') && !is_probable_local_file_ref(first) {
        return false;
    }
    raw.rsplit('/')
        .find(|part| !part.is_empty())
        .is_some_and(is_probable_local_file_ref)
}

fn strip_line_suffix(value: &str) -> &str {
    let Some((prefix, suffix)) = value.split_once(':') else {
        return value;
    };
    let line_ref = suffix.split_once('-').map_or_else(
        || !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()),
        |(start, end)| {
            !start.is_empty()
                && !end.is_empty()
                && start.chars().all(|c| c.is_ascii_digit())
                && end.chars().all(|c| c.is_ascii_digit())
        },
    );
    if line_ref {
        prefix
    } else {
        value
    }
}

fn is_common_file_extension(ext: &str) -> bool {
    matches!(
        ext,
        "adoc"
            | "astro"
            | "awk"
            | "bash"
            | "bat"
            | "bib"
            | "c"
            | "cc"
            | "cfg"
            | "clj"
            | "cljs"
            | "cmake"
            | "conf"
            | "cpp"
            | "cr"
            | "cs"
            | "csh"
            | "css"
            | "csv"
            | "dart"
            | "diff"
            | "env"
            | "erb"
            | "ex"
            | "exs"
            | "fish"
            | "fs"
            | "gemspec"
            | "go"
            | "graphql"
            | "groovy"
            | "h"
            | "haml"
            | "hbs"
            | "heex"
            | "hh"
            | "hpp"
            | "hs"
            | "htm"
            | "html"
            | "ini"
            | "ipynb"
            | "java"
            | "js"
            | "json"
            | "jsonl"
            | "jsonnet"
            | "jsx"
            | "kt"
            | "kts"
            | "less"
            | "lhs"
            | "lock"
            | "lua"
            | "m"
            | "md"
            | "mdx"
            | "mk"
            | "mm"
            | "nix"
            | "patch"
            | "pdf"
            | "php"
            | "plist"
            | "proto"
            | "ps1"
            | "psm1"
            | "py"
            | "pyi"
            | "r"
            | "rake"
            | "rb"
            | "rst"
            | "rs"
            | "scala"
            | "scss"
            | "sh"
            | "sol"
            | "sql"
            | "svelte"
            | "svg"
            | "swift"
            | "tex"
            | "toml"
            | "twig"
            | "ts"
            | "tsx"
            | "tsv"
            | "txt"
            | "vim"
            | "vue"
            | "wasm"
            | "xml"
            | "yaml"
            | "yml"
            | "zsh"
    )
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

    #[test]
    fn filters_filename_like_scheme_less_refs_without_dropping_domains() {
        let text = "See util.rs, cli.py:42, db.rs:75-91, paper.tex, refs.bib, notes.md, notebook.ipynb, figure.svg, src/db.rs, docs/refs.bib, docs.rs/linkify, github.com/org/repo, and example.com/path.";
        let refs = extract_refs_from_text(text, None);
        let values = refs.iter().map(|r| r.value.as_str()).collect::<Vec<_>>();
        assert!(!values.contains(&"util.rs"));
        assert!(!values.contains(&"cli.py:42"));
        assert!(!values.contains(&"db.rs:75-91"));
        assert!(!values.contains(&"paper.tex"));
        assert!(!values.contains(&"refs.bib"));
        assert!(!values.contains(&"notes.md"));
        assert!(!values.contains(&"notebook.ipynb"));
        assert!(!values.contains(&"figure.svg"));
        assert!(!values.contains(&"src/db.rs"));
        assert!(!values.contains(&"docs/refs.bib"));
        assert!(values.contains(&"docs.rs/linkify"));
        assert!(values.contains(&"github.com/org/repo"));
        assert!(values.contains(&"example.com/path"));
    }
}
