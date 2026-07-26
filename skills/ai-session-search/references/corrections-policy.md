# Correction policies

`aise corrections` finds messages where a **person** corrected the agent and classifies each into
a named category. The rules live in `corrections/policy.toml` inside a skill directory.

## How it works

1. `aise` scans messages with `role = 'user'` in **user-started** sessions.
2. Each message is tested against the selected policies in order, and within a policy against its
   categories in declaration order.
3. The **first** matching category wins. The result records the category name and the exact
   substring that matched.
4. Every result carries a receipt per evaluated policy: name, version, and the SHA-256 of the exact
   policy bytes.

Three consequences worth knowing before you write rules:

- **Order is behavior.** A catch-all category must be last, or it swallows everything below it.
- **Every pattern is case-insensitive.** All of a category's patterns are joined into one `(?i)`
  alternation, so `You Forgot` matches a rule written `\byou forgot\b`.
- **The reported match is the leftmost one in the message**, not the first pattern's match. For
  `"you missed X, and you forgot Y"` with patterns ordered `[forgot, missed]`, the reported text is
  `you missed`.

## Schema

```toml
schema_version = 1          # this build understands 1; an unknown value is rejected by name
name = "my-corrections"     # must equal the directory name and the SKILL.md `name`
version = "0.1.0"           # must equal SKILL.md `metadata.version`

[[categories]]
name = "clobber"
patterns = [
  '''\byou overwrote\b''',
  '''\byou clobbered\b''',
]
```

Use TOML multiline literal strings (`'''…'''`) for patterns. They need no backslash doubling, so
`\byou\b` stays readable instead of becoming `"\\byou\\b"`.

Validation is strict, and every failure names the offending field:

| Rejected | Why |
|---|---|
| Unknown top-level or category key | A misspelled `patterns` would leave a category with no rules and no complaint |
| `schema_version` other than `1` | A policy written for a newer `aise` must fail loudly, not lose categories |
| Empty `name`, `version`, or category name | An unnamed category cannot be reported |
| No categories, or a category with no patterns | Matches nothing, silently |
| A repeated category name | The second is unreachable — first match wins |
| A pattern that is empty, invalid, or matches empty text | Would claim every message |

## Selecting a policy

Precedence, highest first. Each rung **replaces** the one below rather than merging, so no run
half-inherits a configured set:

1. `aise corrections --skill NAME` (repeatable; argument order is evaluation order)
2. `[skills].enabled` in the aise config — an explicit `[]` means *evaluate nothing*
3. Legacy `[analytics].correction_patterns`
4. The `ai-session-search` policy built into the executable

Combining the legacy config field with an explicit skill selection is rejected rather than merged.
The built-in name is reserved: a directory claiming it is reported at its real path and refused,
so a stale or damaged install cannot redefine what the product means by a correction.

Being *discovered* is not being *selected*. A skill in a search path does nothing until you name
it, which is why dropping a file into a skills directory can never silently change measured output.

## Writing your own

```sh
aise skills create my-corrections --output-dir ~/.claude/skills
aise skills validate ~/.claude/skills/my-corrections
```

`create` seeds the file with the categories `aise` ships, so the usual path — start from the
defaults, change what you disagree with — needs no transcription. The scaffold is yours: it carries
no managed marker, so `aise` will never rewrite it.

Add its parent to `[skills].search_paths` in the aise config, then:

```sh
aise skills list                                   # confirm it is discovered and valid
aise corrections --skill my-corrections --format json
```

### Keep patterns narrow

The most common mistake is a rule that matches ordinary conversation. Anchor on the assistant:

| Too broad | Why it misfires | Better |
|---|---|---|
| `\brevert\b` | "let's revert to the design doc" | `\byou reverted\b` |
| `\bactually\b` | "actually, use a HashMap" | `\bthat'?s actually wrong\b` |
| `\bstop\b` | "run this once and stop" | `^\s*stop\b` or `\bstop doing\b` |
| `\bbroke\b` | "this broke down into subtasks" | `\bbroke the (build\|tests?)\b` |

Check a candidate against real data before keeping it:

```sh
aise corrections --skill my-corrections --limit 0 --format json | jq -r '.matches[].content'
```

## Using the results

```sh
aise corrections --format json
```

```json
{
  "policies": [{ "name": "ai-session-search", "version": "1.0.0-rc.1", "sha256": "cea1…" }],
  "matches": [
    {
      "session_id": "claude:0f9c…",
      "provider": "claude",
      "timestamp": "2026-06-03T00:00:00+00:00",
      "policy_name": "ai-session-search",
      "category": "skip_step",
      "matched_text": "you forgot",
      "content": "you forgot the migration"
    }
  ]
}
```

`policies` is always present, including when `matches` is empty — otherwise "these rules ran and
found nothing" and "no rules are selected" would look identical.

`matched_text` is the substring that matched, **not** the rule that matched it.

Page with `--limit` and `--offset` (newest first). `--limit 0` returns every match.

### Other surfaces

Same contract everywhere; only the spelling differs.

```sh
aise corrections --skill my-corrections --session-kinds user subagent
```

```python
from ai_session_search import SessionSearch, CorrectionQuery

report = SessionSearch().corrections(CorrectionQuery(skills=["my-corrections"], limit=0))
for receipt in report.policies:
    print(receipt.name, receipt.version, receipt.sha256[:12])
```

MCP agents call `find_corrections`, which adds `all_results` and its own page size because a tool
result lands directly in a context window.

## Subagent sessions

By default only user-started sessions are scanned. In a spawned subagent run the `role='user'`
rows are the **calling agent's** delegation prompt, not anything a person typed — and a prompt like
"don't forget to check the tests" matches a built-in category exactly. Subagent sessions outnumber
user-started ones roughly five to one, so including them by default would drown real corrections.

Pass `--session-kinds user subagent` to scan both, or `--session-kinds subagent` for delegation
prompts alone. This default deliberately differs from `aise search` and `aise list`, which return
both classes.
