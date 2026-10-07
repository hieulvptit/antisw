import { create } from 'zustand';

/** One-time, global SSO login gate - unrelated to any folder tab. */
export type LoginStatus = 'idle' | 'waiting_sso' | 'logged_in' | 'error';

export type SyncStatus = 'idle' | 'syncing' | 'synced' | 'error';
export type WorkspaceStatus = 'unknown' | 'checking' | 'synced' | 'initialization_required' | 'confirmation_required' | 'upload_required' | 'download_required' | 'commit_required' | 'conflict' | 'error';
export type TerminalStatus = 'connecting' | 'connected' | 'closed' | 'error';

/** Which remote CLI a terminal runs. Validated again server-side, but the
 * client never relies on that alone. */
export type RemoteTool = 'codex' | 'claude';

export interface RemoteTerminalReady {
    principal: string;
    host: string;
    ssh_user: string;
}

/** One open folder tab: a local directory synced into its own remote
 * workspace subdirectory (`<workspace_root>/<slug>/`, an absolute path
 * resolved server-side from the SSO session - not under `$HOME`), with its
 * own set of independent terminals. */
export interface FolderTabState {
    tabId: string;
    localDir: string;
    slug: string;
    syncStatus: SyncStatus;
    syncMessage: string | null;
    syncOutputLine: string | null;
    lastSyncedAt: number | null;
    filesSynced: boolean;
    workspaceStatus: WorkspaceStatus;
    workspaceError: string | null;
    terminalIds: string[];
    activeTerminalId: string | null;
}

/** One open terminal (PTY/SSH session) under a folder tab. */
export interface TerminalTabState {
    terminalId: string;
    tabId: string;
    tool: RemoteTool;
    status: TerminalStatus;
    title?: string | null;
}

const LOCAL_DIR_STORAGE_KEY = 'antisw_remote_terminal_local_dir';

function readPersistedLocalDir(): string {
    try {
        return localStorage.getItem(LOCAL_DIR_STORAGE_KEY) || '';
    } catch {
        return '';
    }
}

function persistLocalDir(dir: string) {
    try {
        localStorage.setItem(LOCAL_DIR_STORAGE_KEY, dir);
    } catch {
        // best-effort only (e.g. private browsing / disabled storage)
    }
}

interface RemoteTerminalState {
    // SSO login - one-time, global for the whole session.
    loginStatus: LoginStatus;
    loginUrl: string | null;
    ready: RemoteTerminalReady | null;
    errorMessage: string | null;

    /** Last folder picked (any tab), remembered only as a picker convenience. */
    lastLocalDir: string;
    /**
     * Which remote CLIs the SERVER offers, one "+ <tool>" button each. Comes
     * from the server (claude is off there by default) rather than being
     * hardcoded, so a tool that isn't installed never gets a button. Starts
     * empty and is filled in after login; the server re-checks it on create,
     * so this only ever decides what to show.
     */
    availableTools: RemoteTool[];

    // Folder tabs (outer tab strip).
    folderOrder: string[];
    folders: Record<string, FolderTabState>;
    activeTabId: string | null;

    // Terminals (inner tab strip, scoped to whichever folder owns them).
    terminals: Record<string, TerminalTabState>;

    reconcileSession: (session: {
        folders: { tab_id: string; local_dir: string; slug: string; files_synced: boolean }[];
        terminals: { terminal_id: string; tab_id: string; tool: RemoteTool; title?: string | null; status: TerminalStatus }[];
        inventory_synced: boolean;
    }) => void;
    setLoginStatus: (status: LoginStatus) => void;
    setLoginUrl: (url: string | null) => void;
    setReady: (ready: RemoteTerminalReady | null) => void;
    setError: (message: string | null) => void;
    setLastLocalDir: (dir: string) => void;
    setAvailableTools: (tools: RemoteTool[]) => void;
    resetLogin: () => void;

    addFolder: (tab: { tabId: string; localDir: string; slug: string }) => void;
    removeFolder: (tabId: string) => void;
    setActiveTab: (tabId: string | null) => void;
    updateFolderSync: (
        tabId: string,
        patch: Partial<Pick<FolderTabState, 'syncStatus' | 'syncMessage' | 'syncOutputLine' | 'lastSyncedAt' | 'filesSynced' | 'workspaceStatus' | 'workspaceError'>>,
    ) => void;

    addTerminal: (terminalId: string, tabId: string, tool: RemoteTool) => void;
    removeTerminal: (terminalId: string) => void;
    setActiveTerminal: (tabId: string, terminalId: string | null) => void;
    updateTerminalStatus: (terminalId: string, status: TerminalStatus) => void;
}

