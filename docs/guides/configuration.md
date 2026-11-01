# Providers and configuration

## Which provider

A provider lokode knows is named rather than written out, with `--provider` or with `provider = "openrouter"` in `[backend]` or in a `[backends]` entry.

The provider gives the url, the API and the variable the key is read from.
A `url`, `kind` or `api-key-env` written beside it wins.

| Provider | Url | Key from |
|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openai` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta/openai` | `GEMINI_API_KEY` |
| `mistral` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` |
| `groq` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `deepseek` | `https://api.deepseek.com/v1` | `DEEPSEEK_API_KEY` |
| `xai` | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `together` | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` |
| `fireworks` | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` |
| `cerebras` | `https://api.cerebras.ai/v1` | `CEREBRAS_API_KEY` |
| `deepinfra` | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA_API_KEY` |
| `loken` | `http://127.0.0.1:11435` | none |
| `ollama` | `http://127.0.0.1:11434/v1` | none |
| `lmstudio` | `http://127.0.0.1:1234/v1` | none |
| `llamacpp` | `http://127.0.0.1:8080/v1` | none |
| `vllm` | `http://127.0.0.1:8000/v1` | none |

- `--provider` takes the place of the configured `[backend]`, so no key written for another backend is sent to this one.
  A later file that names a provider does the same.
- Any other OpenAI-compatible server is reached by its url.
  A url ending in `/v1` or `/openai` is the API's root, and a bare host gets `/v1`.
- `/model` lists what the provider serves.

## Which API

`kind = "auto"` reads it from the URL: `api.anthropic.com` is the Messages API, and anything else is OpenAI-compatible.
Name a kind to reach a proxy of Anthropic's format on your own host.

The credential is found the same way for both: the environment, then a named variable, then a file only you can read.
It goes in `Authorization: Bearer` for the OpenAI family and in `x-api-key` for Anthropic.

Reasoning arrives as `<think>` tags, a `reasoning` field, a `reasoning_content` field or Anthropic's thinking blocks.
All 4 are shown the same way, set apart, and never sent back to the model.

## How tools are called

| `--tool-protocol` | |
|---|---|
| `native` | the server's `tools` field and the `tool_calls` it returns |
| `envelope` | one JSON object per turn, held to a grammar built from the tool schemas. It needs only a completion |
| `auto` | the default. The backend is asked once at startup for a call, and a server that answers with prose or refuses the field gets the envelope |

## Configuring it

Settings come from 5 places, and the narrower wins: a flag, the environment, `<root>/.lokode.toml`, `~/.config/lokode/config.toml`, the built-in default.

```toml
# ~/.config/lokode/config.toml, or .lokode.toml in a project
model = "qwen3-coder:latest"
tool-protocol = "auto"          # auto | native | envelope

[models."qwen3-coder:latest"]   # what one model needs, when it differs from [limits]
context-tokens = 32768
max-answer-tokens = 2048

[backend]
url = "http://127.0.0.1:11435"  # or provider = "openrouter", see Which provider
kind = "auto"                   # auto | openai | anthropic
api-key-env = "MY_BACKEND_KEY"  # name a variable; do not write the key here
headers = { "X-Team" = "lokode" }

[limits]
max-steps = 16
context-tokens = 8192
max-answer-tokens = 4096

[web]
allow-hosts = ["docs.example.com"]

[shell]
sandbox = "workspace"           # workspace | read-only | off
network = true                  # false refuses TCP to a sandboxed command
writable = ["~/.local/share/pnpm"]
hidden = ["~/work/secrets"]

[session]
transcripts = true
```

**Flags.**

| Flag | What it does |
|---|---|
| `-m`, `--model` | the model, as the backend names it |
| `-r`, `--root` | the project, `.` when not given |
| `-u`, `--backend-url` | where the backend is |
| `--provider` | a provider by name, in place of the configured backend |
| `--backend-kind` | `openai` or `anthropic`, read off the url when not given |
| `--header NAME=VALUE` | a header sent with every backend request, repeatable |
| `--max-steps`, `--context-tokens`, `--max-answer-tokens` | the limits, over those of the configuration |
| `--tool-protocol` | `auto`, `native` or `envelope` |
| `--no-grammar` | turn off the server-side constraint on the envelope |
| `--sandbox` | `workspace`, `read-only` or `off` |
| `--plan`, `--accept-edits` | start planning, or writing without asking |
| `--allow-host HOST` | let `web_fetch` reach a host on your own network |
| `--trust-project` | take the project's guarded settings for this run |
| `--resume ID`, `--continue` | come back to a recorded session |
| `--no-transcript`, `--log-file` | what is kept of the session, and where the log goes |
| `-p`, `--approve`, `--output` | one prompt without the screen, see [Without the interface](interfaces.md#without-the-interface) |
| `--show-config` | print what everything resolved to |

**A project's file.** Its settings that run programs or loosen approvals wait for trust.

| | |
|---|---|
| Waits for trust | `[hooks]`, `[mcp]`, `[lsp]`, `[shell]`, `[backend]`, `[backends]`, `[web]`, `[session]`, `[approve.allow]` |
| Applies at once | everything else, `[approve.deny]` included |
| Asked | at the first interactive session, and again after a change to them |
| Without a terminal | left out, with a warning. `--trust-project` takes them for one run |

The trusted settings are kept in `$XDG_DATA_HOME/lokode/trusted.json`.

`lokode --show-config` prints the files read and what everything resolved to.
It names the headers without their values.

**Credentials.** `LOKODE_API_KEY` comes first, then the variable named by `api-key-env`, then an `api-key` in the file.
The file is accepted only if nobody else can read it.  
A key about to cross a network unencrypted is warned about.

Environment variables: `LOKODE_MODEL`, `LOKODE_BACKEND_URL`, `LOKODE_LOG_FILE`, `LOKODE_API_KEY`.
