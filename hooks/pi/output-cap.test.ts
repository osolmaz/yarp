import assert from "node:assert/strict"
import { Buffer } from "node:buffer"
import test from "node:test"
import type { ToolResultEvent } from "@earendil-works/pi-coding-agent"
import {
  capToolResultContent,
  DEFAULT_OUTPUT_CAP_BYTES,
  MAX_OUTPUT_CAP_BYTES,
  MIN_OUTPUT_CAP_BYTES,
  parseOutputCapConfiguration,
} from "./output-cap.js"

const archiveRef = "yr_0123456789abcdef0123456789abcdef"

test("uses a 5 KiB default and accepts bounded explicit configuration", () => {
  assert.deepEqual(parseOutputCapConfiguration(undefined), {
    maxBytes: DEFAULT_OUTPUT_CAP_BYTES,
    warning: null,
  })
  assert.deepEqual(parseOutputCapConfiguration("0"), { maxBytes: null, warning: null })
  assert.deepEqual(parseOutputCapConfiguration("8192"), { maxBytes: 8192, warning: null })

  for (const value of ["", "01", " 5120", "512", String(MAX_OUTPUT_CAP_BYTES + 1)]) {
    const configuration = parseOutputCapConfiguration(value)
    assert.equal(configuration.maxBytes, null)
    assert.match(configuration.warning ?? "", /invalid YARP_OUTPUT_CAP_BYTES/u)
  }
  assert.equal(MIN_OUTPUT_CAP_BYTES, 1024)
})

test("passes text through at the exact byte budget", () => {
  const content: ToolResultEvent["content"] = [{ type: "text", text: "x".repeat(1024) }]
  assert.equal(capToolResultContent(content, archiveRef, "complete", 1024), null)
})

test("caps ASCII text with bounded head, tail, and recovery instructions", () => {
  const source = `${"head\n".repeat(1_000)}${"tail\n".repeat(1_000)}`
  const capped = capToolResultContent(
    [{ type: "text", text: source }],
    archiveRef,
    "unknown",
    DEFAULT_OUTPUT_CAP_BYTES,
  )
  assert.notEqual(capped, null)
  if (capped === null) throw new Error("expected capped output")

  const visible = textOnly(capped.content)
  assert.equal(capped.sourceText, source)
  assert.equal(capped.sourceBytes, Buffer.byteLength(source))
  assert.ok(Buffer.byteLength(visible) <= DEFAULT_OUTPUT_CAP_BYTES)
  assert.ok(visible.startsWith("head\n"))
  assert.ok(visible.endsWith("tail\n"))
  assert.match(visible, new RegExp(`yarp search ${archiveRef}`, "u"))
  assert.match(visible, /result_text unknown/u)
})

test("points capped typed summaries at their committed recovery source", () => {
  const capped = capToolResultContent(
    [{ type: "text", text: "typed summary\n".repeat(1_000) }],
    archiveRef,
    "complete",
    1024,
    "source_output",
  )
  assert.notEqual(capped, null)
  if (capped === null) throw new Error("expected capped output")
  assert.match(textOnly(capped.content), /source_output complete/u)
})

test("never splits UTF-8 code points", () => {
  const source = `start\n${"🙂界".repeat(2_000)}\nend`
  const capped = capToolResultContent(
    [{ type: "text", text: source }],
    archiveRef,
    "incomplete",
    1025,
  )
  assert.notEqual(capped, null)
  if (capped === null) throw new Error("expected capped output")

  const visible = textOnly(capped.content)
  assert.ok(Buffer.byteLength(visible) <= 1025)
  assert.equal(visible.includes("�"), false)
  assert.ok(visible.startsWith("start\n"))
  assert.ok(visible.endsWith("\nend"))
  assert.equal(capped.sourceText, source)
})

test("keeps an intervening image before a marker at a text boundary", () => {
  const source = "A".repeat(5_000)
  const single = capToolResultContent(
    [{ type: "text", text: source }],
    archiveRef,
    "complete",
    1024,
  )
  assert.notEqual(single, null)
  if (single === null) throw new Error("expected capped output")
  const prefixBytes = textOnly(single.content).indexOf("\n[yarp:")
  assert.ok(prefixBytes > 0)

  const image = { type: "image" as const, data: "middle", mimeType: "image/png" }
  const split = capToolResultContent(
    [
      { type: "text", text: source.slice(0, prefixBytes) },
      image,
      { type: "text", text: source.slice(prefixBytes) },
    ],
    archiveRef,
    "complete",
    1024,
  )
  assert.notEqual(split, null)
  if (split === null) throw new Error("expected split capped output")
  const imageIndex = split.content.indexOf(image)
  const markerIndex = split.content.findIndex(
    (item) => isTextContent(item) && item.text.includes("[yarp:"),
  )
  assert.ok(imageIndex >= 0)
  assert.ok(markerIndex > imageIndex)
})

test("preserves image order while capping all text blocks together", () => {
  const firstImage = { type: "image" as const, data: "first", mimeType: "image/png" }
  const secondImage = { type: "image" as const, data: "second", mimeType: "image/png" }
  const content: ToolResultEvent["content"] = [
    { type: "text", text: "A".repeat(3_000) },
    firstImage,
    { type: "text", text: "M".repeat(5_000) },
    secondImage,
    { type: "text", text: "Z".repeat(3_000) },
  ]
  const capped = capToolResultContent(content, archiveRef, "complete", 1024)
  assert.notEqual(capped, null)
  if (capped === null) throw new Error("expected capped output")

  assert.equal(capped.sourceText, `${"A".repeat(3_000)}${"M".repeat(5_000)}${"Z".repeat(3_000)}`)
  assert.deepEqual(
    capped.content.filter((item) => isRecord(item) && item["type"] === "image"),
    [firstImage, secondImage],
  )
  const firstImageIndex = capped.content.indexOf(firstImage)
  const secondImageIndex = capped.content.indexOf(secondImage)
  assert.ok(firstImageIndex > 0)
  assert.ok(secondImageIndex > firstImageIndex)
  assert.ok(capped.content.slice(0, firstImageIndex).some((item) => isTextContent(item) && item.text.startsWith("A")))
  assert.ok(capped.content.slice(secondImageIndex + 1).some((item) => isTextContent(item) && item.text.endsWith("Z")))
  assert.ok(Buffer.byteLength(textOnly(capped.content)) <= 1024)
})

function textOnly(content: ToolResultEvent["content"]): string {
  return content
    .filter(isTextContent)
    .map((item) => item.text)
    .join("")
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
