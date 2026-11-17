# Mimint quickstart

Mimint is a personal AI assistant. This guide gets one turn running
against a local or remote backend. All commands use the `mimint-cli`
binary built from this repository:

```sh
cargo build --release
./target/release/mimint-cli <prompt>
```

## 1. Local backend (Ollama) — the default path

Privacy Mode is ON by default, and only loopback backends are
reachable while it is on. Ollama on the same machine is the intended
first backend.

1. Install and start Ollama, and pull a model (any model works; the
   examples use `qwen2.5:3b`):

   ```sh
   ollama pull qwen2.5:3b
   ```

2. Write a config file. Default location:
   `$XDG_CONFIG_HOME/mimint/config.toml`, else
   `~/.config/mimint/config.toml`. (Pass `--config PATH` to use
   another file. The environment variable `MIMINT_CONFIG` is NOT
   read.)

   ```toml
   default_backend = "local"

   [[backend]]
   name = "local"
   kind = "ollama"
   model = "qwen2.5:3b"
   ```

3. Run a turn:

   ```sh
   mimint-cli "What can you do?"
   ```

   No key is needed: a loopback backend only gets a key when it
   explicitly declares `key_env`/`key_file`, and Ollama needs none.

If no backend is configured at all, Mimint probes loopback for
Ollama (port 11434) and LM Studio (port 1234) and prints a
configuration suggestion. The probe changes nothing; exit code 1 in
that state means "not ready to run", not a crash.

LM Studio works the same way with `kind = "lm-studio"` (default
endpoint `http://127.0.0.1:1234/v1`).

## 2. Remote backend (Claude / OpenAI-compatible)

Remote backends are paid backends. Two gates apply, both explicit:

1. **Cost opt-in.** Paid backends are LOCKED unless the config file
   opts in:

   ```toml
   [cost]
   cap_usd = 5.0
   ```

   A missing `[cost]` section locks paid backends exactly like a
   zero cap. `cap_usd` is an opt-in gate, NOT spending metering:
   actual spend is not measured. See
   [privacy-and-cost.md](privacy-and-cost.md).

2. **Privacy Mode OFF.** While Privacy Mode is on, only loopback
   hosts are reachable. Turn it off per run:

   ```sh
   mimint-cli --privacy-mode off "Hello"
   ```

   or in the config file:

   ```toml
   [privacy]
   enabled = false
   ```

   Turning Privacy Mode off means remote providers receive your
   prompts and conversation data. Only do it when you trust the
   configured endpoint.

Example Claude backend (key from an environment variable, never
written in the file):

```toml
[[backend]]
name = "claude"
kind = "claude"
model = "claude-sonnet-4-5"
key_env = "ANTHROPIC_API_KEY"
```

```sh
export ANTHROPIC_API_KEY="..."
mimint-cli --privacy-mode off --backend claude "Hello"
```

Example OpenAI-compatible backend:

```toml
[[backend]]
name = "cloud"
kind = "openai"
model = "gpt-5-mini"
base_url = "https://api.example.com/v1"
key_env = "EXAMPLE_API_KEY"
```

`kind = "openai"` requires an explicit `base_url`; there is no
default remote endpoint. `max_tokens` for Claude defaults to 4096
and can be set per backend.

## 3. Ad-hoc backend (flags, no config file)

```sh
mimint-cli --base-url http://127.0.0.1:11434/v1 --model qwen2.5:3b "Hello"
```

`--base-url`/`--model` may be filled from `MIMINT_BASE_URL` /
`MIMINT_MODEL`. The cost gate applies to ad-hoc backends as well:
a remote ad-hoc endpoint stays locked until the config file opts
in with `[cost] cap_usd > 0`, even when no backend is defined in
that file. A key file for the ad-hoc backend: `--key-file PATH`
(the file must be mode 0600). See
[configuration.md](configuration.md) for the full precedence order.
