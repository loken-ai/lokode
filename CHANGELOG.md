# Changelog

All notable changes to `verve`. The format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/).

## [Unreleased]

### Fixed

- A panic left the terminal in raw mode with no cursor, printing the message you needed to
  read in a mode where it was unreadable. Raw mode and the alternate screen now belong to a
  guard, and a panic hook restores before the default hook prints.
- `Ctrl-C` did nothing at the approval prompt - the one screen it was most wanted on - because
  the branch handling that screen answered the keystroke first. Quit is now tested before
  anything else can swallow a key.
- `Enter` approved. It is also the send key, so a prompt appearing mid-sentence could
  authorise a command that had not been read. Accepting now takes a letter.
- An `[A]lways` answer was recorded against the tool that asked, so agreeing to one `ls`
  handed over the shell for the session and agreeing to one file being written handed over
  every file. Agreements are now scoped to what the prompt showed, and the prompt says which.
- `web_fetch` checked only the URL scheme, and the HTTP client followed up to ten redirects on
  its own - so an approved public URL could answer 302 towards loopback or the cloud metadata
  address. Hosts are resolved rather than trusted, every non-public range is refused, and
  redirects are followed here so the check runs again on each hop.
- Reads had no ceiling: whole files and whole response bodies were loaded before the clamp
  that was meant to bound them. Size is now checked first, bodies stream against a limit, and
  a file that is not text is refused rather than delivered as replacement characters.
- Writes were not atomic despite a comment saying they were, and `apply_patch` applied in a
  loop - so a failure on the third file left the first two changed. Files are replaced by
  rename, and a patch is staged wholly before any of it is committed, with a rollback if the
  commit fails part-way.
- `apply_patch` resolved paths *after* the user approved the diff, so what was agreed to and
  what happened could differ.
- A file could not be created in a directory that did not exist yet: resolution required the
  immediate parent, so the code that creates directories was only reachable when it had
  nothing to do.
- A response cut off by the token limit was indistinguishable from a model that cannot write
  JSON. `finish_reason` is read, and a truncated answer says so.
- The chars-per-token estimate had no feedback loop, and under-counting is the direction that
  loses the system message. It is now measured against the server's own count.
- Nothing was ever retried, and transport failures - a backend still starting up, the most
  retryable thing there is - were treated as final. Failures describing the server's condition
  are retried with a widening gap; failures describing the request are not.
- A stalled stream held the turn for the client's full fifteen-minute timeout.
- The language server was opened once per file and never told about changes, so from the
  agent's first edit every diagnostic described code that no longer existed. Its stderr went
  to `/dev/null`, so a server that refused to start reported nothing and looked like a file
  with nothing wrong with it.
- Quitting aborted the agent mid-write, which could truncate a file.
- Scrolling past the top blanked the chat pane: the window slid off the end of the content
  instead of stopping at its oldest line. Mouse capture had been on from the beginning with
  nothing reading an event, so the wheel did nothing.
- Results were returned against invented ids matching no call the model had made, which a
  strict OpenAI-compatible server rejects.

### Added

- `Esc` interrupts a running turn. Subprocesses are killed rather than orphaned.
- Native tool calling, with the JSON envelope as the fallback. `--tool-protocol auto` starts
  native and drops to the envelope the first time a backend refuses the field, finishing the
  turn either way; `native` and `envelope` pin one.
- `/undo` and `/diff`. Every change the agent makes is committed to a git repository kept
  beside the session, with the project as its work tree and the project's own repository
  excluded. Undoing moves the history forward rather than rewinding it, so a file edited by
  hand between the change and the decision to take it back is not discarded to make the undo
  tidy.
- MCP servers. Whatever a configured server offers is adapted into the same tool the rest of
  the agent already knows, so the grammar, the manifest, the approval flow and the
  checkpoints apply unchanged. What does not carry over is trust: every call is approved,
  every call counts as mutating, and an advertised schema is checked before it can shape the
  grammar.
- `-p` runs one turn without the interface, for a git hook or a pipeline. `--approve` states
  what it may agree to, defaulting to nothing; `--output json` reports what it did; the exit
  code distinguishes finished, failed, and refused by the policy.
- Session transcripts, written as they happen to `$XDG_DATA_HOME/verve/sessions`, with
  `--resume <id>` and `--continue`. A resumed session keeps its id, appends to the file it
  came from, and remembers its model.
