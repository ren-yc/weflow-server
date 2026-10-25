# weflow-server TypeScript example client

**Example only — not published to npm**: TypeScript currently has no real
consumers; this directory is positioned as a "minimal runnable reference" with
no compatibility guarantee and no release channel.

## Contents

- `client.ts` — typed example client: `health` / `accounts` / `wait_ready`
  (wait-only readiness polling, performs no registration) / `ensure_ready`
  (registration + fast-fail on the 200 rejection state in the registration
  response, with the same rejection vocabulary as the Rust/Python behavior
  layer) / `watch` (SSE single-connection multi-frame streaming: byte-level LF
  framing, `id:`/`event:`/`data:` assembly, blank-line-delimited frames,
  `Last-Event-ID` resume, exponential backoff, 1 MiB buffer cap — over-cap
  frames are not delivered and trigger a backoff reconnect). Demo-grade subset:
  the **shape** of framing and backoff matches the Rust/Python behavior layer;
  it does not promise per-semantics equivalence and does not promise
  compatibility.
- `smoke.ts` — executable smoke test: sends requests to a real server and makes
  **real assertions** on the HTTP results ("import succeeded" does not count as
  a smoke test). Step 3 accepts only two legitimate outcomes — a business
  rejection (`StatusError`, message carries state) or a readiness-wait timeout
  after acceptance (`NotReadyError`); any other outcome (415 / 500 / network
  error / unknown exception) exits non-zero. Credentials are injected via
  environment variables; no real data is used.
- `stub-server.mjs` — minimal in-repo fake server: what is fake is the
  **shape**, not the assertions. Use it to run smoke without a real server;
  when the registration response is changed to 500 (`STUB_STATUS=500`) the same
  smoke **must** exit non-zero — this self-check makes "PASS" a failure-capable
  assertion.

## Running

Requires Node **18+** (fetch is built in; earlier versions need an extra flag
and are no longer supported).

```bash
npm install --include=dev    # devDependencies only: typescript and @types/node
                             # (environments with NODE_ENV=production must pass --include=dev,
                             #  otherwise npm silently skips devDependencies and tsc is absent)
npm run typecheck            # tsc --noEmit
npm run build                # tsc compiles to dist/

# Against a real server:
WEFLOW_BASE_URL=http://127.0.0.1:5033 WEFLOW_API_TOKEN=<token> node dist/smoke.js

# Against the in-repo stub (no real server needed):
node stub-server.mjs &                              # port 5099, default registration = business rejection
WEFLOW_API_TOKEN=fictional node dist/smoke.js       # must PASS
# Self-check (must turn red after changing the stub response):
STUB_STATUS=500 node stub-server.mjs &
WEFLOW_API_TOKEN=fictional node dist/smoke.js       # must exit non-zero
```

The TS examples in the two repositories are equivalent: same set of endpoints
and outcome semantics; `watch` here is an async generator while the qqflow
version is callback-based (a shape difference, not a semantic one).

## Boundaries

`release.yml`'s artifact matrix is not modified: the TypeScript side stays
"locally usable + example", and no release artifact is added.
