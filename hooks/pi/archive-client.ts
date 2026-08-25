import { spawn } from "node:child_process"
import { once, type EventEmitter } from "node:events"
import { Buffer } from "node:buffer"
import type { Readable, Writable } from "node:stream"

const MAX_ACK_BUFFER_BYTES = 64 * 1024
const CLOSE_TIMEOUT_MS = 2_000
const INGEST_SCHEMA_VERSION = 1
const MAX_FRAME_BYTES = 256 * 1024 * 1024
const ACK_TIMEOUT_MS = 30_000
const BEGIN_ACK_TIMEOUT_MS = 2_000

export type ArchiveSession = {
  agent: string
  account: string
  sourceSessionId: string
  startedAtMs?: number
}

export type ArchiveCall = {
  sourceCallId: string
  toolName: string
  provider?: string
  model?: string
  workingDirectory?: string
  startedAtMs: number
  requiresStreams: boolean
}

export interface ArchiveSink {
  beginCall(
    session: ArchiveSession,
    call: ArchiveCall,
    inputBefore: unknown,
    inputAfter: unknown,
    capturedAtMs: number,
  ): Promise<string>
  resultBefore(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    capturedAtMs: number,
    fullOutputPath?: string,
  ): Promise<void>
  resultText(
    session: ArchiveSession,
    sourceCallId: string,
    text: string,
    sourceCompleteness: "complete" | "incomplete" | "unknown",
    capturedAtMs: number,
  ): Promise<string>
  stageResult(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    capturedAtMs: number,
  ): Promise<void>
  finishCall(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    requirePreResult: boolean,
    finishedAtMs: number,
  ): Promise<void>
  updateFinalResult(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    finishedAtMs: number,
  ): Promise<void>
  close(): Promise<void>
}

export interface ArchiveWriterProcess extends EventEmitter {
  stdin: Writable
  stdout: Readable
  stderr: Readable
  exitCode: number | null
  kill(signal?: NodeJS.Signals): boolean
}

type SpawnWriter = () => ArchiveWriterProcess

type Pending = {
  resolve: (archiveRef: string | undefined) => void
  reject: (error: Error) => void
}

class ArchiveRejectedError extends Error {}

type Ack = {
  requestId: number
  ok: boolean
  archiveRef?: string
  error?: string
}

export class ArchiveClient implements ArchiveSink {
  private child: ArchiveWriterProcess | null = null
  private nextRequestId = 1
  private pending = new Map<number, Pending>()
  private ackBuffer = ""
  private stderr = ""
  private queue: Promise<void> = Promise.resolve()
  private closing = false

  constructor(
    private readonly spawnWriter: SpawnWriter = defaultSpawnWriter,
    private readonly maxFrameBytes: number = MAX_FRAME_BYTES,
    private readonly ackTimeoutMs: number = ACK_TIMEOUT_MS,
    private readonly beginAckTimeoutMs: number = BEGIN_ACK_TIMEOUT_MS,
  ) {}

  beginCall(
    session: ArchiveSession,
    call: ArchiveCall,
    inputBefore: unknown,
    inputAfter: unknown,
    capturedAtMs: number,
  ): Promise<string> {
    const timeoutMs = Math.min(this.ackTimeoutMs, this.beginAckTimeoutMs)
    return this.send(
      {
        operation: "begin_call",
        session,
        call,
        inputBefore,
        inputAfter,
        capturedAtMs,
      },
      timeoutMs,
      Date.now() + timeoutMs,
    ).then(requireArchiveRef)
  }

  resultBefore(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    capturedAtMs: number,
    fullOutputPath?: string,
  ): Promise<void> {
    return this.send({
      operation: "result_before",
      session,
      sourceCallId,
      result,
      capturedAtMs,
      ...(fullOutputPath === undefined ? {} : { fullOutputPath }),
    }).then(ignoreAck)
  }

  resultText(
    session: ArchiveSession,
    sourceCallId: string,
    text: string,
    sourceCompleteness: "complete" | "incomplete" | "unknown",
    capturedAtMs: number,
  ): Promise<string> {
    return this.send({
      operation: "result_text",
      session,
      sourceCallId,
      text,
      sourceCompleteness,
      capturedAtMs,
    }).then(requireArchiveRef)
  }

