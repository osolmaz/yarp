import assert from "node:assert/strict"
import { Buffer } from "node:buffer"
import { mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import test from "node:test"
import type {
  ExecOptions,
  ExecResult,
  ExtensionAPI,
  ExtensionContext,
  ExtensionEventMap,
  ExtensionEventResultMap,
} from "@earendil-works/pi-coding-agent"
import type {
  ArchiveCall,
  ArchiveSession,
  ArchiveSink,
} from "./archive-client.js"
import type { ResultReducer } from "./result-client.js"
import {
  commandBinding,
  installYarpExtension,
  trustedProjectRulePack,
  YARP_PACKAGE_VERSION,
} from "./yarp.js"

type EventName = keyof ExtensionEventMap
type Handler<K extends EventName> = (
  event: ExtensionEventMap[K],
  context: ExtensionContext,
) => Promise<ExtensionEventResultMap[K] | void> | ExtensionEventResultMap[K] | void

test("keeps the Pi package version and resources in agreement", async () => {
  const packageJson = JSON.parse(
    await readFile(new URL("../../package.json", import.meta.url), "utf8"),
  ) as unknown
  assert.equal(
    typeof packageJson === "object" && packageJson !== null && "version" in packageJson
      ? packageJson.version
      : undefined,
    YARP_PACKAGE_VERSION,
  )
  const pi =
    typeof packageJson === "object" && packageJson !== null && "pi" in packageJson
      ? packageJson.pi
      : undefined
  assert.deepEqual(
    typeof pi === "object" && pi !== null && "extensions" in pi ? pi.extensions : undefined,
    ["./hooks/pi/yarp.ts"],
  )
  assert.deepEqual(
    typeof pi === "object" && pi !== null && "skills" in pi ? pi.skills : undefined,
    ["./skills"],
  )
})

class HandlerRegistry {
  private readonly handlers: { [K in EventName]: Array<Handler<K>> } = {
    session_start: [],
    session_shutdown: [],
    message_end: [],
    tool_call: [],
    tool_result: [],
    tool_execution_start: [],
    tool_execution_end: [],
  }

  add<K extends EventName>(event: K, handler: Handler<K>): void {
    this.handlers[event].push(handler)
  }

  async emit<K extends EventName>(
    event: K,
    payload: ExtensionEventMap[K],
    context: ExtensionContext,
  ): Promise<ExtensionEventResultMap[K] | void> {
    let result: ExtensionEventResultMap[K] | void = undefined
    for (const handler of this.handlers[event]) {
      const current = await handler(payload, context)
      if (current !== undefined) result = current
    }
    return result
  }
}

class MockPi implements ExtensionAPI {
  readonly registry = new HandlerRegistry()
  version: ExecResult = result(0, "yarp 0.2.0\n")
  configuration: ExecResult = configurationResult()
  plan: ExecResult = shellPlan("original", "ordinary")
  restore: ExecResult = result(0, "raw output\n")
  failPlan = false
  failRestore = false
  planArgs: string[] | null = null
  restoreOptions: ExecOptions | undefined

  async exec(command: string, args: string[], options?: ExecOptions): Promise<ExecResult> {
    assert.equal(command, "yarp")
    if (args[0] === "--version") return this.version
    if (args[0] === "config" && args[1] === "show" && args[2] === "--json") {
      return this.configuration
    }
    if (args[0] === "archive" && args[1] === "restore") {
      this.restoreOptions = options
      if (this.failRestore) throw new Error("restore spawn failed")
      return this.restore
    }
    this.planArgs = args
    if (this.failPlan) throw new Error("plan failed")
    return this.plan
  }

  on<K extends EventName>(event: K, handler: Handler<K>): void {
    this.registry.add(event, handler)
  }
}

type BeginRecord = {
  session: ArchiveSession
  call: ArchiveCall
  inputBefore: unknown
  inputAfter: unknown
}

class MemorySink implements ArchiveSink {
  readonly begins: BeginRecord[] = []
  readonly beforeResults: unknown[] = []
  readonly fullOutputPaths: Array<string | undefined> = []
  readonly resultTexts: string[] = []
  readonly resultTextCompleteness: Array<"complete" | "incomplete" | "unknown"> = []
  readonly stagedResults: unknown[] = []
  readonly finishedResults: unknown[] = []
  readonly updatedResults: unknown[] = []
  closed = false
  failBegin = false
  failBefore = false
  failStage = false
  failResultText = false
  failFinish = false
  failUpdate = false
  finishRequiresPreResult: boolean[] = []

  async beginCall(
    session: ArchiveSession,
    call: ArchiveCall,
    inputBefore: unknown,
    inputAfter: unknown,
  ): Promise<string> {
    if (this.failBegin) throw new Error("archive unavailable")
    this.begins.push({ session, call, inputBefore, inputAfter })
    return "yr_0123456789abcdef0123456789abcdef"
  }

  async resultBefore(
    _session: ArchiveSession,
    _sourceCallId: string,
    resultValue: unknown,
    _capturedAtMs: number,
    fullOutputPath?: string,
  ): Promise<void> {
    if (this.failBefore) throw new Error("before failed")
    this.beforeResults.push(resultValue)
    this.fullOutputPaths.push(fullOutputPath)
  }

  async resultText(
    _session: ArchiveSession,
    _sourceCallId: string,
    text: string,
    sourceCompleteness: "complete" | "incomplete" | "unknown",
  ): Promise<string> {
    if (this.failResultText) throw new Error("result text failed")
    this.resultTexts.push(text)
    this.resultTextCompleteness.push(sourceCompleteness)
    return "yr_0123456789abcdef0123456789abcdef"
  }

  async stageResult(
    _session: ArchiveSession,
    _sourceCallId: string,
    resultValue: unknown,
  ): Promise<void> {
    if (this.failStage) throw new Error("stage failed")
    this.stagedResults.push(resultValue)
  }

  async finishCall(
    _session: ArchiveSession,
    _sourceCallId: string,
    resultValue: unknown,
    _isError: boolean,
    requirePreResult: boolean,
  ): Promise<void> {
    if (this.failFinish) throw new Error("finish failed")
    this.finishedResults.push(resultValue)
    this.finishRequiresPreResult.push(requirePreResult)
  }

  async updateFinalResult(
    _session: ArchiveSession,
    _sourceCallId: string,
    resultValue: unknown,
  ): Promise<void> {
    if (this.failUpdate) throw new Error("update failed")
    this.updatedResults.push(resultValue)
  }

  async close(): Promise<void> {
    this.closed = true
  }
}

const context: ExtensionContext = {
  signal: new AbortController().signal,
  cwd: "/repo",
  isProjectTrusted: () => false,
  sessionManager: { getSessionId: () => "session-1" },
  model: { provider: "openai", id: "gpt" },
}

function result(code: number, stdout = "", stderr = ""): ExecResult {
  return { code, stdout, stderr, killed: false }
}

function configurationResult(options: {
  pruningEnabled?: boolean
  archiveEnabled?: boolean
  capBytes?: number
} = {}): ExecResult {
  return result(0, JSON.stringify({
    version: 1,
    pruning: { enabled: options.pruningEnabled ?? true },
    output: {
      cap_bytes: options.capBytes ?? 5120,
      recovery_cap_bytes: 32768,
      recovery_cap_lines: 1900,
    },
    archive: {
      enabled: options.archiveEnabled ?? true,
      path: "/home/test/.local/share/yarp/tool-calls.sqlite3",
    },
    rules: { packs: [] },
  }))
}

function shellPlan(
  execution: "original" | "rewrite",
  policy: "ordinary" | "recovery",
  command?: string,
): ExecResult {
  return result(0, JSON.stringify({
    version: 1,
    execution: execution === "rewrite"
      ? { kind: "rewrite", command }
      : { kind: "original" },
    result: { kind: policy },
  }))
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

const unchangedReducer: ResultReducer = {
  async reduce() {
    return { changed: false }
  },
}

async function start(
  pi: MockPi,
  sink: MemorySink,
  currentContext: ExtensionContext = context,
  reducer: ResultReducer = unchangedReducer,
): Promise<void> {
  await installYarpExtension(pi, () => sink, () => reducer)
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    currentContext,
  )
}

async function call(
  pi: MockPi,
  toolCallId: string,
  toolName: string,
  input: Record<string, unknown>,
  currentContext: ExtensionContext = context,
): Promise<void> {
  await pi.registry.emit(
    "tool_execution_start",
    { type: "tool_execution_start", toolCallId, toolName, args: structuredClone(input) },
    currentContext,
  )
  await pi.registry.emit(
    "tool_call",
    { type: "tool_call", toolCallId, toolName, input },
    currentContext,
  )
}

test("disables every integration path on an exact version mismatch", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.version = result(0, "yarp 0.1.1\n")
  await installYarpExtension(pi, () => sink, () => unchangedReducer)
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    context,
  )
  const input = { command: "cargo test" }
  await call(pi, "mismatch", "bash", input)
  assert.equal(input.command, "cargo test")
  assert.equal(sink.begins.length, 0)
})

