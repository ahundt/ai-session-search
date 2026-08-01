# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/ahundt/ai-session-search/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include the `aise --version` output, the installation method, the operating
system, the exact command or MCP request, and what an attacker would gain. A
reproduction that runs against synthetic fixtures rather than your own session
transcripts is easier to act on and does not disclose your data.

Expect an acknowledgement within seven days. Fixes ship in a new released
version; published registry versions are immutable and are never replaced.

## Supported versions

Security fixes target the latest released version on
[crates.io](https://crates.io/crates/ai-session-search) and
[PyPI](https://pypi.org/project/ai-session-search/). The retired
`ai_session_tools` distribution receives no updates of any kind.

## What this software touches

AI Session Search reads AI coding-agent transcripts that already exist on the
machine running it, and writes an index, cache, and configuration under
platform application directories. Understanding that boundary makes a report
easier to classify:

- **Local data only.** It never fetches conversations from a vendor account and
  never uploads transcripts. The only outbound network request is the optional
  release check against the GitHub releases API, disabled with
  `--skip-release-notification`, `AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION=1`,
  or `[release_notifications].enabled = false`, and never performed by MCP stdio
  or by Rust and Python library calls.
- **Indexed content is sensitive.** The index holds whole conversations,
  including any secrets a transcript captured. Reports about the index leaking
  outside its configured directory, about file permissions on it, or about
  content crossing a documented result boundary are in scope.
- **It writes to harness configuration.** `aise integrations install` edits MCP
  client configuration, instruction files, skills, and command aliases. Reports
  about it modifying or deleting bytes it does not own are in scope.
- **Untrusted input.** Transcripts are parsed from files written by other
  programs. Reports about a malformed transcript causing memory unsafety,
  path traversal on export or restore, or command execution are in scope.

## Supply chain

Every third-party GitHub Action is pinned to a full-length commit SHA. Release
artifacts are built once, verified byte-for-byte, and published with GitHub
build-provenance attestations and PyPI attestations. Publication to crates.io,
PyPI, and GitHub Releases uses OpenID Connect trusted publishing through
protected environments that require maintainer approval; no long-lived registry
token exists. Publication is triggered by a `v*` tag, and the `release-tags`
ruleset restricts creating, moving, and deleting those tags to the repository
admin role.

CI runs `cargo deny check advisories licenses sources bans` against the locked
Rust dependency graph. Exceptions live in `deny.toml`, are recorded one
identifier at a time with the condition that retires them, and never cover a
security vulnerability. CycloneDX SBOMs and separate Rust and Python license
inventories are published with each release.

Attestations record how an artifact was built. They do not establish that it is
safe, and they do not replace inspecting it.
