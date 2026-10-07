const assert = require('node:assert/strict');
const { test, beforeEach } = require('node:test');
const { readFileSync } = require('node:fs');
const ts = require('typescript');

// Exercise the production Zustand store without adding a test runner dependency.
const source = readFileSync(require.resolve('../src/stores/useRemoteTerminalStore.ts'), 'utf8');
const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS } }).outputText;
const loaded = { exports: {} };
new Function('require', 'module', 'exports', compiled)(require, loaded, loaded.exports);
const store = loaded.exports.useRemoteTerminalStore;
const folder = (id) => ({ tab_id: id, local_dir: '', slug: id, files_synced: false });
const terminal = (id, tab, title = id) => ({ terminal_id: id, tab_id: tab, tool: 'codex', title, status: 'connected' });
beforeEach(() => store.getState().resetLogin());

test('remote create, rename and close propagate without changing the selected tab', () => {
    store.getState().reconcileSession({ inventory_synced: true, folders: [folder('project')], terminals: [terminal('a', 'project')] });
    store.getState().setActiveTerminal('project', 'a');
    store.getState().reconcileSession({ inventory_synced: true, folders: [folder('project'), folder('other')], terminals: [terminal('a', 'project', 'Renamed'), terminal('b', 'other')] });
    assert.equal(store.getState().activeTabId, 'project');
    assert.equal(store.getState().folders.project.activeTerminalId, 'a');
    assert.equal(store.getState().terminals.a.title, 'Renamed');
    store.getState().reconcileSession({ inventory_synced: true, folders: [folder('other')], terminals: [terminal('b', 'other')] });
    assert.equal(store.getState().terminals.a, undefined);
    assert.equal(store.getState().activeTabId, 'other');
});

test('an unavailable inventory preserves tabs while an authoritative empty snapshot removes them', () => {
    store.getState().reconcileSession({ inventory_synced: true, folders: [folder('project')], terminals: [terminal('a', 'project')] });
    store.getState().reconcileSession({ inventory_synced: false, folders: [], terminals: [] });
    assert.equal(store.getState().terminals.a.terminalId, 'a');
    store.getState().reconcileSession({ inventory_synced: true, folders: [], terminals: [] });
    assert.deepEqual(store.getState().folderOrder, []);
    assert.deepEqual(store.getState().terminals, {});
});

test('a create response following inventory hydration does not duplicate a terminal', () => {
    store.getState().reconcileSession({ inventory_synced: true, folders: [folder('project')], terminals: [terminal('a', 'project')] });
    store.getState().addTerminal('a', 'project', 'codex');
    assert.deepEqual(store.getState().folders.project.terminalIds, ['a']);
});