test("disables every integration path when resolved configuration is invalid", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.configuration = result(0, '{"version":2}')
  await installYarpExtension(pi, () => sink, () => unchangedReducer)
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    context,
  )
  const input = { command: "cargo test" }
  await call(pi, "bad-config", "bash", input)
  assert.equal(input.command, "cargo test")
  assert.equal(sink.begins.length, 0)
})

test("finds bash and exec_command inputs", () => {
  const bash = { command: "git status" }
  commandBinding("bash", bash)?.replace("changed")
  assert.equal(bash.command, "changed")

  const exec = { cmd: "cargo test" }
  commandBinding("exec_command", exec)?.replace("changed")
  assert.equal(exec.cmd, "changed")

  assert.equal(commandBinding("read", { path: "file" }), null)
  assert.equal(commandBinding("bash", { command: 4 }), null)
  assert.equal(commandBinding("bash", null), null)
})

test("uses a compiled project pack only for a trusted regular file", async () => {
  const directory = await mkdtemp(join(tmpdir(), "yarp-project-rules-"))
  try {
    const ruleDirectory = join(directory, ".yarp")
    const rulePack = join(ruleDirectory, "rules.yrp")
    await mkdir(ruleDirectory)
    await writeFile(rulePack, "compiled")
    const trusted = { cwd: directory, isProjectTrusted: () => true }
    const untrusted = { cwd: directory, isProjectTrusted: () => false }
    assert.equal(await trustedProjectRulePack(trusted), await realpath(rulePack))
    assert.equal(await trustedProjectRulePack(untrusted), null)

    await rm(rulePack)
    await symlink("../outside.yrp", rulePack)
    assert.equal(await trustedProjectRulePack(trusted), null)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("passes a trusted project pack to command rewriting", async () => {
  const directory = await mkdtemp(join(tmpdir(), "yarp-project-rewrite-"))
  try {
    const ruleDirectory = join(directory, ".yarp")
    const rulePack = join(ruleDirectory, "rules.yrp")
    await mkdir(ruleDirectory)
    await writeFile(rulePack, "compiled")
    const trustedContext: ExtensionContext = {
      ...context,
      cwd: directory,
      isProjectTrusted: () => true,
    }
    const pi = new MockPi()
    const sink = new MemorySink()
    const resolvedRulePack = await realpath(rulePack)
    await start(pi, sink, trustedContext)
    await call(pi, "project-rules", "bash", { command: "git status" }, trustedContext)
    assert.deepEqual(pi.planArgs?.slice(0, 6), [
      "plan",
      "--json",
      "--project-root",
      directory,
      "--rule-pack",
      resolvedRulePack,
    ])
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("archives and rewrites supported shell calls", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.plan = shellPlan(
    "rewrite",
    "ordinary",
    "yarp run --archive-agent 'pi' --archive-account 'onur' --archive-session 'session-1' --archive-call 'call-1' -- git status",
  )
  await start(pi, sink)

  const input = { cmd: "git status" }
  await call(pi, "call-1", "exec_command", input)

  assert.match(input.cmd, /^yarp run --archive-agent/)
  assert.equal(sink.begins.length, 1)
  assert.equal(sink.begins[0]?.call.requiresStreams, true)
  assert.deepEqual(sink.begins[0]?.inputBefore, { cmd: "git status" })
  assert.deepEqual(sink.begins[0]?.inputAfter, { cmd: input.cmd })
  assert.deepEqual(pi.planArgs, [
    "plan",
    "--json",
    "--archive-agent",
    "pi",
    "--archive-account",
    sink.begins[0]?.session.account,
    "--archive-session",
    "session-1",
    "--archive-call",
    "call-1",
    "git status",
  ])
})

test("caps wrapped summaries against their exact raw streams", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.plan = shellPlan(
    "rewrite",
    "ordinary",
    "yarp run --archive-call 'wrapped-cap' -- cargo test",
  )
  await start(pi, sink)
  await call(pi, "wrapped-cap", "bash", { command: "cargo test" })
  const wrappedSummary = `typed stream start\n${"typed stream evidence\n".repeat(1_000)}typed stream end\n`
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "wrapped-cap",
      toolName: "bash",
      input: { command: "yarp run --archive-call 'wrapped-cap' -- cargo test" },
      content: [{ type: "text", text: wrappedSummary }],
      details: undefined,
      isError: false,
    },
    context,
  )

  const visible = resultPatchText(patch)
  assert.ok(Buffer.byteLength(visible, "utf8") <= 5 * 1024)
  assert.match(visible, /stdout complete/u)
  assert.deepEqual(sink.resultTexts, [wrappedSummary])
  assert.deepEqual(sink.resultTextCompleteness, ["unknown"])
})

test("archives unchanged non-shell calls and both result stages", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)

  await call(pi, "call-2", "read", { path: "README.md" })
  await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-2",
      toolName: "read",
      input: { path: "README.md" },
      content: [{ type: "text", text: "raw" }],
      details: { lines: 1 },
      isError: false,
    },
    context,
  )
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "call-2",
      toolName: "read",
      result: { content: [{ type: "text", text: "final" }], details: { lines: 1 } },
      isError: false,
    },
    context,
  )

  assert.deepEqual(sink.begins[0]?.inputBefore, { path: "README.md" })
  assert.deepEqual(sink.begins[0]?.inputAfter, { path: "README.md" })
  assert.equal(sink.beforeResults.length, 1)
  assert.equal(sink.stagedResults.length, 1)
  assert.deepEqual(sink.stagedResults[0], sink.beforeResults[0])
  assert.equal(sink.finishedResults.length, 0)
  assert.deepEqual(sink.updatedResults, [
    {
      content: [{ type: "text", text: "final" }],
      details: { lines: 1 },
      isError: false,
      usage: null,
    },
  ])
  assert.deepEqual(sink.fullOutputPaths, [undefined])
  assert.deepEqual(sink.finishRequiresPreResult, [])
})

