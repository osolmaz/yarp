import { lstat, realpath } from "node:fs/promises"
import { userInfo } from "node:os"
import { relative, resolve } from "node:path"
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
import {
  parseResolvedConfiguration,
  type YarpConfiguration,
} from "./configuration.js"
import { capToolResultContent } from "./output-cap.js"
import { parseShellPlan, type ShellPlan } from "./shell-plan.js"
import {
  ResultReducerClient,
  type ResultReducer,
  type SourceCompleteness,
} from "./result-client.js"

const REWRITE_TIMEOUT_MS = 2_000
export const YARP_PACKAGE_VERSION = "0.3.1"

type CommandBinding = {
  command: string
  replace: (command: string) => void
}

type ArchiveSinkFactory = () => ArchiveSink
type ResultReducerFactory = () => ResultReducer

type PendingCall = {
  call: ArchiveCall
  input: unknown
  capturedAtMs: number
}

type ResultHandlingPolicy = "ordinary" | "recovery" | "pass_through"

type ActiveCall = {
  requiresStreams: boolean
  staged: boolean
  archiveRef: string
  sourceCommand: string | null
  executedCommand: string | null
  resultPolicy: ResultHandlingPolicy
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

async function planCommand(
  pi: ExtensionAPI,
  command: string,
  session: ArchiveSession | null,
  toolCallId: string,
  rulePack: string | null,
  projectRoot: string,
  signal?: AbortSignal,
): Promise<ShellPlan> {
  const args = ["plan", "--json"]
  if (rulePack !== null) {
    args.push("--project-root", projectRoot, "--rule-pack", rulePack)
  }
  if (session !== null) {
    args.push(
      "--archive-agent",
      session.agent,
      "--archive-account",
      session.account,
      "--archive-session",
      session.sourceSessionId,
      "--archive-call",
      toolCallId,
    )
  }
  args.push(command)
  const options = signal === undefined
    ? { timeout: REWRITE_TIMEOUT_MS }
    : { timeout: REWRITE_TIMEOUT_MS, signal }
  const result = await pi.exec("yarp", args, options)
  if (result.killed || result.code !== 0) {
    throw new Error(result.stderr.trim() || `shell plan exited ${result.code}`)
  }
  return parseShellPlan(result.stdout)
}

export async function installYarpExtension(
  pi: ExtensionAPI,
  createSink: ArchiveSinkFactory = () => new ArchiveClient(),
  createResultReducer: ResultReducerFactory = () => new ResultReducerClient(),
): Promise<void> {
  const version = await pi.exec("yarp", ["--version"], { timeout: REWRITE_TIMEOUT_MS })
  if (version.code !== 0) {
    console.warn("[yarp] binary not found in PATH; extension disabled")
    return
  }
  if (version.stdout.trim() !== `yarp ${YARP_PACKAGE_VERSION}`) {
    console.warn(
      `[yarp] binary/package version mismatch: expected yarp ${YARP_PACKAGE_VERSION}; extension disabled`,
    )
    return
  }
  const configResult = await pi.exec("yarp", ["config", "show", "--json"], {
    timeout: REWRITE_TIMEOUT_MS,
  })
  if (configResult.killed || configResult.code !== 0) {
    console.warn(
      `[yarp] configuration could not be loaded: ${configResult.stderr.trim() || `exit ${configResult.code}`}; extension disabled`,
    )
    return
  }
  let configuration: YarpConfiguration
  try {
    configuration = parseResolvedConfiguration(configResult.stdout)
  } catch (error) {
    console.warn(`[yarp] configuration response is invalid: ${errorMessage(error)}; extension disabled`)
    return
  }
  const resultReducer = createResultReducer()
  const outputCapBytes = configuration.output.cap_bytes === 0
    ? null
    : configuration.output.cap_bytes

  let sink: ArchiveSink | null = null
  let session: ArchiveSession | null = null
  let projectRulePack: string | null = null
  const activeCalls = new Map<string, ActiveCall>()
  const pendingCalls = new Map<string, PendingCall>()
  const restoredFinalResults = new Map<string, ResultPatch>()
  let archiveWarningReported = false

  pi.on("session_start", async (_event, context) => {
    projectRulePack = await trustedProjectRulePack(context)
    await sink?.close()
    sink = configuration.archive.enabled ? createSink() : null
    session = configuration.archive.enabled ? sessionIdentity(context) : null
    activeCalls.clear()
    pendingCalls.clear()
    restoredFinalResults.clear()
    archiveWarningReported = false
  })

  pi.on("session_shutdown", async () => {
    const current = sink
    sink = null
    session = null
    projectRulePack = null
    activeCalls.clear()
    pendingCalls.clear()
    restoredFinalResults.clear()
    await current?.close()
  })

  pi.on("tool_execution_start", (event, context) => {
    if (sink === null || session === null) return
    const capturedAtMs = Date.now()
    pendingCalls.set(event.toolCallId, {
      call: callIdentity(event, context, false, capturedAtMs),
      input: structuredClone(event.args),
      capturedAtMs,
    })
  })

  pi.on("tool_call", async (event, context) => {
    const archive = configuration.archive.enabled
      ? requireArchive(sink, session, context)
      : null
    const inputBefore = structuredClone(event.input)
    const binding = commandBinding(event.toolName, event.input)
    let rewritten: string | null = null
    let resultPolicy: ResultHandlingPolicy = configuration.pruning.enabled
      ? "ordinary"
      : "pass_through"

    if (binding !== null) {
      try {
        const plan = await planCommand(
          pi,
          binding.command,
          archive?.session ?? null,
          event.toolCallId,
          projectRulePack,
          context.cwd,
          context.signal,
        )
        resultPolicy = configuration.pruning.enabled
          ? plan.result.kind
          : plan.result.kind === "recovery"
            ? "recovery"
            : "pass_through"
        if (configuration.pruning.enabled && plan.execution.kind === "rewrite") {
          rewritten = plan.execution.command
        }
      } catch (error) {
        resultPolicy = "pass_through"
        console.warn(`[yarp] shell planning failed; running and preserving the original command: ${errorMessage(error)}`)
      }
    }

    const sourceCommand = binding?.command ?? null
    const executedCommand = rewritten ?? sourceCommand
    const inputAfter = structuredClone(event.input)
    if (rewritten !== null && binding !== null && rewritten !== binding.command) {
      commandBinding(event.toolName, inputAfter)?.replace(rewritten)
    }

    if (archive !== null) {
      const requiresStreams =
        rewritten !== null && binding !== null && rewritten !== binding.command
      const pending = pendingCalls.get(event.toolCallId)
      const capturedAtMs = pending?.capturedAtMs ?? Date.now()
      try {
        const archiveRef = await archive.sink.beginCall(
          archive.session,
          callIdentity(event, context, requiresStreams, capturedAtMs),
          inputBefore,
          inputAfter,
          capturedAtMs,
        )
        activeCalls.set(event.toolCallId, {
          requiresStreams,
          staged: false,
          archiveRef,
          sourceCommand,
          executedCommand,
          resultPolicy,
        })
      } catch (error) {
        rewritten = null
        resultPolicy = "pass_through"
        if (!archiveWarningReported) {
          archiveWarningReported = true
          console.warn(
            `[yarp] initial archive capture failed; running tools without archive capture: ${errorMessage(error)}`,
          )
        }
      } finally {
        pendingCalls.delete(event.toolCallId)
      }
    }

    if (rewritten !== null && binding !== null && rewritten !== binding.command) {
      binding.replace(rewritten)
    }
  })

  pi.on("tool_result", async (event, context) => {
    if (sink === null || session === null) return
    const active = activeCalls.get(event.toolCallId)
    if (active === undefined) return
    const finalBinding = commandBinding(event.toolName, event.input)
    const resultPolicy = active.executedCommand === null
      || finalBinding?.command === active.executedCommand
      ? active.resultPolicy
      : "pass_through"
    const fullOutputPath = sourceFullOutputPath(event)
    const beforeSnapshot = resultSnapshot(event)
    try {
      await sink.resultBefore(
        session,
        event.toolCallId,
        beforeSnapshot,
        Date.now(),
        fullOutputPath,
      )
    } catch (error) {
      activeCalls.delete(event.toolCallId)
      console.error(`[yarp] result archive failed: ${errorMessage(error)}`)
      if (active.requiresStreams) {
        return restoreRawStreams(pi, session, event.toolCallId, event.isError)
      }
      return undefined
    }

    let patch: ResultPatch | undefined
    let typedRecovery: {
      source: "source_output" | "result_text"
      completeness: SourceCompleteness
    } | null = null
    const completeness = resultCompleteness(event, fullOutputPath !== undefined)
    const hostTextCompleteness = resultCompleteness(event, false)
    const text = singleTextContent(event.content)
    if (
      resultPolicy === "ordinary"
      && !active.requiresStreams
      && active.sourceCommand !== null
      && text !== null
    ) {
      try {
        const reduced = await resultReducer.reduce(
          {
            command: active.sourceCommand,
            text,
            isError: event.isError,
            ...explicitExitCode(event.details),
            archiveRef: active.archiveRef,
            sourceCompleteness: completeness,
            preferArchiveSource: fullOutputPath !== undefined,
          },
          context.signal,
        )
        if (reduced.changed) {
          if (reduced.needsResultText) {
            await sink.resultText(
              session,
              event.toolCallId,
              text,
              completeness,
              Date.now(),
            )
          }
          patch = {
            content: [{ type: "text", text: reduced.content }],
          }
          typedRecovery = {
            source: reduced.source,
            completeness: reduced.sourceCompleteness,
          }
        }
      } catch (error) {
        console.warn(`[yarp] post-result reduction failed; keeping original output: ${errorMessage(error)}`)
      }
    }

    if (resultPolicy !== "recovery" && outputCapBytes !== null) {
      try {
        const content = patch?.content ?? event.content
        const usesRawStreams = typedRecovery === null && active.requiresStreams
        const capped = capToolResultContent(
          content,
          active.archiveRef,
          usesRawStreams ? "complete" : (typedRecovery?.completeness ?? hostTextCompleteness),
          outputCapBytes,
          usesRawStreams ? "stdout" : (typedRecovery?.source ?? "result_text"),
        )
        if (capped !== null) {
          if (typedRecovery === null) {
            await sink.resultText(
              session,
              event.toolCallId,
              capped.sourceText,
              hostTextCompleteness,
              Date.now(),
            )
          }
          patch = { ...patch, content: capped.content }
        }
      } catch (error) {
        console.warn(`[yarp] generic output cap failed; keeping pre-cap output: ${errorMessage(error)}`)
      }
    }

    const stagedSnapshot = resultSnapshot(event, patch?.content)
    try {
      await sink.stageResult(
        session,
        event.toolCallId,
        stagedSnapshot,
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
      return undefined
    }
    return patch
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
    const toolCallId = event.message["toolCallId"]
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
  const path = event.details["fullOutputPath"]
  return typeof path === "string" && path !== "" ? path : undefined
}

function resultSnapshot(
  event: ToolResultEvent,
  content: ToolResultEvent["content"] = event.content,
): Record<string, unknown> {
  return normalizeResult(
    {
      content,
      details: event.details,
      usage: event.usage,
    },
    event.isError,
  )
}

function singleTextContent(content: ToolResultEvent["content"]): string | null {
  if (content.length !== 1) return null
  const item = content[0]
  if (!isRecord(item) || item["type"] !== "text" || typeof item["text"] !== "string") {
    return null
  }
  return item["text"]
}

function resultCompleteness(
  event: ToolResultEvent,
  hasFullOutput: boolean,
): SourceCompleteness {
  if (hasFullOutput) return "complete"
  const truncated = nestedBoolean(event.details, "truncated", 0)
  if (truncated === true) return "incomplete"
  if (truncated === false) return "complete"
  return "unknown"
}

function nestedBoolean(value: unknown, key: string, depth: number): boolean | undefined {
  if (!isRecord(value) || depth > 2) return undefined
  const direct = value[key]
  if (typeof direct === "boolean") return direct
  for (const nestedKey of ["truncation", "details"]) {
    const nested = nestedBoolean(value[nestedKey], key, depth + 1)
    if (nested !== undefined) return nested
  }
  return undefined
}

function explicitExitCode(details: unknown): { exitCode?: number } {
  const exitCode = nestedExitCode(details, 0)
  return exitCode === undefined ? {} : { exitCode }
}

function nestedExitCode(value: unknown, depth: number): number | undefined {
  if (!isRecord(value) || depth > 2) return undefined
  for (const key of ["exit_code", "exitCode"]) {
    const candidate = value[key]
    if (
      typeof candidate === "number"
      && Number.isInteger(candidate)
      && candidate >= -2_147_483_648
      && candidate <= 2_147_483_647
    ) {
      return candidate
    }
  }
  return nestedExitCode(value["details"], depth + 1)
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
  if (normalized["details"] === undefined) normalized["details"] = null
  if (normalized["usage"] === undefined) normalized["usage"] = null
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

export async function trustedProjectRulePack(
  context: Pick<ExtensionContext, "cwd" | "isProjectTrusted">,
): Promise<string | null> {
  if (!context.isProjectTrusted()) return null
  const root = await realpath(context.cwd).catch(() => null)
  if (root === null) return null
  const ruleDirectory = resolve(root, ".yarp")
  const directoryMetadata = await lstat(ruleDirectory).catch(() => null)
  if (directoryMetadata === null || !directoryMetadata.isDirectory() || directoryMetadata.isSymbolicLink()) {
    return null
  }
  const candidate = resolve(ruleDirectory, "rules.yrp")
  const metadata = await lstat(candidate).catch(() => null)
  if (metadata === null || !metadata.isFile() || metadata.isSymbolicLink()) return null
  const resolved = await realpath(candidate).catch(() => null)
  if (resolved === null) return null
  const fromRoot = relative(root, resolved)
  if (fromRoot === "" || fromRoot === ".." || fromRoot.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`)) {
    return null
  }
  return resolved
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
