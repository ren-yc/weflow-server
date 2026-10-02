/**
 * Executable smoke: real HTTP requests against a running weflow-server,
 * asserting actual HTTP results. Credentials come from the environment;
 * no real data. Run: npm run smoke  (server must be reachable at BASE_URL).
 *
 * Fails loudly when the server is unreachable - "import succeeded" is not
 * a smoke.
 */

import { WeflowClient, StatusError } from "./client.js";

const base = process.env.WEFLOW_BASE_URL ?? "http://127.0.0.1:5033";
const token = process.env.WEFLOW_API_TOKEN;

if (!token) {
  console.error("smoke: WEFLOW_API_TOKEN must be set (server prints it on first start)");
  process.exit(2);
}

const client = new WeflowClient(base, token);

// 1) health: free endpoint, must answer 200 with a JSON object
const health = await client.health();
if (typeof health !== "object" || health === null) {
  console.error("smoke: /health did not return an object");
  process.exit(1);
}
console.log("smoke: /health ok, account phase =", health.account ?? "(none)");

// 2) authenticated listing: must answer 200 and carry an accounts array
const accounts = await client.accounts();
if (!Array.isArray(accounts)) {
  console.error("smoke: /api/v1/accounts did not return a list");
  process.exit(1);
}
console.log("smoke: /api/v1/accounts ok,", accounts.length, "account(s)");

// 3) registration refusal surfaces as an error, not a wait: posting a
//    deliberately wrong key to a bound server must not hang
try {
  await client.ensureReady("wxid_smoke_nonexistent", {
    wxid: "wxid_smoke_nonexistent",
    db_path: "X:/definitely/not/a/real/path",
  }, 3);
  console.log("smoke: registration accepted (server had no binding)");
} catch (err) {
  if (err instanceof StatusError || err instanceof Error) {
    console.log("smoke: registration path errored as designed:", (err as Error).message.slice(0, 80));
  }
}

console.log("smoke: PASS");
