import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

const REWRITE_TIMEOUT_MS = 2_000

type CommandBinding = {
  command: string
  replace: (command: string) => void
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

/** Find the command field used by Pi's built-in shell tools. */
export function commandBinding(toolName: string, input: unknown): CommandBinding | null {
  if (!isRecord(input)) return null

  const key = toolName === "bash" ? "command" : toolName === "exec_command" ? "cmd" : null
  if (key === null) return null

  const command = input[key]
  if (typeof command !== "string" || command.trim() === "") return null

  return {
    command,
    replace(rewritten: string) {
      input[key] = rewritten
    },
  }
}

async function rewriteCommand(
  pi: ExtensionAPI,
  command: string,
  signal?: AbortSignal,
): Promise<string | null> {
  const options = signal === undefined
    ? { timeout: REWRITE_TIMEOUT_MS }
    : { timeout: REWRITE_TIMEOUT_MS, signal }
  const result = await pi.exec("yarp", ["rewrite", command], options)
  if (result.killed || result.code !== 0) return null
  return result.stdout.trim() || null
}

export default async function yarpExtension(pi: ExtensionAPI): Promise<void> {
  const version = await pi.exec("yarp", ["--version"], { timeout: REWRITE_TIMEOUT_MS })
  if (version.code !== 0) {
    console.warn("[yarp] binary not found in PATH; extension disabled")
    return
  }

  pi.on("tool_call", async (event, context) => {
    try {
      if (process.env.YARP_DISABLED === "1") return

      const binding = commandBinding(event.toolName, event.input)
      if (binding === null || binding.command.startsWith("yarp ")) return

      const rewritten = await rewriteCommand(pi, binding.command, context.signal)
      if (rewritten !== null && rewritten !== binding.command) binding.replace(rewritten)
    } catch {
      console.warn("[yarp] rewrite failed; running the original command")
    }
  })
}
