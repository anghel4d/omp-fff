import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import type { AutocompleteItem, AutocompleteProvider } from "@oh-my-pi/pi-tui";
import { requestJson } from "./client.ts";

interface CompletionResponse {
  items: Array<{ type: "file" | "directory"; relativePath: string; name: string }>;
}

export function registerCompletion(pi: ExtensionAPI, getCwd: () => string): void {
  pi.on("session_start", async (_event, ctx) => {
    if (typeof ctx.ui.addAutocompleteProvider !== "function") return;
    ctx.ui.addAutocompleteProvider((current) => wrapProvider(current, getCwd));
  });
}

function wrapProvider(current: AutocompleteProvider, getCwd: () => string): AutocompleteProvider {
  return {
    async getSuggestions(lines, cursorLine, cursorCol) {
      const before = lines[cursorLine]?.slice(0, cursorCol) ?? "";
      const match = before.match(/(?:^|[ \t])(@(?:"[^"]*|[^\s]*))$/);
      if (!match) return current.getSuggestions(lines, cursorLine, cursorCol);
      const prefix = match[1];
      const query = prefix.slice(1).replace(/^"/, "").replace(/"$/, "");
      try {
        const response = await requestJson<CompletionResponse>("/complete", {
          path: getCwd(),
          query,
          limit: "20",
        });
        const items = response.items.map((item) => ({
          value: item.relativePath.includes(" ") ? `@"${item.relativePath}"` : `@${item.relativePath}`,
          label: item.name,
          description: item.relativePath,
        }));
        return items.length ? { items, prefix } : null;
      } catch {
        return current.getSuggestions(lines, cursorLine, cursorCol);
      }
    },
    applyCompletion(lines, cursorLine, cursorCol, item, prefix) {
      // Only @-mention completions are ours; everything else (slash commands,
      // paths) must go through the wrapped provider — native slash items carry
      // `value` without the leading slash and rely on its insert logic.
      if (!prefix.startsWith("@")) return current.applyCompletion(lines, cursorLine, cursorCol, item, prefix);
      return applyMention(lines, cursorLine, cursorCol, item, prefix);
    },
    getInlineHint: current.getInlineHint?.bind(current),
    trySyncSlashCompletion: current.trySyncSlashCompletion?.bind(current),
    trySyncInlineReplace: current.trySyncInlineReplace?.bind(current),
    getForceFileSuggestions: current.getForceFileSuggestions?.bind(current),
    shouldTriggerFileCompletion: current.shouldTriggerFileCompletion?.bind(current),
  };
}

function applyMention(
  lines: string[],
  cursorLine: number,
  cursorCol: number,
  item: AutocompleteItem,
  prefix: string,
): { lines: string[]; cursorLine: number; cursorCol: number } {
  const output = [...lines];
  const line = output[cursorLine] ?? "";
  const start = Math.max(0, cursorCol - prefix.length);
  output[cursorLine] = `${line.slice(0, start)}${item.value}${line.slice(cursorCol)}`;
  return { lines: output, cursorLine, cursorCol: start + item.value.length };
}
