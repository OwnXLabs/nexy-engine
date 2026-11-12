# Nexy Engine

A small agent engine for local models, written in Rust. One binary, one SQLite file, any
OpenAI-compatible model server (Ollama, LM Studio, llama.cpp, OpenVINO).

The goal is simple: keep the engine efficient, so the model gets the machine's memory and time.

## Constraints

- **Model time is scarce.** A local model writes tens of tokens a second, shared by every bot.
- **Memory is shared with the GPU** on integrated-graphics and unified-memory machines.
- **Local model servers differ** in how they format tool calls, and each has bugs.
- **Keys and private data live on the owner's machine**, and any web page can try to steer a bot.

## Principles

- **Do one job.** No chat channels or cloud providers; one connection to a local model server.
- **Bots are data.** A waiting bot is a small record and a timer, not a running process.
- **Keep the prompt opening identical** so the model server can reuse its cache.
- **The engine owns tool calling.** Probe the server, pick native tools, forced JSON or a text
  protocol, check every call, retry once with the error, then stop and say why.
- **Set everything explicitly.** Thinking and sampling settings are sent on every request. On
  Ollama the engine uses its native chat API so it can also set `think` and the context length:
  the model's own limit, capped at 32k tokens and by what the machine's memory can hold.
  Reasoning is never shown as a reply; a model that cannot stop thinking keeps it in its own field.
- **Lock down each command.** `run` uses a fresh confined process (Landlock on Linux, a
  deny-by-default `sandbox-exec` profile on macOS): writes only to the workspace and its temp
  folder, no network, CPU, memory and process limits, never as root. If it can't be confined, it
  doesn't run; Windows has no sandbox yet, so `run` is refused there.
- **Nothing a command starts outlives it.** A supervisor ends every process the command started
  when it finishes or times out, including ones that called `setsid` or forked twice: on Linux it
  is a subreaper outside the sandbox; on macOS it runs inside the command's sandbox, which lets it
  signal only that sandbox's processes. At most 4 commands run at once, and each may add at most
  256 processes (threads on Linux) to what its user already runs.

## Benchmark (v0.0.1, November 2026)

Results for Nexy Engine v0.0.1; later releases are not re-run here. Five small coding tasks (fix a bug, add a feature, refactor, build a page from a spec, find a bug
from a stack trace), three runs each, graded by hidden tests. Same machine, same model
(Qwen3.6-35B-A3B int4 on an Intel Arc 140T mini PC), same proxy and grader for every engine.
Editing a test file counts as a fail.

| Engine | Language | Size on disk | Peak RAM | Solved | Median time | Median prompt tokens |
|---|---|---|---|---|---|---|
| Nexy Engine 0.0.1 | Rust | 3.9 MB | 6 MB | 15/15 | 73 s | 13k |
| OpenClaw 2026.10.10 | TypeScript | 958 MB | 1.35 GB | 14/15 | 213 s | 64k |
| ZeroClaw 0.8.7 | Rust | 99 MB | 50 MB | 13/15 | 181 s | 52k |
| OpenCode 2.2.1 | TypeScript | 210 MB | 94 MB | 13/15 | 183 s | 22k |
| Hermes Agent 0.22.6 | Python | 2.4 GB | 400 MB | 11/15 | 396 s | 114k |
| nanobot 0.4.0 | Python | 349 MB | 175 MB | 8/15 | 166 s | 89k |
| NullClaw nightly | Zig | 5.5 MB | 4.3 MB | 7/15 | 142 s | 74k |
| IronClaw 1.4.1 | Rust | 200 MB | 162 MB | 7/15 | 1,200 s | 152k |
| Goose 1.57.0 | Rust | 154 MB | 114 MB | 6/15 | 159 s | 23k |

Read this with care:

- Nexy Engine was developed against these same five tasks. A fresh task set is needed before the
  score means much.