test("caps every large archived text result at 5 KiB by default", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await call(pi, "global-cap", "read", { path: "large.txt" })
  const original = `first line\n${"middle data\n".repeat(1_000)}last line\n`
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "global-cap",
      toolName: "read",
      input: { path: "large.txt" },
      content: [{ type: "text", text: original }],
      details: { truncated: false },
      isError: false,
    },
    context,
  )

  const visible = resultPatchText(patch)
  assert.ok(Buffer.byteLength(visible, "utf8") <= 5 * 1024)
  assert.ok(visible.startsWith("first line\n"))
  assert.ok(visible.endsWith("last line\n"))
  assert.match(visible, /Search omitted output: yarp search yr_0123456789abcdef0123456789abcdef/u)
  assert.deepEqual(sink.resultTexts, [original])
  assert.deepEqual(sink.resultTextCompleteness, ["complete"])
  assert.deepEqual(sink.beforeResults[0], {
    content: [{ type: "text", text: original }],
    details: { truncated: false },
    isError: false,
    usage: null,
  })
})

test("does not treat a capped truncated Bash host result as complete", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await call(pi, "truncated-host-cap", "bash", { command: "printf output" })
  const original = "truncated host text\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "truncated-host-cap",
      toolName: "bash",
      input: { command: "printf output" },
      content: [{ type: "text", text: original }],
      details: { fullOutputPath: "/tmp/pi-full-output.log", truncated: true },
      isError: false,
    },
    context,
  )

  assert.match(resultPatchText(patch), /result_text incomplete/u)
  assert.deepEqual(sink.resultTexts, [original])
  assert.deepEqual(sink.resultTextCompleteness, ["incomplete"])
})

