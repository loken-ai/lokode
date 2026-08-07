# Changelog

All notable changes to `verve`. The format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/).

## [0.1.0]

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
