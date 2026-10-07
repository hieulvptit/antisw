// Run from native Node, not an MSYS shell. Relocate the runtime and remove
// system tools from PATH to catch missing DLLs, path quoting and dependencies.
import assert from 'node:assert/strict';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

if (process.platform === 'darwin') process.exit(0);
const fixture = mkdtempSync(join(tmpdir(), "sync runtime's spaces "));
const runtime = join(fixture, 'tools');
const extension = process.platform === 'win32' ? '.exe' : '';
cpSync(resolve('src-tauri/resources/sync-tools'), runtime, { recursive: true });
const env = { ...process.env, PATH: '', LD_LIBRARY_PATH: join(runtime, 'lib'), MSYS: 'winsymlinks:nativestrict' };
// Windows env keys are case insensitive, but JS object keys are not.
for (const key of Object.keys(env)) {
  if (key !== 'PATH' && key.toUpperCase() === 'PATH') delete env[key];
}
const posix = (path) => process.platform === 'win32'
  ? path.replaceAll('\\', '/').replace(/^([a-z]):/i, (_, drive) => `/${drive.toLowerCase()}`)
  : path;
function run(tool, args, input) {
  const result = spawnSync(join(runtime, 'bin', `${tool}${extension}`), args, {
    env, input, encoding: 'utf8', timeout: 30000,
  });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, `${tool}: ${result.stderr}\n${result.stdout}`);
  return result.stdout;
}
try {
  assert.match(run('rsync', ['--version']), /rsync\s+version\s+3\./i);
  run('ssh', ['-V']);
  const key = join(fixture, 'signing key');
  run('ssh-keygen', ['-t', 'ed25519', '-N', '', '-f', posix(key)]);
  const signature = run('ssh-keygen', ['-Y', 'sign', '-n', 'sync-bundle-test', '-f', posix(key)], 'fixture');
  writeFileSync(`${key}.sig`, signature);
  const allowed = join(fixture, 'allowed signers');
  writeFileSync(allowed, `fixture ${readFileSync(`${key}.pub`, 'utf8')}`);
  run('ssh-keygen', ['-Y', 'verify', '-n', 'sync-bundle-test', '-I', 'fixture', '-f', posix(allowed), '-s', posix(`${key}.sig`)], 'fixture');
  const source = join(fixture, 'local folder');
  const destination = join(fixture, 'remote folder');
  mkdirSync(join(source, '.git'), { recursive: true });
  mkdirSync(destination);
  writeFileSync(join(source, '.git', 'HEAD'), 'ref: refs/heads/main\n');
  writeFileSync(join(source, 'hello world.txt'), 'upload');
  writeFileSync(join(destination, 'deleted.txt'), 'stale');
  run('rsync', ['-azc', '--delete', '--info=progress2', `${posix(source)}/`, `${posix(destination)}/`]);
  assert.equal(readFileSync(join(destination, 'hello world.txt'), 'utf8'), 'upload');
  assert.equal(readFileSync(join(destination, '.git', 'HEAD'), 'utf8'), 'ref: refs/heads/main\n');
  assert.equal(existsSync(join(destination, 'deleted.txt')), false);
  writeFileSync(join(destination, 'hello world.txt'), 'download');
  run('rsync', ['-azc', '--delete', `${posix(destination)}/`, `${posix(source)}/`]);
  assert.equal(readFileSync(join(source, 'hello world.txt'), 'utf8'), 'download');
  // -V exits before connecting. This proves rsync can spawn the private SSH
  // through its own -e parser with spaces and an apostrophe in the path.
  const quote = (value) => `'${value.replaceAll("'", "''")}'`;
  const probe = spawnSync(join(runtime, 'bin', `rsync${extension}`), [
    '-e', `${quote(posix(join(runtime, 'bin', `ssh${extension}`)))} -V`,
    `${posix(source)}/`, 'unused-host:unused-path',
  ], { env, encoding: 'utf8', timeout: 30000 });
  if (probe.error) throw probe.error;
  assert.match(probe.stderr, /OpenSSH/i, `rsync could not spawn bundled SSH: ${probe.stderr}`);
  console.log('Bundled sync runtime passed relocation, signing, upload, download and deletion checks.');
} finally {
  rmSync(fixture, { recursive: true, force: true });
}