- Real logging. `tracing` was a dependency with no events and a subscriber pointed at the
  screen the interface owns; there are now events at the points anyone would ask about, and
  they go to a file.
- Configuration from `~/.config/verve/config.toml` and `<root>/.verve.toml`, from `VERVE_*`
  variables, and from flags, in that order of increasing precedence. `--show-config` says what
  was read and what it resolved to.
- Authentication: `VERVE_API_KEY`, an `api-key-env` naming a variable, or an `api-key` in a
  file that only you can read. `--header name=value` for anything else. A key about to cross a
  network unencrypted is warned about.
- Input history on the arrows, mouse-wheel scrolling, light markdown in the model's prose,
  and colour on anything shaped like a diff.
- `--allow-host` for a service on your own network that `web_fetch` would otherwise refuse.
- `VERVE_LSP_RUST` and its siblings, for a language server that is not on the path under the
  usual name or is there and does not work.
- CI running formatting, clippy as an error, the tests, an MSRV build and a dependency audit.
- `SECURITY.md`, `CONTRIBUTING.md`, and an operational README.

### Changed

- The backend is behind a `Provider` trait, with the OpenAI-compatible format as one
  implementation and the Anthropic Messages API as another. `[backend] kind` selects one;
  `auto` reads it off the URL. What a provider can do is declared rather than guessed - the
  grammar in particular is now sent only where it can be enforced, instead of to servers that
  accept the field and ignore it.
- A reasoning model's scratchpad is separated from its answer, in all four spellings it
  arrives in: inline `<think>` tags, a `reasoning` field, a `reasoning_content` field, and
  Anthropic's thinking blocks. It is shown apart and never enters the conversation.

- `bash` can be confined. `--confine-shell`, or `[shell] confine-writes`, makes everything
  outside the project read-only for an approved command and everything it starts. Linux
  only, and off by default, because it can make a build whose cache lives elsewhere fail in
  a way that looks like a bug in the build - name those directories with `[shell] writable`.

- `grep` no longer requires ripgrep. It is still used when present, and without it the same
  search runs on the walker `glob` already uses, honouring the same ignore files - it was an
  undocumented hard requirement, and a machine without it had no search at all.
- `web_fetch` drops the contents of `script` and `style` rather than only their tags, turns
  the entities that appear in prose, and puts a word boundary where a tag was - adjacent
  blocks used to run together as `TitleSome words`.
- `list_dir` and `glob` clamp their output like every other tool. They were the two that did
  not, which is the case the ceiling exists for.

- Read-only tool calls in one turn run together. Three file reads in a turn used to be three
  round trips one after another, for no reason - none of them can see what the others did. A
  call that changes something is a boundary on both sides.
- Compaction trims well past the budget instead of to it. The cost of compacting is not the
  copying: the server keeps a prefix cache, dropping from the front changes the prefix at the
  second message, and so the whole prompt is processed again. Trimming to just under the line
  meant the next step crossed it and paid that again. Several steps now fit between
  compactions.
- The interface redraws on events rather than on a timer that woke it twelve times a second.

- Tool names are borrowed rather than `&'static str`, so a tool can be named by something
  that was not known at compile time.
- Tests went from 21 to 222. All twelve tools have them now; the LSP pair talk to a stub
  language server, which is what finally tests that an edited file is re-sent. Ten of the twelve tools now have one; the two LSP tools do not,
  because testing them needs a language server.
- `src/tools/tests.rs` was the `run_tests` tool rather than tests, and is now `run_tests.rs`.
- Two dependencies were declared and never used: `schemars`, never imported at all, and
  `reqwest`'s `blocking` and `multipart` features in an entirely async client.

## [0.1.0] - 2026-08-09

### Added

- A coding agent in the terminal, driven by any OpenAI-compatible backend and defaulting to
  a local one.
- Tools for reading, searching and editing files, running commands, and querying a language
  server. Every path-taking tool is confined under a single root, and a call that walks
  outside it is refused rather than resolved.
- An approval flow for edits: a change arrives as a diff to accept or decline, with an
  allowlist so an agreement given once is not asked again.
- Three layers of defence on a model's tool arguments - a server-side grammar, schema
  validation, and a repair pass for the cases where the repair is unambiguous. `--no-grammar`
  disables the first to exercise the other two.
- `--max-steps` bounds how far the loop goes on one turn before control returns to you.
