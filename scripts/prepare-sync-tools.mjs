import { existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';

if (process.platform === 'darwin') process.exit(0);
if (!['linux', 'win32'].includes(process.platform)) {
  throw new Error(`Unsupported sync-tools build platform: ${process.platform}`);
}
let bash = '/bin/bash';
if (process.platform === 'win32') {
  // MSYS2 is a build dependency only. Users receive the small private runtime.
  const root = process.env.MSYS2_ROOT || 'C:/msys64';
  bash = resolve(root, 'usr/bin/bash.exe');
  if (!existsSync(bash)) {
    throw new Error('Building Windows requires MSYS2 with rsync and openssh packages. Set MSYS2_ROOT if installed outside C:/msys64.');
  }
}
const script = resolve('scripts/prepare-sync-tools.sh').replaceAll('\\', '/');
const result = spawnSync(bash, [script], {
  stdio: 'inherit',
  env: { ...process.env, MSYSTEM: 'MSYS', CHERE_INVOKING: '1' },
});
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
const verification = spawnSync(process.execPath, [resolve('scripts/verify-sync-tools.mjs')], { stdio: 'inherit' });
if (verification.error) throw verification.error;
process.exit(verification.status ?? 1);
