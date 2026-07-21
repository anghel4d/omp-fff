import { afterEach, expect, test } from "bun:test";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { destroyAgent, requestJson, requestText } from "../src/client.ts";

let server: Bun.Server<undefined> | undefined;
let temporary: string | undefined;

afterEach(() => {
  server?.stop(true);
  server = undefined;
  destroyAgent();
  if (temporary) fs.rmSync(temporary, { recursive: true, force: true });
  temporary = undefined;
  delete process.env.FFF_ROUTER_RUNTIME_DIR;
});

test("trims successful text and parses JSON", async () => {
  const socket = serve(() => new Response('{"ok":true}\n'));
  expect(await requestText("GET", "/x", {})).toBe('{"ok":true}');
  expect(await requestJson<{ ok: boolean }>("/x", {})).toEqual({ ok: true });
  expect(fs.existsSync(socket)).toBe(true);
});

test("propagates non-200 daemon body", async () => {
  serve(() => new Response("INVALID_PARAM: bad\n", { status: 400 }));
  await expect(requestText("GET", "/x", {})).rejects.toThrow("INVALID_PARAM: bad");
});

test("reports unreachable router", async () => {
  temporary = fs.mkdtempSync(path.join(os.tmpdir(), "omp-fff-client-"));
  process.env.FFF_ROUTER_RUNTIME_DIR = temporary;
  await expect(requestText("GET", "/x", {})).rejects.toThrow(`fff-router unreachable at ${path.join(temporary, "fff.sock")}`);
});

test("aborted request fails before I/O", async () => {
  const controller = new AbortController();
  controller.abort();
  await expect(requestText("GET", "/x", {}, controller.signal)).rejects.toThrow("Operation aborted");
});

function serve(fetch: (request: Request) => Response): string {
  temporary = fs.mkdtempSync(path.join(os.tmpdir(), "omp-fff-client-"));
  process.env.FFF_ROUTER_RUNTIME_DIR = temporary;
  const socket = path.join(temporary, "fff.sock");
  server = Bun.serve({ unix: socket, fetch });
  return socket;
}
