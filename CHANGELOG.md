# Changelog

All notable changes to verve. The format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/).

## [Unreleased]

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

### Added

- Input: Esc interrupts a running turn; subprocesses are killed, not orphaned.
- Tools: native tool calling, JSON envelope fallback; --tool-protocol auto/native/envelope.
- Undo: /undo and /diff commit to a side git repo; undo moves history forward.
- MCP: servers become native tools; each call approved and schema-checked.
- CLI: -p runs headless; --approve sets consent, --output json, exit finished/failed/refused.
- Session: transcripts saved under XDG_DATA_HOME; --resume and --continue append.
- Logging: tracing events now go to a file, not the UI screen.
- Config: config.toml, .verve.toml, VERVE_* vars, flags in rising precedence; --show-config.
- Auth: VERVE_API_KEY, api-key-env, or api-key file; --header name=value; warns on plain keys.
- Input: arrow history, wheel scroll, light markdown, colour on diffs.
- Config: --allow-host lets web_fetch reach a service on your own network.
- LSP: VERVE_LSP_RUST and siblings point to a server off the path or misnamed.
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

## [0.1.0] - 2026-08-09

### Added

- Agent: a terminal coding agent, any OpenAI-compatible backend, local default.
- Tools: read, search, edit, run, query a language server; each confined to a root.
- Approval: each change is a diff to accept or decline; a yes is remembered.
- Tools: three arg defences: server grammar, schema validation, repair; --no-grammar drops one.
- CLI: --max-steps bounds how far the loop runs before control returns.
