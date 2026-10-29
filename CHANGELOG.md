# Changelog

All notable changes to lokode. The format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/).

## [Unreleased]

### Added

- Interface: render answers as markdown, with code coloured by language and numbered diffs.
- Interface: close a turn with a receipt that offers /undo and /diff.
- Interface: keep the plan, the changes, the jobs and the server errors in a rail.
- Interface: tell in the window title and by notification what the session does.
- Interface: say what the files named with @ will cost before the line is sent.
- Interface: draw command answers as pages, with tables, the map and numbered diffs.
- Interface: stop one call, the held model or the project survey from the rail.
- Interface: scroll and pick with the wheel and the click, and unfold a step by its badge.
- Interface: keep a delegated session's steps under its step, and open any on a page.
- Interface: ask before any action that loses something, and keep what was typed.
- Interface: run a command of your own beside the screen, several at once.
- Interface: follow the terminal's colours, tell Esc and Alt chords apart, show frames whole.
- Interface: show each step on one line with what it came to and how long it took.
- Interface: draw again only the entries that changed, so a long session stays fast.
- Interface: lay out turns, lists and the status line as cards and bands.
- Engine: read loken's timings, cache, drafts and energy, and the power drawn.
- Language server: find definitions, uses, implementations, callers and outlines.
- Checks: say whether each write was checked, and report only what it introduced.
- Language server: configure servers in [lsp] and start them with the session.
- Language server: show where each server stands in /lsp and the rail.
- Configuration: ask before a project's hooks, servers, allow rules or shell settings apply.
- Approvals: keep agreements across sessions and write without asking with --accept-edits.
- Shell: run commands in a sandbox by default, and let the model ask to leave it.
- Agents: hand work to named agents with their own tools, in the background in a worktree.
- Plans: put the finished plan to the user, and keep the task list when a session resumes.
- Commands: read commands from the user's configuration and from directories.
- Skills: offer skills by name and description, loaded with their files when the model asks.
- Plugins: install, update and list plugins from git or a marketplace, including Claude Code's.
- Hooks: run hooks at every edge of a session, and read their decision.
- MCP: reach servers over HTTP, sign in, restart them, and let them ask the model or the user.
- Sandbox: confine the commands on macOS with Seatbelt, as Landlock does on Linux.
- Editors: serve Zed, JetBrains, Neovim, Emacs and VS Code over ACP with lokode acp.

### Changed

- Backend: read the window from the model list, and drop the loken headers and the kv-format setting.

### Fixed

- Interface: stop redrawing while idle.
- Interface: keep the last lines of a call that prints fast.
- Language server: answer the server's own requests, count columns as it does, restart it once.
- Commands: keep a project's command from taking the name of a built-in.
- Approvals: tell the model when a rule refused a call, and match rules per command of a line.
- Headless: let --approve edits allow edit_lines.
- Interface: keep the directory of a command handed back to correct with x.
- Delegation: keep a delegated session to reading tools, and bound how many run at once.
- MCP: answer a server's requests, fail at once what waits on a stopped one, and say in /mcp which servers run.

### Removed

## [0.3.0] - 2026-10-24

### Changed

- Project: rename verve to lokode.

## [0.2.0] - 2026-10-18

### Added

- Commands: /model switches backend, /undo reverts, /mcp prompts, /digest forget confirms.
- Completion: arguments now complete, not just names.
- Session: /clear restarts in-session; /resume reopens an earlier one.
- Approval: refuse a call with a reason, or edit it: e sends, x edits the command.
- Approval: /approve lists standing agreements; /approve forget revokes them.
- Input: Alt-enter queues a message until the turn ends; status shows the count.
- Status: shows the last step's rate, and out-of-turn work with its runtime.
- Scroll: scrollbar tracks the pane; Home/End jump, search steps, Ctrl-Y copies.
- Mouse: Ctrl-T toggles app/terminal, freeing its native select and copy.
- Input: Esc interrupts a running turn; subprocesses are killed, not orphaned.
- Tools: native tool calling, JSON envelope fallback; --tool-protocol auto/native/envelope.
- Undo: /undo and /diff commit to a side git repo; undo moves history forward.
- MCP: servers become native tools; each call approved and schema-checked.
- CLI: -p runs headless; --approve sets consent, --output json, exit finished/failed/refused.
- Session: transcripts saved under XDG_DATA_HOME; --resume and --continue append.
- Logging: tracing events now go to a file, not the UI screen.
- Config: config.toml, .lokode.toml, LOKODE_* vars, flags in rising precedence; --show-config.
- Auth: LOKODE_API_KEY, api-key-env, or api-key file; --header name=value; warns on plain keys.
- Input: arrow history, wheel scroll, light markdown, colour on diffs.
- Config: --allow-host lets web_fetch reach a service on your own network.
- LSP: LOKODE_LSP_RUST and siblings point to a server off the path or misnamed.
- CI: formatting, clippy as error, tests, an MSRV build, and a dependency audit.
- Docs: SECURITY.md, CONTRIBUTING.md, and an operational README.

