# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/ahundt/ai-session-search/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include the `aise --version` output, the installation method, the operating
system, the exact command or MCP request, and what an attacker would gain. A
reproduction against synthetic fixtures is easier to act on and discloses none
of your own session content.

Reports go to the maintainer for triage. A fix ships as a new released version,
because a published registry version is immutable.

## Where fixes are published

A fix reaches you as a new release on
[crates.io](https://crates.io/crates/ai-session-search) and
[PyPI](https://pypi.org/project/ai-session-search/), so `aise package update`
picks one up. The retired `ai-session-tools` distribution receives no updates.

## What this software touches

AI Session Search reads AI coding-agent transcripts that already exist on the
machine running it, and writes an index, cache, and configuration under
platform application directories. `aise config paths` prints those read and
write locations.

- **Local data only.** It never fetches conversations from a vendor account and
  never uploads transcripts. The only request `aise` issues is the optional
  release check against the GitHub releases API, which MCP stdio and library
  calls never perform and which `--skip-release-notification`,
  `AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION=1`, or
  `[release_notifications].enabled = false` disables. `aise package update`
  reaches a registry only by running the package manager that owns the install,
  replacing the executable; `aise package status` contacts nothing.
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
artifacts are verified byte-for-byte before publication and carry GitHub
build-provenance attestations; Python distributions also carry PyPI
attestations. The crates.io package is reproduced deterministically before the
protected publish job asks Cargo to package it again.

crates.io and PyPI use OpenID Connect trusted publishing, while GitHub Release
creation uses the workflow's repository-scoped `GITHUB_TOKEN`, so no long-lived
registry token exists. The crates.io, PyPI, and release jobs each run in an
environment requiring maintainer approval. Publication is triggered by a `v*` tag, and
the `release-tags` ruleset restricts creating, moving, and deleting those tags
to the repository admin role.

CI runs `cargo deny check advisories licenses sources bans` against the locked
Rust dependency graph. Exceptions live in `deny.toml`, recorded one identifier
at a time with the condition that retires them. CycloneDX SBOMs and separate
Rust and Python license inventories are published with each release.

Attestations record how an artifact was built. They do not establish that it is
safe, and they do not replace inspecting it.
