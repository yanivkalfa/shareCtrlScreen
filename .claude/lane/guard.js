// guard.js — PreToolUse hook for Bash/PowerShell: refuses raw cargo/npm-build
// commands. Compiling happens only through a lane request (see README.md).
// Exit 2 + stderr = the tool call is blocked and the message is shown to the agent.
let data = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (c) => { data += c; });
process.stdin.on('end', () => {
  let input;
  try { input = JSON.parse(data); } catch { process.exit(0); }
  const cmd = (input && input.tool_input && input.tool_input.command) || '';
  // Lane scripts are allowed to be *started* (they are not cargo themselves).
  if (/lane[\\/](drain|run-request|lane-up)\.ps1/.test(cmd)) process.exit(0);
  // Writing a request file mentions cargo in its body; that is the sanctioned path.
  if (/queue/.test(cmd) && /[\w-]+-\w+\.request\b/.test(cmd)) process.exit(0);
  // A command position only: line start or after a shell separator/quote. A
  // bare "cargo" inside prose (commit messages, comments) is not a build.
  const heavy = /(^|[;&|(`'"])\s*(cargo|rustc)(\.exe)?\s/m.test(cmd)
    || /(^|[;&|(`'"])\s*npm(\.cmd)?\s+(run\s+)?(build|tauri)\b/m.test(cmd);
  if (!heavy) process.exit(0);
  process.stderr.write(
    'BLOCKED by the lane guard: cargo/rustc/npm build never run directly on this machine. ' +
    'Write a request file to .claude/lane/queue/<TASK>-<nn>.request (format in .claude/lane/README.md), ' +
    'end your turn with the line "QUEUED <TASK>-<nn>", and you will be resumed with the result path.\n');
  process.exit(2);
});
