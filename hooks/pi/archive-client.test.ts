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
  signalCode: NodeJS.Signals | null = null
  requests: Request[] = []
  private buffer = Buffer.alloc(0)
  private exited = false

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
    const returnsReference = request.operation === "begin_call" || request.operation === "result_text"
    this.stdout.write(`${JSON.stringify({
      requestId,
      ok,
      ...(ok && returnsReference
        ? { archiveRef: "yr_0123456789abcdef0123456789abcdef" }
        : {}),
      ...(error === undefined ? {} : { error }),
    })}\n`)
  }

  exit(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.exited) return
    this.exited = true
    this.exitCode = code
    this.signalCode = signal
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
  requiresStreams: false,
}

test("sends framed requests and waits for acknowledgements", async () => {
  const process = new FakeProcess((request, writer) => writer.acknowledge(request))
  const client = new ArchiveClient(() => process)
  const archiveRef = await client.beginCall(session, call, { path: "a" }, { path: "a" }, 2)
  assert.equal(archiveRef, "yr_0123456789abcdef0123456789abcdef")
  assert.equal(process.requests.length, 1)
  assert.equal(process.requests[0]?.operation, "begin_call")
  assert.equal(process.requests[0]?.requestId, 1)
  assert.equal(process.requests[0]?.schemaVersion, 1)
  await client.close()
})

test("leaves transport recovery to the Rust bridge", async () => {
  const processes: FakeProcess[] = []
  const client = new ArchiveClient(() => {
    const process = new FakeProcess((_request, writer) => writer.exit(1, null))
    processes.push(process)
    return process
  })

  await assert.rejects(client.beginCall(session, call, {}, {}, 2), /exited with code 1/)
  assert.equal(processes.length, 1)
  assert.equal(processes[0]?.requests[0]?.requestId, 1)
  await client.close()
})

test("does not restart after an asynchronous writer pipe error", async () => {
  const processes: FakeProcess[] = []
  const client = new ArchiveClient(() => {
    const process = new FakeProcess((_request, writer) => {
      queueMicrotask(() => writer.stdin.emit("error", new Error("broken pipe")))
    })
    processes.push(process)
    return process
  })

  await assert.rejects(client.beginCall(session, call, {}, {}, 2), /broken pipe/)
  assert.equal(processes.length, 1)
  await client.close()
})

test("rejects oversized requests before starting the writer", async () => {
  let starts = 0
  const client = new ArchiveClient(() => {
    starts += 1
    return new FakeProcess((request, writer) => writer.acknowledge(request))
  }, 64)

  await assert.rejects(
    client.beginCall(session, call, { content: "x".repeat(128) }, {}, 2),
    /maximum is 64/,
  )
  assert.equal(starts, 0)
  await client.close()
})

test("fails once when acknowledgements time out", async () => {
  let starts = 0
  const client = new ArchiveClient(() => {
    starts += 1
    return new FakeProcess(() => undefined)
  }, 1024, 10)

  await assert.rejects(
    client.beginCall(session, call, {}, {}, 2),
    /acknowledgement timed out/,
  )
  assert.equal(starts, 1)
  await client.close()
})

test("starts a new writer after an acknowledgement timeout", async () => {
  const processes: FakeProcess[] = []
  const client = new ArchiveClient(() => {
    const process = new FakeProcess((request, writer) => {
      if (processes.length > 1) writer.acknowledge(request)
    })
    processes.push(process)
    return process
  }, 1024, 10)

  await assert.rejects(client.beginCall(session, call, {}, {}, 2), /acknowledgement timed out/)
  const archiveRef = await client.beginCall(
    session,
    { ...call, sourceCallId: "call-2" },
    {},
    {},
    3,
  )

  assert.equal(archiveRef, "yr_0123456789abcdef0123456789abcdef")
  assert.equal(processes.length, 2)
  assert.equal(processes[0]?.signalCode, "SIGTERM")
  await client.close()
})

test("counts serialized queue time against the initial capture deadline", async () => {
  const processes: FakeProcess[] = []
  const client = new ArchiveClient(() => {
    const process = new FakeProcess((request, writer) => {
      if (request.operation === "result_before") {
        setTimeout(() => writer.acknowledge(request), 150)
      }
    })
    processes.push(process)
    return process
  }, 1024, 200)

  const earlier = client.resultBefore(session, "earlier-call", {}, 2)
  const startedAt = Date.now()
  await assert.rejects(
    client.beginCall(session, call, {}, {}, 2),
    /acknowledgement timed out after 200 ms/,
  )
  const elapsedMs = Date.now() - startedAt
  await earlier

  assert.equal(processes.length, 1)
  assert.equal(processes[0]?.requests.length, 2)
  assert.ok(elapsedMs >= 180, `initial deadline fired too early after ${elapsedMs} ms`)
  assert.ok(elapsedMs < 300, `queue wait was excluded from the ${elapsedMs} ms deadline`)
  await client.close()
})

test("returns at the initial deadline while an earlier archive request remains queued", async () => {
  const process = new FakeProcess(() => undefined)
  const client = new ArchiveClient(() => process, 1024, 200, 20)

  const earlier = client.resultBefore(session, "earlier-call", {}, 2)
  const startedAt = Date.now()
  await assert.rejects(
    client.beginCall(session, call, {}, {}, 2),
    /acknowledgement timed out after 20 ms/,
  )
  const elapsedMs = Date.now() - startedAt

  assert.ok(elapsedMs >= 10, `initial deadline fired too early after ${elapsedMs} ms`)
  assert.ok(elapsedMs < 100, `caller waited ${elapsedMs} ms for the earlier queue item`)
  assert.equal(process.requests.length, 1)
  await assert.rejects(earlier, /acknowledgement timed out after 200 ms/)
  assert.equal(process.requests.length, 1)
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

test("ignores duplicate acknowledgements after a request commits", async () => {
  const process = new FakeProcess((request, writer) => {
    writer.acknowledge(request)
    writer.acknowledge(request)
  })
  const client = new ArchiveClient(() => process)
  await client.beginCall(session, call, {}, {}, 2)
  await client.resultBefore(
    session,
    "call-1",
    { content: "before" },
    3,
    "/tmp/pi-full.log",
  )
  assert.equal(process.requests.length, 2)
  assert.equal(process.requests[1]?.fullOutputPath, "/tmp/pi-full.log")
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
    client.stageResult(session, "call-1", { content: "staged" }, false, 4),
    client.finishCall(session, "call-1", { content: "preflight" }, true, false, 5),
    client.updateFinalResult(session, "call-1", { content: "final" }, false, 6),
  ])
  assert.deepEqual(seen, [1, 2, 3, 4, 5])
  await client.close()
})

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}
