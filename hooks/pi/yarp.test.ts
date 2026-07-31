import assert from "node:assert/strict"
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
import { commandBinding, installYarpExtension } from "./yarp.js"

type EventName = keyof ExtensionEventMap
type Handler<K extends EventName> = (
  event: ExtensionEventMap[K],
  context: ExtensionContext,
) => Promise<ExtensionEventResultMap[K] | void> | ExtensionEventResultMap[K] | void

class HandlerRegistry {
  private readonly handlers: { [K in EventName]: Array<Handler<K>> } = {
    session_start: [],
    session_shutdown: [],
    tool_call: [],
    tool_result: [],
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
  rewrite: ExecResult = result(3)
  restore: ExecResult = result(0, "raw output\n")
  failRewrite = false
  rewriteArgs: string[] | null = null

  async exec(command: string, args: string[], _options?: ExecOptions): Promise<ExecResult> {
    assert.equal(command, "yarp")
    if (args[0] === "--version") return result(0, "yarp 0.1.0\n")
    if (args[0] === "archive" && args[1] === "restore") return this.restore
    this.rewriteArgs = args
    if (this.failRewrite) throw new Error("rewrite failed")
    return this.rewrite
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
  readonly finishedResults: unknown[] = []
  closed = false
  failBegin = false
  failBefore = false
  failFinish = false
  finishRequiresPreResult: boolean[] = []

  async beginCall(
    session: ArchiveSession,
    call: ArchiveCall,
    inputBefore: unknown,
    inputAfter: unknown,
  ): Promise<void> {
    if (this.failBegin) throw new Error("archive unavailable")
    this.begins.push({ session, call, inputBefore, inputAfter })
  }

  async resultBefore(
    _session: ArchiveSession,
    _sourceCallId: string,
    resultValue: unknown,
  ): Promise<void> {
    if (this.failBefore) throw new Error("before failed")
    this.beforeResults.push(resultValue)
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

  async close(): Promise<void> {
    this.closed = true
  }
}

const context: ExtensionContext = {
  signal: new AbortController().signal,
  cwd: "/repo",
  sessionManager: { getSessionId: () => "session-1" },
  model: { provider: "openai", id: "gpt" },
}

function result(code: number, stdout = ""): ExecResult {
  return { code, stdout, stderr: "", killed: false }
}

async function start(pi: MockPi, sink: MemorySink): Promise<void> {
  await installYarpExtension(pi, () => sink)
  await pi.registry.emit(
    "session_start",
    { type: "session_start", reason: "startup" },
    context,
  )
}

async function call(
  pi: MockPi,
  toolCallId: string,
  toolName: string,
  input: Record<string, unknown>,
): Promise<void> {
  await pi.registry.emit(
    "tool_call",
    { type: "tool_call", toolCallId, toolName, input },
    context,
  )
}

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

test("archives and rewrites supported shell calls", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.rewrite = result(
    0,
    "yarp run --archive-agent 'pi' --archive-account 'onur' --archive-session 'session-1' --archive-call 'call-1' -- git status\n",
  )
  await start(pi, sink)

  const input = { cmd: "git status" }
  await call(pi, "call-1", "exec_command", input)

  assert.match(input.cmd, /^yarp run --archive-agent/)
  assert.equal(sink.begins.length, 1)
  assert.equal(sink.begins[0]?.call.requiresStreams, true)
  assert.deepEqual(sink.begins[0]?.inputBefore, { cmd: "git status" })
  assert.deepEqual(sink.begins[0]?.inputAfter, { cmd: input.cmd })
  assert.deepEqual(pi.rewriteArgs, [
    "rewrite",
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
  assert.equal(sink.finishedResults.length, 1)
  assert.deepEqual(sink.finishedResults[0], sink.beforeResults[0])
  assert.deepEqual(sink.finishRequiresPreResult, [true])
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

test("archive start failure blocks tool mutation", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  sink.failBegin = true
  pi.rewrite = result(0, "yarp run -- git status")
  await start(pi, sink)
  const input = { command: "git status" }
  await assert.rejects(call(pi, "call-4", "bash", input), /archive unavailable/)
  assert.equal(input.command, "git status")
})

test("rewrite failures keep the original command but still archive", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.failRewrite = true
  await start(pi, sink)
  const input = { command: "git status" }
  await call(pi, "call-5", "bash", input)
  assert.equal(input.command, "git status")
  assert.equal(sink.begins.length, 1)
  assert.equal(sink.begins[0]?.call.requiresStreams, false)
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
  sink.failFinish = true
  pi.rewrite = result(0, "yarp run --archive-call 'call-restore' -- git status")
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
})

test("archive opt-out keeps rewriting without archive metadata", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  pi.rewrite = result(0, "yarp run -- git status")
  process.env["YARP_ARCHIVE_DISABLED"] = "1"
  try {
    await installYarpExtension(pi, () => sink)
    await pi.registry.emit(
      "session_start",
      { type: "session_start", reason: "startup" },
      context,
    )
    const input = { command: "git status" }
    await call(pi, "call-6", "bash", input)
    assert.equal(input.command, "yarp run -- git status")
    assert.deepEqual(pi.rewriteArgs, ["rewrite", "git status"])
    assert.equal(sink.begins.length, 0)
  } finally {
    delete process.env["YARP_ARCHIVE_DISABLED"]
  }
})

test("pruning opt-out still archives every call", async () => {
  const pi = new MockPi()
  const sink = new MemorySink()
  process.env["YARP_DISABLED"] = "1"
  try {
    await start(pi, sink)
    const input = { command: "git status" }
    await call(pi, "call-7", "bash", input)
    assert.equal(input.command, "git status")
    assert.equal(pi.rewriteArgs, null)
    assert.equal(sink.begins.length, 1)
  } finally {
    delete process.env["YARP_DISABLED"]
  }
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
  assert.equal(sink.finishedResults.length, 2)
  const errors = sink.finishedResults.map((value) => {
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