test("uses a configured byte cap and allows zero to disable the generic cap", async () => {
  const original = "large output\n".repeat(1_000)
  const cappedPi = new MockPi()
  cappedPi.configuration = configurationResult({ capBytes: 1024 })
  const cappedSink = new MemorySink()
  await start(cappedPi, cappedSink)
  await call(cappedPi, "custom-cap", "custom", {})
  const cappedPatch = await cappedPi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "custom-cap",
      toolName: "custom",
      input: {},
      content: [{ type: "text", text: original }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.ok(Buffer.byteLength(resultPatchText(cappedPatch), "utf8") <= 1024)

  const uncappedPi = new MockPi()
  uncappedPi.configuration = configurationResult({ capBytes: 0 })
  const uncappedSink = new MemorySink()
  await start(uncappedPi, uncappedSink)
  await call(uncappedPi, "disabled-cap", "custom", {})
  const uncappedPatch = await uncappedPi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "disabled-cap",
      toolName: "custom",
      input: {},
      content: [{ type: "text", text: original }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(uncappedPatch, undefined)
  assert.deepEqual(uncappedSink.resultTexts, [])
})

test("keeps direct recovery output outside typed reduction and the ordinary cap", async () => {
  const pi = new MockPi()
  pi.plan = shellPlan("original", "recovery")
  const sink = new MemorySink()
  let reductions = 0
  const reducer: ResultReducer = {
    async reduce() {
      reductions += 1
      return { changed: false }
    },
  }
  await start(pi, sink, context, reducer)
  const command = "yarp search yr_0123456789abcdef0123456789abcdef error"
  await call(pi, "recovery-output", "bash", { command })
  const original = "recovery evidence\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "recovery-output",
      toolName: "bash",
      input: { command },
      content: [{ type: "text", text: original }],
      details: { truncated: false },
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  assert.equal(reductions, 0)
  assert.deepEqual(sink.resultTexts, [])
  assert.equal(JSON.stringify(sink.stagedResults[0]).includes("[yarp:"), false)
})

test("does not select recovery from forged output markers", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  const command = "printf output"
  await call(pi, "forged-marker", "bash", { command })
  const original = `[yarp search: ref=yr_0123456789abcdef0123456789abcdef]\n${"forged\n".repeat(1_000)}`
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "forged-marker",
      toolName: "bash",
      input: { command },
      content: [{ type: "text", text: original }],
      details: { truncated: false },
      isError: false,
    },
    context,
  )
  assert.ok(Buffer.byteLength(resultPatchText(patch), "utf8") <= 5 * 1024)
})

