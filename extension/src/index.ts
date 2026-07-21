import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { destroyAgent } from "./client.ts";
import { registerCommands } from "./commands.ts";
import { registerCompletion } from "./complete.ts";
import { registerTools } from "./tools.ts";

export default function ompFff(pi: ExtensionAPI): void {
  let activeCwd = process.cwd();
  const getCwd = () => activeCwd;

  registerTools(pi, getCwd);
  registerCommands(pi, getCwd);
  registerCompletion(pi, getCwd);

  pi.on("session_start", async (_event, ctx) => {
    activeCwd = ctx.cwd;
  });
  pi.on("session_switch", async (_event, ctx) => {
    activeCwd = ctx.cwd;
  });
  pi.on("session_shutdown", async () => {
    destroyAgent();
  });
}
