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

  export type SessionManagerView = {
    getSessionId(): string
  }

  export type ModelView = {
    provider: string
    id: string
  }

  export type ExtensionContext = {
    signal?: AbortSignal
    cwd: string
    sessionManager: SessionManagerView
    model?: ModelView
  }

  export type ToolCallEvent = {
    type: "tool_call"
    toolCallId: string
    toolName: string
    input: Record<string, unknown>
  }

  export type ToolResultEvent = {
    type: "tool_result"
    toolCallId: string
    toolName: string
    input: Record<string, unknown>
    content: unknown[]
    details: unknown
    isError: boolean
    usage?: unknown
  }

  export type ToolExecutionEndEvent = {
    type: "tool_execution_end"
    toolCallId: string
    toolName: string
    result: unknown
    isError: boolean
  }

  export type SessionStartEvent = {
    type: "session_start"
    reason: "startup" | "reload" | "new" | "resume" | "fork"
  }

  export type SessionShutdownEvent = {
    type: "session_shutdown"
    reason: "quit" | "reload" | "new" | "resume" | "fork"
  }

  export type ExtensionEventMap = {
    session_start: SessionStartEvent
    session_shutdown: SessionShutdownEvent
    tool_call: ToolCallEvent
    tool_result: ToolResultEvent
    tool_execution_end: ToolExecutionEndEvent
  }

  export interface ExtensionAPI {
    exec(command: string, args: string[], options?: ExecOptions): Promise<ExecResult>
    on<K extends keyof ExtensionEventMap>(
      event: K,
      handler: (
        event: ExtensionEventMap[K],
        context: ExtensionContext,
      ) => Promise<void> | void,
    ): void
  }
}