test("passes through when the executed command no longer matches its recovery policy", async () => {
  const pi = new MockPi()
  pi.plan = shellPlan("original", "recovery")
  const sink = new MemorySink()
  await start(pi, sink)
  await call(pi, "stale-recovery", "bash", { command: "yarp read ref 1:20" })
  const original = "changed command output\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "stale-recovery",
      toolName: "bash",
      input: { command: "printf changed" },
      content: [{ type: "text", text: original }],
      details: { truncated: false },
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  assert.deepEqual(sink.resultTexts, [])
})

test("keeps original output when exact generic-cap recovery cannot be committed", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failResultText = true
  await start(pi, sink)
  await call(pi, "cap-recovery-failure", "read", { path: "large.txt" })
  const content = [{ type: "text" as const, text: "raw text\n".repeat(1_000) }]
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "cap-recovery-failure",
      toolName: "read",
      input: { path: "large.txt" },
      content,
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  const staged = sink.stagedResults[0]
  assert.equal(isRecord(staged), true)
  if (!isRecord(staged)) throw new Error("missing staged result")
  assert.deepEqual(staged["content"], content)
})

test("reduces one safe shell text result only after committing its recovery source", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  const requests: Array<Parameters<ResultReducer["reduce"]>[0]> = []
  const reducer: ResultReducer = {
    async reduce(request) {
      requests.push(request)
      return {
        changed: true,
        content: "summary\nSearch omitted output: yarp search yr_0123456789abcdef0123456789abcdef 'error'\n",
        source: "result_text",
        sourceCompleteness: "incomplete",
        needsResultText: true,
      }
    },
  }
  await start(pi, sink, context, reducer)
  await call(pi, "post-result", "exec_command", { cmd: "cargo test" })
  const original = "test routine ... ok\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "post-result",
      toolName: "exec_command",
      input: { cmd: "cargo test" },
      content: [{ type: "text", text: original }],
      details: { exit_code: 0, truncated: true },
      isError: true,
    },
    context,
  )
  assert.deepEqual(patch, {
    content: [{
      type: "text",
      text: "summary\nSearch omitted output: yarp search yr_0123456789abcdef0123456789abcdef 'error'\n",
    }],
  })
  assert.equal(requests.length, 1)
  assert.equal(requests[0]?.exitCode, 0)
  assert.equal(requests[0]?.isError, true)
  assert.equal(requests[0]?.sourceCompleteness, "incomplete")
  assert.equal(requests[0]?.preferArchiveSource, false)
  assert.deepEqual(sink.resultTexts, [original])
  const staged = sink.stagedResults[0]
  assert.equal(isRecord(staged), true)
  if (!isRecord(staged)) throw new Error("missing staged result")
  assert.deepEqual(staged["content"], patch?.content)
})

