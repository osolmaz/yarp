import { Buffer } from "node:buffer"
import { spawn } from "node:child_process"
import { once, type EventEmitter } from "node:events"
import type { Readable, Writable } from "node:stream"

const RESULT_SCHEMA_VERSION = 1
const MAX_FRAME_BYTES = 4 * 1024 * 1024
const RESULT_TIMEOUT_MS = 2_000
const MAX_STDERR_BYTES = 64 * 1024

export type SourceCompleteness = "complete" | "incomplete" | "unknown"

export type ResultReducerRequest = {
  command: string
  text: string
  isError: boolean
  exitCode?: number
  archiveRef: string
  sourceCompleteness: SourceCompleteness
  preferArchiveSource: boolean
}

export type ResultReducerResponse =
  | { changed: false }
  | {
      changed: true
      content: string
      source: "source_output" | "result_text"
      sourceCompleteness: SourceCompleteness
      needsResultText: boolean
    }

export interface ResultReducer {
  reduce(request: ResultReducerRequest, signal?: AbortSignal): Promise<ResultReducerResponse>
}

interface ResultProcess extends EventEmitter {
  stdin: Writable
  stdout: Readable
  stderr: Readable
  exitCode: number | null
  kill(signal?: NodeJS.Signals): boolean
}

type SpawnReducer = () => ResultProcess

export class ResultReducerClient implements ResultReducer {
  constructor(
    private readonly spawnReducer: SpawnReducer = defaultSpawnReducer,
    private readonly timeoutMs: number = RESULT_TIMEOUT_MS,
  ) {}

  async reduce(
    request: ResultReducerRequest,
    signal?: AbortSignal,
  ): Promise<ResultReducerResponse> {
    validateRequest(request)
    const child = this.spawnReducer()
    let stdout = Buffer.alloc(0)
    let stderr = ""
    let failure: Error | null = null
    child.stdout.on("data", (chunk: Buffer | string) => {
      const body = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)
      if (stdout.length + body.length > MAX_FRAME_BYTES + 8) {
        failure = new Error("result-reducer response exceeded 4 MiB")
        child.kill("SIGTERM")
        return
      }
      stdout = Buffer.concat([stdout, body])
    })
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", (chunk: string) => {
      stderr = `${stderr}${chunk}`.slice(-MAX_STDERR_BYTES)
    })
    child.on("error", (error: Error) => {
      failure = error
    })

    const body = Buffer.from(
      JSON.stringify({ schemaVersion: RESULT_SCHEMA_VERSION, ...request }),
      "utf8",
    )
    if (body.length === 0 || body.length > MAX_FRAME_BYTES) {
      child.kill("SIGTERM")
      throw new Error(`result-reducer request is ${body.length} bytes; maximum is ${MAX_FRAME_BYTES}`)
    }
    const header = Buffer.allocUnsafe(8)
    header.writeBigUInt64BE(BigInt(body.length), 0)

    const abort = () => child.kill("SIGTERM")
    signal?.addEventListener("abort", abort, { once: true })
    const timer = setTimeout(() => child.kill("SIGTERM"), this.timeoutMs)
    try {
      if (!child.stdin.write(header)) await once(child.stdin, "drain")
      if (!child.stdin.write(body)) await once(child.stdin, "drain")
      child.stdin.end()
      const [code, processSignal] = await once(child, "close") as [number | null, NodeJS.Signals | null]
      if (signal?.aborted) throw new Error("result-reducer request was aborted")
      if (failure !== null) throw failure
      if (code !== 0) {
        throw new Error(
          `result reducer exited with ${processSignal ?? `code ${String(code)}`}${stderr.trim() === "" ? "" : `: ${stderr.trim()}`}`,
        )
      }
      return parseResponse(stdout)
    } finally {
      clearTimeout(timer)
      signal?.removeEventListener("abort", abort)
      if (child.exitCode === null) child.kill("SIGTERM")
    }
  }
}

function defaultSpawnReducer(): ResultProcess {
  return spawn("yarp", ["result-reduce"], { stdio: ["pipe", "pipe", "pipe"] })
}

function validateRequest(request: ResultReducerRequest): void {
  if (request.command.trim() === "") throw new Error("result-reducer command is empty")
  if (!/^yr_[0-9a-f]{32}$/u.test(request.archiveRef)) {
    throw new Error("result-reducer archive reference is invalid")
  }
  if (request.exitCode !== undefined && !Number.isSafeInteger(request.exitCode)) {
    throw new Error("result-reducer exit code is invalid")
  }
}

function parseResponse(frame: Buffer): ResultReducerResponse {
  if (frame.length < 8) throw new Error("result-reducer response is truncated")
  const length = frame.readBigUInt64BE(0)
  if (length === 0n || length > BigInt(MAX_FRAME_BYTES)) {
    throw new Error(`invalid result-reducer response length ${String(length)}`)
  }
  const expected = Number(length) + 8
  if (frame.length !== expected) throw new Error("result-reducer response length does not match frame")
  let value: unknown
  try {
    value = JSON.parse(frame.subarray(8).toString("utf8"))
  } catch {
    throw new Error("result-reducer response is not valid JSON")
  }
  if (!isRecord(value) || value["schemaVersion"] !== RESULT_SCHEMA_VERSION) {
    throw new Error("result-reducer response has an invalid schema version")
  }
  const changed = value["changed"]
  if (changed === false) {
    if (Object.keys(value).some((key) => !["schemaVersion", "changed", "needsResultText"].includes(key))) {
      throw new Error("unchanged result-reducer response has unexpected fields")
    }
    if (value["needsResultText"] !== false) {
      throw new Error("unchanged result-reducer response has invalid recovery state")
    }
    return { changed: false }
  }
  if (changed !== true) throw new Error("result-reducer response has invalid changed state")
  const content = value["content"]
  const source = value["source"]
  const sourceCompleteness = value["sourceCompleteness"]
  const needsResultText = value["needsResultText"]
  if (typeof content !== "string") throw new Error("result-reducer response content is invalid")
  if (source !== "source_output" && source !== "result_text") {
    throw new Error("result-reducer response source is invalid")
  }
  if (!isSourceCompleteness(sourceCompleteness)) {
    throw new Error("result-reducer response completeness is invalid")
  }
  if (typeof needsResultText !== "boolean" || needsResultText !== (source === "result_text")) {
    throw new Error("result-reducer response recovery state is invalid")
  }
  const allowed = new Set([
    "schemaVersion",
    "changed",
    "content",
    "source",
    "sourceCompleteness",
    "needsResultText",
  ])
  if (Object.keys(value).some((key) => !allowed.has(key))) {
    throw new Error("result-reducer response has unexpected fields")
  }
  return { changed, content, source, sourceCompleteness, needsResultText }
}

function isSourceCompleteness(value: unknown): value is SourceCompleteness {
  return value === "complete" || value === "incomplete" || value === "unknown"
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}
