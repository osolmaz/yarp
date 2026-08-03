import { isAbsolute } from "node:path"

export const CONFIG_VERSION = 1
export const DEFAULT_OUTPUT_CAP_BYTES = 5 * 1024
export const MIN_OUTPUT_CAP_BYTES = 1024
export const MAX_OUTPUT_CAP_BYTES = 16 * 1024 * 1024
export const DEFAULT_RECOVERY_CAP_BYTES = 32 * 1024
export const MIN_RECOVERY_CAP_BYTES = 1024
export const MAX_RECOVERY_CAP_BYTES = 48 * 1024
export const DEFAULT_RECOVERY_CAP_LINES = 1900
export const MIN_RECOVERY_CAP_LINES = 1
export const MAX_RECOVERY_CAP_LINES = 1900

export type YarpConfiguration = {
  version: 1
  pruning: {
    enabled: boolean
  }
  output: {
    cap_bytes: number
    recovery_cap_bytes: number
    recovery_cap_lines: number
  }
  archive: {
    enabled: boolean
    path: string
  }
  rules: {
    packs: string[]
  }
}

export function parseResolvedConfiguration(text: string): YarpConfiguration {
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    throw new Error("configuration response is not valid JSON")
  }
  const root = exactRecord(value, ["version", "pruning", "output", "archive", "rules"], "configuration")
  if (root["version"] !== CONFIG_VERSION) {
    throw new Error(`configuration version must be ${CONFIG_VERSION}`)
  }
  const pruning = exactRecord(root["pruning"], ["enabled"], "pruning")
  const output = exactRecord(
    root["output"],
    ["cap_bytes", "recovery_cap_bytes", "recovery_cap_lines"],
    "output",
  )
  const archive = exactRecord(root["archive"], ["enabled", "path"], "archive")
  const rules = exactRecord(root["rules"], ["packs"], "rules")
  const pruningEnabled = requiredBoolean(pruning["enabled"], "pruning.enabled")
  const capBytes = requiredInteger(output["cap_bytes"], "output.cap_bytes")
  if (capBytes !== 0 && (capBytes < MIN_OUTPUT_CAP_BYTES || capBytes > MAX_OUTPUT_CAP_BYTES)) {
    throw new Error("output.cap_bytes is outside the supported range")
  }
  const recoveryCapBytes = requiredInteger(
    output["recovery_cap_bytes"],
    "output.recovery_cap_bytes",
  )
  if (
    recoveryCapBytes < MIN_RECOVERY_CAP_BYTES
    || recoveryCapBytes > MAX_RECOVERY_CAP_BYTES
  ) {
    throw new Error("output.recovery_cap_bytes is outside the supported range")
  }
  const recoveryCapLines = requiredInteger(
    output["recovery_cap_lines"],
    "output.recovery_cap_lines",
  )
  if (
    recoveryCapLines < MIN_RECOVERY_CAP_LINES
    || recoveryCapLines > MAX_RECOVERY_CAP_LINES
  ) {
    throw new Error("output.recovery_cap_lines is outside the supported range")
  }
  const archiveEnabled = requiredBoolean(archive["enabled"], "archive.enabled")
  const archivePath = requiredString(archive["path"], "archive.path")
  if (!isAbsolute(archivePath)) {
    throw new Error("archive.path must be absolute")
  }
  const packsValue = rules["packs"]
  if (!Array.isArray(packsValue)) {
    throw new Error("rules.packs must be an array")
  }
  const packs = packsValue.map((entry, index) => {
    const pack = requiredString(entry, `rules.packs[${index}]`)
    if (!isAbsolute(pack)) {
      throw new Error(`rules.packs[${index}] must be absolute`)
    }
    return pack
  })
  return {
    version: 1,
    pruning: { enabled: pruningEnabled },
    output: {
      cap_bytes: capBytes,
      recovery_cap_bytes: recoveryCapBytes,
      recovery_cap_lines: recoveryCapLines,
    },
    archive: { enabled: archiveEnabled, path: archivePath },
    rules: { packs },
  }
}

function exactRecord(
  value: unknown,
  keys: readonly string[],
  name: string,
): Record<string, unknown> {
  if (!isRecord(value)) {
    throw new Error(`${name} must be an object`)
  }
  const actual = Object.keys(value).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new Error(`${name} contains missing or unknown fields`)
  }
  return value
}

function requiredBoolean(value: unknown, name: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`${name} must be a boolean`)
  }
  return value
}

function requiredInteger(value: unknown, name: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw new Error(`${name} must be an integer`)
  }
  return value
}

function requiredString(value: unknown, name: string): string {
  if (typeof value !== "string" || value === "") {
    throw new Error(`${name} must be a non-empty string`)
  }
  return value
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
