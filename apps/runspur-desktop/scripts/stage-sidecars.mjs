import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const target = 'x86_64-pc-windows-msvc';
if (process.platform !== 'win32' || process.arch !== 'x64') {
  throw new Error(`Runspur Desktop requires ${target}.`);
}

const input = new URL('../../../target/release/', import.meta.url);
const output = new URL('../src-tauri/binaries/', import.meta.url);
const config = JSON.parse(readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
mkdirSync(output, { recursive: true });

for (const name of ['schedulerd', 'schedulerctl', 'scheduler-mcp']) {
  const source = new URL(`${name}.exe`, input);
  let version;
  try {
    version = execFileSync(fileURLToPath(source), ['--version'], { encoding: 'utf8', windowsHide: true }).trim();
  } catch (error) {
    throw new Error(`Cannot read ${fileURLToPath(source)}. Build scheduler-app with cargo build --locked --release -p scheduler-app --bins.`, { cause: error });
  }
  if (version !== `scheduler-app ${config.version}`) {
    throw new Error(`${name} reports ${version}. The desktop and scheduler must both be ${config.version}.`);
  }
  copyFileSync(source, new URL(`${name}-${target}.exe`, output));
}
