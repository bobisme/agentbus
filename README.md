# agentbus

Watch what the coding agents on your machine are doing, and publish it somewhere
anything can subscribe to.

It observes Claude Code, Codex, OpenCode and agy (Antigravity CLI, Gemini),
normalises them into one event vocabulary, and writes two things:

- **an event log** — bounded JSONL generations, tailable and recently replayable
- **a state snapshot** — what is true right now, for subscribers that only want that

The observer renders nothing. Building a status bar, a notifier, or a
multiplexer plugin on top is the point; the same binary also includes a native
reader for seeing the whole machine at once.

## System-wide UI

```bash
agentbus ui          # verified running agents only
agentbus ui --all    # include unverified and historical records
```

The default is deliberately strict. Recent transcripts and hook reports can
remain after their process exits, so only a registration whose pid and process
start time still match is presented as running. Press `a` to include everything
the observer knows, `j`/`k` to move, `c` to change density, and `q` to leave.

The native UI shows agents from every supported multiplexer as well as bare
terminals, with the same normalised state, task, model, tool/context statistics,
and nested subagents. Multiplexer-specific actions such as previewing or focusing
a pane remain the responsibility of that multiplexer's subscriber.

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
| agy | PreInvocation, Stop | `integrations/agy-hooks.json`, installed by `just sync-agy [dir]` |

Claude Code's `~/.claude/settings.json` and Codex's `~/.codex/hooks.json` share
the same shape:

```json
{ "hooks": { "UserPromptSubmit": [
  { "hooks": [ { "type": "command", "command": "/home/you/.local/bin/agentbus hook register" } ] }
] } }
```

## Where state comes from

Ranked by trust:

1. **Transcripts** — `~/.claude/projects/**`, `~/.codex/sessions/**`, and
   `~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl`.
   No agent cooperation, no races, and far more detail than hooks: titles,
   prompts, tool calls, tokens, turn boundaries. The primary source.
