import path from "node:path";
import { Text } from "@oh-my-pi/pi-tui";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { requestText } from "./client.ts";

export function resolveDir(cwd: string, input?: string): string {
  if (!input) return cwd;
  if (path.isAbsolute(input)) return input;
  return path.resolve(cwd, input);
}

export function registerTools(pi: ExtensionAPI, getCwd: () => string): void {
  const z = pi.zod;
  const common = {
    path: z.string().optional().describe("Directory to search; workspace-relative or absolute."),
    context: z.number().int().min(0).max(20).optional(),
    limit: z.number().int().min(1).max(200).optional(),
    cursor: z.string().optional(),
  };

  pi.registerTool({
    name: "ffgrep",
    label: "FFF Grep",
    approval: "read",
    // Custom tools default to "discoverable", which xdev unmounts from the
    // top-level schema; APPEND_SYSTEM.md makes these first-choice, so they
    // must stay callable.
    loadMode: "essential",
    description: "Grep file contents via the resident fff index. Smart-case, frecency-ranked. Inline constraint prefixes: `*.rs term`, `src/ term`, `!test/ term`. Matches single lines — use ONE specific term.",
    parameters: z.object({
      pattern: z.string().min(1),
      ...common,
      caseSensitive: z.boolean().optional(),
    }),
    async execute(_id, args, signal) {
      if (signal?.aborted) throw new Error("Operation aborted");
      const mode = regexMode(args.pattern) ? "regex" : undefined;
      const text = await requestText("GET", "/grep", {
        path: resolveDir(getCwd(), args.path),
        pattern: args.pattern,
        context: numberString(args.context),
        limit: numberString(args.limit),
        cursor: args.cursor,
        case: args.caseSensitive === true ? "1" : undefined,
        mode,
      }, signal);
      return { content: [{ type: "text" as const, text }] };
    },
    renderCall(args, _options, theme) {
      return new Text(theme.fg("dim", `ffgrep /${args.pattern}/ in ${args.path ?? "."}`), 0, 0);
    },
  });

  pi.registerTool({
    name: "fffind",
    label: "FFF Find",
    approval: "read",
    loadMode: "essential",
    description: "Fuzzy path search, whole-path matching, frecency-ranked. Inline constraints: `*.ts query`, `src/ query`.",
    parameters: z.object({
      query: z.string().min(1),
      path: common.path,
      limit: common.limit,
      cursor: common.cursor,
    }),
    async execute(_id, args, signal) {
      if (signal?.aborted) throw new Error("Operation aborted");
      const text = await requestText("GET", "/find", {
        path: resolveDir(getCwd(), args.path),
        query: args.query,
        limit: numberString(args.limit),
        cursor: args.cursor,
      }, signal);
      return { content: [{ type: "text" as const, text }] };
    },
    renderCall(args, _options, theme) {
      return new Text(theme.fg("dim", `fffind ${args.query} in ${args.path ?? "."}`), 0, 0);
    },
  });

  pi.registerTool({
    name: "fff-multi-grep",
    label: "FFF Multi Grep",
    approval: "read",
    loadMode: "essential",
    description: "Search for ANY of several literal patterns (OR). Include snake_case/camelCase/PascalCase variants.",
    parameters: z.object({
      patterns: z.array(z.string().min(1)).min(1),
      constraints: z.string().optional(),
      ...common,
    }),
    async execute(_id, args, signal) {
      if (signal?.aborted) throw new Error("Operation aborted");
      const text = await requestText("GET", "/multigrep", {
        path: resolveDir(getCwd(), args.path),
        pattern: args.patterns,
        constraint: args.constraints,
        context: numberString(args.context),
        limit: numberString(args.limit),
        cursor: args.cursor,
      }, signal);
      return { content: [{ type: "text" as const, text }] };
    },
    renderCall(args, _options, theme) {
      return new Text(theme.fg("dim", `fff-multi-grep ${args.patterns.join(" | ")} in ${args.path ?? "."}`), 0, 0);
    },
  });
}

export function regexMode(pattern: string): boolean {
  if (escapeRegex(pattern) === pattern) return false;
  try {
    new RegExp(pattern);
    return true;
  } catch {
    return false;
  }
}

function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function numberString(value: number | undefined): string | undefined {
  return value === undefined ? undefined : String(value);
}
