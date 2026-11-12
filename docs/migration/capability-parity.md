# Capability routes

The supported CLI, MCP, Python, and Rust routes are documented in the
[README task index](../../README.md#common-tasks),
[generated message-search reference](../README.md#generated-message-search-caller-contract), and
[library guide](../development/library-api.md). They share the Rust service layer.

MCP tools use closed input schemas (`additionalProperties=false`) and validate arguments before
index work. File recovery, export, analysis, and explicit reindexing use the CLI or libraries.
Skill instruction prose is loaded by the harness; AI Session Search runs deterministic rules from
`aise-capability.toml`. See [message classification](../../skills/ai-session-search/references/message-classification.md).
