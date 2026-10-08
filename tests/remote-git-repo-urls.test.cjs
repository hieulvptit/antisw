const assert = require('node:assert/strict');
const { test } = require('node:test');
const { readFileSync } = require('node:fs');
const ts = require('typescript');
const source = readFileSync(require.resolve('../src/utils/remoteGitRepoUrls.ts'), 'utf8');
function load(storage) {
    const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS } }).outputText;
    const loaded = { exports: {} };
    new Function('module', 'exports', 'localStorage', compiled)(loaded, loaded.exports, storage);
    return loaded.exports;
}
const ready = { principal: 'alice', host: 'server', ssh_user: 'cdx-alice' };
test('repository destination survives reload, is shared by workspace, and can be replaced', () => {
    const values = new Map();
    const storage = { getItem: (key) => values.get(key), setItem: (key, value) => values.set(key, value) };
    const first = load(storage); first.saveGitRepoUrl(ready, 'project', 'https://gitlab.com/team/old.git');
    const reloaded = load(storage);
    assert.equal(reloaded.readGitRepoUrl(ready, 'project'), 'https://gitlab.com/team/old.git');
    reloaded.saveGitRepoUrl(ready, 'project', 'https://github.com/team/new.git');
    assert.equal(first.readGitRepoUrl(ready, 'project'), 'https://github.com/team/new.git');
    assert.equal(first.readGitRepoUrl({ ...ready, principal: 'bob' }, 'project'), '');
    assert.equal(first.readGitRepoUrl({ ...ready, host: 'other-server' }, 'project'), '');
    assert.equal(first.readGitRepoUrl(ready, 'other-project'), '');
});
test('URL storage excludes credentials and handles unavailable storage', () => {
    const values = new Map();
    const urls = load({ getItem: (key) => values.get(key), setItem: (key, value) => values.set(key, value) });
    for (const url of ['https://user:secret@gitlab.com/team/repo', 'https://gitlab.com/team/repo?token=secret', 'ssh://gitlab.com/team/repo']) urls.saveGitRepoUrl(ready, 'project', url);
    assert.equal(values.size, 0);
    const unavailable = load({ getItem() { throw Error('disabled'); }, setItem() { throw Error('disabled'); } });
    assert.equal(unavailable.readGitRepoUrl(ready, 'project'), '');
    assert.doesNotThrow(() => unavailable.saveGitRepoUrl(ready, 'project', 'https://gitlab.com/team/repo'));
});
