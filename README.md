# lokode

A coding agent that runs in your terminal, against a model on your own machine. It needs no
account, sends no request off the machine and asks you to adopt no editor.

> **EXPERIMENTAL.** This component is young and not production ready. Its interface, its
> configuration and its defaults change without notice. Point it at work you can afford to
> lose, read [SECURITY.md](SECURITY.md) before pointing it anywhere else, and keep the change
> history it writes.

```sh
lokode --model qwen3-coder:latest --root .
```

It speaks the OpenAI-compatible API, so it works against [loken](https://github.com/loken-ai/loken),
ollama, vLLM, llama.cpp or any other server of that API. The default backend is
`http://127.0.0.1:11435`. It also speaks the Anthropic Messages API.

## What it does

It reads, searches and edits files under one root, runs commands and asks a language server.
`--max-steps` caps the steps of one turn.

The sections of this page. Those marked local rely on the model running on your machine.

| | |
|---|---|
| [The screen](#the-screen) | the conversation, a rail with what runs, the plan, the changes, the window and the engine, the status line and the keys in force |
| [Using it](#using-it) | one line per step, completion on `/` and `@`, steps you point at, unfold and open |
| [What runs](#what-runs) | what is running, where it is, and stopping one thing while the rest goes on |
| [Approvals](#where-it-is-careful) | a change is a diff to accept or decline, asked in the conversation |
| [Plan and act](#planning-before-acting) | read without changing anything, then carry it out |
| [Undo](#taking-something-back) | a change history of its own, beside your git |
| [Sessions](#sessions) | every turn recorded and resumable |
| [Roles](#one-model-per-kind-of-work) *(local)* | a model per kind of work, a reviewer, and a stronger model when a change fails its check |
| [Checks](#changing-a-file) | the language server after every write, or the compiler where there is none |
| [Navigation](#asking-the-language-server) | definitions, uses, implementations, callers and outlines from the language server |
| [Background discovery](#working-it-out-while-you-are-not-waiting) *(local)* | the project worked out while nobody waits, kept between sessions |
| [Speculation](#using-it) *(local)* | the next read guessed while the model is still writing |
| [The engine](#what-the-engine-measures) *(local)* | what loken measured of each request, its energy and the power of the machine |
| [Delegation](#handing-off-a-search) | a search answered in a window of its own |
| [Plans](#work-of-several-steps) | work of several steps kept as a list |
| [Hooks](#hooks) | commands around a tool call and at the end of a turn |
| [MCP](#tools-from-elsewhere) | tools, documents and prompts from another server |
| [Headless](#without-the-interface) | one prompt, a JSON report, an exit code |

## What the engine measures

Against a loken server, lokode reads what the server measured of each request.

| Reading | Where |
|---|---|
| how long the request waited, read the prompt and wrote the answer | the rail |
| how far the server has read a long prompt | the status line, while it reads |
| how full the key-value cache is | the rail |
| the power the machine's accelerators draw, while a turn runs | the rail |
| how much of a speculative draft the server kept | the rail |
| the energy each request drew | the rail, the receipt, `/cost`, the session file and `-p --output json` |

The power comes from the device listing loken serves to its own interface. It is read every
two seconds while a turn runs, and shown as read. The energy comes from the server, request by
request.

Energy is counted by what a request was for: a step, a guessed read, a review, a repaired reply
or the startup probe. A server that measures nothing shows nothing, never a zero.

| Mark | Meaning |
|---|---|
| `≈` | other requests shared the server |
| `~` | part of the energy was estimated |

## Where it is careful

- **One root.** Every tool that takes a path is confined under `--root`, and a path that leaves
  it is refused. A file may be created in a directory that does not exist yet. The check
  applies to where the write lands.
- **A project is trusted first.** A project's `.lokode.toml` can run programs or loosen
  approvals. Its `[hooks]`, `[mcp]`, `[lsp]`, `[shell]`, `[backend]`, `[web]`, `[session]` and
  `[approve.allow]` apply once you trust them. The first session in the project shows them
  and asks. A change to them asks again.
- **Changes are approved.** A change arrives in the conversation as a diff to accept or
  decline. An agreement covers what you were shown: this command, writes under this
  directory, requests to this host. The prompt says what `a` covers before you press it.
- **A patch is all or nothing.** Every file of it is resolved and applied in memory before you
  are asked. A failure while writing puts back what was already written.
- **Tool arguments are checked.** They are validated against the tool's schema, repaired where
  the repair is unambiguous and refused otherwise. `--no-grammar` turns off the server-side
  constraint.
- **Egress goes to public addresses only.** `web_fetch` resolves the host, follows redirects
  itself and checks every hop. `--allow-host` names an exception on your own network.

Read [SECURITY.md](SECURITY.md) first. An approved `bash` command is not confined to the root
by default. On Linux, `--confine-shell` makes everything outside the project read-only for it.

## Install

```sh
cargo install --path .     # or cargo build --release, then target/release/lokode
```

It needs Rust 1.92 or later and a Unix-like system. The shell tool runs `/bin/sh`.

| Optional | What it adds |
|---|---|
| ripgrep (`rg`) | a faster `grep`. Without it the built-in walker runs the same search with the same ignore files |
| a language server | the checks after a write and the navigation tools, for the languages listed in [Asking the language server](#asking-the-language-server) |

## The screen

A session in the middle of a turn, at 120 columns:

<!-- screen:120x32 -->
```text
 lokode  /p/demo  main ●2                                                          qwen3-coder  127.0.0.1:11435  native
  the client awaits its reply without a bound
 ● read_file  src/mcp/client.rs                                  guessed  212 lines  PLAN                           1/3
 ▏     1 use std::time::Duration;                                                    ● read the MCP client
 ▏     2 use tokio::sync::oneshot;                                                   ◉ a timeout for every server
 ▏                                                                                   ○ test a silent server
 ● bash  cargo test mcp                                             14 passed  9.8s
 ▏   test mcp::client::a_silent_server_times_out ... ok                              CHANGES                      +5 -1
                                                                                     src/mcp/client.rs            +3 -1
The client waits on rx.await forever. I add timeout-secs:                            src/config.rs                +2 -0

• one setting per server, thirty seconds unless it says otherwise                    CONTEXT                        23%
• a test against a server that never answers                                         ████████··························
                                                                                     █ instructions 10%  █ you 3%
  rust                                                                               █ model 4%  █ tools 6%
  let reply = timeout(self.timeout, rx).await??;
                                                                                     ENGINE
 ◉ edit src/mcp/client.rs  waiting for you                                           qwen3-coder
    src/mcp/client.rs                                                                ▄▆████ 93 tok/s
    ⋯ line 212                                                                       power   ▄▇██ 312 W  87% busy
    212 -     let reply = rx.await?;                                                 time    read 0.7s · write 3.1s
    212 +     let reply = timeout(self.timeout, rx).await??;                         prompt  6.1k, 4.2k held
                                                                                     cache   ▰▰▱▱▱▱▱▱▱▱ 15%
    y  once   a  always in src/mcp/   n  no   e  no, and say why                     drafts  82% kept
                                                                                     energy  ≈0.31 Wh  session ≈0.42 Wh
 ✓ 38.0s · 7 step(s) · 8.4k read, 1.1k written · changed 2 files · 0.42 Wh
                                                                                     TURNS TO PUT BACK
! backend error: gave up after 3 attempts                                              1  src/config.rs

❯
    ACT    ctx 23%
  shift-tab write  enter send  alt-↑ steps  ctrl-f find  /help more  ctrl-c leave
```
<!-- /screen -->

| Part | What it holds |
|---|---|
| the band | the project, the branch and its changes, the model, the backend and the tool protocol |
| the conversation | your questions, the answers, one row per step with what it came to, and a receipt per turn |
| the rail | what runs, the plan, the files changed, what fills the window, the engine and the turns to put back. It shows from 120 columns, or with `Ctrl-B` |
| the input | what you type, with completion on `/` and `@` |
| the status line | the mode, what the turn is doing and for how long, and how full the window is |
| the line of keys | the keys of what is in front: the input, a list, a page or the step pointed at |

## Using it

A `/` at the start of a line opens the commands, and an `@` opens the files under the root.
Both lists narrow as you type.

| Key | In a completion list |
|---|---|
| `Tab` | take the item. A command that takes an argument gets a space after it |
| `Enter` | take a command and send it, or take a path |

A command's answer longer than a sentence opens on a page over the conversation. `Esc`,
`Enter` or `q` closes it.

`Alt-Up` and `Alt-Down` move between the steps of the session. What the step pointed at
offers:

| Pointed at | Keys |
|---|---|
| a step | `Space` unfolds it, `Enter` opens it on a page, `Ctrl-C` stops it while it runs |
| a receipt | `u` puts the turn back, `d` shows what it changed |
| a question | `Enter` brings it back to the input |

| Key | |
|---|---|
| `Shift-Tab` | act, write without asking, or plan, in turn |
| `Enter` | send, take what the list offers, or bring back the question you are pointing at |
| `Tab` | take what the list offers |
| `Shift-Enter` | newline |
| `Esc` | interrupt the turn, stop a command of yours, close a list, drop a selection, clear the line |
| `Esc` twice | bring back the last question |
| `Alt-Up` / `Alt-Down` | move between the steps of the session |
| `Space` | open a block that was counted, while pointing at it |
| `Enter` | open the step you are pointing at on a page, with every step of a session it ran |
| `u` / `d` | put back the turn whose receipt you are pointing at, or see what it changed |
| `Ctrl-F` | find something said earlier, then walk the matches with enter and up |
| `Ctrl-B` | show or hide the rail: the plan, what changed, the window, the speed, the turns to put back |
| `Ctrl-G` | write the prompt in `$EDITOR` and bring it back |
| `Ctrl-Y` | put the selection, or the block pointed at, on the clipboard |
| `y` / `a` / `n` | answer a change: once, always in the stated scope, no |
| `e` / `x` | refuse and say what to do instead, or refuse and correct the command yourself |
| `Enter` while it works | hand the line to the turn in flight |
| `Alt-Enter` while it works | ask it once the turn is over |
| `Up` / `Down` | walk what you cleared, then what you have already sent |
| `PageUp` / `PageDown`, `Home` / `End`, wheel | scroll |
| click | unfold a step, answer a question by its badge, take an item of a list |
| drag with the mouse | select here, and scroll when dragging past an edge |
| `Shift` + drag | select in the terminal itself, for its own copy key |
| `Ctrl-T` | hand the mouse to the terminal, or take it back |
| `Ctrl-C` | stop the turn or a command of yours, clear the line, or leave when pressed twice on an empty line |
| `Ctrl-D`, `Ctrl-Q` | leave, and ask first when a turn, an answer or a typed line would be lost |

It asks before something is lost. Leaving asks for the key a second time. The others open a
list with `no` selected.

| Action | Asks when |
|---|---|
| `Ctrl-D`, `Ctrl-Q`, `Ctrl-C` on an empty line | a turn runs, a question waits or a line is typed |
| `/clear` | always |
| `u` on a receipt | always |
| `Enter` on a question | turns came after it |

| On the line | |
|---|---|
| `!command` | run the command here, beside the screen, under a step of its own. Its output joins the conversation |
| `#rule` | append a rule to `.lokode.md` and tell the model at once |
| `@path` | send that file, or that directory's listing, with the question |

| Command | |
|---|---|
| `/plan`, `/act` | plan without changing anything, then carry it out |
| `/model` | choose from what the backend serves, with the one answering marked |
| `/clear`, `/resume` | start again, or come back to a session recorded earlier |
| `/retry`, `/retry <model>` | ask the last question again, of the same model or another |
| `/copy`, `/copy last` | the conversation, or the last answer, to the clipboard and to a file |
| `/undo`, `/diff` | put the project back to before a turn, or see everything changed |
| `/context`, `/compact`, `/cost` | where the window stands, elide now, what it spent |
| `/jobs` | what is running now, to point at its step or stop work done outside a turn |
| `/lsp` | the language servers: where each stands, the files it reports, why one is down |
| `/approve`, `/approve forget` | what is in force here and in the configuration, or take it back |
| `/why`, `/digest`, `/tools`, `/mcp` | what is in force, what is known, what can be called |

A command that takes a name offers the names. `/model`, `/undo`, `/resume` and `/mcp` open a
list that narrows as you type. `/help` lists every command, with those the project ships.

The window title says whether the session works or waits for you. Ghostty, WezTerm, Windows
Terminal and iTerm2 from 3.6 also show it as progress in the tab. While the terminal is not in
front, a waiting question and the end of a turn arrive as a desktop notification, or as a bell
where the terminal has none.

lokode takes the mouse for the wheel, clicks and a selection that scrolls past the edge.
`Ctrl-Y` puts a selection on the terminal's clipboard, through ssh and tmux. Hold `Shift`
while dragging for the terminal's own selection, or press `Ctrl-T` to hand it the mouse.

Answers are rendered as markdown: fenced code coloured by language, headings, lists, quotes,
tables, links and emphasis. A diff is numbered, with the changed words marked. There are no
borders.

A line sent while the agent works joins the turn at the next step, and the screen says when.
A slash command typed then runs at once.

A file named with `@` goes with the question. A directory sends its listing. The status line
says what the files will cost before the line is sent. What is attached is bounded like any
other content. A mention that is not a path here is left alone, and nothing outside the
project can be named.

A shell command and a test suite show the end of their output under their step while they
run. The tool's result replaces it when they finish. The model reads the whole output.

The status line says what the turn is doing and for how long: fitting the history, waiting for
the model, reading the prompt, thinking, answering, repairing a reply or running a tool. While loken reads a long prompt, it
shows how far it has got.

While the model writes its answer, a small model guesses the next read and the read is made
ahead. The step that asks for it is answered without a round trip, and its row says
`guessed`. A guess never delays the turn, and a guess that would change something is dropped.
The guessing model is the project's search model, or the one answering. It runs against a
backend on this machine or this network, never a hosted one. `speculate = false` turns it off.

A turn ends on a receipt: its time, its steps, what it read and wrote, the files it changed
and its energy. A quick turn that changed nothing gets none.

The loop declines three things:

- a read already made this turn, with nothing written since, is answered with its result, and
  the model is told it repeats itself
- a whole-file write over a file changed on disk since the model read it is refused
- a path that does not exist is answered with where the file is

`/retry` asks the last question again. What the agent did after it leaves the conversation.
`/retry <model>` asks another model. The transcript keeps both attempts, and `/undo` puts back
the files of the first.

A turn that gets nowhere is handed back early. Three steps in a row where every call failed or
repeated itself end it, and so do six failed calls in a turn that changed nothing. The screen
names the tool that kept failing.

At an approval, a click on a badge answers like its key. `Enter` does nothing there.

## What runs

The rail opens with what is running: each step that has not ended with how long it has run,
the call a delegated session is on, the work done outside a turn, and a language server
reading or building the project.

| To | Do |
|---|---|
| go to a step that runs | choose it in `/jobs`, or click it in the rail |
| stop one call while the turn goes on | `Ctrl-C` on its step |
| stop the whole turn | `Esc` |
| stop a command of yours | `Ctrl-C` on its step, or `Ctrl-C` or `Esc` on an empty line for the latest |
| stop work done outside a turn | choose it in `/jobs` |
| see where the language servers stand | `/lsp`, or choose a server in `/jobs` |

A stopped call ends in an error that tells the model the person stopped it, and the turn goes
on. Work outside a turn is the model held ready on the backend and the project being looked
into. Looking into the project, once stopped, stays stopped for the session.

Commands of your own run side by side, each under its step, with their output as it comes.

## Planning before acting

`/plan` refuses every tool that writes, runs a command or reaches the network, before it runs,
and tells the model so. The model reads, searches and ends with numbered steps. `/act` leaves
the mode. `--plan` starts in it, and `mode` in the configuration makes it a project's default.

Nothing has to be declined while planning. A project can plan on its strongest model and act
on a smaller one, with [roles](#one-model-per-kind-of-work).

## Changing a file

| Tool | |
|---|---|
| `write_file` | replace a file |
| `edit_file` | replace one unique substring |
| `edit_lines` | apply several such edits to one file in order, all or none |
| `apply_patch` | apply a unified diff across several files |

After a write, the language servers are told the files changed and were saved. Once they have
read the new text and have no work left in hand, what they report is compared with what they
reported before. The tool result lists the errors the write introduced, in any file, with
their code and their source line. Without a language server for the file, the compiler is
asked to compile, once per step and with a bounded wait.

| A write's step says | When |
|---|---|
| `checked` | the servers or the compiler report no new error |
| `check failed` | they report errors the write introduced |
| `not checked` | none could tell, and the result says why |

The rail's CHANGES marks a file the servers report errors in with `✕` and their count. Only
a write that was checked counts when the session decides to escalate.

When a change fails its check and a later change clears the same complaint, the fix is filed
against the complaint. The next occurrence arrives with the note. Line and column numbers and
paths are left out of the comparison. `/why` lists what has been learned.

## Asking the language server

| Tool | What it answers |
|---|---|
| `lsp_definition` | where a symbol is defined |
| `lsp_references` | every place a symbol is used |
| `lsp_implementations` | the implementations of a trait, an interface or an abstract method |
| `lsp_calls` | the functions that call a function, or that it calls |
| `lsp_hover` | the type and the documentation of a symbol |
| `lsp_symbols` | the outline of a file, or the symbols matching a name across the project |
| `lsp_diagnostics` | what the server reports about a file |

| Language | Server | Extensions |
|---|---|---|
| rust | `rust-analyzer` | rs |
| python | `pyright-langserver --stdio` | py, pyi |
| typescript | `typescript-language-server --stdio` | ts, tsx, mts, cts, js, jsx, mjs, cjs |
| go | `gopls` | go |
| c | `clangd` | c, h, cpp, cc, cxx, hpp, hh, hxx |
| zig | `zls` | zig |
| bash | `bash-language-server start` | sh, bash |
| lua | `lua-language-server` | lua |
| ruby | `ruby-lsp` | rb |

At startup the session reads which of these languages the project's files are written in,
and starts their installed servers in the background. The tools are offered only when one of
them can answer for the project. A server that stops is started again once. A second stop, or
a server that cannot start, leaves it down for the session with what it said.

The `[lsp]` section changes a built-in server, turns one off or adds one:

```toml
[lsp]
enabled = true                       # false turns every language server off

[lsp.servers.rust]
settings = { rust-analyzer = { check = { command = "clippy" } } }

[lsp.servers.ruby]
enabled = false

[lsp.servers.elixir]                 # a new language needs a command and its extensions
command = "elixir-ls"
extensions = ["ex", "exs"]
```

A server also takes `args`, `init` for its initialization options and `env`. `settings` is
what the server is answered when it asks for its configuration. `LOKODE_LSP_<LANGUAGE>`, as
in `LOKODE_LSP_RUST`, replaces a server's command and arguments, which helps when the program
on your `PATH` is a `rustup` shim for a component that is not installed.

A symbol is named by its file, its line and its name as written there. The column is
optional. A name not found on the line is looked for on the lines around it, and the answer
says where it was found. Each place found comes back as `path:line:column` with its source
line.

## Taking something back

Every change the agent makes is committed to a git repository kept beside the session, with
the project as its work tree. Your own repository is not touched.

```
/diff    everything the agent has changed this session
/undo    put back the most recent turn that changed something
/help
```

`/undo` again goes one turn further back. An undo is itself a commit carrying the older state,
so an edit you made by hand in between stays in the history. `u` on a receipt puts that turn
back, after asking. See [SECURITY.md](SECURITY.md): the history holds copies of your files and
outlives the session.

## Sessions

Each session is written to `$XDG_DATA_HOME/lokode/sessions/<id>.jsonl` as it runs.

```sh
lokode --continue              # the most recent session for this root
lokode --resume <id>           # a particular one
lokode --no-transcript         # record nothing
```

A resumed session keeps its id, appends to its file and remembers its model, so `--model`
becomes optional.

Logs go to `$XDG_STATE_HOME/lokode/lokode.log`, never to the terminal. `RUST_LOG=lokode=debug`
raises the level, and `--log-file` moves the file.

## Settling approvals in advance

An `[approve]` section states agreements once, in a file that can be reviewed:

```toml
[approve]
max-auto = 50                      # per turn, then it asks again

[approve.allow]
bash = ["cargo test*", "git status"]
write = ["src/*"]
egress = ["docs.rs"]

[approve.deny]
bash = ["git push*", "*rm -rf*"]
```

- A refusal always wins over an agreement.
- A pattern takes one wildcard, `*`, for any run of characters.
- Write rules are relative to the project.
- `max-auto` caps the agreements used in one turn.
- `/approve` says what is in force.

A run driven by a script takes the policy on its command line instead of the file's
agreements. The file's refusals still apply.

## One model per kind of work

```toml
model = "qwen3:8b"              # everything not named below

[roles]
plan = "qwen3.5:35b"
edit = "qwen3-coder:30b"
search = "qwen3:1.7b"
repair = "qwen3:0.6b"
review = "qwen3:4b"
escalate = "qwen3.5:35b"
```

| Role | When it answers |
|---|---|
| `plan` | from `/plan` to `/act`, said on the screen both ways |
| `edit` | loaded in the background while a plan is written, ready for acting |
| `search` | a delegated question |
| `repair` | a malformed reply to reshape |
| `review` | before anything changes, to say whether it does what was asked |
| `escalate` | after a change fails its check |

The band shows the model answering now.

An objection from `review` stops the call and goes back to the model that proposed it. A
reviewer that cannot be reached never blocks anything.

A step whose change fails its check raises the session to `escalate` for the steps that follow,
and says so. Two writes that pass bring it back down. A step that only reads moves nothing.

What each model managed is remembered per project, outside the repository: how many of its
last fifty writes passed their check. When the cheap model fails most of its writes in a
project, the next turn starts on the stronger one and says so. It decides only after four
writes. `/why` says what is in force and on what evidence.

While a plan is written, the `edit` model, or else the default one, is asked for in the
background, so the server loads it before `/act`. It never delays or fails a turn. It runs against a backend on
this machine or this network only. `preload = false` turns it off.

## Choosing a model

The model comes from, in order: the flag, the model a resumed session last used, the one the
configuration remembers, and a backend that serves a single model. With several and none
remembered, the session opens without one and says so. `/model` lists what the backend serves,
and `/model <name>` switches and remembers the name in your configuration.

A switch keeps the conversation. The new model's window comes from its own table in the
configuration when there is one. What the previous model taught about its tokenizer is
measured again.

## Working it out while you are not waiting

`--discover`, or `discover = true` in the configuration, adds work done outside turns. While
nothing else runs, a read-only session answers one question about the project from a queue and
writes down what it found. The queue and the findings outlive the session.

- It runs only while nothing else does, and stops when you send a line.
- It asks the server at a priority that yields.
- Every tool that writes, runs a command or reaches the network refuses.
- It shows in the rail while it works, and `/jobs` stops it for the session.

What it learned goes in front of the model next session, as a short paragraph. `/digest` shows
it with its sources, and `/digest forget` clears it. What is worth keeping belongs in
`.lokode.md`.

## What the project tells the agent

`.lokode.md` at the root of the project and `~/.config/lokode/lokode.md` for yourself are
appended to the system message of every session. Yours comes first, and each is named.

`/init` writes a first draft from the repository: the language, the test command, the layout.
Keep it short, since it holds the window for the whole session. What does not fit is cut at a
line boundary, and the session says so.

## Where the window stands

The window is read from the backend's model list (`max_model_len`), and `--context-tokens`
caps it. The status line shows how full it is: amber past three quarters, red once the oldest
steps start being elided. `/context` says what holds it, largest first: the instructions and
the tools, what you said, what the model said, the tool results. `/compact` elides now.

## What it cost

`/cost` says what the session spent: steps, tokens in and out, the rate, the time in the model
and in tools, the energy by purpose, then the last steps one by one.

Every step is written to the transcript with what was known before it and what it cost.

## Handing off a search

`delegate` gives one self-contained question a session of its own, with its own window and the
same tools, and returns only its answer. The search does not fill the conversation's window.

Its steps stay under its own step, folded into a count once it ends. `Space` unfolds them, and
`Enter` opens a page with each step, its evidence and the answer. `Ctrl-C` on its step stops it
alone.

The sub-session cannot delegate again, and its step limit is lower than a turn's. It runs under
the caller's approvals.

## Work of several steps

`update_plan` is a tool the model calls to keep a task list: done, in progress, left. The rail
shows it under PLAN with how far it has got. It writes nothing and needs no approval.

## Hooks

```toml
[hooks]
pre-tool = "./scripts/guard.sh"
post-tool = "cargo fmt"
turn-end = "cargo test --quiet"
```

| Hook | |
|---|---|
| `pre-tool` | runs before a tool that changes something. A non-zero exit refuses the call, and its output goes to the model |
| `post-tool` | runs after such a tool. Its output goes to the model |
| `turn-end` | runs when a turn ends, abandoned or not. Its output goes to you |

The tool hooks run only around tools that change something. A hook gets the tool's name, the
project root and its edge in the environment. Its time and its output are bounded.

## Commands a project ships

A Markdown file under `.lokode/commands` is a command: `review.md` makes `/review`, and its
text becomes the prompt. What is typed after the name replaces `$ARGS`, or is appended when the
file has none. `/help` lists them with the built-ins.

## Tools from elsewhere

A Model Context Protocol server you configure adds its tools, named `<server>__<tool>`.

```toml
[mcp.servers.github]
command = "mcp-server-github"
args = []
env = { GITHUB_TOKEN = "..." }     # prefer naming a variable your shell already has
```

- Every call is approved, and treated as able to change the project.
- The schema a server advertises is checked before it shapes the grammar. A tool that fails the
  check is refused by name.
- A server that does not start takes only itself down.
- Its resources become one tool, `<server>__read_resource`, whose description lists them.

`/mcp` lists the running servers and their prompts. `/mcp <server> <prompt>` makes one the
turn's prompt.

## Without the interface

`-p` runs one turn and exits.

```sh
lokode -p "fix the clippy warnings" --approve edits --output json
```

| Policy | |
|---|---|
| `--approve none` | the default. Anything that would change something is declined, and reading and searching still work |
| `--approve edits` | writes, edits and patches are allowed. The shell, the test suite and the network stay refused |
| `--approve all` | everything is allowed |

Each call is judged on its own. A policy never grants a standing agreement.

`--output text` streams the answer to stdout and the progress to stderr. `--output json` writes
one object at the end: the result, every tool call with its output, what was refused and the
energy. The call of a write carries `check`: `passed`, `failed` or `not-checked`.

| Exit code | |
|---|---|
| `0` | finished |
| `1` | something failed: a backend error, or the step limit |
| `2` | a tool was refused by the policy |

## Which API

`kind = "auto"` reads it from the URL: `api.anthropic.com` is the Messages API, and anything
else is OpenAI-compatible. Name a kind to reach a proxy of Anthropic's format on your own host.

The credential is found the same way for both: the environment, then a named variable, then a
file only you can read. It goes in `Authorization: Bearer` for the OpenAI family and in
`x-api-key` for Anthropic.

The differences stop at the boundary: the system prompt, the tool results and the stream.
Reasoning arrives as `<think>` tags, a `reasoning` field, a `reasoning_content` field or
Anthropic's thinking blocks. All four are shown the same way, set apart, and never sent back to
the model.

## How tools are called

| `--tool-protocol` | |
|---|---|
| `native` | the server's `tools` field and the `tool_calls` it returns |
| `envelope` | one JSON object per turn, held to a grammar built from the tool schemas. It needs only a completion |
| `auto` | the default. The backend is asked once at startup for a call, and a server that answers with prose or refuses the field gets the envelope |

The band says which protocol is in force.

## Configuring it

Settings come from five places, and the narrower wins: a flag, the environment,
`<root>/.lokode.toml`, `~/.config/lokode/config.toml`, the built-in default.

```toml
# ~/.config/lokode/config.toml, or .lokode.toml in a project
model = "qwen3-coder:latest"
tool-protocol = "auto"          # auto | native | envelope

[backend]
url = "http://127.0.0.1:11435"
kind = "auto"                   # auto | openai | anthropic
api-key-env = "MY_BACKEND_KEY"  # name a variable; do not write the key here
headers = { "X-Team" = "lokode" }

[limits]
max-steps = 16
context-tokens = 8192
max-answer-tokens = 4096

[web]
allow-hosts = ["docs.example.com"]

[session]
transcripts = true
```

**A project's file.** Its settings that run programs or loosen approvals wait for trust.

| | |
|---|---|
| Waits for trust | `[hooks]`, `[mcp]`, `[lsp]`, `[shell]`, `[backend]`, `[web]`, `[session]`, `[approve.allow]` |
| Applies at once | everything else, `[approve.deny]` included |
| Asked | at the first interactive session, and again after a change to them |
| Without a terminal | left out, with a warning. `--trust-project` takes them for one run |

The trusted settings are kept in `$XDG_DATA_HOME/lokode/trusted.json`.

`lokode --show-config` prints the files read and what everything resolved to. It names the
headers without their values.

**Credentials.** `LOKODE_API_KEY` comes first, then the variable named by `api-key-env`, then
an `api-key` in the file. The file is accepted only if nobody else can read it. A key about to
cross a network unencrypted is warned about.

Environment variables: `LOKODE_MODEL`, `LOKODE_BACKEND_URL`, `LOKODE_LOG_FILE`,
`LOKODE_API_KEY`.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT OR Apache-2.0, at your option.
