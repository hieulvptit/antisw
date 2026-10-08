const assert = require('node:assert/strict');
const { test } = require('node:test');
const { readFileSync } = require('node:fs');
const ts = require('typescript');

// Execute the page's actual handler with dialog and remote operations mocked.
const source = ts.createSourceFile('RemoteTerminal.tsx', readFileSync(require.resolve('../src/pages/RemoteTerminal.tsx'), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
let handler;
function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(source) === 'handleAddFolder') handler = node.initializer.getText(source);
    ts.forEachChild(node, visit);
}
visit(source);
assert.ok(handler);
const compiled = ts.transpileModule(`const handler = ${handler};`, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;

async function add(status, selected = '/local/project') {
    const calls = [];
    const dependencies = {
        isTauri: () => true,
        t: (key) => key,
        openDialog: async () => selected,
        setLastLocalDir: () => {},
        invoke: async (command, args) => {
            calls.push([command, args]);
            return { tab_id: 'tab', slug: 'project' };
        },
        addFolder: (folder) => calls.push(['add', folder]),
        checkWorkspace: async () => status ? { status } : null,
        setSyncChoiceTabId: (id) => calls.push(['download-choice', id]),
        handleSyncFolder: async (id) => calls.push(['upload', id]),
        useRemoteTerminalStore: { getState: () => ({ folders: { tab: { workspaceError: 'server unreachable' } } }) },
        describeRemoteError: String,
        showToast: (...args) => calls.push(['toast', ...args]),
    };
    const run = new Function(...Object.keys(dependencies), `${compiled}\nreturn handler;`)(...Object.values(dependencies));
    await run();
    return calls;
}

test('adding an uploadable project syncs immediately after registering the local folder', async () => {
    for (const status of ['upload_required', 'initialization_required', 'confirmation_required', 'synced']) {
        const calls = await add(status);
        assert.equal(calls[0][0], 'remote_terminal_add_folder');
        assert.equal(calls[1][0], 'add');
        assert.deepEqual(calls[2], ['upload', 'tab']);
    }
});

test('remote changes require a download choice without uploading local files', async () => {
    const calls = await add('download_required');
    assert.deepEqual(calls[2], ['download-choice', 'tab']);
    assert.ok(!calls.some(([action]) => action === 'upload'));
});

test('conflicts and commit requirements do not trigger a transfer', async () => {
    for (const status of ['conflict', 'commit_required', 'error']) {
        assert.equal((await add(status)).length, 2);
    }
});

test('failed checks show the stored error and do not upload', async () => {
    const calls = await add(null);
    assert.deepEqual(calls[2], ['toast', 'server unreachable', 'error']);
    assert.ok(!calls.some(([action]) => action === 'upload'));
});

test('cancelling folder selection does not register or sync anything', async () => {
    assert.deepEqual(await add('upload_required', null), []);
});
