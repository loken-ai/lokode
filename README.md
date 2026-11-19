<!-- site: page slides sound=soft header=lokode class="brand brand-lokode" footer="{number} / {total}" reveal -->

<!-- site: say narration=/lokode/narration/overview-1-1.mp3 caption="lokode is a coding agent that lives in your terminal, and talks to a model on your own machine." mark="A coding agent that runs in your terminal, against a model on your own machine" -->
<!-- site: say narration=/lokode/narration/overview-1-2.mp3 caption="No account. Nothing leaves the machine. And no editor to adopt." mark="It needs no account, sends no request off the machine and asks you to adopt no editor" -->
<!-- site: say narration=/lokode/narration/overview-1-3.mp3 caption="It is experimental. Point it at work you can afford to lose." focus="EXPERIMENTAL" -->

# lokode

A coding agent that runs in your terminal, against a model on your own machine.
It needs no account, sends no request off the machine and asks you to adopt no editor.

<!-- site: page title="The lokode tour" -->

> **EXPERIMENTAL.** This component is young and not production ready. Its interface, its
> configuration and its defaults change without notice. Point it at work you can afford to
> lose, read [SECURITY.md](SECURITY.md) before pointing it anywhere else, and keep the change
> history it writes.

```sh
lokode --model qwen3-coder:latest --root .
```

