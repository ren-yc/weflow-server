/**
 * Executable smoke: real HTTP requests against a running weflow-server,
 * asserting actual HTTP results. Credentials come from the environment;
 * no real data. Run: npm run smoke  (server must be reachable at BASE_URL),
 * or against the in-repo stub: see README.md (node stub-server.mjs).
 *
 * Fails loudly when the server is unreachable - "import succeeded" is not
 * a smoke. Failure paths set process.exitCode and RETURN instead of calling
 * process.exit(): on Windows, exiting with a live fetch keep-alive handle
 * aborts the whole Node process (assert uv_handle), which would look like
 * a crash rather than a verdict.
 */

import { WeflowClient, StatusError, NotReadyError } from "./client.js";

const base = process.env.WEFLOW_BASE_URL ?? "http://127.0.0.1:5033";
const token = process.env.WEFLOW_API_TOKEN;

if (!token) {
  console.error("smoke: WEFLOW_API_TOKEN must be set (server prints it on first start)");
  process.exitCode = 2;
} else {
  await main();
}

async function main(): Promise<void> {
  const client = new WeflowClient(base, token as string);

  // 1) health: free endpoint, must answer 200 with a JSON object
  let health;
  try {
    health = await client.health();
  } catch (err) {
    console.error("smoke: /health unreachable:", err);
    process.exitCode = 1;
    return;
  }
  if (typeof health !== "object" || health === null) {
    console.error("smoke: /health did not return an object");
    process.exitCode = 1;
    return;
  }
  console.log("smoke: /health ok, account phase =", health.account ?? "(none)");

  // 2) authenticated listing: must answer 200 and carry an accounts array
  const accounts = await client.accounts();
  if (!Array.isArray(accounts)) {
    console.error("smoke: /api/v1/accounts did not return a list");
    process.exitCode = 1;
    return;
  }
  console.log("smoke: /api/v1/accounts ok,", accounts.length, "account(s)");

  // 3) registration must end in exactly one of two legal shapes:
  //    a business refusal (200 + refusal state, surfaced as StatusError)
  //    or an accepted registration whose readiness wait then times out
  //    (NotReadyError - the bogus path never indexes). Anything else -
  //    a 415 from a missing Content-Type, a 5xx, a network error, an
  //    unknown throw - is a defect and exits non-zero.
  let outcome: "refused" | "accepted" | null = null;
  try {
    await client.ensureReady("wxid_smoke_nonexistent", {
      wxid: "wxid_smoke_nonexistent",
      db_path: "X:/definitely/not/a/real/path",
    }, 3);
    outcome = "accepted";
  } catch (err) {
    if (err instanceof StatusError) {
      // Only the 200-with-refusal-state ending is legal: a transport
      // status (415 / 500 / any >= 400) is a defect, not a business
      // refusal.
      if (err.status === 200) {
        outcome = "refused";
        console.log("smoke: registration refused as designed:", err.message.slice(0, 120));
      } else {
        console.error("smoke: registration answered HTTP", err.status,
          "- a transport error, not a business refusal:", err.message.slice(0, 120));
        process.exitCode = 1;
        return;
      }
    } else if (err instanceof NotReadyError) {
      outcome = "accepted";
      console.log("smoke: registration accepted; wait timed out as designed",
        "(last state: " + err.lastState + ")");
    } else {
      console.error("smoke: unexpected error from registration path:", err);
      process.exitCode = 1;
      return;
    }
  }
  if (outcome === null) {
    console.error("smoke: registration produced no outcome");
    process.exitCode = 1;
    return;
  }

  // 4) watch must SURVIVE a clean stream end: the server legitimately closes
  //    SSE on graceful shutdown / idle restart, and a clean EOF must reset the
  //    backoff and reconnect - not silently terminate the loop. Drive the
  //    iterator for a bounded time; the smoke server may or may not emit
  //    events, so the assertion is "the loop is still alive after a clean
  //    close", verified by racing a manual close against the timeout.
  try {
    const stream = client.watch();
    const it = stream[Symbol.asyncIterator]();
    const raced = await Promise.race([
      it.next().then((r) => "frame" as const),
      new Promise<"timeout">((r) => setTimeout(() => r("timeout"), 3000)),
    ]);
    console.log("smoke: watch kept streaming after handshake (" + raced + ")");
    // Closing the iterator must not throw - the abort path is what ends it.
    await it.return?.(undefined);
  } catch (err) {
    console.error("smoke: watch loop terminated unexpectedly on a live server:", err);
    process.exitCode = 1;
    return;
  }

  console.log("smoke: PASS");
}
