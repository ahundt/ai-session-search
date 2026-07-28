---
name: corrections
description: Find recurring cases where a person corrected an AI agent, classify each matched user message with deterministic ordered rules, and turn repeated evidence into preventive guidance.
metadata:
  version: 1.0.0-rc.1
---
<!-- ai-session-search-managed-skill v1 -->

# Corrections

Use this skill when the task is to find what a person repeatedly had to correct in prior AI
sessions. Aise executes the adjacent `capability.toml` deterministically; the skill instructions
explain how to scope, interpret, and follow up on those results.

```sh
aise skills corrections --when 30d --format json
aise skills corrections --path ~/source/project --limit 50
aise skills corrections --skill ./my-review --format json
```

Only user-role messages in user-started sessions are scanned by default. Add
`--session-kinds user subagent` only when delegation prompts are intentionally part of the
analysis. Categories and selected skills are evaluated in declaration order, and the first match
wins. Every run reports the exact capability digest.

Read `references/message-classification.md` before changing categories or regular expressions.
If a correction requires implementation, inspect existing dependencies and abstractions first. Reuse a fitting library; add a mature, widely used library only when verified
contract, lifecycle, platform, performance, dependency, and release fit make it safer and simpler than custom code.
