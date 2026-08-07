# verve

A coding agent that runs in your terminal, against a model on your own machine. No account,
no request leaving the box, no editor to adopt.

```sh
verve --model qwen3-coder:latest --root .
```

It speaks the OpenAI-compatible API, so it works against [LLMuse](https://github.com/loken-ai/loken)
or anything else that serves one - the default backend is `http://127.0.0.1:11435`.

## What it does

Reads, searches and edits files under one root, runs commands, and talks to a language server
for the things a language server is better at than a grep. The loop is bounded: `--max-steps`
caps how far it will go on a single turn before handing control back.

## Where it is careful

- **One root, and it is enforced.** Every path-taking tool is confined under `--root`. A tool
  call that walks outside it is refused rather than resolved.
- **Edits are approved, not applied.** A change arrives as a diff to accept or decline, and
  the allowlist remembers what you have already agreed to so the second identical question is
  not asked.
- **A model's tool arguments are repaired before they are trusted.** They arrive slightly
  wrong often enough that guessing is not acceptable: the arguments are validated against a
  schema, repaired where the repair is unambiguous, and rejected where it is not. `--no-grammar`
  turns off the server-side constraint to exercise that path on purpose.

## Build

```sh
cargo build --release      # target/release/verve
```

## Licence

MIT OR Apache-2.0, at your option.
