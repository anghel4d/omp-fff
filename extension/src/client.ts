import http from "node:http";
import os from "node:os";
import path from "node:path";

let agent: http.Agent | undefined;

export function runtimeDir(): string {
  if (process.env.FFF_ROUTER_RUNTIME_DIR) return process.env.FFF_ROUTER_RUNTIME_DIR;
  const base = process.env.XDG_RUNTIME_DIR ?? `/run/user/${process.getuid?.() ?? os.userInfo().uid}`;
  return path.join(base, "fff-router");
}

export function socketPath(): string {
  return path.join(runtimeDir(), "fff.sock");
}

export function healthFile(): string {
  return path.join(runtimeDir(), "health.json");
}

export function destroyAgent(): void {
  agent?.destroy();
  agent = undefined;
}

export async function requestText(
  method: string,
  route: string,
  params: URLSearchParams | Record<string, string | string[] | undefined>,
  signal?: AbortSignal,
): Promise<string> {
  const query = params instanceof URLSearchParams ? params : toParams(params);
  const pathname = query.size ? `${route}?${query}` : route;
  try {
    return await attempt(method, pathname, signal, method === "GET");
  } catch (error) {
    if (isConnectFailure(error)) {
      throw new Error(`fff-router unreachable at ${socketPath()} — is fff-router.service running? (fff-router-sync --check)`);
    }
    throw error;
  }
}

export async function requestJson<T>(
  route: string,
  params: URLSearchParams | Record<string, string | string[] | undefined>,
  signal?: AbortSignal,
): Promise<T> {
  return JSON.parse(await requestText("GET", route, params, signal)) as T;
}

async function attempt(method: string, pathname: string, signal: AbortSignal | undefined, mayRetry: boolean): Promise<string> {
  try {
    return await once(method, pathname, signal);
  } catch (error) {
    if (mayRetry && retryable(error)) {
      destroyAgent();
      return once(method, pathname, signal);
    }
    throw error;
  }
}

function once(method: string, pathname: string, signal?: AbortSignal): Promise<string> {
  if (signal?.aborted) return Promise.reject(abortError());
  agent ??= new http.Agent({ keepAlive: true, maxSockets: 4 });
  const { promise, resolve, reject } = Promise.withResolvers<string>();
  let received = 0;
  const req = http.request({
    method,
    socketPath: socketPath(),
    path: pathname,
    agent,
    signal,
    headers: { host: "fff" },
  }, (response) => {
    const chunks: Buffer[] = [];
    response.on("data", (chunk: Buffer) => {
      received += chunk.length;
      chunks.push(chunk);
    });
    response.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      if (response.statusCode !== 200) reject(new Error(body.trim()));
      else resolve(body.trimEnd());
    });
  });
  req.setTimeout(120_000, () => req.destroy(new Error("fff-router request timed out")));
  req.on("error", (error: NodeJS.ErrnoException) => {
    Object.assign(error, { bytesReceived: received });
    reject(error);
  });
  req.end();
  return promise;
}

function toParams(input: Record<string, string | string[] | undefined>): URLSearchParams {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(input)) {
    if (Array.isArray(value)) value.forEach((entry) => params.append(key, entry));
    else if (value !== undefined) params.set(key, value);
  }
  return params;
}

function retryable(error: unknown): boolean {
  const value = error as NodeJS.ErrnoException & { bytesReceived?: number };
  return value.bytesReceived === 0 && (value.code === "ECONNRESET" || value.code === "EPIPE");
}

function isConnectFailure(error: unknown): boolean {
  if (!error || typeof error !== "object" || !("code" in error)) return false;
  const code = error.code;
  return code === "ENOENT" || code === "ECONNREFUSED" || code === "ENOTSOCK" || code === "FailedToOpenSocket";
}

function abortError(): Error {
  const error = new Error("Operation aborted");
  error.name = "AbortError";
  return error;
}
