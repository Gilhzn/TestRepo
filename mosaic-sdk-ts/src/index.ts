/**
 * Mosaic SDK for TypeScript / Node.js.
 *
 * Talks to a running `mosaic-serve` instance over HTTP. Designed for AI
 * agents and tooling written in JavaScript / TypeScript. The Rust SDK
 * (`mosaic-sdk` crate) is the in-process equivalent.
 */

// Use the runtime's global fetch (Node 18+).
declare const fetch: (input: string, init?: any) => Promise<any>;
declare const WebSocket: any;

export interface HealthResponse {
  ok: boolean;
  repo: string;
  changes: number;
  branches: number;
}

export interface BranchSummary {
  name: string;
  tips: string[];
}

export interface ChangeSummary {
  id: string;
  author: string;
  intent: string | null;
  timestamp: number;
  deps: string[];
  file_count: number;
}

export interface ChangeDetail {
  id: string;
  author: string;
  intent: string | null;
  timestamp: number;
  deps: string[];
  files: { path: string; kind: string; size: number }[];
}

export interface ApplyReport {
  applied: string[];
  skipped: string[];
}

export class MosaicError extends Error {
  status?: number;
  constructor(message: string, status?: number) {
    super(message);
    this.name = "MosaicError";
    this.status = status;
  }
}

export class MosaicClient {
  private base: string;

  constructor(baseUrl: string) {
    this.base = baseUrl.replace(/\/$/, "");
  }

  baseUrl(): string {
    return this.base;
  }

  async health(): Promise<HealthResponse> {
    return this.getJson<HealthResponse>("/api/v1/health");
  }

  async branches(): Promise<BranchSummary[]> {
    return this.getJson<BranchSummary[]>("/api/v1/branches");
  }

  async branch(name: string): Promise<BranchSummary> {
    return this.getJson<BranchSummary>(
      `/api/v1/branches/${encodeURIComponent(name)}`,
    );
  }

  async changes(): Promise<ChangeSummary[]> {
    return this.getJson<ChangeSummary[]>("/api/v1/changes");
  }

  async change(id: string): Promise<ChangeDetail> {
    return this.getJson<ChangeDetail>(
      `/api/v1/changes/${encodeURIComponent(id)}`,
    );
  }

  /**
   * Fetch the bundle of changes the server has on `branch` that the
   * caller (identified by `have`) is missing.
   */
  async pullBundle(branch: string, have: string[] = []): Promise<Uint8Array> {
    const params = new URLSearchParams({
      branch,
      have: have.join(","),
    });
    const res = await fetch(`${this.base}/api/v1/missing?${params}`);
    if (!res.ok) {
      throw new MosaicError(
        `pullBundle failed: ${res.status} ${res.statusText}`,
        res.status,
      );
    }
    return new Uint8Array(await res.arrayBuffer());
  }

  /**
   * Push a bundle of changes (raw bytes, bincode-encoded with
   * MOSAICB1 magic) into the server.
   */
  async pushBundle(bundle: Uint8Array): Promise<ApplyReport> {
    const res = await fetch(`${this.base}/api/v1/bundle`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: bundle,
    });
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      throw new MosaicError(
        `pushBundle failed: ${res.status} ${res.statusText} ${text}`,
        res.status,
      );
    }
    return (await res.json()) as ApplyReport;
  }

  /**
   * Poll until the server reports the given change id is present, or
   * timeout. Useful for agents waiting on a collaborator's push.
   */
  async waitForChange(
    id: string,
    opts: { timeoutMs?: number; pollMs?: number } = {},
  ): Promise<void> {
    const timeoutMs = opts.timeoutMs ?? 30_000;
    const pollMs = opts.pollMs ?? 500;
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      const list = await this.changes();
      if (list.some((c) => c.id === id)) return;
      await new Promise((r) => setTimeout(r, pollMs));
    }
    throw new MosaicError(`timeout waiting for change ${id.slice(0, 12)}`);
  }

  private async getJson<T>(path: string): Promise<T> {
    const res = await fetch(`${this.base}${path}`);
    if (!res.ok) {
      throw new MosaicError(
        `GET ${path} failed: ${res.status} ${res.statusText}`,
        res.status,
      );
    }
    return (await res.json()) as T;
  }
}

/**
 * Real-time co-editing session over WebSocket.
 *
 * Each connected peer in the same document name receives every binary
 * update from every other peer. The server doesn't parse the bytes —
 * they're typically Yjs (or any other CRDT) update payloads.
 *
 * @example
 *   const session = await LiveSession.connect("ws://server:7700/ws/doc/payments.rs");
 *   session.onUpdate((bytes) => applyToLocalCrdt(bytes));
 *   session.send(localUpdate);
 *   ...
 *   await session.close();
 */
export class LiveSession {
  private socket: any;
  private listeners: Array<(bytes: Uint8Array) => void> = [];
  private closed = false;

  static async connect(url: string): Promise<LiveSession> {
    const session = new LiveSession();
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      ws.onopen = () => {
        session.socket = ws;
        ws.onmessage = (ev: any) => {
          if (ev.data instanceof ArrayBuffer) {
            const bytes = new Uint8Array(ev.data);
            for (const fn of session.listeners) fn(bytes);
          }
        };
        ws.onclose = () => {
          session.closed = true;
        };
        ws.onerror = () => {
          session.closed = true;
        };
        resolve(session);
      };
      ws.onerror = (err: any) => reject(new MosaicError(`ws connect failed: ${err}`));
    });
  }

  onUpdate(fn: (bytes: Uint8Array) => void): void {
    this.listeners.push(fn);
  }

  send(update: Uint8Array): void {
    if (this.closed) throw new MosaicError("session closed");
    this.socket.send(update);
  }

  isClosed(): boolean {
    return this.closed;
  }

  async close(): Promise<void> {
    if (!this.closed && this.socket) {
      this.socket.close();
      this.closed = true;
    }
  }
}

/**
 * Read-only convenience wrapper over a branch. An agent uses this to
 * inspect remote state before deciding what to push.
 */
export class BranchView {
  constructor(
    private client: MosaicClient,
    public readonly name: string,
  ) {}

  async tips(): Promise<string[]> {
    const summary = await this.client.branch(this.name).catch((e) => {
      if (e instanceof MosaicError && e.status === 404) {
        return { name: this.name, tips: [] };
      }
      throw e;
    });
    return summary.tips;
  }

  async pull(have: string[] = []): Promise<Uint8Array> {
    return this.client.pullBundle(this.name, have);
  }
}
