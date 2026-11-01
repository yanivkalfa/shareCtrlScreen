// guard.test.js — regression cases for guard.js. Run: node .claude/lane/guard.test.js
const { spawnSync } = require('child_process');
const path = require('path');
const guard = path.join(__dirname, 'guard.js');
const cases = [
  ['cd x && cargo check -p engine', 2],
  ['cargo --version', 2],
  ["wsl -e bash -lc 'source ~/.cargo/env; cargo test'", 2],
  ['powershell -Command "cargo build -p app"', 2],
  ['npm run build', 2],
  ['rustc --version', 2],
  ['git commit -m "lane for every cargo run"', 0],
  ['ls native/target', 0],
  ['grep -rn cargo native/README.md', 0],
  ['powershell -File .claude\\lane\\lane-up.ps1', 0],
  ['$q="C:\\x\\.claude\\lane\\queue"; @"\ncargo check -p engine\n"@ | Set-Content "$q\\M2-01.request"', 0],
];
let failed = 0;
for (const [cmd, want] of cases) {
  const r = spawnSync(process.execPath, [guard], { input: JSON.stringify({ tool_name: 'Bash', tool_input: { command: cmd } }) });
  const ok = r.status === want;
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} exit=${r.status} want=${want}  ${JSON.stringify(cmd)}`);
}
process.exit(failed ? 1 : 0);