- One machine, one model, small tasks. Other setups will give other results.
- Most other engines include a web dashboard and many channels and providers in their size.
  Nexy Engine has no UI and one provider. Engine to engine, a static Nexy Engine build is 3.7 MB
  and NullClaw's static release is 5.5 MB.
- NullClaw and OpenCode RAM figures are approximate: NullClaw's counts its main process only, and
  OpenCode's run had not finished when measured.

## Run

```sh
cargo build --release
NEXY_ENGINE_MODEL_URL=http://127.0.0.1:11434 NEXY_ENGINE_MODEL=qwen3.6:35b-a3b ./target/release/nexy-engine serve
```

Commands: `serve`, `probe`, `ask TEXT`, `idle-memory N`. Bots are defined in `packs/`. The API listens on
loopback and needs `NEXY_ENGINE_KEY` to listen anywhere else. Without a key it answers only requests
whose `Host` is `127.0.0.1`, `localhost` or `[::1]` on its own port, or a name listed in
`NEXY_ENGINE_HOSTS` (comma separated); anything else gets `421`, which stops DNS rebinding.

## API

- `POST /v1/chat/completions`: OpenAI-shaped. The engine keeps each session's history; only the
  last user message is read. `"research": true` makes the engine search and read the top pages
  before the model answers. Links in a message are always read first. With `"stream": true` the
  reply streams as server-sent events as the model writes it; tool calls and reasoning are held
  back, tool use shows as `: tool <name>` comments, and the last event carries `usage` and `nexy`.
- `POST /v1/responses` and `GET /v1/uhp`: the [Unified Harness Protocol](https://unifiedharnessprotocol.org)
  (core class, version 2026-10-19). Each bot is a harness.
- `/api/sessions`, `/api/jobs`: history and scheduled tasks (routines). A schedule is `every 30m`,
  `daily 08:00` or standard 5-field cron such as `0 9 * * 1-5`, and new routines run at most every
  5 minutes. Clock times are read in the owner's time zone: send an IANA name such as
  `"tz": "Asia/Manila"` with `POST /api/jobs`, with chat completions (top level) or with responses
  (`metadata.tz`); routines the model creates take the chat's zone. Without one it is UTC, and
  jobs made before zones stay UTC. Clock changes keep the wall-clock time: `daily 08:00` in
  `Europe/London` runs at 08:00 in both GMT and BST; a time skipped in spring runs just after the
  jump, and a time repeated in autumn runs once. Unknown zones get `400`. `GET /api/jobs` lists
  each job's `tz`. A user keeps at most 50 routines and a bot holds at most 200, waiting
  ones included (`409` when full). Bots with the `routine` tool can also create, list and delete
  routines in chat.
- Routine approval: once outside content is in a chat's context (a web page, search result,
  command output, file or saved memory, in this turn or earlier ones, or a system message the
  caller has not vouched for), a routine the model creates is saved as `"status": "pending"`
  with no next run, and the model cannot delete routines. Only the owner can then act, through
  `POST /api/jobs/{id}/confirm` (schedules it; an optional `{"tz": …}` body sets the zone it runs in) or `POST /api/jobs/{id}/discard` (deletes it);
  neither is a tool. `GET /api/jobs` lists `status` and `origin`. A caller whose system message
  holds only its own text sends `"context_clean": true` (chat completions) or
  `metadata.context_clean` (responses).
- Writes after outside content: once a web page, search result or passed-in content is in a
  chat's context, `write_file`, `edit_file`, `remember` and `run` are refused for the rest of that
  chat, and the reply tells the owner, who can ask again in a new chat. Reading the workspace or
  saved facts does not block writes, since only the owner or a clean turn put things there.
- `x-nexy-user`: the user a request acts for, set by the trusted caller. Sessions, memory, jobs and
  workspaces are kept per user.
- `/health` answers `{"ok":true}`; details need the key.

## Test

```sh
./tests/run.sh
```

## Licence

MIT
