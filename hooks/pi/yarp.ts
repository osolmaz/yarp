import { userInfo } from "node:os"
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolCallEvent,
  ToolExecutionEndEvent,
  ToolResultEvent,
} from "@earendil-works/pi-coding-agent"
import {
  ArchiveClient,
  type ArchiveCall,
  type ArchiveSession,
  type ArchiveSink,
} from "./archive-client.js"

const REWRITE_TIMEOUT_MS = 2_000

type CommandBinding = {
  command: string
  replace: (command: string) => void
}

type ArchiveSinkFactory = () => ArchiveSink

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
  session: ArchiveSession | null,
  toolCallId: string,
  signal?: AbortSignal,
): Promise<string | null> {
  const args = session === null
    ? ["rewrite", command]
    : [
        "rewrite",
        "--archive-agent",
        session.agent,
        "--archive-account",
        session.account,
        "--archive-session",
        session.sourceSessionId,
        "--archive-call",
        toolCallId,
        command,
      ]
  const options = signal === undefined
    ? { timeout: REWRITE_TIMEOUT_MS }
    : { timeout: REWRITE_TIMEOUT_MS, signal }
  const result = await pi.exec("yarp", args, options)
  if (result.killed || result.code !== 0) return null
  return result.stdout.trim() || null
}

export async function installYarpExtension(
  pi: ExtensionAPI,
  createSink: ArchiveSinkFactory = () => new ArchiveClient(),
): Promise<void> {
  const version = await pi.exec("yarp", ["--version"], { timeout: REWRITE_TIMEOUT_MS })
  if (version.code !== 0) {
    console.warn("[yarp] binary not found in PATH; extension disabled")
    return
  }

  let sink: ArchiveSink | null = null
  let session: ArchiveSession | null = null
  const activeCallIds = new Set<string>()

  pi.on("session_start", async (_event, context) => {
    if (archiveDisabled()) return
    await sink?.close()
    sink = createSink()
    session = sessionIdentity(context)
    activeCallIds.clear()
  })

  pi.on("session_shutdown", async () => {
    const current = sink
    sink = null
    session = null
    activeCallIds.clear()
    await current?.close()
  })

  pi.on("tool_call", async (event, context) => {
    const archive = archiveDisabled() ? null : requireArchive(sink, session, context)
    const inputBefore = structuredClone(event.input)
    const binding = commandBinding(event.toolName, event.input)
    let rewritten: string | null = null

    if (
      process.env["YARP_DISABLED"] !== "1"
      && binding !== null
      && !binding.command.startsWith("yarp ")
    ) {
      try {
        rewritten = await rewriteCommand(
          pi,
          binding.command,
          archive?.session ?? null,
          event.toolCallId,
          context.signal,
        )
      } catch {
        console.warn("[yarp] rewrite failed; running the original command")
      }
    }

    const inputAfter = structuredClone(event.input)
    if (rewritten !== null && binding !== null && rewritten !== binding.command) {
      commandBinding(event.toolName, inputAfter)?.replace(rewritten)
    }

    if (archive !== null) {
      await archive.sink.beginCall(
        archive.session,
        callIdentity(event, context),
        inputBefore,
        inputAfter,
        Date.now(),
      )
      activeCallIds.add(event.toolCallId)
    }

    if (rewritten !== null && binding !== null && rewritten !== binding.command) {
      binding.replace(rewritten)
    }
  })

  pi.on("tool_result", async (event) => {
    if (sink === null || session === null || !activeCallIds.has(event.toolCallId)) return
    await sink.resultBefore(session, event.toolCallId, resultSnapshot(event), Date.now())
  })

  pi.on("tool_execution_end", async (event) => {
    if (sink === null || session === null || !activeCallIds.has(event.toolCallId)) return
    await sink.finishCall(
      session,
      event.toolCallId,
      executionEndSnapshot(event),
      event.isError,
      Date.now(),
    )
    activeCallIds.delete(event.toolCallId)
  })
}

export default async function yarpExtension(pi: ExtensionAPI): Promise<void> {
  await installYarpExtension(pi)
}

function requireArchive(
  sink: ArchiveSink | null,
  session: ArchiveSession | null,
  context: ExtensionContext,
): { sink: ArchiveSink; session: ArchiveSession } {
  if (sink === null || session === null) {
    throw new Error(
      `YARP archive is required but was not initialized for session ${context.sessionManager.getSessionId()}`,
    )
  }
  return { sink, session }
}

function sessionIdentity(context: ExtensionContext): ArchiveSession {
  return {
    agent: "pi",
    account: localAccount(),
    sourceSessionId: context.sessionManager.getSessionId(),
    startedAtMs: Date.now(),
  }
}

function callIdentity(event: ToolCallEvent, context: ExtensionContext): ArchiveCall {
  const call: ArchiveCall = {
    sourceCallId: event.toolCallId,
    toolName: event.toolName,
    workingDirectory: context.cwd,
    startedAtMs: Date.now(),
  }
  if (context.model !== undefined) {
    call.provider = context.model.provider
    call.model = context.model.id
  }
  return call
}

function resultSnapshot(event: ToolResultEvent): Record<string, unknown> {
  return {
    content: event.content,
    details: event.details ?? null,
    isError: event.isError,
    usage: event.usage ?? null,
  }
}

function executionEndSnapshot(event: ToolExecutionEndEvent): Record<string, unknown> {
  if (!isRecord(event.result)) return { result: event.result, isError: event.isError }
  return {
    content: event.result.content ?? null,
    details: event.result.details ?? null,
    isError: event.isError,
    usage: event.result.usage ?? null,
    terminate: event.result.terminate ?? null,
  }
}

function archiveDisabled(): boolean {
  return process.env["YARP_ARCHIVE_DISABLED"] === "1"
}

function localAccount(): string {
  try {
    return userInfo().username || "unknown"
  } catch {
    return process.env["USER"] ?? process.env["USERNAME"] ?? "unknown"
  }
}