test("applies the configured cap after a large typed summary", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  const typedSummary = `typed start\n${"typed evidence\n".repeat(1_000)}typed end\n`
  const reducer: ResultReducer = {
    async reduce() {
      return {
        changed: true,
        content: typedSummary,
        source: "result_text",
        sourceCompleteness: "incomplete",
        needsResultText: true,
      }
    },
  }
  await start(pi, sink, context, reducer)
  await call(pi, "large-typed-summary", "exec_command", { cmd: "cargo test" })
  const original = "raw test output\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "large-typed-summary",
      toolName: "exec_command",
      input: { cmd: "cargo test" },
      content: [{ type: "text", text: original }],
      details: { truncated: true },
      isError: true,
    },
    context,
  )

  const visible = resultPatchText(patch)
  assert.ok(Buffer.byteLength(visible, "utf8") <= 5 * 1024)
  assert.ok(visible.startsWith("typed start\n"))
  assert.ok(visible.endsWith("typed end\n"))
  assert.match(visible, /result_text incomplete/u)
  assert.deepEqual(sink.resultTexts, [original])
})

test("prefers a documented complete Bash source for a composite command", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  const requests: Array<Parameters<ResultReducer["reduce"]>[0]> = []
  const reducer: ResultReducer = {
    async reduce(request) {
      requests.push(request)
      return {
        changed: true,
        content: "complete-source summary",
        source: "source_output",
        sourceCompleteness: "complete",
        needsResultText: false,
      }
    },
  }
  await start(pi, sink, context, reducer)
  const command = "rg TODO . | sort | head -50"
  await call(pi, "source-output", "bash", { command })
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "source-output",
      toolName: "bash",
      input: { command },
      content: [{ type: "text", text: "host-visible text" }],
      details: { fullOutputPath: "/tmp/pi-full-output.log", truncated: true },
      isError: false,
    },
    context,
  )
  assert.equal(requests[0]?.command, command)
  assert.equal(requests[0]?.preferArchiveSource, true)
  assert.equal(requests[0]?.sourceCompleteness, "complete")
  assert.deepEqual(sink.resultTexts, [])
  assert.deepEqual(patch, {
    content: [{ type: "text", text: "complete-source summary" }],
  })
})

test("post-result reduction passes through compound and recovery failures", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failResultText = true
  let calls = 0
  const reducer: ResultReducer = {
    async reduce() {
      calls += 1
      return {
        changed: true,
        content: "summary",
        source: "result_text",
        sourceCompleteness: "unknown",
        needsResultText: true,
      }
    },
  }
  await start(pi, sink, context, reducer)
  await call(pi, "post-result-failure", "exec_command", { cmd: "cargo test && cargo test" })
  const content = [{ type: "text", text: "raw output" }]
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "post-result-failure",
      toolName: "exec_command",
      input: { cmd: "cargo test && cargo test" },
      content,
      details: {},
      isError: false,
    },
    context,
  )
  assert.equal(calls, 1)
  assert.equal(patch, undefined)
  const staged = sink.stagedResults[0]
  assert.equal(isRecord(staged), true)
  if (!isRecord(staged)) throw new Error("missing staged result")
  assert.deepEqual(staged["content"], content)
})

test("passes only built-in Bash full-output paths to the archive", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await call(pi, "call-full-output", "bash", { command: "printf output" })
  await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-full-output",
      toolName: "bash",
      input: { command: "printf output" },
      content: [{ type: "text", text: "truncated" }],
      details: {
        truncation: { truncated: true },
        fullOutputPath: "/tmp/pi-bash-full.log",
      },
      isError: false,
    },
    context,
  )
  await call(pi, "call-untrusted-output", "custom", { path: "ignored" })
  await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-untrusted-output",
      toolName: "custom",
      input: { path: "ignored" },
      content: [{ type: "text", text: "custom" }],
      details: { fullOutputPath: "/home/user/.ssh/id_rsa" },
      isError: false,
    },
    context,
  )
  assert.deepEqual(sink.fullOutputPaths, ["/tmp/pi-bash-full.log", undefined])
})

test("finalizes preflight errors that skip tool_result", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await call(pi, "call-3", "missing", {})
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "call-3",
      toolName: "missing",
      result: { content: [{ type: "text", text: "Tool not found" }] },
      isError: true,
    },
    context,
  )
  assert.equal(sink.beforeResults.length, 0)
  assert.equal(sink.finishedResults.length, 1)
  assert.deepEqual(sink.finishRequiresPreResult, [false])
})

test("archives calls rejected before the tool_call hook", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await pi.registry.emit(
    "tool_execution_start",
    {
      type: "tool_execution_start",
      toolCallId: "missing-before-hook",
      toolName: "missing",
      args: { value: "raw" },
    },
    context,
  )
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "missing-before-hook",
      toolName: "missing",
      result: { content: [{ type: "text", text: "Tool missing not found" }] },
      isError: true,
    },
    context,
  )
  assert.deepEqual(sink.begins[0]?.inputBefore, { value: "raw" })
  assert.deepEqual(sink.begins[0]?.inputAfter, { value: "raw" })
  assert.deepEqual(sink.finishRequiresPreResult, [false])
})

