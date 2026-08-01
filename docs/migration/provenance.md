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

Both source projects declare Apache License 2.0 at the commits this repository
imported, and the merged project is Apache-2.0 with Andrew Hundt as the principal
copyright holder.

sessiongrep did not start there. Commit `f043695` (2026-05-08, Nisarg Patel)
created its `LICENSE` as the MIT License, `Copyright (c) 2026 Brain Company`.
That upstream names itself two ways in its own commits, `Brain Company` in the
MIT notice and `Brain Co. Technologies Inc.` in the Apache appendix; `NOTICE`
uses the second as the formal entity name.
Its author relicensed it in commit `43a3b97` (2026-05-27, Nisarg Patel),
`chore: relicense from MIT to Apache-2.0`, which also updated the crate manifest.
The selected follower commit `a0f6cfd` is a descendant of that relicensing, so
the code entered this repository already under Apache-2.0. No relicensing was
performed here.

The Apache-2.0 text that relicensing commit introduced was not the verbatim
upstream license: it dropped "reasonable and customary use in" from section 6,
reflowed section 9, removed the leading blank line, and filled the appendix
example with a copyright holder name. That text reached this repository unchanged
and was replaced with the verbatim
[Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0.txt),
sha256 `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30`, which a
repository contract test now pins for both shipped copies.

## Per-file copyright attribution

Original commit authorship remains in Git history. `.mailmap` collapses duplicate
identities so `git shortlog -sne` reports one entry per person.

Source files carry [REUSE 3.3](https://reuse.software/spec-3.3/)
`SPDX-FileCopyrightText` and `SPDX-License-Identifier` headers. Andrew Hundt is
named in every source file as the principal copyright holder. A file names an
additional holder where that contributor's work is still materially present in
it.

`scripts/measure_authorship.py` is the tool for deciding that, by aggregating
`git blame --line-porcelain -w -M` per author over the tracked source files. Run
it when adding a file or after substantial rewriting; it prints which files name
whom under the current tree.

Its counts are an input to a judgement, not a score. They are deliberately not
recorded here: they shift with ordinary refactoring, and a contributor's standing
does not. Copyright follows creative contribution, not line count, and a holder
whose lines have since been rewritten still holds copyright in the history. Add a
header line freely; remove one only with that contributor's agreement.

`NOTICE` names every contributor, with no threshold.

This provenance statement records origin without implying endorsement. Dependency
license inventories, NOTICE handling, and SBOM artifacts remain release gates.
