import { userInfo } from "node:os"
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolCallEvent,
  ToolExecutionEndEvent,
  ToolExecutionStartEvent,
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

type PendingCall = {
  call: ArchiveCall
  input: unknown
  capturedAtMs: number
}

type ResultPatch = {
  content?: ToolResultEvent["content"]
  details?: unknown
  isError?: boolean
  usage?: NonNullable<ToolResultEvent["usage"]>
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
  const activeCalls = new Map<string, { requiresStreams: boolean; staged: boolean }>()
  const pendingCalls = new Map<string, PendingCall>()
  const restoredFinalResults = new Map<string, ResultPatch>()

  pi.on("session_start", async (_event, context) => {
    if (archiveDisabled()) return
    await sink?.close()
    sink = createSink()
    session = sessionIdentity(context)
    activeCalls.clear()
    pendingCalls.clear()
    restoredFinalResults.clear()
  })

  pi.on("session_shutdown", async () => {
    const current = sink
    sink = null
    session = null
    activeCalls.clear()
    pendingCalls.clear()
    restoredFinalResults.clear()
    await current?.close()
  })

  pi.on("tool_execution_start", (event, context) => {
    if (archiveDisabled() || sink === null || session === null) return
    const capturedAtMs = Date.now()
    pendingCalls.set(event.toolCallId, {
      call: callIdentity(event, context, false, capturedAtMs),
      input: structuredClone(event.args),
      capturedAtMs,
    })
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
      const requiresStreams =
        rewritten !== null && binding !== null && rewritten !== binding.command
      const pending = pendingCalls.get(event.toolCallId)
      const capturedAtMs = pending?.capturedAtMs ?? Date.now()
      await archive.sink.beginCall(
        archive.session,
        callIdentity(event, context, requiresStreams, capturedAtMs),
        inputBefore,
        inputAfter,
        capturedAtMs,
      )
      pendingCalls.delete(event.toolCallId)
      activeCalls.set(event.toolCallId, { requiresStreams, staged: false })
    }

    if (rewritten !== null && binding !== null && rewritten !== binding.command) {
      binding.replace(rewritten)
    }
  })

  pi.on("tool_result", async (event) => {
    if (sink === null || session === null) return
    const active = activeCalls.get(event.toolCallId)
    if (active === undefined) return
    const snapshot = resultSnapshot(event)
    try {
      await sink.resultBefore(
        session,
        event.toolCallId,
        snapshot,
        Date.now(),
        sourceFullOutputPath(event),
      )
      await sink.stageResult(
        session,
        event.toolCallId,
        snapshot,
        event.isError,
        Date.now(),
      )
      active.staged = true
    } catch (error) {
      activeCalls.delete(event.toolCallId)
      console.error(`[yarp] result archive failed: ${errorMessage(error)}`)
      if (active.requiresStreams) {
        return restoreRawStreams(pi, session, event.toolCallId, event.isError)
      }
    }
  })

  pi.on("tool_execution_end", async (event) => {
    if (sink === null || session === null) return
    const active = activeCalls.get(event.toolCallId)
    const pending = pendingCalls.get(event.toolCallId)
    if (active === undefined && pending === undefined) return
    try {
      if (active?.staged === true) {
        await sink.updateFinalResult(
          session,
          event.toolCallId,
          executionEndSnapshot(event),
          event.isError,
          Date.now(),
        )
      } else {
        if (pending !== undefined) {
          await sink.beginCall(
            session,
            pending.call,
            pending.input,
            pending.input,
            pending.capturedAtMs,
          )
        }
        await sink.finishCall(
          session,
          event.toolCallId,
          executionEndSnapshot(event),
          event.isError,
          false,
          Date.now(),
        )
      }
    } catch (error) {
      console.error(`[yarp] final result archive failed: ${errorMessage(error)}`)
      if (active?.staged === true && active.requiresStreams) {
        const restored = await restoreRawStreams(pi, session, event.toolCallId, event.isError)
        if (restored !== undefined) restoredFinalResults.set(event.toolCallId, restored)
      }
    } finally {
      activeCalls.delete(event.toolCallId)
      pendingCalls.delete(event.toolCallId)
    }
  })

  pi.on("message_end", (event) => {
    if (event.message.role !== "toolResult") return
    const toolCallId = event.message.toolCallId
    if (typeof toolCallId !== "string") return
    const restored = restoredFinalResults.get(toolCallId)
    if (restored === undefined) return
    restoredFinalResults.delete(toolCallId)
    return {
      message: {
        ...event.message,
        content: restored.content ?? [],
        isError: restored.isError ?? false,
      },
    }
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

function callIdentity(
  event: ToolCallEvent | ToolExecutionStartEvent,
  context: ExtensionContext,
  requiresStreams: boolean,
  startedAtMs = Date.now(),
): ArchiveCall {
  const call: ArchiveCall = {
    sourceCallId: event.toolCallId,
    toolName: event.toolName,
    workingDirectory: context.cwd,
    startedAtMs,
    requiresStreams,
  }
  if (context.model !== undefined) {
    call.provider = context.model.provider
    call.model = context.model.id
  }
  return call
}

function sourceFullOutputPath(event: ToolResultEvent): string | undefined {
  if (event.toolName !== "bash" || !isRecord(event.details)) return undefined
  const path = event.details.fullOutputPath
  return typeof path === "string" && path !== "" ? path : undefined
}

function resultSnapshot(event: ToolResultEvent): Record<string, unknown> {
  return normalizeResult(
    {
      content: event.content,
      details: event.details,
      usage: event.usage,
    },
    event.isError,
  )
}

function executionEndSnapshot(event: ToolExecutionEndEvent): Record<string, unknown> {
  if (!isRecord(event.result)) return { result: event.result, isError: event.isError }
  return normalizeResult(event.result, event.isError)
}

function normalizeResult(
  result: Record<string, unknown>,
  isError: boolean,
): Record<string, unknown> {
  const normalized: Record<string, unknown> = { ...result, isError }
  if (normalized.details === undefined) normalized.details = null
  if (normalized.usage === undefined) normalized.usage = null
  return normalized
}

async function restoreRawStreams(
  pi: ExtensionAPI,
  session: ArchiveSession,
  toolCallId: string,
  isError: boolean,
): Promise<ResultPatch | undefined> {
  const args = [
    "archive",
    "restore",
    "--archive-agent",
    session.agent,
    "--archive-account",
    session.account,
    "--archive-session",
    session.sourceSessionId,
    "--archive-call",
    toolCallId,
  ]
  let result: Awaited<ReturnType<ExtensionAPI["exec"]>>
  try {
    result = await pi.exec("yarp", args)
  } catch (error) {
    console.error(`[yarp] raw result restore failed: ${errorMessage(error)}`)
    return undefined
  }
  if (result.killed || result.code !== 0) {
    console.error(`[yarp] raw result restore failed: ${result.stderr.trim() || `exit ${result.code}`}`)
    return undefined
  }
  return {
    content: [{ type: "text", text: `${result.stdout}${result.stderr}` }],
    isError,
  }
}

function archiveDisabled(): boolean {
  return process.env["YARP_ARCHIVE_DISABLED"] === "1"
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function localAccount(): string {
  try {
    return userInfo().username || "unknown"
  } catch {
    return process.env["USER"] ?? process.env["USERNAME"] ?? "unknown"
  }
}
