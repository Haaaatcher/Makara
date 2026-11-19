---
name: makara
description: Manage the user's AI API credentials (base_url + api_key) with the Makara CLI (`makara`) — an encrypted local vault that syncs through the user's own GitHub private repo. Use whenever the user asks to store, look up, check, copy, add, edit, or delete API keys or provider credentials, sync credentials across devices, or asks things like "which keys do I have", "is my OpenAI key still valid", or "put my Anthropic key into this config" — even when Makara is not mentioned by name.
---

# Makara — AI API credential vault CLI

`makara` stores AI API credentials as *records* (name, protocol label, base URL, API key, models, notes) in an encrypted vault (Argon2id + AES-256-GCM), optionally synced through the user's own GitHub private repo. After a one-time `makara init`, the passphrase is remembered in the system credential store, so day-to-day commands run without prompting.

Protocol is a user-chosen label (OpenAI / Anthropic / any custom text) — never infer or normalize it; base URLs are stored as-is.

## Safety rules (read first)

- **Full keys are never printed.** Every output masks keys (`sk-t...f456`) by design — terminal buffers, screenshots, and scrollback leak. Do not try to unmask, parse vault files, or grep the data directory to recover a key.
- **The only sanctioned key exit is the clipboard.** To see or hand over a key, run `makara key <target>` and tell the user it is on the clipboard.
- If a key must land inside a config file (only when the user explicitly asks for that exact file): run `makara key <target>`, read the clipboard in the same shell (`pbpaste` / PowerShell `Get-Clipboard` / `xclip -o -selection clipboard`), write it into the file, then remind the user to clear clipboard history. The CLI does not auto-clear the clipboard.
- Passing `-k <key>` on the command line exposes the key in shell history and process lists. Fine on the user's own machine; flag it if history is shared or logged.
- `makara export` writes **plaintext JSON** and asks twice before writing. Run it only on explicit request, never commit or sync the exported file, and delete it once used.
- Never place keys or the passphrase into logs, commit messages, issue text, or error output.

## Find records

```bash
makara list              # table: #, name, protocol, base URL, masked key, models, check
makara show <target>     # full details of one record, key still masked
makara search openai     # fuzzy: name, base URL, notes, protocol label
```

`<target>` = name (preferred), full id or id prefix, or `#index` from the **latest `makara list`**. `#` indexes in `search` results are display order and differ from `list` — never reuse them for `show`/`edit`/`delete`; target by name or id instead.

## Copy a key

```bash
makara key <target>      # full key to clipboard, never printed
```

## Apply credentials to a tool config

```bash
makara use <target> [--into claude|codex] [--model <id>] [--model-map <spec>] [--no-model] [--project] [--yes] [--dry-run]
```

The sanctioned way to put a key into a config file — prefer it over clipboard + manual editing. It shows a plan first (paths and fields, key masked) and backs up the original file once as `.bak`. **Claude Code** (`--into claude`, default): writes the record's base URL and key into `~/.claude/settings.json` env block; model mapping is per-alias (`opus`/`sonnet`/`haiku` → `ANTHROPIC_DEFAULT_*_MODEL` keys) via three interactive menus or `--model-map opus=X,sonnet=Y,haiku=Z` (subset ok). **Codex CLI** (`--into codex`, baselined against Codex 0.170.0): writes `~/.codex/config.toml` (`model_provider` + `[model_providers.makara]` with base_url, `wire_api = "responses"` — chat support was removed from Codex, so `openai-chat` records are refused — and the key in `experimental_bearer_token`); single main-model key — interactive menu or `--model <id>` (`--model` is rejected for Claude). `--no-model` skips mapping for either target. Default scope is the user config; `--project` (Claude only) writes the gitignored `.claude/settings.local.json`; `--project --shared` writes the committed file — warn the user about the git-leak risk. Anthropic-family records go to Claude, OpenAI-family to Codex; mismatches are refused. Non-interactive shells (your usual case) keep pure-parameter semantics: pass the record id/name, `--yes`, and the target's model flag. Omitted pieces only prompt in a human's interactive terminal — never pipe input expecting menus.

