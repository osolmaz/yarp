import assert from "node:assert/strict"
import test from "node:test"
import type {
  ExecOptions,
  ExecResult,
  ExtensionAPI,
  ToolCallContext,
  ToolCallEvent,
} from "@earendil-works/pi-coding-agent"
import yarpExtension, { commandBinding } from "./yarp.js"

type Handler = (event: ToolCallEvent, context: ToolCallContext) => Promise<void> | void

class MockPi implements ExtensionAPI {
  handler: Handler | null = null
  rewrite: ExecResult = result(3)
  failRewrite = false

  async exec(command: string, args: string[], _options?: ExecOptions): Promise<ExecResult> {
    assert.equal(command, "yarp")
    if (args[0] === "--version") return result(0, "yarp 0.1.0\n")
    if (this.failRewrite) throw new Error("rewrite failed")
    return this.rewrite
  }

  on(event: "tool_call", handler: Handler): void {
    assert.equal(event, "tool_call")
    this.handler = handler
  }

  async call(toolName: string, input: unknown): Promise<void> {
    assert.notEqual(this.handler, null)
    await this.handler?.(
      { toolName, input },
      { signal: new AbortController().signal },
    )
  }
}

function result(code: number, stdout = ""): ExecResult {
  return { code, stdout, stderr: "", killed: false }
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

test("rewrites supported commands for both shell tools", async () => {
  const pi = new MockPi()
  pi.rewrite = result(0, "yarp run -- git status\n")
  await yarpExtension(pi)

  const bash = { command: "git status" }
  await pi.call("bash", bash)
  assert.equal(bash.command, "yarp run -- git status")

  const exec = { cmd: "git status" }
  await pi.call("exec_command", exec)
  assert.equal(exec.cmd, "yarp run -- git status")
})

test("leaves commands unchanged when rewriting is unsupported or fails", async () => {
  const pi = new MockPi()
  await yarpExtension(pi)

  const unsupported = { command: "cat .env" }
  await pi.call("bash", unsupported)
  assert.equal(unsupported.command, "cat .env")

  pi.failRewrite = true
  const failed = { command: "git status" }
  await pi.call("bash", failed)
  assert.equal(failed.command, "git status")

  const alreadyWrapped = { command: "yarp run -- git status" }
  await pi.call("bash", alreadyWrapped)
  assert.equal(alreadyWrapped.command, "yarp run -- git status")
})

test("respects the disable switch", async () => {
  const pi = new MockPi()
  pi.rewrite = result(0, "yarp run -- git status")
  await yarpExtension(pi)
  process.env.YARP_DISABLED = "1"
  try {
    const input = { command: "git status" }
    await pi.call("bash", input)
    assert.equal(input.command, "git status")
  } finally {
    delete process.env.YARP_DISABLED
  }
})
