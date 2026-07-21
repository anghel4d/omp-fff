import fs from "node:fs";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { healthFile, requestText } from "./client.ts";

export interface HealthRow {
  id: string;
  path: string;
  files: number;
  state: "starting" | "scanning" | "ready" | "unhealthy";
  lastScanAt: string | null;
  error: string | null;
}

export function readSnapshot(): HealthRow[] | null {
  try {
    const value: unknown = JSON.parse(fs.readFileSync(healthFile(), "utf8"));
    if (!Array.isArray(value)) return null;
    return value.filter(isHealthRow);
  } catch {
    return null;
  }
}

export function registerCommands(pi: ExtensionAPI, getCwd: () => string): void {
  pi.registerCommand("fff-health", {
    description: "Show resident FFF router health",
    handler: async (_args, ctx) => {
      const rows = readSnapshot();
      if (!rows) {
        ctx.ui.notify("no health snapshot — fff-router has not started (fff-router-sync --sync)", "warning");
        return;
      }
      const unhealthy = rows.filter((row) => row.state === "unhealthy").length;
      const age = snapshotAgeSeconds();
      const details = rows
        .filter((row) => row.state !== "ready")
        .map((row) => `${row.id}: ${row.state}${row.error ? ` (${row.error})` : ""}`);
      const message = [`fff-router: ${rows.length} roots, ${unhealthy} unhealthy, snapshot ${age}s old`, ...details].join("\n");
      ctx.ui.notify(message, age > 60 || unhealthy > 0 ? "warning" : "info");
    },
  });

  pi.registerCommand("fff-atlas", {
    description: "Show indexed FFF roots",
    handler: async (_args, ctx) => {
      const rows = readSnapshot();
      if (!rows) {
        ctx.ui.notify("no health snapshot — fff-router has not started (fff-router-sync --sync)", "warning");
        return;
      }
      const lines = ["id | path | files | state | lastScan"];
      for (const row of rows) lines.push(`${row.id} | ${row.path} | ${row.files} | ${row.state} | ${row.lastScanAt ?? "-"}`);
      ctx.ui.notify(lines.join("\n"), "info");
    },
  });

  pi.registerCommand("fff-rescan", {
    description: "Rescan the current indexed root, or all roots with /fff-rescan all",
    handler: async (args, ctx) => {
      try {
        const path = args.trim() === "all" ? undefined : getCwd();
        const text = await requestText("POST", "/rescan", { path });
        ctx.ui.notify(text, "info");
      } catch (error) {
        ctx.ui.notify(error instanceof Error ? error.message : String(error), "error");
      }
    },
  });
}

function snapshotAgeSeconds(): number {
  try {
    return Math.max(0, Math.floor(Date.now() / 1000 - fs.statSync(healthFile()).mtimeMs / 1000));
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

function isHealthRow(value: unknown): value is HealthRow {
  if (!value || typeof value !== "object") return false;
  return "id" in value && typeof value.id === "string"
    && "path" in value && typeof value.path === "string"
    && "files" in value && typeof value.files === "number"
    && "state" in value && typeof value.state === "string";
}