2. **Hooks** — for the things transcripts cannot carry: a subagent's completion
   and result, permission prompts, and agy's turn boundary.

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
{ "version": 2, "sessions": [ {
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

`version` is the schema of this document. Accept the versions you know and warn
on anything else — the failure it exists to prevent is silent, not loud. Version
2 renamed `pane: {zellij_session, pane_id}` to `location: {mux, session, pane}`;
a subscriber reading the old path against the new document matches no session,
never binds, and reports a timeout while the agent sits there having already
answered. Purely additive fields do not bump it, which is the distinction the
number is there to express.

Better still, do not read this file. `agentbus sessions` answers the same
questions and keeps the schema an internal transport rather than a contract.

`location.mux` is `zellij`, `tmux`, `wezterm` or `kitty`, detected from the
environment the hook ran in. A subscriber should ignore locations it cannot
render rather than assuming its own.

All three location fields are **empty when the agent is not in a multiplexer we
recognise** — a bare terminal, a CI job, a PTY runtime of someone's own. The
session is still published: the registration also carries the agent's pid and
start time, which is an exact process identity, so a supervisor can say "this
session is the one I spawned" instead of inferring it from a matching `cwd` and
plausible timing — which is ambiguous the moment two agents share a repo. An
empty location is also what a subscriber already sees when a binding goes stale,
so it needs no new handling.

### Turn boundaries

`prompt` opens a turn and `turn_end` closes it, **for every source**. A
subscriber can wait on that pair without knowing which agent it is watching,
which is the point of a normalised vocabulary — gate on the events, or on the
snapshot's `state`, whichever suits.

`turn_end` carries the agent's final message from every source too. Claude's own
end-of-turn record holds nothing but a duration, so the normaliser remembers the
last assistant text of the turn and attaches it there; Codex carries it on the
completion event. Neither asymmetry reaches a subscriber.

It arrives in two spellings, because they have different jobs:

- `result` — collapsed to one line and capped at 160 characters. A preview, for
  a status bar. Lossy by design.
- `result_full` — the message, untruncated, paragraphs intact.

Read `result_full`. `result` exists for renderers, and a consumer that took it
for the answer silently lost everything past the first sentence.

## Storage and retention

The default state directory is `${XDG_STATE_HOME:-$HOME/.local/state}/agentbus`.
Every artifact has a bounded role:

| Path | Purpose | Bound |
|---|---|---|
| `events.jsonl` | Active verbose event generation | Rotates at 64 MiB |
| `events.jsonl.*.sealed` | Recent verbose history | Seven days and 512 MiB total with the active log |
| `completions.json` | Generation watermark plus ordered `turn_end` results for `wait` | Seven days, 10,000 records, 64 MiB |
| `cursors.json` | Restart-safe transcript publication offsets | 10,000 existing transcripts |
| `hook-spool/*.ready` | Lock-free durable hook ingress | Drained immediately; abandoned records age out after seven days or 64 MiB |
| `inbox.jsonl` | Retained hook facts needed to reconstruct state | Seven days for timestamped records, 10,000 records, 64 MiB |
| `register.jsonl` | Newest known registration per session | 10,000 sessions |

The event limits can be changed with `--log-generation-mib`, `--log-max-mib`,
and `--log-max-days`. The generation size is automatically clamped to the total
cap. Follow the active pathname with `tail -F`, not `tail -f`: rotation renames a
complete old generation and creates a new `events.jsonl`.

`wait` does not read the verbose log. It takes a generation watermark from
`completions.json`, before resolving the session, and selects the first matching
completion after that generation. This keeps rotation out of the supervision
correctness path. A `--since` older than retained completion history fails
explicitly as `expired` (exit 1) instead of silently returning a newer or stale
answer. Individual completion results larger than 1 MiB are marked and truncated
in this bounded projection; the verbose event remains the diagnostic source.

On first upgrade, an already-oversized `events.jsonl` is renamed with a
`.legacy` suffix and deliberately excluded from automatic deletion. Verify the
new publisher, completion waits, and subscriber state before removing that one
file. Unknown files and symlinks are never retention candidates.

Hooks write unique temporary files and atomically rename them to `.ready`; they
never wait for the observer or take its publisher lock. The observer drains and
deduplicates those records before compacting the legacy-compatible JSONL
projections. A crash after journal append but before spool acknowledgement is
therefore replay-safe.

## Asking

Reading the snapshot file directly works, and every consumer that did it wrote
the same four things: find the file, parse its schema, resolve an identity to a
session, and keep up as both change. Two verbs replace all of it.

```bash
agentbus sessions [--pid N] [--session S] [--cwd D] [--json]
agentbus wait (--session S | --pid N | --cwd D) [--timeout SECS] [--since EPOCH[.FRAC]] [--json]
```

**`--pid` matches the process named or any descendant of it.** agentbus
registers the *agent* process, which is not always the one a supervisor spawned
— codex runs behind a node shim, so the supervisor holds the shim and the
registration holds the real binary one level below. For claude the two coincide,
which is what makes this an easy bug to ship: it works until it is pointed at
codex.

**Since codex 0.159 there is no codex process per session to register.** Every
TUI hands its turns to one shared `codex app-server` daemon, which runs the
hooks, and nothing a hook can see names the TUI. So a hook under the daemon
records it as the session's *host*: `sessions` shows such a session as
`hosted`, with pid 0. `--pid N` then finds it through the codex client at or
below N: the session whose cwd is that client's, registered after the client
started. That is exact when each agent has its own directory, which concurrent
agents need anyway. Two sessions that fit, such as two clients in one
directory, or one client that started a second thread, fail as ambiguous (exit
1). They are never guessed between. `--pid` of the daemon itself also fails
at once, and so does a client whose cwd cannot be read, instead of waiting out
the timeout.

**`wait` blocks until the current-or-next turn ends** and prints the full answer
on stdout, or `{status, session, result, duration_ms}` with `--json`. It exits
`0` done, `3` blocked, `4` timeout, `1` for resolution, observer, or retained
history errors.
`blocked` being distinct is the point of it: an agent sitting on a permission
prompt looks exactly like a slow one to anything watching a screen, and burns
the caller's whole timeout.

It never reports a turn that ended before it started — internally a generation
watermark on the bounded completion index, taken at entry. That is the one piece
a caller cannot do for itself across two processes without persisting shared
state. The
consequence is that a caller submitting first should mark the moment and say so:

```bash
t=$(date +%s.%N)
send_prompt_somehow
agentbus wait --pid $AGENT_PID --since $t --json
```

Without `--since`, a turn that finishes between submitting and calling is behind
the watermark, and the wait sits there until the *next* one.

Mark with the fraction. `--since` admits any turn that ended at or after it,
and a caller driving one session turn after turn often marks the next turn in
the same second the last one ended. At whole seconds (`date +%s`)
that last turn qualifies again and comes back, instantly, as the answer to the
prompt just sent. Turn ends are stamped to the millisecond, so a mark as fine
tells them apart.

A session that has not registered yet is waited for rather than rejected — an
agent does not appear until its first prompt, so the session a supervisor just
gave work to routinely does not exist at the moment it asks.

### agy is the odd one

Its transcript is plain JSONL with a monotonic `step_index` and an ISO
`created_at`, written for every conversation whether or not hooks are set up, so
discovery needs no cooperation. The `.db` files under `conversations/` are the
conversation store, are protobuf, and are deliberately never read.

What it does not write is a turn boundary, and it is not derivable. The obvious
rule — a `PLANNER_RESPONSE` bearing prose and no tool calls, which is agy's own
`NO_TOOL_CALL` stop reason — gives 13 candidates across a 7-turn conversation,
because the model narrates between tool batches. So the `Stop` hook carries it,
the way Claude's `turn_duration` record does.

**Without the hooks installed, agy sessions still appear** — with prompts, tool
calls and labels — but never leave `working`, since nothing tells the bus a turn
ended. `just sync-agy` installs them once, for every project.

They go in `~/.gemini/config/hooks.json`. agy's own documentation says only
"your customization root", which is misleading: `.agents/` is a customization
root for skills and rules but is **not read for hooks at any level**, so a
`hooks.json` placed there looks right and never fires. The real answer came from
`strace` — agy probes four paths and opens that one.

Two things it gets from the hook payload that its transcript never states: the
working directory (`workspacePaths`) and the model (`modelName`). It spells its
payload keys camelCase, being protojson, and reports `transcriptPath` without
the `.jsonl` extension the file actually has.

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
agentbus sessions   # the same question, answered rather than dumped
```

The reading verbs find the running observer's files themselves, via a small
record it writes to its state dir on startup. Nothing needs to know that the
service is pointed somewhere else — which it usually is, since a sandboxed
subscriber gets `--snapshot` aimed at a directory it can read.

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
