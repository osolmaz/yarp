import { spawn } from "node:child_process"
import { once, type EventEmitter } from "node:events"
import { Buffer } from "node:buffer"
import type { Readable, Writable } from "node:stream"

const MAX_ACK_BUFFER_BYTES = 64 * 1024
const CLOSE_TIMEOUT_MS = 2_000
const INGEST_SCHEMA_VERSION = 1
const MAX_FRAME_BYTES = 256 * 1024 * 1024
const ACK_TIMEOUT_MS = 30_000

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
  ): Promise<void>
  resultBefore(
    session: ArchiveSession,
    sourceCallId: string,
    result: unknown,
    capturedAtMs: number,
    fullOutputPath?: string,
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
  resolve: () => void
  reject: (error: Error) => void
}

class ArchiveRejectedError extends Error {}

type Ack = {
  requestId: number
  ok: boolean
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
  ) {}

  beginCall(
    session: ArchiveSession,
    call: ArchiveCall,
    inputBefore: unknown,
    inputAfter: unknown,
    capturedAtMs: number,
  ): Promise<void> {
    return this.send({
      operation: "begin_call",
      session,
      call,
      inputBefore,
      inputAfter,
      capturedAtMs,
    })
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
    })
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
    })
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
    })
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

  private send(operation: Record<string, unknown>): Promise<void> {
    if (this.closing) return Promise.reject(new Error("YARP archive client is closing"))
    const requestId = this.nextRequestId++
    const request = { ...operation, requestId, schemaVersion: INGEST_SCHEMA_VERSION }
    const task = this.queue.then(() => this.sendWithRetry(requestId, request))
    this.queue = task.catch(() => undefined)
    return task
  }

  private async sendWithRetry(
    requestId: number,
    request: Record<string, unknown>,
  ): Promise<void> {
    try {
      await this.sendOnce(requestId, request)
    } catch (firstError) {
      if (firstError instanceof ArchiveRejectedError) throw firstError
      await this.stopBrokenChild()
      try {
        await this.sendOnce(requestId, request)
      } catch (secondError) {
        const first = errorMessage(firstError)
        const second = errorMessage(secondError)
        throw new Error(`archive writer failed after restart: ${first}; ${second}`)
      }
    }
  }

  private async sendOnce(
    requestId: number,
    request: Record<string, unknown>,
  ): Promise<void> {
    const body = Buffer.from(JSON.stringify(request), "utf8")
    if (body.length === 0 || body.length > this.maxFrameBytes) {
      throw new ArchiveRejectedError(
        `archive request is ${body.length} bytes; maximum is ${this.maxFrameBytes}`,
      )
    }
    const child = this.ensureChild()
    const header = Buffer.allocUnsafe(8)
    header.writeBigUInt64BE(BigInt(body.length), 0)

    const acknowledgement = new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        const pending = this.pending.get(requestId)
        if (pending === undefined) return
        this.pending.delete(requestId)
        pending.reject(new Error(`archive acknowledgement timed out after ${this.ackTimeoutMs} ms`))
        if (this.child === child) child.kill("SIGTERM")
      }, this.ackTimeoutMs)
      this.pending.set(requestId, {
        resolve: () => {
          clearTimeout(timer)
          resolve()
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
    child.stdout.on("data", (chunk: string) => {
      if (this.child === child) this.receiveAcks(chunk)
    })
    child.stderr.on("data", (chunk: string) => {
      if (this.child === child) {
        this.stderr = `${this.stderr}${chunk}`.slice(-MAX_ACK_BUFFER_BYTES)
      }
    })
    child.on("error", (error: Error) => {
      if (this.child === child) this.rejectPending(error)
    })
    child.on("exit", (code: number | null, signal: NodeJS.Signals | null) => {
      if (this.child !== child) return
      const suffix = this.stderr.trim()
      const detail = suffix === "" ? "" : `: ${suffix}`
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
      this.rejectPending(new Error("archive writer acknowledgement exceeded 64 KiB"))
      this.child?.kill("SIGTERM")
      return
    }
    for (;;) {
      const newline = this.ackBuffer.indexOf("\n")
      if (newline < 0) return
      const line = this.ackBuffer.slice(0, newline)
      this.ackBuffer = this.ackBuffer.slice(newline + 1)
      const ack = parseAck(line)
      if (ack === null) {
        this.rejectPending(new Error("archive writer returned an invalid acknowledgement"))
        this.child?.kill("SIGTERM")
        return
      }
      const pending = this.pending.get(ack.requestId)
      if (pending === undefined) continue
      this.pending.delete(ack.requestId)
      if (ack.ok) pending.resolve()
      else pending.reject(new ArchiveRejectedError(ack.error ?? "archive writer rejected the request"))
    }
  }

  private rejectPending(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
  }

  private async stopBrokenChild(): Promise<void> {
    const child = this.child
    this.child = null
    if (child === null || child.exitCode !== null) return
    child.kill("SIGTERM")
    await Promise.race([
      once(child, "exit"),
      new Promise<void>((resolve) => setTimeout(resolve, CLOSE_TIMEOUT_MS)),
    ])
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
  if (typeof value.requestId !== "number" || !Number.isSafeInteger(value.requestId)) return null
  if (typeof value.ok !== "boolean") return null
  if (value.error !== undefined && typeof value.error !== "string") return null
  return value.error === undefined
    ? { requestId: value.requestId, ok: value.ok }
    : { requestId: value.requestId, ok: value.ok, error: value.error }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