lokode speaks the OpenAI-compatible API and works against [loken](https://github.com/loken-ai/loken), ollama, vLLM, llama.cpp or any other server of that API.
The default backend is `http://127.0.0.1:11435`.  
lokode also speaks the Anthropic Messages API and knows [17 providers](https://loken-ai.github.io/lokode/configuration/#which-provider) by name.

Beside its own screen, it serves editors over the Agent Client Protocol (`lokode acp`) and a browser page (`lokode serve`), and a terminal can attach to a session of `lokode serve` (`lokode attach`).

Plugins add commands, agents, skills, hooks, tool servers and language servers, including the plugins written for other coding agents.

<!-- site: slide narration=/lokode/narration/overview-2-0.mp3 caption="What does it do? It reads, searches and edits your files, runs commands in a sandbox, and asks a language server." -->
<!-- site: say narration=/lokode/narration/overview-2-1.mp3 caption="Every change is a diff. You accept it, or you decline it." focus="a change is a diff to accept or decline" -->
<!-- site: say narration=/lokode/narration/overview-2-2.mp3 caption="And you can take it back, from a change history of its own, beside your git." focus="a change history of its own" -->
<!-- site: say narration=/lokode/narration/overview-2-3.mp3 caption="Commands run in a sandbox. They write only in the project, and they cannot read your credentials." focus="cannot read your credentials" -->
<!-- site: say narration=/lokode/narration/overview-2-4.mp3 caption="17 providers by name, or any server that speaks the OpenAI API." focus="17 providers by name" -->

## What it does

lokode reads, searches and edits files under one root, runs commands in a sandbox and asks a language server.  
Without `--max-steps`, a turn goes on while it changes files.  
It stops after as many steps without a change as its first change took, or when its own steps no longer fit the window.  
`--max-steps` stops each turn at that many steps.

Each row opens its guide at <https://loken-ai.github.io/lokode/>.
Rows marked local rely on the model running on your machine.

| | |
|---|---|
| [The screen](#the-screen) | the conversation, a rail with what runs, the plan, the changes, the window and the engine, the status line and the keys in force |
| [Using it](https://loken-ai.github.io/lokode/using/#using-it) | one line per step, completion on `/` and `@`, steps you point at, unfold and open |
| [What runs](https://loken-ai.github.io/lokode/using/#what-runs) | what is running, where it is, and stopping one thing while the rest goes on |
| [Approvals](https://loken-ai.github.io/lokode/safety/#where-it-is-careful) | a change is a diff to accept or decline, asked in the conversation |
| [Plan and act](https://loken-ai.github.io/lokode/agents/#planning-before-acting) | read without changing anything, then carry it out |
| [Undo](https://loken-ai.github.io/lokode/editing/#taking-something-back) | a change history of its own, beside your git |
| [Sessions](https://loken-ai.github.io/lokode/editing/#sessions) | every turn recorded and resumable |
| [Roles](https://loken-ai.github.io/lokode/models/#one-model-per-kind-of-work) *(local)* | a model per kind of work, a reviewer, and a stronger model when a change fails its check |
| [Checks](https://loken-ai.github.io/lokode/editing/#changing-a-file) | the language server after every write, or the compiler where there is none |
| [Navigation](https://loken-ai.github.io/lokode/editing/#asking-the-language-server) | definitions, uses, implementations, callers and outlines from the language server |
| [Background discovery](https://loken-ai.github.io/lokode/models/#working-it-out-while-you-are-not-waiting) *(local)* | the project worked out while nobody waits, kept between sessions |
| [Speculation](https://loken-ai.github.io/lokode/using/#using-it) *(local)* | the next read guessed while the model is still writing |
| [The engine](https://loken-ai.github.io/lokode/models/#what-the-engine-measures) *(local)* | what loken measured of each request, its energy and the power of the machine |
| [Sandbox](https://loken-ai.github.io/lokode/safety/#where-it-is-careful) | the commands it runs write only in the project, cannot read your credentials, and reach the network through a proxy that asks about each host |
| [Agents](https://loken-ai.github.io/lokode/agents/#handing-work-to-an-agent) | a search or a task handed to a named agent, in a window of its own, in the background and in a copy of the project |
| [Plans](https://loken-ai.github.io/lokode/agents/#work-of-several-steps) | work of several steps kept as a list |
| [Hooks](https://loken-ai.github.io/lokode/extending/#hooks) | commands around a tool call and at the end of a turn |
| [Sounds](https://loken-ai.github.io/lokode/extending/#sounds) | a sound when a turn, an agent or a question ends, for a success or an error |
| [Commands, skills and plugins](https://loken-ai.github.io/lokode/extending/#commands-written-as-files) | prompts, know-how and extensions kept as files, installed from git or a marketplace |
| [MCP](https://loken-ai.github.io/lokode/extending/#tools-from-elsewhere) | tools, documents and prompts from another server, signed in to when it asks |
| [Providers](https://loken-ai.github.io/lokode/configuration/#which-provider) | 17 providers by name, or any server of the OpenAI API |
| [Editors and browser](https://loken-ai.github.io/lokode/interfaces/#in-an-editor) | `lokode acp` for Zed, JetBrains, Neovim, Emacs and VS Code, `lokode serve` for a page, `lokode attach` for a terminal |
| [Headless](https://loken-ai.github.io/lokode/interfaces/#without-the-interface) | one prompt, a JSON report, an exit code |

<!-- site: say narration=/lokode/narration/overview-3-1.mp3 caption="Installing takes one command. Cargo builds it. You need Rust, and a Unix like system." focus="cargo install" mark="cargo install --path ." -->

## Install

```sh
cargo install --path .     # or cargo build --release, then target/release/lokode
```

lokode needs Rust 1.92 or later.
The shell tool runs `/bin/sh` on Linux and macOS, and Git Bash or PowerShell on Windows.

| Optional | What it adds |
|---|---|
| ripgrep (`rg`) | a faster `grep`. Without it the built-in walker runs the same search with the same ignore files |
| a language server | the checks after a write and the navigation tools, for the languages listed in [Asking the language server](https://loken-ai.github.io/lokode/editing/#asking-the-language-server) |

<!-- site: say narration=/lokode/narration/overview-4-1.mp3 caption="Here is a session, in the middle of a turn. The conversation, what runs, the plan, the changes, and how much context is used." focus="A session in the middle of a turn" -->

## The screen

A session in the middle of a turn, at 120 columns:

<!-- site: replace src=/lokode/img/session-120-light.png dark=/lokode/img/session-120-dark.png alt="A lokode session in the middle of a turn" caption="A lokode session in the middle of a turn" -->
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

• one setting per server, 30 seconds unless it says otherwise                        CONTEXT                        23%
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
<!-- site: end -->

| Part | What it holds |
|---|---|
| the band | the project, the branch and its changes, the model, the backend and the tool protocol |
| the conversation | your questions, the answers, one row per step with what it came to, and a receipt per turn |
| the rail | what runs, the plan, the files changed, what fills the window, the engine and the turns to put back. It shows from 120 columns, or with `Ctrl-B` |
| the input | what you type, with completion on `/` and `@` |
| the status line | the mode, what the turn is doing and for how long, and how full the window is |
| the line of keys | the keys of what is in front: the input, a list, a page or the step pointed at |

<!-- site: say narration=/lokode/narration/overview-5-1.mp3 caption="To work on lokode, run the tests, clippy, and the format check." focus="cargo test" -->
<!-- site: say narration=/lokode/narration/overview-5-2.mp3 caption="The rest is in the contributing guide." focus="CONTRIBUTING.md" -->

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

<!-- site: say narration=/lokode/narration/overview-6-1.mp3 caption="MIT or Apache 2. Your choice." focus="MIT OR Apache-2.0" -->

## Licence

MIT OR Apache-2.0, at your option.