### Changed

- Backend: a Provider trait, OpenAI and Anthropic impls, auto-selected by URL.
- Reasoning: parsed four ways, shown apart from the answer, never sent back.
- Shell: bash writes confined via --confine-shell or [shell] confine-writes.
- Tools: grep no longer needs ripgrep; a glob walker does the same search.
- Tools: web_fetch strips script/style, decodes entities, breaks at tags.
- Limits: list_dir and glob now clamp output like every other tool.
- Tools: read-only calls run together; a mutating call bounds both sides.
- Compaction: trims well past the budget, so several steps fit per compaction.
- UI: redraws on events, not a timer waking twelve times a second.
- Tools: tool names are borrowed, not static str, so they can be set at runtime.
- Tests: 21 to 222; an LSP stub verifies re-send, the LSP tools uncovered.
- Tests: src/tools/tests.rs was the run_tests tool, now renamed run_tests.rs.
- Deps: dropped two unused, schemars and reqwest's blocking/multipart features.

### Fixed

- Backend: a long streamed answer is no longer cut at fifteen minutes.
- Prompt: the contract says paths are relative to the project root.
- Bash: a refused working directory says to leave cwd out to run at the root.
- Backend: guesses, reviews and the readiness probe went unanswered by reasoning models.
- UI: blanked 10-25s waiting on a cold model; now draws first, probes behind.
- Output: an answer showed twice when final text arrived on a later chunk.
- Commands: four matched the whole line, not the verb, so an argument got help.
- Input: the reason typed at a refusal lost spaces to a key binding.
- Scroll: a drag against an edge did not scroll; selection stayed in the window.
- Input: Escape cleared the input for good; the up arrow now restores it.
- Session: a resumed session counted restored messages but showed none.
- Terminal: a panic left it raw; a guard restores it before printing.
- Approval: Ctrl-C did nothing at the prompt; quit is now tested first.
- Approval: Enter both approved and sent, risking an unread command; now a key.
- Approval: an Always applied to the whole tool; now scoped to the prompt.
- Security: web_fetch resolves hosts, blocks private ranges, rechecks redirects.
- Limits: reads now check size first, stream bodies, refuse non-text.
- Writes: no more partial changes; atomic rename, all-or-nothing apply_patch.
- Patch: apply_patch resolved paths after approval; now before, as agreed.
- Files: none could be made in a missing directory; mkdir now runs when needed.
- Output: a token-limit cutoff read as bad JSON; now reported as truncation.
- Tokens: chars-per-token now tracks the server count; system message kept.
- Retry: server failures now retry with backoff; request errors do not.
- Streaming: a stalled stream held the turn for the full fifteen-minute timeout.
- LSP: never told of edits, so diagnostics went stale; stderr hid failures.
- Quit: quitting mid-write could truncate a file.
- Scroll: past the top blanked the pane; the wheel was captured but unread.
- Tools: results used ids matching no call, which strict servers reject.
- CLI: --help shows what lokode is instead of a note on its flags; --version answers.
- Security: rustls 0.23.46, past four advisories on its TLS client.

## [0.1.0] - 2026-08-09

### Added

- Agent: a terminal coding agent, any OpenAI-compatible backend, local default.
- Tools: read, search, edit, run, query a language server; each confined to a root.
- Approval: each change is a diff to accept or decline; a yes is remembered.
- Tools: three arg defences: server grammar, schema validation, repair; --no-grammar drops one.
- CLI: --max-steps bounds how far the loop runs before control returns.
