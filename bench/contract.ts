import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = "/tmp/fff-contract";
const fixture = path.join(root, "fixture");
const oldHome = path.join(root, "a");
const newHome = path.join(root, "b");
const oldRouter = path.join(os.homedir(), ".local/share/fff-router/index.ts");
const newRouter = path.resolve("target/release/fff-routerd");

interface Case {
  name: string;
  method: "GET" | "POST";
  route: string;
  params: Array<[string, string]>;
  compare?: boolean;
}

const cases: Case[] = [
  { name: "find", method: "GET", route: "/find", params: [["path", fixture], ["query", "alpha"]] },
  { name: "find-space", method: "GET", route: "/find", params: [["path", fixture], ["query", "space"]] },
  { name: "grep", method: "GET", route: "/grep", params: [["path", fixture], ["pattern", "needle"]] },
  { name: "grep-context", method: "GET", route: "/grep", params: [["path", fixture], ["pattern", "needle"], ["context", "1"]] },
  { name: "grep-long", method: "GET", route: "/grep", params: [["path", fixture], ["pattern", "LONGMARK"]] },
  { name: "multigrep", method: "GET", route: "/multigrep", params: [["path", fixture], ["pattern", "needle"], ["pattern", "second"]] },
  { name: "invalid-limit", method: "GET", route: "/grep", params: [["path", fixture], ["pattern", "needle"], ["limit", "0"]] },
  { name: "invalid-pattern", method: "GET", route: "/grep", params: [["path", fixture], ["pattern", ".*"]] },
  { name: "unindexed", method: "GET", route: "/find", params: [["path", "/tmp"], ["query", "x"]] },
  { name: "constraint-new", method: "GET", route: "/multigrep", params: [["path", fixture], ["pattern", "needle"], ["constraint", "*.txt"]], compare: false },
];

async function main(): Promise<void> {
  prepare();
  if (!fs.existsSync(oldRouter)) throw new Error(`old router missing: ${oldRouter}`);
  if (!fs.existsSync(newRouter)) throw new Error(`new daemon missing: ${newRouter}; run cargo build --release`);
  const oldRun = path.join(oldHome, "run");
  const newRun = path.join(newHome, "run");
  const oldProcess = Bun.spawn([process.execPath, "run", oldRouter], { env: environment(oldHome, oldRun), stdout: "pipe", stderr: "pipe" });
  const newProcess = Bun.spawn([newRouter], { env: environment(newHome, newRun), stdout: "pipe", stderr: "pipe" });
  try {
    await Promise.all([waitReady(path.join(oldRun, "fff.sock")), waitReady(path.join(newRun, "fff.sock"))]);
    for (const item of cases) {
      const freshOld = item.compare === false ? undefined : await call(path.join(oldRun, "fff.sock"), item);
      const freshNew = await call(path.join(newRun, "fff.sock"), item);
      if (freshOld && normalize(freshOld.body) !== normalize(freshNew.body)) {
        throw new Error(`${item.name} mismatch\nOLD ${freshOld.status}:\n${freshOld.body}\nNEW ${freshNew.status}:\n${freshNew.body}`);
      }
      if (freshOld && freshOld.status !== freshNew.status) {
        throw new Error(`${item.name} status mismatch: ${freshOld.status} != ${freshNew.status}`);
      }
      if (item.compare === false && freshNew.status !== 200) {
        throw new Error(`${item.name} new-only case failed: ${freshNew.status} ${freshNew.body}`);
      }
      console.log(`ok ${item.name}`);
    }
  } finally {
    oldProcess.kill("SIGTERM");
    newProcess.kill("SIGTERM");
    await Promise.allSettled([oldProcess.exited, newProcess.exited]);
  }
}

function prepare(): void {
  fs.rmSync(root, { recursive: true, force: true });
  fs.mkdirSync(path.join(fixture, "nested"), { recursive: true });
  fs.writeFileSync(path.join(fixture, "alpha.txt"), "before\nneedle\nafter\nsecond\n");
  fs.writeFileSync(path.join(fixture, "name with spaces.md"), "space needle\n");
  fs.writeFileSync(path.join(fixture, "nested", "long.txt"), `LONGMARK ${"x".repeat(600)}\n`);
  for (const home of [oldHome, newHome]) {
    fs.mkdirSync(path.join(home, ".config/fff-router"), { recursive: true });
    fs.mkdirSync(path.join(home, ".local/state/fff-router"), { recursive: true });
    fs.writeFileSync(path.join(home, ".config/fff-router/roots.json"), JSON.stringify([
      { id: "repo", path: "/home/pyrus/nixos-config" },
      { id: "fixture", path: fixture },
    ]));
  }
}

function environment(home: string, runtime: string): Record<string, string> {
  fs.mkdirSync(runtime, { recursive: true });
  return { ...process.env, HOME: home, FFF_ROUTER_RUNTIME_DIR: runtime, FFF_ROUTER_CONFIG: path.join(home, ".config/fff-router/roots.json"), FFF_ROUTER_STATE_DIR: path.join(home, ".local/state/fff-router") } as Record<string, string>;
}

async function waitReady(socket: string): Promise<void> {
  for (let attempt = 0; attempt < 180; attempt++) {
    try {
      const response = await call(socket, { name: "health", method: "GET", route: "/healthz", params: [] });
      const health: unknown = JSON.parse(response.body);
      if (Array.isArray(health) && health.some((row) => isReadyFixture(row))) return;
    } catch {}
    await Bun.sleep(1000);
  }
  throw new Error(`router did not become ready: ${socket}`);
}

async function call(socket: string, item: Case): Promise<{ status: number; body: string }> {
  const query = new URLSearchParams(item.params);
  const route = query.size ? `${item.route}?${query}` : item.route;
  const result = Bun.spawnSync(["curl", "-sS", "--unix-socket", socket, "-X", item.method, "-w", "\n%{http_code}", `http://fff${route}`]);
  const output = result.stdout.toString();
  const split = output.lastIndexOf("\n");
  return { status: Number(output.slice(split + 1)), body: output.slice(0, split) };
}

function normalize(value: string): string {
  return value.trimEnd().replace(/cursor="\d+"/g, 'cursor="<token>"').replace(/\d{4}-\d{2}-\d{2}T[^"\n]+/g, "<timestamp>");
}

function isReadyFixture(value: unknown): boolean {
  return Boolean(value && typeof value === "object" && "id" in value && value.id === "fixture" && "state" in value && value.state === "ready");
}

await main();
