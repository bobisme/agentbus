# agentbus

Watch what the coding agents on your machine are doing, and publish it somewhere
anything can subscribe to.

It observes Claude Code, Codex and OpenCode, normalises them into one event
vocabulary, and writes two things:

- **an event log** — append-only JSONL, tailable and replayable
- **a state snapshot** — what is true right now, for subscribers that only want that

It renders nothing. Building a status bar, a notifier, or a multiplexer plugin on
top is the point.

## Install

```bash
just install     # binary into ~/.local/bin
just service     # ...and run it as a user service
```

## Wire up the agents

Every hook is `agentbus hook …`, and all of them exit 0 unconditionally — a
monitoring hook must never be able to wedge an agent.

| Agent | Event | Command |
|---|---|---|
| Claude Code, Codex | SessionStart, UserPromptSubmit | `hook register` |
| Claude Code, Codex | SubagentStart / SubagentStop | `hook subagent start` / `stop` |
| Claude Code, Codex | PermissionRequest | `hook state blocked permission` |
| OpenCode | (plugin) | `integrations/opencode.js`, installed by `just sync-opencode` |

Claude Code's `~/.claude/settings.json` and Codex's `~/.codex/hooks.json` share
the same shape:

```json
{ "hooks": { "UserPromptSubmit": [
  { "hooks": [ { "type": "command", "command": "/home/you/.local/bin/agentbus hook register" } ] }
] } }
```

## Where state comes from

Ranked by trust:

1. **Transcripts** — `~/.claude/projects/**`, `~/.codex/sessions/**`. No agent
   cooperation, no races, and far more detail than hooks: titles, prompts, tool
   calls, tokens, turn boundaries. The primary source.
2. **Hooks** — for the two things transcripts cannot carry: a subagent's
   completion and result, and permission prompts.

Hooks are an *identity bridge*, not a data source. Transcripts say everything
about a session except where it is: nothing on disk records which pane an agent
occupies, agents do not hold their transcript open, and matching on `cwd` is
ambiguous the moment two agents share a project directory. So one hook reports
that, and only that.

**`blocked` only ever arrives by report.** A permission prompt is UI state and
reaches no transcript. **There is deliberately no `done`** — an agent that just
finished is one you have not given the next thing to yet, which is idle.

## Subscribing

The snapshot is a single JSON document, rewritten atomically whenever the state
it describes changes:

```json
{ "version": 1, "sessions": [ {
  "session": "f431ece9-…", "source": "claude",
  "title": "…", "label": "the current task", "state": "working",
  "tokens": { "output": 1291114, "context": 163187 },
  "location": { "mux": "zellij", "session": "wise-apricot", "pane": "4" },
  "subagents": [ { "id": "…", "state": "done", "agent_type": "Explore",
                   "description": "…", "result": "…" } ]
} ] }
```

`--snapshot <PATH>` publishes elsewhere, which is how a sandboxed subscriber gets
it — point it inside whatever directory that subscriber can read. A pinned path
is written only once its directory exists, so it is safe to aim at a directory
some other program creates later.

`location.mux` is `zellij`, `tmux`, `wezterm` or `kitty`, detected from the
environment the hook ran in. A subscriber should ignore locations it cannot
render rather than assuming its own.

## Liveness

A registration holds while the exact process that made it is alive — identified
by pid *and* start time, since a recycled pid cannot share a start time. It
deliberately does not read `/proc/<pid>/environ` to confirm the pane: that needs
ptrace access, and under `ptrace_scope=1` only a descendant of the agent has it.
It works run by hand from inside the pane and fails as a service, which is how
this actually runs.

## Poking at it

```bash
just state       # what subscribers see
just snapshot    # fold recent history once, without publishing
just events      # follow normalised events
```

## Notes for anyone extending it

- **Adding an agent** means writing one normaliser. Subscribers never see a
  native schema.
- **Derive nothing you can observe.** Every real bug here came from reading a
  record as something it was not: Claude's `last-prompt` is a restatement written
  *after* the turn ends, not the start of one; a Codex subagent's rollout records
  its *parent's* id in `session_id` and its own in `id`.
- **Expiry belongs to the publisher.** A subscriber that recomputes "finished"
  from each snapshot cannot know how long ago it happened.
- **Publish on state difference, not on event arrival.** Expiry and a
  registration going stale both change what is true while producing no event.
