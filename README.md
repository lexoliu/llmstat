# llmstat

Token usage distribution and cost across local LLM CLIs — one report over
**Devin CLI**, **Claude Code**, and **Codex CLI**, priced with the LiteLLM
pricebook.

Pure Rust, one-shot output (no fullscreen TUI): prints the report and exits.
Colors/ANSI only on a TTY; respects `NO_COLOR`.

```
cargo install --git https://github.com/lexoliu/devin-tokenviz
```

## Usage

```
llmstat                 # all recorded history (default: `all`)
llmstat day             # last 24 hours, hourly timeline  (alias: 24h)
llmstat week            # last 7 days, daily timeline
llmstat month           # last 30 days, daily timeline
```

```
llmstat --sources devin,claude      # read only these sources
llmstat --devin-transcripts DIR     # override ~/.local/share/devin/cli/transcripts
llmstat --devin-db FILE             # override the sessions.db path
llmstat --devin-transcripts-only    # skip sessions.db (transcripts only)
llmstat --claude-dir DIR            # override ~/.claude/projects
llmstat --codex-dir DIR             # override ~/.codex
llmstat --pricing rules.toml        # extra pricing rules (see below)
llmstat --refresh-prices            # re-fetch the LiteLLM pricebook
```

Without `--sources`, every source whose data directory exists is read; the
three scans run concurrently.

## Sample output (day)

```
llmstat · last 24h · Sep 11 19:18 → Sep 12 19:18
3182 sessions · 15,602 calls · 1.58B tokens
devin: 81 transcripts · +23469 calls (2.82B tok) recovered from sessions.db
claude: 4776 transcript files · 121842 dupes skipped
codex: 2178 rollout files
prices: litellm (cache)
input 114M · cached 1.47B · output 5.52M
list (equiv.) $928.61   actual $185.09   · some models unpriced

── by model ─────────────────────────────────────────
 SRC    MODEL           TOTAL  SHARE       IN   CACHED      OUT  PRICED AS      LIST   ACTUAL  DIST
 devin  SWE-2           1.22B  76.8%     110M    1.10B    4.68M  kimi-k3 *    $730.27    $0.00  ██████████████
 claude claude-opus-5    317M  20.0%    2.42M     314M     636K  claude-opus-5 $185.09  $185.09  ████
 ...
```

- `list` = what the usage would cost at public list price
- `actual` = what the CLI actually charges — **$0.00 in green** for models
  the CLI offers free (their list price is struck through), red for real spend
- `*` marks free-in-CLI models; `?` means no price could be found

## Data sources

| Source | Files | Notes |
|---|---|---|
| **Devin CLI** | `~/.local/share/devin/cli/transcripts/*.json` + `sessions.db` | transcripts only serialize the *current* chain; the db additionally recovers calls from resumed/compact/forked chains and subagent sessions that never get a transcript. Every message node's `metadata.num_tokens_preceding` equals the exact `prompt_tokens` of its inference call (verified against `response_dimensions`). Recovered calls get exact input tokens; cached/output are split at that session's observed ratio and flagged as estimated. |
| **Claude Code** | `~/.claude/projects/**/*.jsonl` | `type:"assistant"` records carry `message.model` + `usage`. Cache-write tokens count as input; `cache_read` is the discounted part. Responses are deduped globally by `message.id` + `requestId` — Claude copies history into new transcript files on resume/compact. |
| **Codex CLI** | `~/.codex/sessions/**`, `~/.codex/archived_sessions/` | `event_msg`/`token_count` payloads carry per-call `last_token_usage` (input / cached input / output / reasoning output). Model comes from `turn_context`/`session_meta`. Events are deduped by `(session, timestamp, cumulative total)` — the same event lives in both directories. |

Devin's sessions.db is multi-GB and insert-only; matched rows are cached in
`~/.cache/llmstat/` and each run scans only the new `row_id` tail — parallel
range scans over several read-only connections, with a sequential prefetch
warming the OS page cache. First run ~3s, later runs ~0.1s.

## Pricing

Base prices come from LiteLLM's
[`model_prices_and_context_window.json`](https://github.com/BerriAI/litellm),
fetched once and cached for 24h in `~/.cache/llmstat/` (a stale cache is used
offline). Model names are normalized and matched by exact key, then by `-`
-delimited prefix.

Rules layer on top of LiteLLM for semantics it can't express — free-in-CLI
models priced at an equivalent public model:

| Model | Priced as | Status |
|---|---|---|
| SWE-1.7 | `kimi-k2.7-code` ($0.95/$0.19/$4.00 per 1M) | free in Devin → $0.00 |
| SWE-2 | `kimi-k3` ($3.00/$0.30/$15.00) | free in Devin → $0.00 |
| Adaptive | `kimi-k3` (est.) | free in Devin → $0.00 |
| GLM-5 | `glm-5` | free in Devin → $0.00 |

A rule may carry `as` = a LiteLLM key, so the equivalent model's *current*
price is borrowed from the pricebook and the rule's own numbers only apply
when the key isn't listed.

Custom rules — `~/.config/llmstat.toml` or `--pricing file.toml`:

```toml
# substring match on the normalized model name (lowercase, non-alnum → '-')
[[rule]]
pattern = "swe-2"
label = "SWE-2"
free = true            # list price struck through, actual $0.00
as = "kimi-k3"         # borrow LiteLLM's current price for this key

[[rule]]
pattern = "my-proxy-model"
input = 2.0            # USD per 1M input tokens
cached = 0.2           # optional, defaults to input
output = 8.0           # USD per 1M output tokens
```

User rules take precedence over built-ins and LiteLLM.

## Notes

- Recovered calls show exact input tokens; the cached/output split is
  estimated and marked in the output.
- Models with no matching price are shown as `?` and excluded from totals.
- Devin's db scan assumes `message_nodes` is append-only (`row_id`
  AUTOINCREMENT); a rebuilt database invalidates the cache automatically.
