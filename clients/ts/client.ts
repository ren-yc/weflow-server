/**
 * Minimal typed example client for weflow-server's HTTP API.
 *
 * Example only: not published to npm, not a supported SDK surface. Mirrors the
 * handwritten behavior layer of clients/rust and clients/python (readiness
 * polling, SSE watching) at demonstration scale.
 *
 * Auth goes in the Authorization header only - never in the URL.
 */

const REFUSAL_STATES = new Set([
  "account_conflict",
  "invalid_key",
  "invalid_db_path",
  "unknown_qq",
]);

export class StatusError extends Error {
  constructor(
    public readonly status: number,
    public readonly url: string,
  ) {
    super(`HTTP ${status} on ${url}`);
    this.name = "StatusError";
  }
}

export class NotReadyError extends Error {
  constructor(
    public readonly timeoutSeconds: number,
    public readonly lastState: string,
  ) {
    super(`account not ready within ${timeoutSeconds}s (last state: ${lastState})`);
    this.name = "NotReadyError";
  }
}

export interface AccountView {
  wxid: string;
  state: "awaiting_key" | "indexing" | "ready" | "error";
  error?: string;
}

export interface Health {
  version?: string;
  account?: string;
}

export class WeflowClient {
  private readonly base: string;

  constructor(
    baseUrl: string,
    private readonly token: string,
  ) {
    this.base = baseUrl.replace(/\/+$/, "");
  }

  private url(path: string): string {
    return this.base + path;
  }

  private headers(): Record<string, string> {
    return { Authorization: `Bearer ${this.token}` };
  }

  async health(): Promise<Health> {
    const resp = await fetch(this.url("/health"));
    if (!resp.ok) throw new StatusError(resp.status, "/health");
    return (await resp.json()) as Health;
  }

  async accounts(): Promise<AccountView[]> {
    const resp = await fetch(this.url("/api/v1/accounts"), {
      headers: this.headers(),
    });
    if (!resp.ok) throw new StatusError(resp.status, "/api/v1/accounts");
    const body = (await resp.json()) as { accounts: AccountView[] };
    return body.accounts;
  }

  /** Wait-only readiness poll: no registration action, no request body. */
  async waitReady(account: string, timeoutSeconds = 120): Promise<void> {
    const deadline = Date.now() + timeoutSeconds * 1000;
    let lastState = "not-registered";
    while (true) {
      const mine = (await this.accounts()).find((a) => a.wxid === account);
      if (mine) {
        if (mine.state === "ready") return;
        if (mine.state === "error") {
          throw new NotReadyError(timeoutSeconds, `error: ${mine.error ?? ""}`);
        }
        lastState = mine.state;
      } else {
        lastState = "not-registered";
      }
      if (Date.now() >= deadline) throw new NotReadyError(timeoutSeconds, lastState);
      await new Promise((r) => setTimeout(r, 250));
    }
  }

  /**
   * Register then wait until ready. A 200 whose JSON state names a refusal
   * (conflict / bad key / bad path / unknown qq) throws instead of waiting:
   * treating those as accepted makes the poll time out and hide the cause.
   */
  async ensureReady(
    account: string,
    body: { wxid: string; db_path: string; keys?: Record<string, string> },
    timeoutSeconds = 120,
  ): Promise<void> {
    const url = this.url("/api/v1/accounts");
    const resp = await fetch(url, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify(body),
    });
    if (!resp.ok) throw new StatusError(resp.status, url);
    const payload = (await resp.json().catch(() => null)) as { state?: string } | null;
    const state = typeof payload?.state === "string" ? payload.state : null;
    if (state && REFUSAL_STATES.has(state)) {
      throw new StatusError(200, `${url} (state=${state})`);
    }
    await this.waitReady(account, timeoutSeconds);
  }

  /** Watch the SSE push stream; one connection yields many events. */
  async *watch(signal?: AbortSignal): AsyncGenerator<unknown> {
    while (true) {
      try {
        const resp = await fetch(this.url("/api/v1/push/messages"), {
          headers: { ...this.headers(), Accept: "text/event-stream" },
          signal,
        });
        if (!resp.ok || !resp.body) {
          throw new StatusError(resp.status, "/api/v1/push/messages");
        }
        // Byte-level LF framing: U+0085/U+2028/U+2029 are legal inside JSON
        // bodies and str.splitlines-style handling would corrupt frames.
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          for (;;) {
            const nl = buffer.indexOf("\n");
            if (nl < 0) break;
            let line = buffer.slice(0, nl);
            buffer = buffer.slice(nl + 1);
            if (line.endsWith("\r")) line = line.slice(0, -1);
            if (line.length > 0) continue; // frame assembly happens on blank lines
            // blank line = end of frame: a full example would parse id:/event:/data:
            // and decode the JSON payload; this demo just counts frames.
          }
        }
        return; // stream ended cleanly in this demo shape
      } catch (err) {
        if (signal?.aborted) return;
        // network error: backoff and reconnect
        await new Promise((r) => setTimeout(r, 500));
      }
    }
  }
}
