# Changelog

## v0.0.1 (2026-11-15)

First release of token-reasoner (`lge`): a local code graph that sits between Codex and
OpenAI and sends the model less text for the same work, at the same model and reasoning
level.

- **Proxy** (`lge serve`, 127.0.0.1:8787): one line in `~/.codex/config.toml`
  (`openai_base_url`) routes Codex through lge. Adds the graph's context to a message about
  the code, with the conclusions first; shortens test, lint and type-check output to
  failures; caps long searches; points repeated output to its first copy; keeps the prompt
  cache intact; fails open. A page shows input, cached, output and reasoning tokens per
  session.
- **Code graph** on LadybugDB (embedded): 36 languages through tree-sitter (Python,
  TypeScript and JavaScript with imports, routes, tables and queues; 33 more with
  definitions and calls), plus outlines of HTML, CSS, Markdown, SQL, YAML, TOML and JSON.
  Every edge records whether it is verified, inferred or unresolved. Changed files are
  re-indexed before each request.
- **Memory**: decisions and conventions stored by the agent, and every commit and merged
  pull request, recalled with the code they concern.
- **Graph tools** over MCP (HTTP and stdio): search, change impact, execution flow,
  dependencies, exact source, architecture, recall, retain.
- **Install**: `scripts/install-macos.sh` (Rust, OpenSSL 3, build, daemon, Codex
  connection, optional start at login), or Docker.

Tested with real Codex sessions on macOS (codex-cli 0.160.0, ChatGPT login). Token savings
are not measured across a body of work yet; the page shows each session's numbers.