test("archive start failure blocks tool mutation", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failBegin = true
  pi.plan = shellPlan("rewrite", "ordinary", "yarp run -- git status")
  await start(pi, sink)
  const input = { command: "git status" }
  await assert.rejects(call(pi, "call-4", "bash", input), /archive unavailable/)
  assert.equal(input.command, "git status")
})

test("shell planning failures preserve the original command and result", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.failPlan = true
  await start(pi, sink)
  const input = { command: "git status" }
  await call(pi, "call-5", "bash", input)
  assert.equal(input.command, "git status")
  assert.equal(sink.begins.length, 1)
  assert.equal(sink.begins[0]?.call.requiresStreams, false)
  const original = "planning failure output\n".repeat(1_000)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-5",
      toolName: "bash",
      input,
      content: [{ type: "text", text: original }],
      details: { truncated: false },
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  assert.deepEqual(sink.resultTexts, [])
})

test("leaves a call incomplete when pre-result capture fails", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failBefore = true
  await start(pi, sink)
  await call(pi, "call-before-failure", "read", { path: "README.md" })
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-before-failure",
      toolName: "read",
      input: { path: "README.md" },
      content: [{ type: "text", text: "raw" }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "call-before-failure",
      toolName: "read",
      result: { content: [{ type: "text", text: "raw" }] },
      isError: false,
    },
    context,
  )
  assert.equal(sink.finishedResults.length, 0)
})

test("restores raw shell output when result finalization fails", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failStage = true
  pi.plan = shellPlan(
    "rewrite",
    "ordinary",
    "yarp run --archive-call 'call-restore' -- git status",
  )
  pi.restore = { code: 0, stdout: "raw stdout\n", stderr: "raw stderr\n", killed: false }
  await start(pi, sink)
  await call(pi, "call-restore", "bash", { command: "git status" })
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-restore",
      toolName: "bash",
      input: { command: "rewritten" },
      content: [{ type: "text", text: "pruned" }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.deepEqual(patch, {
    content: [{ type: "text", text: "raw stdout\nraw stderr\n" }],
    isError: false,
  })
  assert.equal(sink.finishedResults.length, 0)
  assert.equal(pi.restoreOptions, undefined)
})

test("restores raw shell output when final reconciliation fails", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failUpdate = true
  pi.plan = shellPlan(
    "rewrite",
    "ordinary",
    "yarp run --archive-call 'call-final-restore' -- git status",
  )
  pi.restore = { code: 0, stdout: "raw stdout\n", stderr: "raw stderr\n", killed: false }
  await start(pi, sink)
  await call(pi, "call-final-restore", "bash", { command: "git status" })
  await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-final-restore",
      toolName: "bash",
      input: { command: "rewritten" },
      content: [{ type: "text", text: "pruned" }],
      details: undefined,
      isError: false,
    },
    context,
  )
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "call-final-restore",
      toolName: "bash",
      result: { content: [{ type: "text", text: "pruned" }] },
      isError: false,
    },
    context,
  )
  const patch = await pi.registry.emit(
    "message_end",
    {
      type: "message_end",
      message: {
        role: "toolResult",
        toolCallId: "call-final-restore",
        content: [{ type: "text", text: "pruned" }],
        isError: false,
      },
    },
    context,
  )
  assert.deepEqual(patch, {
    message: {
      role: "toolResult",
      toolCallId: "call-final-restore",
      content: [{ type: "text", text: "raw stdout\nraw stderr\n" }],
      isError: false,
    },
  })
})

test("contains raw restore transport failures", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failStage = true
  pi.failRestore = true
  pi.plan = shellPlan(
    "rewrite",
    "ordinary",
    "yarp run --archive-call 'call-restore-error' -- git status",
  )
  await start(pi, sink)
  await call(pi, "call-restore-error", "bash", { command: "git status" })
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-restore-error",
      toolName: "bash",
      input: { command: "rewritten" },
      content: [{ type: "text", text: "pruned" }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
})

test("reports preflight archive failures without rejecting the event", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failFinish = true
  await start(pi, sink)
  await call(pi, "preflight-archive-failure", "missing", {})
  await pi.registry.emit(
    "tool_execution_end",
    {
      type: "tool_execution_end",
      toolCallId: "preflight-archive-failure",
      toolName: "missing",
      result: { content: [{ type: "text", text: "Tool not found" }] },
      isError: true,
    },
    context,
  )
  assert.equal(sink.finishedResults.length, 0)
})

