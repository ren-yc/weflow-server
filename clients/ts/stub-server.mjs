// Minimal stand-in for the weflow-server HTTP face, for clients/ts smoke.
//
// It exists so \`smoke.ts\` can be gated on real HTTP outcomes without a
// running server or real data: \`npm run smoke\` against this stub must pass,
// and turning the registration answer into a transport error (STUB_STATUS=500)
// must make the same smoke exit non-zero - the smoke's step-3 assertions are
// only a gate if something can fail them.
//
// Endpoints (exactly the three smoke.ts touches):
//   GET  /health            -> 200 JSON ({"version": "stub", "account": "none"})
//   GET  /api/v1/accounts   -> 200 JSON ({"accounts": []}) unless STUB_READY=1,
//                              in which case it lists the last registered
//                              account as "ready"
//   POST /api/v1/accounts   -> requires Content-Type: application/json (415
//                              otherwise, mirroring the real server's axum
//                              behavior); answers STUB_STATUS (default 200)
//                              with {"success": true, "state": STUB_STATE},
//                              STUB_STATE default "account_conflict" - a
//                              business refusal, the shape smoke.ts expects.
//
// No Authorization check: the stub mirrors wire shapes, not server security;
// the header assertion lives in smoke.ts's own request path.
//
// Usage (from clients/ts):
//   node stub-server.mjs                      # port 5099, refusal by default
//   STUB_STATUS=500 node stub-server.mjs      # registration becomes 500
//   STUB_READY=1 node stub-server.mjs         # accept + account ready
//   node dist/smoke.js with WEFLOW_BASE_URL=http://127.0.0.1:5099

import { createServer } from "node:http";

const port = Number(process.env.STUB_PORT ?? 5099);
const regStatus = Number(process.env.STUB_STATUS ?? 200);
const regState = process.env.STUB_STATE ?? "account_conflict";
const ready = process.env.STUB_READY === "1";

let registered = null;

const json = (res, status, body) => {
  const data = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json" });
  res.end(data);
};

createServer((req, res) => {
  const url = req.url ?? "/";
  if (req.method === "GET" && url === "/health") {
    json(res, 200, { version: "stub", account: "none" });
    return;
  }
  if (req.method === "GET" && url === "/api/v1/accounts") {
    json(res, 200, {
      success: true,
      accounts: ready && registered
        ? [{ wxid: registered.wxid, db_storage: "", message_count: 0, state: "ready" }]
        : [],
    });
    return;
  }
  if (req.method === "POST" && url === "/api/v1/accounts") {
    const ct = req.headers["content-type"] ?? "";
    if (!ct.startsWith("application/json")) {
      // Mirrors the real server: axum's Json extractor rejects a present but
      // non-JSON content type with 415. A client that omits the header must
      // fail here, not silently "work" against a stub that accepts anything.
      json(res, 415, { success: false, code: 415, message: "unsupported media type" });
      return;
    }
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      if (regStatus >= 400) {
        json(res, regStatus, { success: false, code: regStatus, message: "stub transport error" });
        return;
      }
      try {
        registered = JSON.parse(body);
      } catch {
        registered = null;
      }
      json(res, regStatus, { success: true, state: regState });
    });
    return;
  }
  json(res, 404, { success: false, code: 404, message: "not found" });
}).listen(port, "127.0.0.1", () => {
    console.log('stub-server listening on http://127.0.0.1:' + port +
    ' (state=' + regState + ', status=' + regStatus + ', ready=' + ready + ')');
});
