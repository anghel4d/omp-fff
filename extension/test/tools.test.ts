import { afterEach, expect, test } from "bun:test";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import ompFff from "../src/index.ts";
import { destroyAgent } from "../src/client.ts";
import { regexMode, resolveDir } from "../src/tools.ts";
import { recordedPi } from "./helpers.ts";

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

test("registers three tools, three commands, and lifecycle events", () => {
  const recorder = recordedPi();
  ompFff(recorder.pi);
  expect(recorder.tools.map((tool) => tool.name)).toEqual(["ffgrep", "fffind", "fff-multi-grep"]);
  expect(recorder.commands.map((command) => command.name)).toEqual(["fff-health", "fff-atlas", "fff-rescan"]);
  expect(recorder.events.map((event) => event.name)).toContain("session_start");
  expect(recorder.events.map((event) => event.name)).toContain("session_shutdown");
});

test("resolves relative and absolute directories", () => {
  expect(resolveDir("/work", undefined)).toBe("/work");
  expect(resolveDir("/work", "src")).toBe("/work/src");
  expect(resolveDir("/work", "/tmp")).toBe("/tmp");
});

test("regex detection requires valid metacharacters", () => {
  expect(regexMode("needle")).toBe(false);
  expect(regexMode("needle.*here")).toBe(true);
  expect(regexMode("[")).toBe(false);
});

test("maps grep and multigrep arguments to the wire", async () => {
  const paths: string[] = [];
  serve((request) => {
    paths.push(new URL(request.url).pathname + new URL(request.url).search);
    return new Response("ok\n");
  });
  const recorder = recordedPi();
  ompFff(recorder.pi);
  const grep = executable(recorder.tools.find((tool) => tool.name === "ffgrep"));
  await grep({ pattern: "a.*b", path: "/repo", context: 2, limit: 9, caseSensitive: true });
  const multi = executable(recorder.tools.find((tool) => tool.name === "fff-multi-grep"));
  await multi({ patterns: ["One", "one"], constraints: "*.ts", path: "/repo" });
  expect(paths[0]).toContain("/grep?");
  expect(paths[0]).toContain("pattern=a.*b");
  expect(paths[0]).toContain("mode=regex");
  expect(paths[0]).toContain("case=1");
  expect(paths[1]).toContain("pattern=One");
  expect(paths[1]).toContain("pattern=one");
  expect(paths[1]).toContain("constraint=*.ts");
});

function executable(tool: Record<string, unknown> | undefined): (args: Record<string, unknown>) => Promise<unknown> {
  if (!tool || typeof tool.execute !== "function") throw new Error("tool execute missing");
  const execute = tool.execute;
  return (args) => execute("id", args, undefined, undefined, { cwd: "/work" });
}

function serve(fetch: (request: Request) => Response): void {
  temporary = fs.mkdtempSync(path.join(os.tmpdir(), "omp-fff-tools-"));
  process.env.FFF_ROUTER_RUNTIME_DIR = temporary;
  server = Bun.serve({ unix: path.join(temporary, "fff.sock"), fetch });
}
