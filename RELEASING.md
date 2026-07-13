# Releasing ai_session_search

## Version number locations (keep in sync)

| File | Location |
|------|----------|
| `pyproject.toml` | line 7: `version = "X.Y.Z"` |
| `ai_session_search/__init__.py` | line 61: fallback `__version__ = "X.Y.Z"` |

The canonical version is read at runtime via `importlib.metadata.version("ai_session_search")` (set by the installed package metadata from `pyproject.toml`). The fallback in `__init__.py` is only used when the package is not installed (e.g. running directly from source without `uv tool install`).

## Version bump checklist

1. Update `pyproject.toml` `version = "X.Y.Z"`
2. Update `ai_session_search/__init__.py` fallback `__version__ = "X.Y.Z"`
3. Run full tests: `uv run pytest tests/ -q -m 'not integration'`
4. Commit: `git commit -m "chore(version): bump to X.Y.Z"`
5. Reinstall: `uv tool install -e .`
6. Verify: `aise --version`  # → "aise X.Y.Z"
7. Tag: `git tag vX.Y.Z`
8. Push: `git push origin main --tags`

## PyPI Publishing (automated via GitHub Actions)

After pushing the tag (step above), GitHub Actions will:
1. Run the full CI test suite (via reusable `ci.yml` workflow)
2. Verify the tag version matches `pyproject.toml` version
3. Build wheel + sdist with `uv build`
4. Publish to TestPyPI (requires `testpypi` environment)
5. Publish to PyPI (requires `pypi` environment approval if configured)

### First-time setup (one-time)

1. Create accounts on [pypi.org](https://pypi.org/account/register/) and [test.pypi.org](https://test.pypi.org/account/register/)
2. Enable 2FA on both accounts (required by PyPI)
3. Configure **Trusted Publishers** on both sites:
   - PyPI Project Name: `ai-session-tools`
   - Owner: `ahundt`
   - Repository: `ai-session-search`
   - Workflow: `publish.yml`
   - Environment: `pypi` (or `testpypi`)
4. Create GitHub Environments in repo Settings → Environments:
   - `testpypi` (no protection rules needed)
   - `pypi` (add required reviewers for manual approval gate)
5. Pin action versions to commit SHAs: `npx pin-github-action .github/workflows/publish.yml`

### First-time TestPyPI verification

```bash
uv pip install --index-url https://test.pypi.org/simple/ \
    --extra-index-url https://pypi.org/simple/ \
    ai-session-tools
```

### Manual publishing (fallback)

```bash
uv build
uv publish  # uses Trusted Publisher OIDC if run in GitHub Actions
```
