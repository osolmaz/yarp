# Pi extension

This extension archives every Pi tool call through Pi's public tool lifecycle events. It stores tool inputs and results before and after YARP processing without adding entries to Pi sessions or using Pi internals.

For supported `bash` and `exec_command` calls, the extension asks the local `yarp` binary whether the command can be wrapped safely. Unsupported commands and rewrite errors run unchanged. Wrapped commands add their exact stdout and stderr to the same archive before and after pruning.

When Pi reports the project as trusted, the extension also checks the single conventional path `.yarp/rules.yrp` and passes that compiled pack to the Rust binary. It rejects symlinked paths and never scans for source rules. Untrusted projects use embedded rules only.

The extension starts a session-scoped `yarp archive ingest` process and closes it during `session_shutdown`. It does not install a system or user service.

Set `YARP_DISABLED=1` to disable rewriting while keeping capture active. Set `YARP_ARCHIVE_DISABLED=1` to disable archive capture.
