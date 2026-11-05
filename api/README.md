# API

Base `/v8/management`, `Authorization: Bearer <management key>`. Shapes are the Go deck's, unchanged.

| Endpoint | Schema | Example |
|---|---|---|
| `GET /deck/usage?range&by&measure&w.<dim>&q&limit&within` | [usage](usage.schema.json) | [usage.json](examples/usage.json) |
| `GET /deck/sessions?range` | [sessions](sessions.schema.json) | [sessions.json](examples/sessions.json) |
| `GET /deck/credits` | [credits](credits.schema.json) | [credits.json](examples/credits.json) |
| `GET /deck/events` (SSE `data:` frames) | [live-session](live-session.schema.json) (each `s:` value) | [snapshot](examples/events-snapshot.json), [patch](examples/events-patch.json), [stale](examples/events-stale.json) |
| `POST /v0/management/usage/events` (gzip JSON batch from other machines) | | [batch.json](examples/batch.json) |

Examples are synthetic: real answers with every name, id and number replaced.

The schemas come from the Rust types (`views::schemas`): every listed field is always present, and no others. `cargo test schemas` fails when they drift from the types or an example stops matching; `UPDATE_SCHEMAS=1 cargo test schemas` rewrites them.
