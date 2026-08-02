import assert from "node:assert/strict"
import { Buffer } from "node:buffer"
import { EventEmitter } from "node:events"
import { PassThrough, Writable } from "node:stream"
import test from "node:test"
import { ResultReducerClient } from "./result-client.js"

class FakeResultProcess extends EventEmitter {
  readonly stdout = new PassThrough()
  readonly stderr = new PassThrough()
  readonly stdin: Writable
  exitCode: number | null = null
  request: Record<string, unknown> | null = null
  private input = Buffer.alloc(0)

  constructor(private readonly response: Record<string, unknown> | Buffer) {
    super()
    this.stdin = new Writable({
      write: (chunk: Buffer, _encoding, callback) => {
        this.input = Buffer.concat([this.input, chunk])
        callback()
      },
      final: (callback) => {
        try {
          this.finish()
          callback()
        } catch (error) {
          callback(error instanceof Error ? error : new Error(String(error)))
        }
      },
    })
  }

  kill(signal: NodeJS.Signals = "SIGTERM"): boolean {
    if (this.exitCode !== null) return false
    this.exitCode = 1
    queueMicrotask(() => this.emit("exit", null, signal))
    return true
  }

  private finish(): void {
    const length = Number(this.input.readBigUInt64BE(0))
    const value: unknown = JSON.parse(this.input.subarray(8, 8 + length).toString("utf8"))
    if (!isRecord(value)) throw new Error("invalid request")
    this.request = value
    if (Buffer.isBuffer(this.response)) {
      this.stdout.write(this.response)
    } else {
      const body = Buffer.from(JSON.stringify(this.response))
      const header = Buffer.allocUnsafe(8)
      header.writeBigUInt64BE(BigInt(body.length), 0)
      this.stdout.write(Buffer.concat([header, body]))
    }
    this.exitCode = 0
    queueMicrotask(() => this.emit("exit", 0, null))
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

const request = {
  command: "cargo test",
  text: "raw output",
  isError: false,
  exitCode: 0,
  archiveRef: "yr_0123456789abcdef0123456789abcdef",
  sourceCompleteness: "unknown" as const,
  preferArchiveSource: false,
}

test("sends and validates one bounded framed result-reducer request", async () => {
  const process = new FakeResultProcess({
    schemaVersion: 1,
    changed: true,
    content: "summary",
    source: "result_text",
    sourceCompleteness: "unknown",
    needsResultText: true,
  })
  const client = new ResultReducerClient(() => process)
  const response = await client.reduce(request)
  assert.deepEqual(response, {
    changed: true,
    content: "summary",
    source: "result_text",
    sourceCompleteness: "unknown",
    needsResultText: true,
  })
  assert.equal(process.request?.["schemaVersion"], 1)
  assert.equal(process.request?.["command"], "cargo test")
})

test("rejects malformed responses without returning unverified content", async () => {
  const process = new FakeResultProcess(Buffer.from("short"))
  const client = new ResultReducerClient(() => process)
  await assert.rejects(client.reduce(request), /truncated/)
})

test("rejects inconsistent recovery metadata", async () => {
  const process = new FakeResultProcess({
    schemaVersion: 1,
    changed: true,
    content: "summary",
    source: "source_output",
    sourceCompleteness: "complete",
    needsResultText: true,
  })
  const client = new ResultReducerClient(() => process)
  await assert.rejects(client.reduce(request), /recovery state/)
})
