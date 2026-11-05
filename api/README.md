# API

Base `/v8/management`, `Authorization: Bearer <management key>`. Shapes are the Go deck's, unchanged.

| Endpoint | Example |
|---|---|
| `GET /deck/usage?range&by&measure&w.<dim>&q&limit&within` | [usage.json](examples/usage.json) |
| `GET /deck/sessions?range` | [sessions.json](examples/sessions.json) |
| `GET /deck/credits` | [credits.json](examples/credits.json) |
| `GET /deck/events` (SSE `data:` frames) | [snapshot](examples/events-snapshot.json), [patch](examples/events-patch.json), [stale](examples/events-stale.json) |
| `POST /v0/management/usage/events` (gzip JSON batch from other machines) | [batch.json](examples/batch.json) |

Examples are synthetic: real answers with every name, id and number replaced.
