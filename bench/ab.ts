import fs from "node:fs";
import path from "node:path";
import { z } from "../extension/node_modules/zod/index.js";
import ompFff from "../extension/src/index.ts";

const cwd = "/home/pyrus/nixos-config";
const socket = process.env.FFF_AB_SOCKET ?? `/run/user/${process.getuid()}/fff-router/fff.sock`;
const runs = Number(process.env.FFF_AB_RUNS ?? 40);
const warmups = Number(process.env.FFF_AB_WARMUPS ?? 5);
const piFffPath = "/home/pyrus/.omp/plugins/node_modules/@ff-labs/pi-fff/src/index.ts";

interface ToolDefinition {
  name: string;
  execute: (id: string, args: Record<string, unknown>, signal?: AbortSignal) => Promise<unknown>;
}

interface Summary {
  min_ms: number;
  median_ms: number;
  mean_ms: number;
  p95_ms: number;
  max_ms: number;
  median_ci95_low_ms: number;
  median_ci95_high_ms: number;
}

async function main(): Promise<void> {
  const extensionTool = await loadOmpFff();
  const piTool = fs.existsSync(piFffPath) ? await loadPiFff() : undefined;
  const variants: Array<[string, () => Promise<void>]> = [
    ["omp_fff_extension", async () => { await extensionTool.execute("bench", { pattern: "fffRouterSeedRoots", path: cwd }); }],
    ["bash_cli", async () => { checkedSpawn(["fff", "grep", cwd, "fffRouterSeedRoots"]); }],
    ["direct_curl", async () => { checkedSpawn(["curl", "-fsS", "--unix-socket", socket, "--get", "--data-urlencode", `path=${cwd}`, "--data-urlencode", "pattern=fffRouterSeedRoots", "http://fff/grep"]); }],
  ];
  if (piTool) variants.splice(1, 0, ["pi_fff", async () => { await piTool.execute("bench", { pattern: "fffRouterSeedRoots" }); }]);
  const samples = await measureVariants(variants, runs, warmups, 42);
  const summaries = Object.fromEntries(Object.entries(samples).map(([name, values]) => [name, summarize(values)]));
  const comparisons: Record<string, unknown> = {};
  if (samples.bash_cli) comparisons.omp_vs_bash = pairedComparison(samples.bash_cli, samples.omp_fff_extension, 43);
  if (samples.direct_curl) comparisons.omp_vs_curl = pairedComparison(samples.direct_curl, samples.omp_fff_extension, 44);
  if (samples.pi_fff) comparisons.omp_vs_pi_fff = pairedComparison(samples.pi_fff, samples.omp_fff_extension, 45);
  const result = { generatedAt: new Date().toISOString(), cwd, socket, runs, warmups, summaries, comparisons, samples };
  fs.writeFileSync(path.resolve("bench/results.json"), JSON.stringify(result, null, 2));
  console.log(JSON.stringify({ summaries, comparisons }, null, 2));
}

async function loadOmpFff(): Promise<ToolDefinition> {
  const tools: ToolDefinition[] = [];
  ompFff(fakePi(tools));
  const tool = tools.find((candidate) => candidate.name === "ffgrep");
  if (!tool) throw new Error("omp-fff ffgrep tool not registered");
  return tool;
}

async function loadPiFff(): Promise<ToolDefinition> {
  const tools: ToolDefinition[] = [];
  const module = await import(piFffPath);
  module.default(fakePi(tools, true));
  const events = eventHandlers;
  const start = events.find((event) => event.name === "session_start");
  if (start) await start.handler({}, { cwd, ui: { notify() {} }, sessionManager: { getEntries: () => [] } });
  const tool = tools.find((candidate) => candidate.name === "ffgrep") ?? tools.find((candidate) => candidate.name === "grep");
  if (!tool) throw new Error("pi-fff grep tool not registered");
  return tool;
}

const eventHandlers: Array<{ name: string; handler: (...args: unknown[]) => unknown }> = [];

function fakePi(tools: ToolDefinition[], piFff = false): never {
  const value = {
    zod: z,
    typebox: { Object: () => ({}), String: () => ({}), Optional: (v: unknown) => v, Number: () => ({}), Boolean: () => ({}), Array: () => ({}), Union: () => ({}) },
    getFlag: () => undefined,
    registerFlag() {},
    registerTool(tool: ToolDefinition) { tools.push(tool); },
    registerCommand() {},
    on(name: string, handler: (...args: unknown[]) => unknown) { eventHandlers.push({ name, handler }); },
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    pi: {},
    arktype: {},
    setLabel() {},
  };
  return value as never;
}

async function measureVariants(
  variants: Array<[string, () => Promise<void>]>,
  count: number,
  warmupCount: number,
  seed: number,
): Promise<Record<string, number[]>> {
  for (const [, call] of variants) for (let i = 0; i < warmupCount; i++) await call();
  const samples = Object.fromEntries(variants.map(([name]) => [name, [] as number[]]));
  const rng = random(seed);
  for (let index = 0; index < count; index++) {
    const shuffled = [...variants].sort(() => rng() - 0.5);
    for (const [name, call] of shuffled) {
      const start = Bun.nanoseconds();
      await call();
      samples[name].push((Bun.nanoseconds() - start) / 1_000_000);
    }
  }
  return samples;
}

function summarize(samples: number[]): Summary {
  const ordered = [...samples].sort((a, b) => a - b);
  const [low, high] = medianCi(ordered, 0);
  return {
    min_ms: ordered[0],
    median_ms: median(ordered),
    mean_ms: ordered.reduce((a, b) => a + b, 0) / ordered.length,
    p95_ms: ordered[Math.ceil(0.95 * ordered.length) - 1],
    max_ms: ordered.at(-1) ?? 0,
    median_ci95_low_ms: low,
    median_ci95_high_ms: high,
  };
}

function medianCi(samples: number[], seed: number, resamples = 10_000): [number, number] {
  const rng = random(seed);
  const medians = Array.from({ length: resamples }, () => median(Array.from({ length: samples.length }, () => samples[Math.floor(rng() * samples.length)]))).sort((a, b) => a - b);
  return [medians[Math.floor(0.025 * resamples)], medians[Math.floor(0.975 * resamples)]];
}

function pairedComparison(oldSamples: number[], newSamples: number[], seed: number, resamples = 10_000): Record<string, number> {
  const rng = random(seed);
  const reductions = Array.from({ length: resamples }, () => {
    const indices = Array.from({ length: oldSamples.length }, () => Math.floor(rng() * oldSamples.length));
    const oldMedian = median(indices.map((index) => oldSamples[index]));
    const newMedian = median(indices.map((index) => newSamples[index]));
    return (1 - newMedian / oldMedian) * 100;
  }).sort((a, b) => a - b);
  return {
    median_reduction_pct: (1 - median(newSamples) / median(oldSamples)) * 100,
    reduction_ci95_low_pct: reductions[Math.floor(0.025 * resamples)],
    reduction_ci95_high_pct: reductions[Math.floor(0.975 * resamples)],
    speedup: median(oldSamples) / median(newSamples),
  };
}

function median(values: number[]): number {
  const ordered = [...values].sort((a, b) => a - b);
  const middle = Math.floor(ordered.length / 2);
  return ordered.length % 2 ? ordered[middle] : (ordered[middle - 1] + ordered[middle]) / 2;
}

function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state * 1_664_525 + 1_013_904_223) >>> 0;
    return state / 2 ** 32;
  };
}

function checkedSpawn(command: string[]): void {
  const result = Bun.spawnSync(command, { cwd });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString() || `${command[0]} failed`);
}

await main();
