export type ShellPlan = {
  version: 1
  execution:
    | { kind: "original" }
    | { kind: "rewrite"; command: string }
  result: {
    kind: "ordinary" | "recovery"
  }
}

export function parseShellPlan(text: string): ShellPlan {
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    throw new Error("shell plan is not valid JSON")
  }
  const root = exactRecord(value, ["version", "execution", "result"], "shell plan")
  if (root["version"] !== 1) {
    throw new Error("shell plan version must be 1")
  }
  const executionRecord = record(root["execution"], "shell plan execution")
  const executionKind = executionRecord["kind"]
  const execution = executionKind === "original"
    ? parseOriginal(executionRecord)
    : executionKind === "rewrite"
      ? parseRewrite(executionRecord)
      : invalidExecutionKind()
  const result = exactRecord(root["result"], ["kind"], "shell plan result")
  const resultKind = result["kind"]
  if (resultKind !== "ordinary" && resultKind !== "recovery") {
    throw new Error("shell plan result kind is invalid")
  }
  if (execution.kind === "rewrite" && resultKind !== "ordinary") {
    throw new Error("rewritten shell plans must use the ordinary result policy")
  }
  return {
    version: 1,
    execution,
    result: { kind: resultKind },
  }
}

function parseOriginal(value: Record<string, unknown>): { kind: "original" } {
  exactKeys(value, ["kind"], "original shell plan execution")
  return { kind: "original" }
}

function parseRewrite(value: Record<string, unknown>): { kind: "rewrite"; command: string } {
  exactKeys(value, ["kind", "command"], "rewrite shell plan execution")
  const command = value["command"]
  if (typeof command !== "string" || command.trim() === "") {
    throw new Error("rewrite shell plan command must be a non-empty string")
  }
  return { kind: "rewrite", command }
}

function invalidExecutionKind(): never {
  throw new Error("shell plan execution kind is invalid")
}

function exactRecord(
  value: unknown,
  keys: readonly string[],
  name: string,
): Record<string, unknown> {
  const parsed = record(value, name)
  exactKeys(parsed, keys, name)
  return parsed
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[], name: string): void {
  const actual = Object.keys(value).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new Error(`${name} contains missing or unknown fields`)
  }
}

function record(value: unknown, name: string): Record<string, unknown> {
  if (!isRecord(value)) {
    throw new Error(`${name} must be an object`)
  }
  return value
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