test("archive opt-out keeps rewriting without archive metadata", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.plan = shellPlan("rewrite", "ordinary", "yarp run -- git status")
  pi.configuration = configurationResult({ archiveEnabled: false })
  await installYarpExtension(pi, () => sink)
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    context,
  )
  const input = { command: "git status" }
  await call(pi, "call-6", "bash", input)
  assert.equal(input.command, "yarp run -- git status")
  assert.deepEqual(pi.planArgs, ["plan", "--json", "git status"])
  assert.equal(sink.begins.length, 0)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-6",
      toolName: "bash",
      input,
      content: [{ type: "text", text: "uncapped\n".repeat(1_000) }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
})

test("pruning opt-out still archives every call", async () => {
  const pi = new MockPi()
  pi.configuration = configurationResult({ pruningEnabled: false })
  const sink = new MemorySink()
  await start(pi, sink)
  const input = { command: "git status" }
  await call(pi, "call-7", "bash", input)
  assert.equal(input.command, "git status")
  assert.equal(pi.planArgs, null)
  assert.equal(sink.begins.length, 1)
  const patch = await pi.registry.emit(
    "tool_result",
    {
      type: "tool_result",
      toolCallId: "call-7",
      toolName: "bash",
      input,
      content: [{ type: "text", text: "uncapped\n".repeat(1_000) }],
      details: undefined,
      isError: false,
    },
    context,
  )
  assert.equal(patch, undefined)
  assert.deepEqual(sink.resultTexts, [])
})

test("archives every built-in and custom tool name", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  const tools = [
    "bash",
    "exec_command",
    "read",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "custom_tool",
  ]
  for (const [index, toolName] of tools.entries()) {
    const input = toolName === "bash"
      ? { command: "echo unsupported" }
      : toolName === "exec_command"
        ? { cmd: "echo unsupported" }
        : { value: index }
    await call(pi, `call-tool-${index}`, toolName, input)
  }
  assert.deepEqual(sink.begins.map((entry) => entry.call.toolName), tools)
})

test("correlates parallel results by call id", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await Promise.all([
    call(pi, "parallel-a", "read", { path: "a" }),
    call(pi, "parallel-b", "read", { path: "b" }),
  ])
  await Promise.all([
    pi.registry.emit(
      "tool_result",
      {
        type: "tool_result",
        toolCallId: "parallel-b",
        toolName: "read",
        input: { path: "b" },
        content: [{ type: "text", text: "b" }],
        details: undefined,
        isError: false,
      },
      context,
    ),
    pi.registry.emit(
      "tool_result",
      {
        type: "tool_result",
        toolCallId: "parallel-a",
        toolName: "read",
        input: { path: "a" },
        content: [{ type: "text", text: "a" }],
        details: undefined,
        isError: true,
      },
      context,
    ),
  ])
  assert.equal(sink.stagedResults.length, 2)
  const errors = sink.stagedResults.map((value) => {
    if (typeof value !== "object" || value === null || !("isError" in value)) {
      throw new Error("missing isError")
    }
    assert.equal(typeof value.isError, "boolean")
    return value.isError
  })
  assert.deepEqual(errors.sort(), [false, true])
})

test("reload closes the old writer and uses a new one", async () => {
  const pi = new MockPi()
  const first = new MemorySink()
  const second = new MemorySink()
  const sinks = [first, second]
  await installYarpExtension(pi, () => {
    const sink = sinks.shift()
    if (sink === undefined) throw new Error("unexpected sink request")
    return sink
  })
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    context,
  )
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "reload" },
    context,
  )
  await call(pi, "after-reload", "read", { path: "README.md" })
  assert.equal(first.closed, true)
  assert.equal(first.begins.length, 0)
  assert.equal(second.begins.length, 1)
})

function resultPatchText(
  patch: ExtensionEventResultMap["tool_result"] | void,
): string {
  if (patch === undefined || !("content" in patch) || patch.content === undefined) {
    throw new Error("missing result patch content")
  }
  return patch.content
    .map((item) => isRecord(item) && item["type"] === "text" && typeof item["text"] === "string"
      ? item["text"]
      : "")
    .join("")
}

test("session shutdown closes the archive writer", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  await start(pi, sink)
  await pi.registry.emit(
    "session_shutdown",
    { type: "session_shutdown", reason: "quit" },
    context,
  )
  assert.equal(sink.closed, true)
})
