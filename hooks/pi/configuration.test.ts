import assert from "node:assert/strict"
import test from "node:test"
import {
  DEFAULT_OUTPUT_CAP_BYTES,
  DEFAULT_RECOVERY_CAP_BYTES,
  DEFAULT_RECOVERY_CAP_LINES,
  parseResolvedConfiguration,
} from "./configuration.js"

function validConfiguration(): Record<string, unknown> {
  return {
    version: 1,
    pruning: { enabled: true },
    output: {
      cap_bytes: DEFAULT_OUTPUT_CAP_BYTES,
      recovery_cap_bytes: DEFAULT_RECOVERY_CAP_BYTES,
      recovery_cap_lines: DEFAULT_RECOVERY_CAP_LINES,
    },
    archive: {
      enabled: true,
      path: "/home/test/.local/share/yarp/tool-calls.sqlite3",
    },
    rules: { packs: [] },
  }
}

test("parses one strict resolved configuration", () => {
  const parsed = parseResolvedConfiguration(JSON.stringify(validConfiguration()))
  assert.equal(parsed.version, 1)
  assert.equal(parsed.pruning.enabled, true)
  assert.equal(parsed.output.cap_bytes, 5120)
  assert.equal(parsed.output.recovery_cap_bytes, 32768)
  assert.equal(parsed.output.recovery_cap_lines, 1900)
})

test("rejects malformed, incomplete, and extended responses", () => {
  assert.throws(() => parseResolvedConfiguration("{"), /not valid JSON/u)
  const missing = validConfiguration()
  delete missing["output"]
  assert.throws(
    () => parseResolvedConfiguration(JSON.stringify(missing)),
    /missing or unknown fields/u,
  )
  const extended = { ...validConfiguration(), extra: true }
  assert.throws(
    () => parseResolvedConfiguration(JSON.stringify(extended)),
    /missing or unknown fields/u,
  )
})

test("rejects invalid bounds and unresolved paths", () => {
  const invalidCap = validConfiguration()
  invalidCap["output"] = {
    cap_bytes: 1,
    recovery_cap_bytes: DEFAULT_RECOVERY_CAP_BYTES,
    recovery_cap_lines: DEFAULT_RECOVERY_CAP_LINES,
  }
  assert.throws(
    () => parseResolvedConfiguration(JSON.stringify(invalidCap)),
    /output\.cap_bytes/u,
  )
  const relativeArchive = validConfiguration()
  relativeArchive["archive"] = { enabled: true, path: "archive.sqlite3" }
  assert.throws(
    () => parseResolvedConfiguration(JSON.stringify(relativeArchive)),
    /archive\.path must be absolute/u,
  )
  const relativePack = validConfiguration()
  relativePack["rules"] = { packs: ["rules.yrp"] }
  assert.throws(
    () => parseResolvedConfiguration(JSON.stringify(relativePack)),
    /rules\.packs\[0\] must be absolute/u,
  )
})