export const useRemoteTerminalStore = create<RemoteTerminalState>((set) => ({
    loginStatus: 'idle',
    loginUrl: null,
    ready: null,
    errorMessage: null,
    lastLocalDir: readPersistedLocalDir(),
    availableTools: [],

    folderOrder: [],
    folders: {},
    activeTabId: null,

    terminals: {},

    reconcileSession: (session) => set((state) => {
        const folders: Record<string, FolderTabState> = session.inventory_synced ? {} : { ...state.folders };
        const terminals: Record<string, TerminalTabState> = session.inventory_synced ? {} : { ...state.terminals };
        for (const folder of session.folders) {
            const old = state.folders[folder.tab_id];
            folders[folder.tab_id] = old ? { ...old } : {
                tabId: folder.tab_id, localDir: folder.local_dir, slug: folder.slug,
                syncStatus: 'idle', syncMessage: null, syncOutputLine: null, lastSyncedAt: null,
                filesSynced: folder.files_synced, workspaceStatus: 'unknown', workspaceError: null,
                terminalIds: [], activeTerminalId: null,
            };
            folders[folder.tab_id].localDir = folder.local_dir;
            folders[folder.tab_id].filesSynced = folder.files_synced;
        }
        for (const terminal of session.terminals) {
            terminals[terminal.terminal_id] = {
                terminalId: terminal.terminal_id, tabId: terminal.tab_id, tool: terminal.tool,
                title: terminal.title, status: terminal.status,
            };
        }
        for (const folder of Object.values(folders)) {
            const ids = Object.values(terminals).filter((t) => t.tabId === folder.tabId).map((t) => t.terminalId);
            folders[folder.tabId] = {
                ...folder, terminalIds: ids,
                activeTerminalId: folder.activeTerminalId && ids.includes(folder.activeTerminalId)
                    ? folder.activeTerminalId : ids[0] ?? null,
            };
        }
        const folderOrder = [...state.folderOrder.filter((id) => folders[id]),
            ...session.folders.map((f) => f.tab_id).filter((id) => !state.folderOrder.includes(id))];
        return {
            folders, terminals, folderOrder,
            activeTabId: state.activeTabId && folders[state.activeTabId] ? state.activeTabId : folderOrder[0] ?? null,
        };
    }),
    setLoginStatus: (loginStatus) => set({ loginStatus }),
    setLoginUrl: (loginUrl) => set({ loginUrl }),
    setReady: (ready) => set({ ready }),
    setError: (errorMessage) => set({ errorMessage }),
    setLastLocalDir: (dir) => {
        persistLocalDir(dir);
        set({ lastLocalDir: dir });
    },
    setAvailableTools: (availableTools) => set({ availableTools }),
    // Note: lastLocalDir is intentionally NOT cleared here - it is a
    // remembered picker preference, not session state. availableTools IS
    // cleared: it describes the server we were talking to.
    resetLogin: () => set({
        loginStatus: 'idle',
        loginUrl: null,
        ready: null,
        errorMessage: null,
        availableTools: [],
        folderOrder: [],
        folders: {},
        activeTabId: null,
        terminals: {},
    }),

    addFolder: (tab) => set((state) => ({
        folderOrder: [...state.folderOrder, tab.tabId],
        folders: {
            ...state.folders,
            [tab.tabId]: {
                tabId: tab.tabId,
                localDir: tab.localDir,
                slug: tab.slug,
                syncStatus: 'idle',
                syncMessage: null,
                syncOutputLine: null,
                lastSyncedAt: null,
                filesSynced: false,
                workspaceStatus: 'unknown',
                workspaceError: null,
                terminalIds: [],
                activeTerminalId: null,
            },
        },
        activeTabId: tab.tabId,
    })),

    removeFolder: (tabId) => set((state) => {
        const { [tabId]: _removed, ...restFolders } = state.folders;
        const folderOrder = state.folderOrder.filter((id) => id !== tabId);
        const terminals = Object.fromEntries(
            Object.entries(state.terminals).filter(([, term]) => term.tabId !== tabId),
        );
        const activeTabId = state.activeTabId === tabId
            ? (folderOrder[folderOrder.length - 1] ?? null)
            : state.activeTabId;
        return { folders: restFolders, folderOrder, terminals, activeTabId };
    }),

    setActiveTab: (activeTabId) => set({ activeTabId }),

    updateFolderSync: (tabId, patch) => set((state) => {
        const tab = state.folders[tabId];
        if (!tab) return state;
        return { folders: { ...state.folders, [tabId]: { ...tab, ...patch } } };
    }),

    addTerminal: (terminalId, tabId, tool) => set((state) => {
        const tab = state.folders[tabId];
        if (!tab) return state;
        return {
            terminals: {
                ...state.terminals,
                [terminalId]: { terminalId, tabId, tool, status: 'connecting' },
            },
            folders: {
                ...state.folders,
                [tabId]: {
                    ...tab,
                    terminalIds: tab.terminalIds.includes(terminalId) ? tab.terminalIds : [...tab.terminalIds, terminalId],
                    activeTerminalId: terminalId,
                },
            },
        };
    }),

    removeTerminal: (terminalId) => set((state) => {
        const term = state.terminals[terminalId];
        const { [terminalId]: _removed, ...restTerminals } = state.terminals;
        if (!term) return { terminals: restTerminals };
        const tab = state.folders[term.tabId];
        if (!tab) return { terminals: restTerminals };
        const terminalIds = tab.terminalIds.filter((id) => id !== terminalId);
        const activeTerminalId = tab.activeTerminalId === terminalId
            ? (terminalIds[terminalIds.length - 1] ?? null)
            : tab.activeTerminalId;
        return {
            terminals: restTerminals,
            folders: { ...state.folders, [term.tabId]: { ...tab, terminalIds, activeTerminalId } },
        };
    }),

    setActiveTerminal: (tabId, terminalId) => set((state) => {
        const tab = state.folders[tabId];
        if (!tab) return state;
        return { folders: { ...state.folders, [tabId]: { ...tab, activeTerminalId: terminalId } } };
    }),

    updateTerminalStatus: (terminalId, status) => set((state) => {
        const term = state.terminals[terminalId];
        if (!term) return state;
        return { terminals: { ...state.terminals, [terminalId]: { ...term, status } } };
    }),
}));
