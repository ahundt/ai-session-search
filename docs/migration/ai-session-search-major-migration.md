# Rust migration

The Rust migration is complete. One core crate owns parsing, indexing, search, file reconstruction,
export, analysis, configuration, and publication. The CLI and MCP server call those services;
the Python extension converts their requests and results through PyO3.

For current interfaces, use the [library guide](../development/library-api.md).
For crate boundaries and packaging, use [Rust/Python API architecture](rust-python-api-architecture.md).
The [provenance record](provenance.md) retains source-history and licensing attribution.
