declare module "@earendil-works/pi-coding-agent" {
  export type ExecResult = {
    code: number
    stdout: string
    stderr: string
    killed: boolean
  }

  export type ExecOptions = {
    timeout?: number
    signal?: AbortSignal
  }

  export type ToolCallEvent = {
    toolName: string
    input: unknown
  }

  export type ToolCallContext = {
    signal: AbortSignal
  }

  export interface ExtensionAPI {
    exec(command: string, args: string[], options?: ExecOptions): Promise<ExecResult>
    on(
      event: "tool_call",
      handler: (event: ToolCallEvent, context: ToolCallContext) => Promise<void> | void,
    ): void
  }
}
