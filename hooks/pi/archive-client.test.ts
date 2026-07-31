import assert from "node:assert/strict"
import { Buffer } from "node:buffer"
import { EventEmitter } from "node:events"
import { PassThrough, Writable } from "node:stream"
import test from "node:test"
import {
  ArchiveClient,
  type ArchiveSession,
  type ArchiveWriterProcess,
} from "./archive-client.js"

type Request = Record<string, unknown>
type RequestHandler = (request: Request, process: FakeProcess) => void

class FakeProcess extends EventEmitter implements ArchiveWriterProcess {
  readonly stdout = new PassThrough()
  readonly stderr = new PassThrough()
  readonly stdin: Writable
  exitCode: number | null = null
  requests: Request[] = []
  private buffer = Buffer.alloc(0)

  constructor(private readonly handleRequest: RequestHandler) {
    super()
    this.stdout.setEncoding("utf8")
    this.stderr.setEncoding("utf8")
    this.stdin = new Writable({
      write: (chunk: Buffer, _encoding, callback) => {
        try {
          this.accept(chunk)
          callback()
        } catch (error) {
          callback(error instanceof Error ? error : new Error(String(error)))
        }
      },
      final: (callback) => {
        this.exit(0, null)
        callback()
      },
    })
  }

  kill(signal: NodeJS.Signals = "SIGTERM"): boolean {
    if (this.exitCode !== null) return false
    this.exit(null, signal)
    return true
  }

  acknowledge(request: Request, ok = true, error?: string): void {
    const requestId = request.requestId
    assert.equal(typeof requestId, "number")
    this.stdout.write(`${JSON.stringify({ requestId, ok, error })}\n`)
  }

  exit(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.exitCode !== null) return
    this.exitCode = code ?? 1
    queueMicrotask(() => this.emit("exit", code, signal))
  }

  private accept(chunk: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, chunk])
    while (this.buffer.length >= 8) {
      const length = Number(this.buffer.readBigUInt64BE(0))
      if (this.buffer.length < 8 + length) return
      const body = this.buffer.subarray(8, 8 + length)
      this.buffer = this.buffer.subarray(8 + length)
      const value: unknown = JSON.parse(body.toString("utf8"))
      assert.equal(isRecord(value), true)
      if (!isRecord(value)) throw new Error("request is not an object")
      this.requests.push(value)
      this.handleRequest(value, this)
    }
  }
}

const session: ArchiveSession = {
  agent: "pi",
  account: "onur",
  sourceSessionId: "session-1",
  startedAtMs: 1,
}

const call = {
  sourceCallId: "call-1",
  toolName: "read",
  workingDirectory: "/repo",
  startedAtMs: 2,
}

test("sends framed requests and waits for acknowledgements", async () => {
  const process = new FakeProcess((request, writer) => writer.acknowledge(request))
  const client = new ArchiveClient(() => process)
  await client.beginCall(session, call, { path: "a" }, { path: "a" }, 2)
  assert.equal(process.requests.length, 1)
  assert.equal(process.requests[0]?.operation, "begin_call")
  assert.equal(process.requests[0]?.requestId, 1)
  await client.close()
})

test("restarts once and reuses the request id after transport failure", async () => {
  const processes: FakeProcess[] = []
  const client = new ArchiveClient(() => {
    const index = processes.length
    const process = new FakeProcess((request, writer) => {
      if (index === 0) writer.exit(1, null)
      else writer.acknowledge(request)
    })
    processes.push(process)
    return process
  })

  await client.beginCall(session, call, {}, {}, 2)
  assert.equal(processes.length, 2)
  assert.equal(processes[0]?.requests[0]?.requestId, 1)
  assert.equal(processes[1]?.requests[0]?.requestId, 1)
  await client.close()
})

test("does not retry a rejected archive operation", async () => {
  let starts = 0
  const client = new ArchiveClient(() => {
    starts += 1
    return new FakeProcess((request, writer) => {
      writer.acknowledge(request, false, "snapshot conflict")
    })
  })

  await assert.rejects(
    client.beginCall(session, call, {}, {}, 2),
    /snapshot conflict/,
  )
  assert.equal(starts, 1)
  await client.close()
})

test("serializes concurrent requests", async () => {
  const seen: number[] = []
  const process = new FakeProcess((request, writer) => {
    const requestId = request.requestId
    assert.equal(typeof requestId, "number")
    if (typeof requestId !== "number") return
    seen.push(requestId)
    setImmediate(() => writer.acknowledge(request))
  })
  const client = new ArchiveClient(() => process)

  await Promise.all([
    client.beginCall(session, call, {}, {}, 2),
    client.resultBefore(session, "call-1", { content: "before" }, 3),
    client.finishCall(session, "call-1", { content: "after" }, false, 4),
  ])
  assert.deepEqual(seen, [1, 2, 3])
  await client.close()
})

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}
