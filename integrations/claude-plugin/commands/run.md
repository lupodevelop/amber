---
description: Run a shell command inside a disposable amber microVM sandbox
argument-hint: <shell command>
allowed-tools: Bash(*)
---

Run this command inside an isolated amber microVM (not on the host) using the
sandbox helper, and report its stdout, stderr, and exit code:

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/amber-exec.sh" '$ARGUMENTS'
```

If the helper reports amber isn't set up, consult the `amber-install` skill, then
retry. Show the user the command's output and its exit code.
