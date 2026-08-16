# Message-classification capability

`aise skills corrections` finds messages where a **person** corrected the agent and classifies
each into a named category. The deterministic rules live in `aise-capability.toml`, directly beside
`SKILL.md` in the `ai-session-search` skill package.

## How it works

1. `aise` scans messages with `role = 'user'` in **user-started** sessions.
2. Each message is tested against the selected policies in order, and within a policy against its
   categories in declaration order.
3. The **first** matching category wins. The result records the category name and the exact
   substring that matched.
4. Every result carries package and capability provenance plus a receipt per evaluated rule set:
   name, version, and the SHA-256 of the exact `aise-capability.toml` bytes.

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
kind = "message-classification"

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
| A `kind` other than `message-classification` | The deterministic executor must not guess how to interpret the file |
| Empty category name | An unnamed category cannot be reported |
| No categories, or a category with no patterns | Matches nothing, silently |
| A repeated category name | The second is unreachable — first match wins |
| A pattern that is empty, invalid, or matches empty text | Would claim every message |

## Selecting packages

The first token after `aise skills` selects the primary package by catalog name, skill directory,
or exact `SKILL.md` path. Repeat `--skill NAME_OR_PATH` to evaluate additional
message-classification packages afterward. Argument order is evaluation order:

```sh
aise skills corrections --skill my-review --skill ../team-rules
```

The embedded `corrections` capability, shipped in the `ai-session-search` package, is the
product default. Other names are discovered beneath
`[skills].search_paths`; explicit paths let a person run a package without adding it to the
catalog. Being *discovered* is not being *selected*, so adding a directory to a search path never
silently changes measured output.

## Writing your own

```sh
aise skills create my-corrections --capability message-classification --output-dir ~/.claude/skills
aise skills validate ~/.claude/skills/my-corrections
```

`--capability message-classification` seeds `aise-capability.toml` with the categories `aise`
ships, so the usual path — start from the defaults, change what you disagree with — needs no
transcription; without the flag `create` writes only `SKILL.md`, a harness-only skill with no
capability for `aise` to run. `--output-dir` names the parent directory; set
`[skills].authoring_root` in `config.toml` to omit it. The scaffold is yours: it carries no
managed marker, so `aise` leaves it alone on `aise skills update`.

Add its parent to `[skills].search_paths` in the aise config (`aise config file` prints the
path), then:

```sh
aise skills list                                   # confirm it is discovered and valid
aise skills my-corrections --format json
```

Edit `aise-capability.toml` to change what is measured: `[[categories]]` entries with a `name` and
a `patterns` list of Rust regexes (see [Schema](#schema)); order is precedence and the first
matching category wins. `SKILL.md` beside it is what the harness reads; `aise` runs only the
capability file.

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
aise skills my-corrections --limit 0 --format json \
  | jq -r '.output.result.report.matches[].content'
```

## Using the results

```sh
aise skills corrections --format json
```

```json
{
  "requested_selector": { "name": "corrections" },
  "resolved_skill": {
    "name": "corrections",
    "package_version": "1.0.0-rc.1",
    "selected_location": { "kind": "embedded" },
    "execution_source": { "kind": "embedded" }
  },
  "output": {
    "capability": "message-classification",
    "result": {
      "receipt": {
        "name": "corrections",
        "version": "1.0.0-rc.1",
        "sha256": "cea1…"
      },
      "report": {
        "policies": [
          { "name": "corrections", "version": "1.0.0-rc.1", "sha256": "cea1…" }
        ],
        "matches": [
          {
            "session_id": "claude:0f9c…",
            "provider": "claude",
            "timestamp": "2026-06-03T00:00:00+00:00",
            "policy_name": "corrections",
            "category": "skip_step",
            "matched_text": "you forgot",
            "content": "you forgot the migration"
          }
        ]
      }
    }
  }
}
```

`report.policies` is always present, including when `report.matches` is empty. Otherwise "these
rules ran and found nothing" and "no rules ran" would look identical.
`output.receipt` is the primary selected skill's policy receipt and equals the first entry in
`report.policies`; the list also records every additional `--skill` policy in evaluation order.

`matched_text` is the substring that matched, **not** the rule that matched it.

CLI JSON and JSONL runs retain complete classified messages by default for scripts that need the
typed report. For bounded delivery, pass `--detail compact` or explicit
`--field-view-chars N`/`--match-view-chars N`; bounded matches return `message_ref`, classification
coordinates, and bounded `presentation` views instead of the full `content`. Use that reference
with `aise messages get --seq` when the complete turn is needed. These flags affect presentation
only, never classification, ordering, page membership, or policy receipts.

Page with `--limit` and `--offset` (newest first). `--limit 0` returns every match.
Selected packaged and direct capability definitions share a 1 MiB aggregate parsing safety
ceiling. If their combined canonical input exceeds it, Aise reports the consumed and attempted byte
counts with guidance; it never truncates rules or search results to fit.

### Other surfaces

Same contract everywhere; only the spelling differs.

```sh
aise skills my-corrections --session-kinds user subagent
```

```python
from ai_session_search import (
    MessageClassificationQuery,
    SessionSearch,
    SkillRunQuery,
    SkillSelector,
)

report = SessionSearch().run_skill(
    SkillRunQuery(
        skill=SkillSelector(name="my-corrections"),
        input=MessageClassificationQuery(limit=0),
    )
)
for receipt in report.output.report.policies:
    print(receipt.name, receipt.version, receipt.sha256[:12])
```

MCP agents call `run_skill_capability`. It executes only the deterministic
`message-classification` capability declared by adjacent `aise-capability.toml`; it does not load,
interpret, or follow the AI instructions in `SKILL.md`. The MCP client or harness interprets
`SKILL.md`. MCP adds `all_results` and its own page size because a tool result lands directly in a
context window, and explicit path selectors must be authorized by `[skills].search_paths`.

## Subagent sessions

By default only user-started sessions are scanned. In a spawned subagent run the `role='user'`
rows are the **calling agent's** delegation prompt, not anything a person typed — and a prompt like
"don't forget to check the tests" matches a built-in category exactly. Subagent sessions outnumber
user-started ones roughly five to one, so including them by default would drown real corrections.

Pass `--session-kinds user subagent` to scan both, or `--session-kinds subagent` for delegation
prompts alone. This default deliberately differs from `aise search` and `aise list`, which return
both classes.
