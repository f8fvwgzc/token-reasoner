# The proxy: lge between the agent and the model

```
Codex desktop app / Codex CLI  ->  lge on 127.0.0.1:8787  ->  OpenAI
```

`lge serve` is one local process (or one container) that is:

- a **proxy** for the provider's Responses API: it forwards every request, shortens
  what it understands, and relays the streamed answer untouched;
- the **owner of the graphs**: one database per folder you add, kept fresh;
- the **graph tools** for the agent, over MCP on HTTP (`/mcp`);
- a **page** (`/`) that shows input, cached, output and reasoning tokens per session,
  what the proxy removed and added, and the folders in the graph.

The model and the reasoning level are whatever the agent asked for. The proxy never
changes them.

## What it changes in a request

Only two kinds of item in the conversation, and nothing else in the body:

| Item | What happens | Why it is safe |
|---|---|---|
| Output of a test, lint or type-check run | Reduced to its failures, or to one line when it passed | Only for tools whose output format is recognised; an unknown format is left alone |
| Output of `rg` / `grep` with more than 60 matching lines | Capped: the files the task is about first, six lines per file, and a line saying what was left out | The closing line tells the model to narrow the search |
| Output of `cat file`, or of a read that starts at line 1, for a file in the graph | A skeleton: functions the task is not about keep their signature and first docstring line; each left-out body is replaced by a marker with the exact `sed -n 'A,Bp' file` that reads it | Only when the file on disk is exactly what was indexed; a second read of the same file is always sent whole |
| A message the user typed, when it names a symbol or clearly matches one | One extra message is added after it: the evidence pack for that message, in `<lge_context>` tags | Small talk gets nothing; the same starting points are never added twice |

Never touched: the system prompt, tool definitions, reasoning items, tool calls,
patches, diffs, reads of a line range that does not start at the top, and any output
the proxy cannot classify with certainty.

## Why the prompt cache keeps working

The agent resends the whole conversation on every turn, and the provider bills the
unchanged start of it at the cached rate. A proxy that rewrote an old item differently
on a later turn would turn cached tokens into full-price tokens and raise the cost.

So every decision is made once and remembered for the conversation: the first time an
item is seen it is rewritten (or left alone), and on every later turn the same item
gets the same bytes. Context is inserted at the same place with the same text. A test
(`the cached prefix must not change`) holds this.

## Failing open

If the body cannot be read, parsed or rewritten, for any reason, the original bytes are
forwarded with the original headers. The response is relayed as it arrives; the proxy
only reads the token usage out of the stream. If the provider cannot be reached, the
client gets a 502 with a message that names the proxy.

## What is stored

Token counts per conversation, the first typed message cut to 200 characters, the model
name, and the router's recommendation. Nothing else of a conversation is written to
disk. Credentials are passed through and never stored or logged.

## Run it

Directly:

    lge serve                       # 127.0.0.1:8787, data in ~/.lge

In Docker (the same layout as a Headroom container):

    export LGE_WORKSPACE=$HOME/code # the folder that holds your repositories
    docker compose up -d --build
    open http://127.0.0.1:8787

The workspace is mounted read-only at the same path it has on the host, so the paths
the agent reports are the paths the daemon reads. `~/.codex` is mounted read-only for
the "import past sessions" button, which reads token counts only. Graphs and the usage
log live in the `lge-data` volume.

## Connect Codex

The page shows these lines with the right address. Add them to `~/.codex/config.toml`;
the CLI and the desktop app read the same file:

```toml
model_provider = "lge"

[model_providers.lge]
name = "OpenAI through lge"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
requires_openai_auth = true
```

Optional, for the graph tools:

    codex mcp add local_graph --url http://127.0.0.1:8787/mcp

To stop using the proxy, remove the `model_provider` line.

## Settings

On the page, or `daemon.json` in the data directory:

| Setting | Default | Meaning |
|---|---|---|
| Add graph context | on | The evidence pack after a typed message about the code |
| Shorten outputs | on | File skeletons, search caps, check results |
| Index on first use | on | A git repository inside the workspace is indexed the first time the agent works in it |
| Context budget | 1,500 tokens | Size of one added pack |

## What is not verified

- The proxy is tested against a stand-in provider over real sockets, not against the
  real endpoint. The first real session is the test of the wire format.
- No saving has been measured. The page shows real token counts per session and what
  was removed; "before and after" needs past sessions imported and new sessions run.
- A model may react to a shortened file by reading more ranges than it saves. The
  session table shows requests and tokens per session so this can be seen; the switch
  for shortening is on the page.