  stageResult(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    capturedAtMs: number,
  ): Promise<void> {
    return this.send({
      operation: "stage_result",
      session,
      sourceCallId,
      result,
      isError,
      capturedAtMs,
    }).then(ignoreAck)
  }

  finishCall(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    requirePreResult: boolean,
    finishedAtMs: number,
  ): Promise<void> {
    return this.send({
      operation: "finish_call",
      session,
      sourceCallId,
      result,
      isError,
      requirePreResult,
      finishedAtMs,
    }).then(ignoreAck)
  }

  updateFinalResult(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    isError: boolean,
    finishedAtMs: number,
  ): Promise<void> {
    return this.send({
      operation: "update_final_result",
      session,
      sourceCallId,
      result,
      isError,
      finishedAtMs,
    }).then(ignoreAck)
  }

  async close(): Promise<void> {
    this.closing = true
    await this.queue.catch(() => undefined)
    const child = this.child
    this.child = null
    if (child === null || child.exitCode !== null) return
    child.stdin.end()
    const timer = setTimeout(() => child.kill("SIGTERM"), CLOSE_TIMEOUT_MS)
    try {
      await once(child, "exit")
    } finally {
      clearTimeout(timer)
    }
  }

  private send(
    operation: Record<string, unknown>,
    timeoutMs = this.ackTimeoutMs,
    deadlineAtMs?: number,
  ): Promise<string | undefined> {
    if (this.closing) return Promise.reject(new Error("YARP archive client is closing"))
    const requestId = this.nextRequestId++
    const request = { ...operation, requestId, schemaVersion: INGEST_SCHEMA_VERSION }
    const queuedTask = this.queue.then(() => {
      const remainingMs = deadlineAtMs === undefined ? timeoutMs : deadlineAtMs - Date.now()
      if (remainingMs <= 0) {
        throw new Error(`archive acknowledgement timed out after ${timeoutMs} ms`)
      }
      return this.sendOnce(requestId, request, remainingMs, timeoutMs)
    })
    this.queue = queuedTask.then(ignoreAck, () => undefined)
    if (deadlineAtMs === undefined) return queuedTask
    const remainingMs = deadlineAtMs - Date.now()
    if (remainingMs <= 0) {
      return Promise.reject(new Error(`archive acknowledgement timed out after ${timeoutMs} ms`))
    }
    return new Promise<string | undefined>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`archive acknowledgement timed out after ${timeoutMs} ms`)),
        remainingMs,
      )
      void queuedTask.then(
        (archiveRef) => {
          clearTimeout(timer)
          resolve(archiveRef)
        },
        (error: unknown) => {
          clearTimeout(timer)
          reject(error instanceof Error ? error : new Error(String(error)))
        },
      )
    })
  }

  private async sendOnce(
    requestId: number,
    request: Record<string, unknown>,
    timeoutMs: number,
    reportedTimeoutMs = timeoutMs,
  ): Promise<string | undefined> {
    const body = Buffer.from(JSON.stringify(request), "utf8")
    if (body.length === 0 || body.length > this.maxFrameBytes) {
      throw new ArchiveRejectedError(
        `archive request is ${body.length} bytes; maximum is ${this.maxFrameBytes}`,
      )
    }
    const child = this.ensureChild()
    const header = Buffer.allocUnsafe(8)
    header.writeBigUInt64BE(BigInt(body.length), 0)

    const acknowledgement = new Promise<string | undefined>((resolve, reject) => {
      const timer = setTimeout(() => {
        const pending = this.pending.get(requestId)
        if (pending === undefined) return
        this.pending.delete(requestId)
        pending.reject(new Error(`archive acknowledgement timed out after ${reportedTimeoutMs} ms`))
        if (this.child === child) {
          this.child = null
          child.kill("SIGTERM")
        }
      }, timeoutMs)
      this.pending.set(requestId, {
        resolve: (archiveRef) => {
          clearTimeout(timer)
          resolve(archiveRef)
        },
        reject: (error) => {
          clearTimeout(timer)
          reject(error)
        },
      })
    })
    try {
      if (!child.stdin.write(header)) await once(child.stdin, "drain")
      if (!child.stdin.write(body)) await once(child.stdin, "drain")
    } catch (error) {
      this.pending.delete(requestId)
      throw error
    }
    return acknowledgement
  }

  private ensureChild(): ArchiveWriterProcess {
    const current = this.child
    if (current !== null && current.exitCode === null) return current

    const child = this.spawnWriter()
    this.child = child
    this.ackBuffer = ""
    this.stderr = ""
    child.stdout.setEncoding("utf8")
    child.stderr.setEncoding("utf8")
    child.stdin.on("error", (error: Error) => {
      if (this.child === child) {
        this.child = null
        this.rejectPending(error)
      }
    })
    child.stdout.on("data", (chunk: string) => {
      if (this.child === child) this.receiveAcks(chunk)
    })
    child.stderr.on("data", (chunk: string) => {
      if (this.child === child) {
        this.stderr = `${this.stderr}${chunk}`.slice(-MAX_ACK_BUFFER_BYTES)
      }
    })
    child.on("error", (error: Error) => {
      if (this.child === child) {
        this.child = null
        this.rejectPending(error)
      }
    })
    child.on("exit", (code: number | null, signal: NodeJS.Signals | null) => {
      if (this.child !== child) return
      const suffix = this.stderr.trim()
      const detail = suffix === "" ? "" : `: ${suffix}`
      this.child = null
      this.rejectPending(
        new Error(
          `archive writer exited with ${signal === null ? `code ${String(code)}` : signal}${detail}`,
        ),
      )
    })
    return child
  }

  private receiveAcks(chunk: string): void {
    this.ackBuffer += chunk
    if (this.ackBuffer.length > MAX_ACK_BUFFER_BYTES) {
      const child = this.child
      this.child = null
      this.rejectPending(new Error("archive writer acknowledgement exceeded 64 KiB"))
      child?.kill("SIGTERM")
      return
    }
    for (;;) {
      const newline = this.ackBuffer.indexOf("\n")
      if (newline < 0) return
      const line = this.ackBuffer.slice(0, newline)
      this.ackBuffer = this.ackBuffer.slice(newline + 1)
      const ack = parseAck(line)
      if (ack === null) {
        const child = this.child
        this.child = null
        this.rejectPending(new Error("archive writer returned an invalid acknowledgement"))
        child?.kill("SIGTERM")
        return
      }
      const pending = this.pending.get(ack.requestId)
      if (pending === undefined) continue
      this.pending.delete(ack.requestId)
      if (ack.ok) pending.resolve(ack.archiveRef)
      else pending.reject(new ArchiveRejectedError(ack.error ?? "archive writer rejected the request"))
    }
  }

  private rejectPending(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
  }
}

function defaultSpawnWriter(): ArchiveWriterProcess {
  return spawn("yarp", ["archive", "ingest"], {
    stdio: ["pipe", "pipe", "pipe"],
  })
}

function parseAck(line: string): Ack | null {
  let value: unknown
  try {
    value = JSON.parse(line)
  } catch {
    return null
  }
  if (!isRecord(value)) return null
  const requestId = value["requestId"]
  const ok = value["ok"]
  const archiveRef = value["archiveRef"]
  const error = value["error"]
  if (typeof requestId !== "number" || !Number.isSafeInteger(requestId)) return null
  if (typeof ok !== "boolean") return null
  if (archiveRef !== undefined && !isArchiveRef(archiveRef)) return null
  if (error !== undefined && typeof error !== "string") return null
  return {
    requestId,
    ok,
    ...(archiveRef === undefined ? {} : { archiveRef }),
    ...(error === undefined ? {} : { error }),
  }
}

function requireArchiveRef(value: string | undefined): string {
  if (value === undefined) throw new Error("archive writer omitted the call reference")
  return value
}

function ignoreAck(_value: string | undefined): void {}

function isArchiveRef(value: unknown): value is string {
  return typeof value === "string" && /^yr_[0-9a-f]{32}$/u.test(value)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}
