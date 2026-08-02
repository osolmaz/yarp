# Pi extension

This extension archives every Pi tool call through Pi's documented public lifecycle events. It stores tool inputs and results before and after YARP processing without adding entries to Pi sessions or using Pi internals. The package and Rust binary must have the same exact version; a mismatch disables capture and pruning for that session.

For supported `bash` and `exec_command` calls, the extension asks the local `yarp` binary whether the command can be wrapped safely. Unsupported commands and rewrite errors run unchanged. Wrapped commands add their exact stdout and stderr to the same archive before and after typed summarization.

When a safe shell command cannot be wrapped, the `tool_result` hook may invoke one bounded `yarp result-reduce` process through a length-framed stdin protocol. It accepts only one text item, gives explicit exit codes precedence, and uses Pi's documented complete Bash output when available. If host text is the only source and a summary wins, the extension commits that exact text before returning the result patch. Multiple content items, structured output, unsafe command graphs, archive failures, protocol failures, and reducer failures pass through unchanged.

When Pi reports the project as trusted, the extension also checks the single conventional path `.yarp/rules.yrp` and passes that compiled pack to the Rust binary. It rejects symlinked paths and never scans for source rules. Untrusted projects use embedded rules only.

The extension starts a session-scoped `yarp archive ingest` process and closes it during `session_shutdown`. It does not install a system or user service.

Set `YARP_DISABLED=1` to disable rewriting while keeping capture active. Set `YARP_ARCHIVE_DISABLED=1` to disable archive capture.
