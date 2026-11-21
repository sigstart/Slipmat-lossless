# AGENTS.md

Project instructions for Codex. Read this file before working in the repository.

## Instruction sources

`CLAUDE.md` remains the canonical shared project guide. Read it in full before
starting any task and follow every rule as if it appeared in this file. Treat
references to Claude or Claude Code as references to Codex.

If `CLAUDE.local.md` exists, read it in full after `CLAUDE.md`. It contains
personal context and machine-specific guidance. Its instructions take
precedence when they conflict with the shared guide.

Keep these files as the source of truth. Do not copy their contents into this
file; parallel copies would drift and leave Claude Code and Codex following
different project rules.

## FairPlay evaluation

Follow [the phase-one spec](docs/specs/SPEC-fairplay-phase-1.md) for work on
replacing Widevine with FairPlay. Adam is the sibling reference checkout at
`../INSPIRATION/apple-music-linux/`. Read only the files needed for the assigned
question. Use synthetic fixtures and keep real account data out of offline tests.
Follow the phase-one spec when moving from offline checks to a native playback
prototype.

### Required delegation

For phase-one execution, delegate each applicable row once per milestone. The
repository profiles in `.codex/agents/` pin the models and reasoning effort.
When the session exposes only explicit model overrides, submit the indicated
model and effort with a fresh context and give the agent its profile instructions,
repo path, task, and relevant excerpts. If a model is unavailable, disclose the
fallback and choose the closest available model tier.

| Work | Profile | Model | Effort |
| --- | --- | --- | --- |
| Map Slipmat's child protocol and daemon behavior | `playback_mapper` | `gpt-6-luna` | Low |
| Review Adam's dependency provenance, redistribution evidence, and authentication risks | `provenance_reviewer` | `gpt-6.2-sol` | Medium |
| Design the playback boundary after both reviews and assess the evidence for live integration | `playback_architect` | `gpt-6.2-astra` | High |
| Implement and verify the offline contract test after the design is settled | `playback_test_worker` | `gpt-6.2-sol` | Medium |

Run the two reviews in parallel. Send their concise findings to the architect.
The lead resolves conflicts, decides whether the evidence supports integration,
assigns one writer for the test, and verifies the result. Keep agent handoffs to
file and symbol references, decisions, unresolved questions, and checks run.
Workers return results to the lead without spawning further agents. Routine
documentation fixes and follow-up checks stay with the lead.
