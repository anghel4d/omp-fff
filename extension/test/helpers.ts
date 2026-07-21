import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { z } from "zod";

export interface RecordedPi {
  pi: ExtensionAPI;
  tools: Array<Record<string, unknown>>;
  commands: Array<{ name: string; definition: Record<string, unknown> }>;
  events: Array<{ name: string; handler: (...args: unknown[]) => unknown }>;
}

export function recordedPi(): RecordedPi {
  const tools: Array<Record<string, unknown>> = [];
  const commands: Array<{ name: string; definition: Record<string, unknown> }> = [];
  const events: Array<{ name: string; handler: (...args: unknown[]) => unknown }> = [];
  const value: Record<string, unknown> = {
    zod: z,
    registerTool(definition: Record<string, unknown>) { tools.push(definition); },
    registerCommand(name: string, definition: Record<string, unknown>) { commands.push({ name, definition }); },
    on(name: string, handler: (...args: unknown[]) => unknown) { events.push({ name, handler }); },
  };
  return { pi: value as unknown as ExtensionAPI, tools, commands, events };
}