## Add a record

Non-interactive (agent-friendly):

```bash
makara add -n "OpenAI primary" -p openai -b "https://api.openai.com/v1" -k "sk-..." -m "gpt-4o" --notes "prod"
```

`-p` is a free label (`openai`, `anthropic`, or custom text). `-m` is repeatable or comma-separated. Without `-m`/`--notes` the CLI prompts for them; in a non-interactive shell EOF skips them safely.

Interactive fallback: `makara add` prompts field by field; feed answers one per line via stdin if needed. A save auto-runs a key check against `GET /models` (a failing check never blocks the save).

## Edit, delete, check

```bash
makara edit <target>     # interactive, field by field; Enter keeps current value; blank + Enter clears a field
makara delete <target>   # asks [y/N]; in non-interactive shells pipe `y` (printf 'y\n' | makara delete <target>), EOF = safe default N
makara check [target]    # GET /models probe, no target = all records
```

Deletion is tombstone-based: the record stays deleted on every device after sync and is purged after a 30-day grace period. This is intentional — do not try to hard-delete records from the data directory.

`check` verdicts: `valid` / `invalid` / `no permission` (valid but lacks access) / `rate limited` (valid but quota exhausted) / `unverifiable` (endpoint not probeable).

## Sync, status, remote

```bash
makara sync              # pull + merge + push
makara status            # remote, unpushed changes, last sync, tombstone window, record count
makara remote set <owner/repo>   # bind or rebind
makara remote unset      # unbind (local vault untouched)
```

Sync merges by `updated_at` (newest wins) and applies tombstones, so records deleted on one device do not resurrect from another. Devices can diverge offline; `makara sync` reconciles.

## Passphrase and lock

- The passphrase is remembered in the system credential store for the auto-lock window (default 10 minutes, rolling renewal while in use); `makara lock` forgets it immediately.
- `makara passwd set` re-encrypts everything and pushes; other devices unlock with the new passphrase after their next sync.
- A forgotten passphrase is unrecoverable — all data is lost. Never guess or brute-force.

## First-time setup on a new machine

```bash
makara init              # interactive: optional GitHub device-flow auth, auto-creates a private repo, then sets the passphrase
makara init <owner/repo> # skip the sync question
```

Re-running `init` on an initialized machine is a safe merge-based re-bind; data is not lost. All devices must use the same passphrase.

## Import / export

```bash
makara export [path]     # PLAINTEXT JSON — see safety rules; directory arg auto-names makara-export.json without overwriting
makara import <path>     # merge records from a previously exported JSON
```

## Environment notes

- Data directory: `~/.config/myapikey` (macOS: `~/Library/Application Support/myapikey`, Windows: `%APPDATA%\myapikey`).
- Set `MYAPIKEY_HOME=/tmp/...` to sandbox all data before experimenting or testing commands — never probe against the user's real vault.
- In SSH sessions on macOS, the remembered passphrase is skipped automatically (the keychain prompt can't be shown over SSH); the CLI says so and asks for the passphrase manually. Set the `MYAPIKEY_PASSPHRASE` env var to keep scripts unattended.
- Interactive prompts read stdin line by line; on EOF they take the safe default (confirmations default to N), so pipelines work without a TTY.

## Troubleshooting

- `not initialized` → run `makara init` first.
- `push failed` during sync → another device pushed new data; run `makara sync` once (auto-merge, then push).
- GitHub device code expired → codes last 15 minutes; rerun the command.
- Unbound after working before (standalone mode) → the GitHub token was lost or revoked; data is intact. `makara remote set <owner/repo>` restores the binding, sync reconciles.
- Update check → `makara update` (compares GitHub releases, prints the upgrade command; never modifies files itself).
