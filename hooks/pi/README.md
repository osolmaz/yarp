# Pi extension

This extension checks Pi's `bash` and `exec_command` calls before execution. It asks the local `yarp` binary whether the command can be wrapped safely. Unsupported commands and rewrite errors run unchanged.

The extension uses Pi's public `tool_call` hook. It does not change Pi sessions, other persistent data, or Pi internals.

Set `YARP_DISABLED=1` to disable rewriting without removing the extension.
