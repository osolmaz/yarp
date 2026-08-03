import { Buffer } from "node:buffer"
import type { ToolResultEvent } from "@earendil-works/pi-coding-agent"
import type { SourceCompleteness } from "./result-client.js"

export const DEFAULT_OUTPUT_CAP_BYTES = 5 * 1024
export const MIN_OUTPUT_CAP_BYTES = 1024
export const MAX_OUTPUT_CAP_BYTES = 16 * 1024 * 1024

export type OutputCapConfiguration = {
  maxBytes: number | null
  warning: string | null
}

export type CappedToolContent = {
  content: ToolResultEvent["content"]
  sourceText: string
  sourceBytes: number
}

export function parseOutputCapConfiguration(
  value: string | undefined,
): OutputCapConfiguration {
  if (value === undefined) {
    return { maxBytes: DEFAULT_OUTPUT_CAP_BYTES, warning: null }
  }
  if (!/^(?:0|[1-9][0-9]*)$/u.test(value)) {
    return invalidConfiguration()
  }
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed)) return invalidConfiguration()
  if (parsed === 0) return { maxBytes: null, warning: null }
  if (parsed < MIN_OUTPUT_CAP_BYTES || parsed > MAX_OUTPUT_CAP_BYTES) {
    return invalidConfiguration()
  }
  return { maxBytes: parsed, warning: null }
}

export function capToolResultContent(
  content: ToolResultEvent["content"],
  archiveRef: string,
  completeness: SourceCompleteness,
  maxBytes: number,
): CappedToolContent | null {
  if (
    !Number.isSafeInteger(maxBytes)
    || maxBytes < MIN_OUTPUT_CAP_BYTES
    || maxBytes > MAX_OUTPUT_CAP_BYTES
  ) {
    throw new Error("output cap is outside the supported range")
  }
  if (!/^yr_[0-9a-f]{32}$/u.test(archiveRef)) {
    throw new Error("output cap archive reference is invalid")
  }

  const sourceText = content
    .filter(isTextContent)
    .map((item) => item.text)
    .join("")
  const source = Buffer.from(sourceText, "utf8")
  if (source.length <= maxBytes) return null

  const marker = recoveryMarker(archiveRef, completeness, source.length, maxBytes)
  const markerBytes = Buffer.byteLength(marker, "utf8")
  const retainedBudget = maxBytes - markerBytes
  if (retainedBudget <= 0) {
    throw new Error("output cap cannot fit its recovery marker")
  }
  const prefixEnd = utf8PrefixEnd(source, Math.floor(retainedBudget / 2))
  const suffixStart = utf8SuffixStart(source, source.length - (retainedBudget - prefixEnd))
  if (prefixEnd >= suffixStart) {
    throw new Error("output cap did not omit any source text")
  }

  const capped: ToolResultEvent["content"] = []
  let sourceOffset = 0
  let markerInserted = false
  for (const item of content) {
    if (!isTextContent(item)) {
      capped.push(item)
      continue
    }
    const body = Buffer.from(item.text, "utf8")
    const itemStart = sourceOffset
    const itemEnd = itemStart + body.length

    if (itemStart < prefixEnd) {
      const end = Math.min(itemEnd, prefixEnd) - itemStart
      pushText(capped, body.subarray(0, end).toString("utf8"))
    }
    if (!markerInserted && itemEnd >= prefixEnd) {
      pushText(capped, marker)
      markerInserted = true
    }
    if (itemEnd > suffixStart) {
      const start = Math.max(itemStart, suffixStart) - itemStart
      pushText(capped, body.subarray(start).toString("utf8"))
    }
    sourceOffset = itemEnd
  }
  if (!markerInserted) pushText(capped, marker)

  const visibleBytes = capped.reduce<number>(
    (total, item) => total + (isTextContent(item) ? Buffer.byteLength(item.text, "utf8") : 0),
    0,
  )
  if (visibleBytes > maxBytes) {
    throw new Error("output cap exceeded its configured byte budget")
  }
  return { content: capped, sourceText, sourceBytes: source.length }
}

function invalidConfiguration(): OutputCapConfiguration {
  return {
    maxBytes: null,
    warning:
      `invalid YARP_OUTPUT_CAP_BYTES; expected 0 or ${MIN_OUTPUT_CAP_BYTES} through ${MAX_OUTPUT_CAP_BYTES}`,
  }
}

function recoveryMarker(
  archiveRef: string,
  completeness: SourceCompleteness,
  sourceBytes: number,
  maxBytes: number,
): string {
  return `\n[yarp: ${sourceBytes} byte(s) capped at ${maxBytes}; ref=${archiveRef}; result_text ${completeness}]\nSearch omitted output: yarp search ${archiveRef} 'term|alternate'\n`
}

function utf8PrefixEnd(body: Buffer, budget: number): number {
  let end = Math.min(body.length, budget)
  while (end > 0 && end < body.length && isContinuationByte(body[end])) end -= 1
  return end
}

function utf8SuffixStart(body: Buffer, requestedStart: number): number {
  let start = Math.max(0, requestedStart)
  while (start < body.length && isContinuationByte(body[start])) start += 1
  return start
}

function isContinuationByte(value: number | undefined): boolean {
  return value !== undefined && (value & 0xc0) === 0x80
}

function pushText(content: ToolResultEvent["content"], text: string): void {
  if (text !== "") content.push({ type: "text", text })
}

type TextContent = {
  type: "text"
  text: string
}

function isTextContent(value: unknown): value is TextContent {
  return isRecord(value) && value["type"] === "text" && typeof value["text"] === "string"
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}
