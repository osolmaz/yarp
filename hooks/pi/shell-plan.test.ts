import assert from "node:assert/strict"
import test from "node:test"
import { parseShellPlan } from "./shell-plan.js"

test("parses original recovery and rewritten ordinary plans", () => {
  assert.deepEqual(
    parseShellPlan('{"version":1,"execution":{"kind":"original"},"result":{"kind":"recovery"}}'),
    {
      version: 1,
      execution: { kind: "original" },
      result: { kind: "recovery" },
    },
  )
  assert.deepEqual(
    parseShellPlan('{"version":1,"execution":{"kind":"rewrite","command":"yarp run -- git status"},"result":{"kind":"ordinary"}}'),
    {
      version: 1,
      execution: { kind: "rewrite", command: "yarp run -- git status" },
      result: { kind: "ordinary" },
    },
  )
})

test("rejects malformed, extended, and unknown plan variants", () => {
  for (const value of [
    "{}",
    '{"version":2,"execution":{"kind":"original"},"result":{"kind":"ordinary"}}',
    '{"version":1,"execution":{"kind":"other"},"result":{"kind":"ordinary"}}',
    '{"version":1,"execution":{"kind":"rewrite","command":""},"result":{"kind":"ordinary"}}',
    '{"version":1,"execution":{"kind":"rewrite","command":"yarp read ref stdout 1:2"},"result":{"kind":"recovery"}}',
    '{"version":1,"execution":{"kind":"original","extra":true},"result":{"kind":"ordinary"}}',
    '{"version":1,"execution":{"kind":"original"},"result":{"kind":"other"}}',
  ]) {
    assert.throws(() => parseShellPlan(value), Error)
  }
})
