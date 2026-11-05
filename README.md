# broadcom-usage

Collects AI coding-agent usage (Claude Code, Codex, Pi, Grok, Muse) from this machine,
other machines' pushes, GitHub Actions artifacts and a proxy's per-request receipts,
prices it, and answers [BROADCOM](https://github.com/AVGVSTVS96/broadcom)'s Usage,
Sessions and Credits views and its live-sessions stream.

```
transcripts ─┐
Mac pushes ──┤                     ┌─ GET /deck/usage, /deck/sessions, /deck/credits
GH artifacts ┼─▶ SQLite (usage.db) ┤
receipts ────┘   records → events  └─ GET /deck/events (SSE: s:<session>, app:<name>)
```

- `records` keeps every device's records as sent, forever. `events` holds one resolved
  row per record: the best copy across devices, priced, with a proxy receipt standing in
  for the transcript record of the same request. It's maintained on write, so a view
  reads one indexed time range.
- Inputs are followed from where they stopped. A file that was replaced is read again,
  which changes nothing already held.
- Shapes match the Go deck it replaces; see [api/](api/README.md).

## Run

```
cargo install --path .
broadcom-usage serve --config ~/.config/broadcom-usage/config.json
```

Every config field is optional; see `src/config.rs`. Clients send the management key as
`Authorization: Bearer <key>` or `X-Management-Key: <key>`.
