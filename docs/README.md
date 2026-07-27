# AI Session Search documentation

Use the repository [README](../README.md) for product capabilities and a quick
start. The documents below separate user procedures from maintainer records.

## User and operator guides

| Task | Guide |
| --- | --- |
| Install, verify, update, or uninstall `aise` and its MCP integrations | [Installation](development/installation.md) |
| Resolve CLI, environment, TOML, Rust, Python, and MCP settings | [Configuration](development/configuration.md) |
| Build, verify, and publish Cargo, PyPI, and GitHub artifacts | [Releasing packages](development/releasing.md) |
| Preserve cumulative product contracts while changing the repository | [Maintainer requirements and design decisions](development/maintainer-requirements-and-design-decisions.md) |

For a new installation, follow the installation guide's five steps in order.
For an existing installation with unexpected paths or settings, run `aise
config origins`, then use the configuration guide. Maintainers preparing a
release should complete the releasing guide's ordered workflow without skipping
exact-artifact verification or protected-environment approval.

## Architecture and migration records

These files explain completed design decisions and preserve migration evidence;
they are not installation checklists.

| Record | Purpose |
| --- | --- |
| [Major migration](migration/ai-session-search-major-migration.md) | Ordered migration ledger, checkpoints, and remaining gates |
| [Capability parity](migration/capability-parity.md) | Legacy capability disposition and semantic-duplication decisions |
| [Rust/Python API architecture](migration/rust-python-api-architecture.md) | Public API boundaries and distribution contract |
| [Provenance](migration/provenance.md) | Source histories, transformation, licensing, and credit |
