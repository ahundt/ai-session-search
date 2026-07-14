# AI Session Search migration provenance

## Source histories

| Role | Source | Selected source commit | Imported commit |
|---|---|---|---|
| Leader | `ai_session_tools` | `163db93e1106121957287acda9b8cb1c924f3a49` | unchanged |
| Follower | `sessiongrep` | `a0f6cfd3dc61f9dd0fb2b4c0f78f73e30f4001ce` | `8bc33ed7ce534184e48e69d476ab76818d27dbef` |

The histories were joined by merge commit
`90f6e594a0f14cd983238eaf1cf41e7ee6bb7af1`. Its first parent is the unchanged
aise leader and its second parent is the filtered, prefixed sessiongrep follower.
The selected ancestries contain 189 leader commits and 186 follower commits, with
zero shared commits. Together with the merge commit, the initial monorepo contains
376 reachable commits.

## Transformation

The follower was cloned from a complete, non-shallow local mirror. Local unpushed
commit `a0f6cfd` was fetched into that mirror without changing or publishing the
source repository. `git_history_cleanup_helper` commits `ee6e07c` and `b9f4758`
provided the clone-first merge, verified bundles, source fingerprints, exact path
exclusion, binary-preservation policy, commit maps, and final verification.

The follower history was rewritten to:

1. Remove `docs/demo.gif` and blob
   `97a0cac14675c31ec8aa5bb23735733e358b934d` from all destination history.
2. Move the remaining follower tree under `rust/ai-session-search-core/` (initially
   imported as `rust/sessiongrep/`, then renamed in the major-version cutover).
3. Preserve all other source, binary assets, commit authors, messages, dates, and
   selected ancestry.

The removed path, blob, and README reference are unreachable from destination
refs. Original-to-filtered commit maps and verified source bundles are private
migration audit artifacts and are not part of the published repository.

## Licensing and credit

Both source projects declare Apache License 2.0. Original commit authorship remains
in Git history. The imported sessiongrep work includes contributions by Andrew
Hundt and Nisarg Patel. This provenance statement records origin without implying
endorsement. Dependency license inventories, NOTICE handling, and SBOM artifacts
remain release gates.
