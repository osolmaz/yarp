# Pi extension

This extension archives every Pi tool call through Pi's documented public lifecycle events. It stores tool inputs and results before and after YARP processing without adding entries to Pi sessions or using Pi internals. The package and Rust binary must have the same exact version; a mismatch disables capture and pruning for that session.

For supported `bash` and `exec_command` calls, the extension asks the local `yarp` binary whether the command can be wrapped safely. Unsupported commands and rewrite errors execute unchanged. Wrapped commands add their exact stdout and stderr to the same archive before and after typed summarization.

When a safe shell command cannot be wrapped, the `tool_result` hook may invoke one bounded `yarp result-reduce` process through a length-framed stdin protocol. It accepts one text item, gives explicit exit codes precedence, and uses Pi's documented complete Bash output when available. If host text is the only source and a typed summary wins, the extension commits that exact text before returning the result patch. Unsafe command graphs, protocol failures, and reducer failures bypass typed summarization.

After typed summarization, the same public hook caps remaining tool-result text at 5,120 UTF-8 bytes by default. The cap covers all text blocks together and includes its recovery marker. An oversized typed summary reuses its committed raw streams, `source_output`, or `result_text` source. A wrapped summary also commits its exact visible text as a fallback while raw streams remain the first search sources. Other text commits its exact ordered content to `result_text/before` before YARP retains UTF-8-safe content from the beginning and end. Image blocks remain unchanged in their original order and do not count toward the text budget. If recovery capture fails, the pre-cap result passes through.

Set `YARP_OUTPUT_CAP_BYTES` to `0` to disable only the generic cap, or to an exact budget from 1,024 through 16,777,216 bytes. An invalid value disables the generic cap for that session and reports a warning. `YARP_DISABLED=1` disables rewriting and all result pruning while keeping capture active. `YARP_ARCHIVE_DISABLED=1` disables archive capture and the generic cap.

When Pi reports the project as trusted, the extension also checks the single conventional path `.yarp/rules.yrp` and passes that compiled pack to the Rust binary. It rejects symlinked paths and never scans for source rules. Untrusted projects use embedded rules only.

The extension starts a session-scoped `yarp archive ingest` process and closes it during `session_shutdown`. It does not install a system or user service.
