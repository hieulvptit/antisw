import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Modal, QRCode } from 'antd';
import { listen } from '@tauri-apps/api/event';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import {
    LogIn, Loader2, Terminal as TerminalIcon, XCircle,
    FolderOpen, RefreshCw, Plus, X, Share2,
} from 'lucide-react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';

import { request as invoke } from '../utils/request';
import { showToast } from '../components/common/ToastContainer';
import { isTauri } from '../utils/env';
import { useRemoteTerminalStore, RemoteTerminalReady, RemoteTool, WorkspaceStatus } from '../stores/useRemoteTerminalStore';
import { installImeInput } from '../utils/xtermImeInput';
import { debugState, escapeForLog, installDebugSwitches, record } from '../utils/remoteTerminalDebug';

interface AddFolderResult {
    tab_id: string;
    slug: string;
}

interface WorkspaceCheckPayload {
    status: Exclude<WorkspaceStatus, 'unknown' | 'checking' | 'error'>;
    client_id: string | null;
    session_version: number;
    sync_revision: number;
}

interface SyncOutputPayload {
    tab_id: string;
    line: string;
}

interface TerminalOutputPayload {
    terminal_id: string;
    data: string;
}

interface TerminalClosedPayload {
    terminal_id: string;
    reason: string;
}

interface RestoredFolderPayload {
    tab_id: string;
    local_dir: string;
    slug: string;
    files_synced: boolean;
}

interface RestoredTerminalPayload {
    terminal_id: string;
    tab_id: string;
    tool: RemoteTool;
}

interface RestoredSessionPayload {
    ready: RemoteTerminalReady;
    folders: RestoredFolderPayload[];
    terminals: RestoredTerminalPayload[];
}

/** Format elapsed time since `timestamp` as a short "Xs" / "Xm" / "Xh" string. */
function formatElapsed(timestamp: number): string {
    const seconds = Math.max(0, Math.floor((Date.now() - timestamp) / 1000));
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    return `${hours}h`;
}

/**
 * Ceiling on the input hold, so continuous typing still streams rather than
 * buffering a whole paragraph. The hold ITSELF defaults to 0 - see
 * `debugState.holdMs` - because any non-zero value makes the echo arrive in
 * visible bursts: the remote, not xterm, is what draws the characters, so a
 * hold is added directly to the delay before they appear. `__rtHold(ms)` in
 * the console turns it on for a session without a rebuild.
 */
const INPUT_MAX_HOLD_MS = 700;

/** Last path segment, tolerant of both `/` and `\` separators. */
function basename(path: string): string {
    const trimmed = path.replace(/[\\/]+$/, '');
    const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
    return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

interface XtermInstance {
    term: Terminal;
    fitAddon: FitAddon;
    resizeObserver: ResizeObserver;
    /** Removes the IME input listeners installed on this terminal. */
    disposeImeInput: () => void;
    /** Re-fits and, only if the result actually differs from what was last
     * sent, forwards the new size to the backend PTY. See the comment above
     * its definition in `attachTerminalRef` for why this dedupe matters. */
    sendResize: () => void;
    /** Unconditionally forces the remote PTY to repaint, even when the size
     * hasn't changed. See the comment above its definition in
     * `attachTerminalRef` for why this is needed on top of `sendResize`. */
    forceRemoteRedraw: () => void;
}

function RemoteTerminal() {
    const { t } = useTranslation();
    const {
        loginStatus, ready, errorMessage,
        folderOrder, folders, activeTabId,
        terminals, availableTools,
        setLoginStatus, setLoginUrl, setReady, setError, setLastLocalDir, setAvailableTools, resetLogin,
        addFolder, removeFolder, setActiveTab, updateFolderSync,
        addTerminal, removeTerminal, setActiveTerminal, updateTerminalStatus,
    } = useRemoteTerminalStore();

    const [shareUrl, setShareUrl] = useState<string | null>(null);
    const [isSharing, setIsSharing] = useState(false);
    const [isOpeningTerminal, setIsOpeningTerminal] = useState(false);
    const openingTerminalRef = useRef(false);
    const [remoteConnected, setRemoteConnected] = useState(false);
    const [authRequired, setAuthRequired] = useState(false);
    const describeRemoteError = useCallback((error: unknown): string => {
        const message = String(error);
        if (message.includes('workspace_protocol_not_support')) {
            return t('remote_terminal.workspace.protocol_not_supported');
        }
        if (message.includes('remote_terminal_auth_required')
            || message.includes('workspace_check_unauthorized')
            || (message.includes('term_auth_rejected') && message.includes('401'))) {
            setAuthRequired(true);
            return t('remote_terminal.auth.required');
        }
        return message;
    }, [t]);

    useEffect(() => {
        if (loginStatus !== 'logged_in') {
            setRemoteConnected(false);
            return;
        }
        let cancelled = false;
        let checking = false;
        const checkConnection = async () => {
            if (checking) return;
            checking = true;
            try {
                const connected = await invoke<boolean>('remote_terminal_check_connection');
                if (!cancelled) setRemoteConnected(connected && navigator.onLine);
            } catch {
                if (!cancelled) setRemoteConnected(false);
            } finally {
                checking = false;
            }
        };
        void checkConnection();
        const interval = setInterval(() => void checkConnection(), 10000);
        window.addEventListener('focus', checkConnection);
        window.addEventListener('online', checkConnection);
        const onOffline = () => setRemoteConnected(false);
        window.addEventListener('offline', onOffline);
        return () => {
            cancelled = true;
            clearInterval(interval);
            window.removeEventListener('focus', checkConnection);
            window.removeEventListener('online', checkConnection);
            window.removeEventListener('offline', onOffline);
        };
    }, [loginStatus]);

    // xterm.js instances live outside React state/store - one per terminal_id,
    // created once and kept mounted (hidden via CSS) for the terminal's whole
    // lifetime so switching tabs never loses scrollback or requires a new PTY.
    const xtermInstancesRef = useRef<Map<string, XtermInstance>>(new Map());
    // The terminal viewport container: always present once a folder tab is
    // active, regardless of which (if any) specific terminal is mounted -
    // used to measure the real cols/rows BEFORE opening a new terminal.
    const viewportRef = useRef<HTMLDivElement | null>(null);
    // One stable `ref` callback per terminal_id, cached across renders. A
    // fresh inline arrow function passed to `ref` every render (e.g.
    // `ref={(el) => attachTerminalRef(el, terminalId)}`) is a NEW callback
    // identity each time, and React detaches (calls it with `null`) then
    // reattaches (calls it with the element again) on every single
    // re-render whose ref prop identity changed - even though the DOM node
    // itself never changed. With `forceTick` re-rendering this component
    // every second while any tab is "synced", that tore down and rebuilt
    // the xterm.js instance every second; xterm's dispose() doesn't fully
    // clean up the DOM it created, so this left stray leftover textareas/
    // rows stacked on top of the new ones (visible as duplicate inputs and
    // overlapping text). Caching the callback per id keeps its identity
    // stable across re-renders, so it only fires on a REAL mount/unmount.
    // Entries are keyed by terminal id and live as long as that terminal is in
    // the store - they are pruned by the effect below, NEVER from inside the
    // ref callback's own detach branch. Deleting there looked harmless but
    // made the cache self-defeating: React commits a changed ref by calling
    // the OLD callback with `null` and only THEN the new one with the
    // element, so the detach ran AFTER render had already cached the new
    // callback and deleted that fresh entry. The next render therefore missed
    // the cache again, built yet another callback, and React detached/
    // reattached once more - so a single detach (React 19 StrictMode performs
    // one on mount by design) put this into a state where EVERY subsequent
    // render tore down and rebuilt the xterm.js instance. That is what made
    // the console vanish on "Sync now": syncing re-renders this component
    // (progress updates, then a tick per second for "synced Xs ago"), and
    // each of those renders replaced the terminal with a brand-new, empty one
    // - a bare blinking cursor.
    const terminalRefCallbacksRef = useRef<Map<string, (el: HTMLDivElement | null) => void>>(new Map());
    // Terminal ids whose xterm.js instance has been built at least once, so a
    // REBUILT instance can be told apart from a first mount. A rebuilt one is
    // empty while the remote pty is unchanged, and `sendResize` would dedupe
    // the very call that could repaint it, so those need a forced redraw.
    const createdOnceRef = useRef<Set<string>>(new Set());
    const [, forceRerender] = useState(0);
    useEffect(() => {
        // Re-render once on mount so terminal ids added before this effect
        // runs (shouldn't normally happen) still get their divs attached.
        forceRerender((v) => v + 1);
    }, []);

    // Tick every second while any tab shows a "synced Xs ago" status.
    const [, forceTick] = useState(0);
    useEffect(() => {
        const anySynced = folderOrder.some((id) => folders[id]?.syncStatus === 'synced');
        if (!anySynced) return;
        const id = setInterval(() => forceTick((v) => v + 1), 1000);
        return () => clearInterval(id);
    }, [folderOrder, folders]);

    const activeTab = activeTabId ? folders[activeTabId] : null;

    // xterm.js does NOT grab keyboard focus on its own when `term.open()` is
    // called, and toggling a container from display:none back to block does
    // not restore it either - without an explicit `.focus()` call, keypresses
    // simply go nowhere (no error, nothing visibly wrong, just silence).
    const focusTerminal = useCallback((terminalId: string | null | undefined) => {
        if (!terminalId) return;
        xtermInstancesRef.current.get(terminalId)?.term.focus();
    }, []);

    // Output that arrived before its xterm.js instance existed, keyed by
    // terminal id and replayed in order once it does.
    //
    // This is NOT an optimization - dropping it corrupts the terminal. On a
    // session restore the backend spawns the ssh/tmux PTY and starts
    // streaming immediately, while this page only creates the xterm.js
    // instances after `remote_terminal_restore_session` resolves, the store
    // hydrates, and React mounts the terminal divs. Everything tmux emits in
    // that window is its TERMINAL INITIALIZATION: enter alternate screen
    // (`\x1b[?1049h`), clear, set the scroll region, then paint the screen.
    // Silently discarding it (what `instances.get(id)?.term.write(...)` used
    // to do) left the terminal in the PRIMARY buffer with no scroll region,
    // so every later absolute-positioned repaint from tmux accumulated down
    // a scrollable buffer instead of overwriting a fixed alt screen - the
    // duplicated, stacked copies of the codex UI. A real ssh terminal never
    // misses this prefix, which is why the same session looks fine when
    // opened directly on the server.
    const pendingOutputRef = useRef<Map<string, string[]>>(new Map());
    // Bounded so a terminal whose div never mounts can't grow without limit.
    const MAX_PENDING_OUTPUT_CHARS = 1_000_000;
    const bufferPendingOutput = useCallback((terminalId: string, data: string) => {
        const chunks = pendingOutputRef.current.get(terminalId) ?? [];
        chunks.push(data);
        let total = chunks.reduce((sum, c) => sum + c.length, 0);
        while (total > MAX_PENDING_OUTPUT_CHARS && chunks.length > 1) {
            total -= chunks.shift()!.length;
        }
        pendingOutputRef.current.set(terminalId, chunks);
    }, []);

    // Throttle the "failed to send input" toast per terminal so a broken
    // connection doesn't spam a toast on every single keystroke - but still
    // always console.error every failure so it's never silently swallowed.
    const lastWriteErrorToastRef = useRef<Map<string, number>>(new Map());
    const reportWriteError = useCallback((terminalId: string, error: unknown) => {
        console.error(`[RemoteTerminal] Failed to write to terminal ${terminalId}:`, error);
        const now = Date.now();
        const last = lastWriteErrorToastRef.current.get(terminalId) ?? 0;
        if (now - last > 4000) {
            lastWriteErrorToastRef.current.set(terminalId, now);
            showToast(t('remote_terminal.toast.write_failed'), 'error');
        }
    }, [t]);

    // Input has to leave THIS side in order, one send at a time.
    //
    // `invoke` does not guarantee that: each command becomes its own task on
    // the Rust async runtime, so two calls issued microseconds apart can be
    // picked up by different workers and arrive swapped. Human typing never
    // exposed that - keystrokes are ~50ms apart - but a Vietnamese IME does.
    // EVKey/Telex writes a tone mark as "backspace, then the new character",
    // both fired inside the SAME JS tick. Swap those two and the backspace
    // lands after the character and deletes it; the IME then keeps counting
    // backspaces against a screen that no longer matches, and the word comes
    // apart - "những" arriving as "ữ" is exactly that cascade.
    //
    // So the order is pinned where it is actually known: here, in
    // single-threaded JS, in onData order. One invoke in flight per terminal,
    // and anything typed meanwhile is appended to the next payload. The Rust
    // writer queue still serializes behind this; it just cannot reconstruct an
    // order that was already lost crossing the IPC boundary.
    const pendingInputRef = useRef<Map<string, string>>(new Map());
    const inputInFlightRef = useRef<Set<string>>(new Set());
    const flushInput = useCallback((terminalId: string) => {
        if (inputInFlightRef.current.has(terminalId)) return;
        const data = pendingInputRef.current.get(terminalId);
        if (!data) return;
        pendingInputRef.current.set(terminalId, '');
        inputInFlightRef.current.add(terminalId);
        record('send', `"${escapeForLog(data)}"`);
        invoke('remote_terminal_write', { terminalId, data })
            .catch((e) => reportWriteError(terminalId, e))
            .finally(() => {
                inputInFlightRef.current.delete(terminalId);
                scheduleFlushRef.current(terminalId);
            });
    }, [reportWriteError]);

    // Optionally hold typed text for a moment before sending it, instead of
    // firing a request per keystroke.
    //
    // A Vietnamese IME does not type a word once - it retypes it. "Nhưngx"
    // becomes "Những" by deleting "ưng" and inserting "ững", so every tone
    // mark puts a burst of backspaces and a replacement on the wire. Holding
    // the text lets those cancel out locally: a backspace that lands on a
    // character still sitting in this buffer just removes it, and that round
    // trip never happens.
    //
    // OFF by default (`debugState.holdMs === 0`). A hold does batch the burst,
    // but the characters on screen come from the REMOTE echo, so it is added
    // straight to the delay before anything appears - which reads as the
    // terminal stuttering, one burst per pause. `__rtHold(ms)` turns it on
    // live if it is ever worth that trade.
    const inputTimerRef = useRef<Map<string, number>>(new Map());
    const inputHoldStartRef = useRef<Map<string, number>>(new Map());

    const cancelFlushTimer = useCallback((terminalId: string) => {
        const handle = inputTimerRef.current.get(terminalId);
        if (handle !== undefined) {
            clearTimeout(handle);
            inputTimerRef.current.delete(terminalId);
        }
    }, []);

    const scheduleFlush = useCallback((terminalId: string) => {
        if (!pendingInputRef.current.get(terminalId)) {
            cancelFlushTimer(terminalId);
            inputHoldStartRef.current.delete(terminalId);
            return;
        }
        if (debugState.holdMs <= 0) {
            cancelFlushTimer(terminalId);
            inputHoldStartRef.current.delete(terminalId);
            flushInput(terminalId);
            return;
        }
        const now = Date.now();
        const heldSince = inputHoldStartRef.current.get(terminalId) ?? now;
        inputHoldStartRef.current.set(terminalId, heldSince);
        cancelFlushTimer(terminalId);
        const delay = Math.max(0, Math.min(debugState.holdMs, INPUT_MAX_HOLD_MS - (now - heldSince)));
        const handle = window.setTimeout(() => {
            inputTimerRef.current.delete(terminalId);
            inputHoldStartRef.current.delete(terminalId);
            flushInput(terminalId);
        }, delay);
        inputTimerRef.current.set(terminalId, handle);
    }, [cancelFlushTimer, flushInput]);

    // `flushInput` re-arms the hold for whatever piled up during its request,
    // but is defined first (they call each other), so it goes through a ref.
    const scheduleFlushRef = useRef(scheduleFlush);
    scheduleFlushRef.current = scheduleFlush;

    // The single entry point for anything typed into a terminal, whether it
    // came from xterm's own `onData` or from the IME listeners installed by
    // `installImeInput`. Both arrive on the same JS task queue, so appending
    // here in call order is what keeps a "delete, then re-insert" rewrite from
    // an IME in the order the IME meant it.
    const sendInput = useCallback((terminalId: string, data: string) => {
        // Recorded next to the DOM events that produced it: together they say
        // whether a missing character was never emitted at all, or was emitted
        // and then lost on the wire. The two look identical on screen.
        record('queue', `"${escapeForLog(data)}"`);

        // Anything with a C0 byte in it - Enter, Tab, Escape, an arrow key's
        // escape sequence, Ctrl-C - is not text being composed and must not
        // wait: it is how the user acts on what they typed, and a TUI that
        // gets it late feels broken. It also ends the current hold, so the
        // text keeps its place in front of the key that acts on it.
        const urgent = /[\u0000-\u001f]/.test(data);

        const buffered = Array.from(pendingInputRef.current.get(terminalId) ?? '');
        for (const ch of data) {
            const last = buffered.length > 0 ? buffered[buffered.length - 1].codePointAt(0)! : -1;
            // A backspace only cancels a PRINTABLE character that has not left
            // yet. Against an empty buffer, or against a control byte or an
            // earlier backspace that could not be collapsed either, it has to
            // travel - the character it means to delete is already on the
            // remote screen.
            if (!urgent && ch === '\u007f' && last >= 0x20 && last !== 0x7f) {
                buffered.pop();
            } else {
                buffered.push(ch);
            }
        }
        pendingInputRef.current.set(terminalId, buffered.join(''));

        if (urgent) {
            cancelFlushTimer(terminalId);
            inputHoldStartRef.current.delete(terminalId);
            flushInput(terminalId);
            return;
        }
        scheduleFlush(terminalId);
    }, [cancelFlushTimer, flushInput, scheduleFlush]);

    // `attachTerminalRef` must keep an EMPTY dep list (see
    // `terminalRefCallbacksRef` above - a changing identity there rebuilt the
    // xterm instance on every render), so it reaches `sendInput` through a ref
    // rather than closing over it.
    const sendInputRef = useRef(sendInput);
    sendInputRef.current = sendInput;

    // Debug hook: `__rt()` in the devtools console reports what each terminal
    // actually holds right now. A blank terminal has turned out to be several
    // different faults - no data arriving, data arriving but the renderer
    // paused, or a geometry nobody intended - and they are indistinguishable
    // by looking at it. This says which one it is in one call.
    useEffect(() => {
        (window as unknown as Record<string, unknown>).__rt = () => {
            const out: Record<string, unknown> = {};
            xtermInstancesRef.current.forEach((inst, id) => {
                const core = (inst.term as unknown as { _core?: { _renderService?: { _isPaused?: boolean } } })._core;
                const buffer = inst.term.buffer.active;
                let nonEmptyRows = 0;
                for (let i = 0; i < inst.term.rows; i++) {
                    if ((buffer.getLine(i)?.translateToString(true) ?? '').trim().length > 0) nonEmptyRows++;
                }
                out[id.slice(0, 8)] = {
                    size: `${inst.term.cols}x${inst.term.rows}`,
                    nonEmptyRows,
                    totalRows: inst.term.rows,
                    scrollback: buffer.length,
                    rendererPaused: core?._renderService?._isPaused ?? 'n/a',
                    active: id === activeTab?.activeTerminalId,
                };
            });
            return out;
        };
        installDebugSwitches();
    }, [activeTab?.activeTerminalId]);

    const attachTerminalRef = useCallback((el: HTMLDivElement | null, terminalId: string) => {
        if (el) {
            if (xtermInstancesRef.current.has(terminalId)) return;
            // Temporary: a terminal repeatedly going blank turned out to look
            // exactly like an xterm instance being rebuilt underneath it, and
            // resizes were being sent once a second for an UNCHANGED size -
            // which only happens with a fresh closure, i.e. a fresh instance.
            // This says outright whether that is what is happening.
            record('xterm.create', terminalId.slice(0, 8));

            const term = new Terminal({
                // `convertEol` forces every bare LF ('\n') to also reset the
                // cursor column to 0 (as if it were CRLF). That's fine for a
                // plain shell but wrong for a full-screen app like tmux/codex
                // that positions its cursor itself - such apps send '\n' on
                // purpose (move down a row, KEEP the column) e.g. when
                // repainting a specific column range across several rows
                // without a full clear. Forcing it to CRLF desyncs xterm's
                // cursor from where the app thinks it is, and every
                // subsequent absolute-position draw then lands on the wrong
                // row/column - producing exactly the reordered/missing/
                // overlapping text seen in this remote terminal.
                cursorBlink: true,
                fontSize: 13,
                fontFamily: 'Menlo, Consolas, "Courier New", monospace',
                theme: {
                    background: '#0d1117',
                    foreground: '#e6edf3',
                },
            });
            const fitAddon = new FitAddon();
            term.loadAddon(fitAddon);
            term.open(el);
            fitAddon.fit();
            term.focus();

            // ResizeObserver can fire repeatedly for the SAME final size
            // (sub-pixel layout jitter, a focus-triggered reflow elsewhere on
            // the page - e.g. just clicking into this terminal to focus it -
            // or the initial fire it does as soon as `.observe()` is called,
            // which duplicates the size already sent when this PTY was
            // opened). Every `remote_terminal_resize` call forwards a fresh
            // SIGWINCH to the remote tmux/codex session, which forces a full
            // repaint, and a transient 0x0 reading (possible mid-layout,
            // before the container has a real size yet) is worse: some
            // remote TUIs treat a 0x0 window as "not visible" and stop
            // rendering into it until a real resize arrives. Both looked
            // exactly like "the terminal disappears" after interacting with
            // it, so resizes are only forwarded when the size actually
            // changed and is non-zero.
            //
            // These MUST start as sentinels rather than `term.cols/rows`: the
            // remote PTY was NOT necessarily opened at the size this local
            // terminal just fitted to. A RESTORED terminal's PTY is
            // reconnected at a hardcoded 80x24 placeholder (see
            // `restore_session`, which explicitly defers to "the frontend's
            // resize call shortly after mount"), and even a freshly opened
            // one was sized from `measureViewport`'s throwaway probe, which
            // can disagree with what this real terminal ends up fitting to.
            // Seeding with the local size made `sendResize` believe the
            // backend already had it and dedupe away the one call that would
            // have corrected it - leaving the remote tmux permanently
            // SMALLER than the local xterm. tmux then paints its whole screen
            // into the top 24 rows and never touches anything below or to the
            // right, so stale content from an earlier paint stays on screen
            // forever: a second, garbled copy of the UI (duplicated input
            // box, scrambled text) below the real one.
            let lastSentCols = -1;
            let lastSentRows = -1;
            const sendResize = () => {
                const { cols, rows } = term;
                if (cols <= 0 || rows <= 0) return;
                if (cols === lastSentCols && rows === lastSentRows) return;
                lastSentCols = cols;
                lastSentRows = rows;
                invoke('remote_terminal_resize', { terminalId, cols, rows, forceRepaint: false }).catch(() => { });
            };

            // Switching tabs can need a fresh screen even when geometry has
            // not changed. Ask the server to redraw this viewer's tmux client
            // without resizing the shared pane used by the web share viewer.
            const forceRemoteRedraw = () => {
                const { cols, rows } = term;
                if (cols <= 0 || rows <= 0) return;
                lastSentCols = cols;
                lastSentRows = rows;
                // A redraw is scoped by this viewer's session token server-side.
                invoke('remote_terminal_resize', { terminalId, cols, rows, forceRepaint: true }).catch(() => { });
            };

            // Replay anything that streamed in before this instance existed
            // (see `pendingOutputRef`) FIRST, so the terminal gets tmux's
            // alt-screen/scroll-region initialization before anything else
            // is written to it.
            const pending = pendingOutputRef.current.get(terminalId);
            if (pending) {
                pendingOutputRef.current.delete(terminalId);
                for (const chunk of pending) term.write(chunk);
            }

            // Push this terminal's real size to the backend right away. The
            // switch effect's `forceRemoteRedraw` can't be relied on for
            // this: on a session restore it runs BEFORE these divs mount
            // (the instance doesn't exist yet, so it bails), and its deps
            // don't change again afterwards - so without this call a
            // restored terminal would sit at its 80x24 placeholder until
            // the user happened to switch tabs. Ordered after the replay so
            // the repaint tmux sends in response lands last and wins.
            //
            // A REBUILT instance needs the forced variant instead. The remote
            // pty is still at the size it already had, so `sendResize` would
            // send one no-op resize that raises no SIGWINCH, nothing repaints,
            // and this brand-new empty buffer stays empty. A first mount
            // deliberately does NOT force it: the pty was just opened at the
            // measured size, and reflowing it twice straight away is what used
            // to garble the remote TUI's first frame.
            if (createdOnceRef.current.has(terminalId)) {
                forceRemoteRedraw();
            } else {
                createdOnceRef.current.add(terminalId);
                sendResize();
            }

            term.onData((data) => sendInputRef.current(terminalId, data));

            // Text an input method inserts never reaches `onData` - xterm 6
            // drops it. See `installImeInput` for what exactly it drops and
            // why; without this, Vietnamese Telex loses characters.
            const disposeImeInput = installImeInput(term, (data) => sendInputRef.current(terminalId, data));

            const resizeObserver = new ResizeObserver(() => {
                // Inactive terminals are hidden with `visibility: hidden`
                // (see the terminal viewport's comment), which keeps a real
                // on-screen box, so this normally never sees a zero size.
                // Kept as a safety net anyway: xterm-addon-fit's
                // proposeDimensions() clamps its MINIMUM to 2 cols x 1 row
                // rather than returning undefined for a zero-size container,
                // which would defeat the `cols <= 0 || rows <= 0` guard in
                // `sendResize` below (2 and 1 are both > 0) - fit() would
                // shrink the terminal to 2x1 and forward that as a real
                // SIGWINCH to the remote tmux session, which redraws
                // destructively at that size. Skip fitting/resizing entirely
                // if the container ever does have no real on-screen size.
                if (el.offsetWidth === 0 || el.offsetHeight === 0) return;
                fitAddon.fit();
                sendResize();
            });
            resizeObserver.observe(el);

            xtermInstancesRef.current.set(terminalId, { term, fitAddon, resizeObserver, disposeImeInput, sendResize, forceRemoteRedraw });
        } else {
            const inst = xtermInstancesRef.current.get(terminalId);
            if (inst) {
                record('xterm.dispose', terminalId.slice(0, 8));
                inst.resizeObserver.disconnect();
                inst.disposeImeInput();
                inst.term.dispose();
                xtermInstancesRef.current.delete(terminalId);
            }
            // Nothing else is dropped here. A detach does NOT mean the
            // terminal is gone - React detaches a ref whenever its identity
            // changes, and StrictMode does one on every mount - so the cached
            // ref callback (see `terminalRefCallbacksRef`) and any buffered
            // output must survive it. Output that streams in while no
            // instance exists keeps accumulating in `pendingOutputRef` and is
            // replayed by the branch above when the instance comes back;
            // discarding it here threw away tmux's alt-screen initialization.
            // The effect below prunes all of this once a terminal is really
            // removed from the store.
        }
    }, []);

    // Prune per-terminal bookkeeping for terminals that are actually gone
    // (closed, or their folder tab was closed) - the one place allowed to,
    // since it goes by the store rather than by a ref detach.
    useEffect(() => {
        const live = new Set(Object.keys(terminals));
        const pruneMap = (map: Map<string, unknown>) => {
            for (const id of [...map.keys()]) {
                if (!live.has(id)) map.delete(id);
            }
        };
        // A held flush for a terminal that no longer exists has nothing to
        // write to, so cancel the timer rather than let it fire into a closed
        // session.
        for (const id of [...inputTimerRef.current.keys()]) {
            if (!live.has(id)) cancelFlushTimer(id);
        }
        pruneMap(terminalRefCallbacksRef.current);
        pruneMap(pendingOutputRef.current);
        pruneMap(lastWriteErrorToastRef.current);
        pruneMap(pendingInputRef.current);
        pruneMap(inputHoldStartRef.current);
        for (const id of [...createdOnceRef.current]) {
            if (!live.has(id)) createdOnceRef.current.delete(id);
        }
        for (const id of [...inputInFlightRef.current]) {
            if (!live.has(id)) inputInFlightRef.current.delete(id);
        }
    }, [terminals, cancelFlushTimer]);

    // Leaving this page must not swallow text that is still being held: send
    // it now rather than let the timer fire after the component is gone.
    useEffect(() => {
        const timers = inputTimerRef.current;
        const pending = pendingInputRef.current;
        const inFlight = inputInFlightRef.current;
        return () => {
            for (const [id, handle] of timers) {
                clearTimeout(handle);
                const data = pending.get(id);
                // A request still in flight owns the order: its own completion
                // handler re-arms the flush, so writing here as well could
                // overtake it. Leave that one to finish.
                if (data && !inFlight.has(id)) {
                    pending.set(id, '');
                    invoke('remote_terminal_write', { terminalId: id, data }).catch(() => { });
                }
            }
            timers.clear();
        };
    }, []);

    // Returns the same callback instance for a given terminalId across
    // renders - see the comment above `terminalRefCallbacksRef`.
    const getTerminalRefCallback = useCallback((terminalId: string) => {
        let cb = terminalRefCallbacksRef.current.get(terminalId);
        if (!cb) {
            cb = (el: HTMLDivElement | null) => attachTerminalRef(el, terminalId);
            terminalRefCallbacksRef.current.set(terminalId, cb);
        }
        return cb;
    }, [attachTerminalRef]);

    // Re-fit, re-focus AND force a remote repaint of the newly active
    // terminal whenever the active folder/terminal changes: clicking a
    // tab/folder button steals DOM focus away from xterm's hidden textarea,
    // and a container's real on-screen size can only be trusted once it's
    // actually laid out as visible again. `term.refresh()` here is a cheap
    // belt-and-suspenders repaint of whatever's already in the local buffer
    // - since inactive terminals are now hidden with `visibility: hidden`
    // rather than `display: none` (see the terminal viewport's comment
    // below), xterm.js's own internal IntersectionObserver-driven render
    // pause never engages for them in the first place, so this call is
    // normally a no-op; it's kept as a cheap safety net.
    //
    // `forceRemoteRedraw()` (NOT `sendResize()`) is the one that actually
    // matters here: switching back to an already-open tab typically reports
    // the exact same cols/rows it already had, so a plain `sendResize()`
    // would correctly no-op (that dedupe is what the ResizeObserver above
    // needs). A viewer-specific redraw supplies the fresh screen without
    // sending SIGWINCH to the shared pane. Otherwise the view could show nothing
    // but a bare cursor after switching, until some unrelated action (like
    // typing) happened to make codex redraw on its own.
    //
    // None of this is restored automatically, so all of it must be done
    // explicitly.
    useEffect(() => {
        const activeTerminalId = activeTab?.activeTerminalId;
        if (!activeTerminalId) return;
        const inst = xtermInstancesRef.current.get(activeTerminalId);
        if (!inst) return;
        requestAnimationFrame(() => {
            inst.fitAddon.fit();
            inst.term.focus();
            inst.forceRemoteRedraw();
            inst.term.refresh(0, inst.term.rows - 1);
        });
    }, [activeTabId, activeTab?.activeTerminalId]);

    // Merge a restored/reconnected session into the store. Only ADDS folders
    // and terminals the store doesn't already know about (reading fresh
    // state via getState(), not the closed-over snapshot) so calling this
    // again - e.g. navigating back to this page mid-session, or right after
    // a fresh login - never duplicates entries already open in this process.
    const hydrateSession = useCallback((session: RestoredSessionPayload) => {
        const current = useRemoteTerminalStore.getState();
        setReady(session.ready);
        setAuthRequired(false);
        setLoginStatus('logged_in');
        session.folders.forEach((f) => {
            if (!current.folders[f.tab_id]) {
                addFolder({ tabId: f.tab_id, localDir: f.local_dir, slug: f.slug });
                updateFolderSync(f.tab_id, { filesSynced: f.files_synced });
            }
        });
        session.terminals.forEach((term) => {
            if (!current.terminals[term.terminal_id]) {
                addTerminal(term.terminal_id, term.tab_id, term.tool);
            }
        });

        // Which "+ <tool>" buttons to offer. Asked here rather than once at
        // mount because it describes the server we just connected to.
        //
        // Falls back to codex on ANY failure, and treats an empty list the
        // same way. Deriving the buttons from this list means an unreachable
        // or outdated endpoint left the screen with no buttons at all and no
        // way to open a terminal - a total dead end over something cosmetic.
        // codex is the server's own default and is always installed, so
        // assuming it degrades to "you can still work" instead of "you can do
        // nothing". A tool that is genuinely disabled is still refused by
        // /term/create, so guessing here cannot let anything extra run.
        invoke<string[]>('remote_terminal_list_tools')
            .then((tools) => {
                const usable = (tools || []).filter((tool): tool is RemoteTool => tool === 'codex' || tool === 'claude');
                setAvailableTools(usable.length > 0 ? usable : ['codex']);
            })
            .catch((e) => {
                console.error('[RemoteTerminal] Failed to list remote tools, falling back to codex:', e);
                setAvailableTools(['codex']);
            });
    }, [setReady, setLoginStatus, addFolder, addTerminal, setAvailableTools, updateFolderSync]);

    // Restore the whole screen once on mount: after a full app relaunch this
    // logs back in (if the persisted key is still valid) and reconnects every
    // folder tab and terminal; within an already-running session it just
    // reconciles any terminal that silently dropped while this page was
    // unmounted (no listener around to catch its `closed` event). A no-op
    // (returns null) when there's nothing persisted - the login screen shows
    // as usual in that case.
    useEffect(() => {
        invoke<RestoredSessionPayload | null>('remote_terminal_restore_session')
            .then((session) => {
                if (session) hydrateSession(session);
            })
            .catch((e) => console.error('[RemoteTerminal] Failed to restore session:', e));
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    // Subscribe once to backend events for the whole lifecycle of this page,
    // routing each payload to the matching folder tab / terminal instance by id.
    useEffect(() => {
        const unlistenPromises = [
            listen<string>('remote-terminal://login-url', (event) => {
                setLoginUrl(event.payload);
            }),
            listen<RemoteTerminalReady>('remote-terminal://login-success', () => {
                // Re-fetch (rather than trust the event payload alone) so any
                // folder tabs/terminals already persisted from before this
                // login get reconnected too, not just the bare login state.
                invoke<RestoredSessionPayload | null>('remote_terminal_restore_session')
                    .then((session) => {
                        if (session) hydrateSession(session);
                    })
                    .catch((e) => console.error('[RemoteTerminal] Failed to restore session after login:', e));
            }),
            listen<string>('remote-terminal://login-error', (event) => {
                setError(event.payload);
                setLoginStatus('error');
                showToast(t('remote_terminal.toast.login_failed'), 'error');
            }),
            listen<SyncOutputPayload>('remote-terminal://sync-output', (event) => {
                updateFolderSync(event.payload.tab_id, { syncOutputLine: event.payload.line });
            }),
            listen<TerminalOutputPayload>('remote-terminal://output', (event) => {
                record('recv', `"${escapeForLog(event.payload.data)}"`);
                const inst = xtermInstancesRef.current.get(event.payload.terminal_id);
                if (inst) {
                    inst.term.write(event.payload.data);
                } else {
                    bufferPendingOutput(event.payload.terminal_id, event.payload.data);
                }
            }),
            listen<TerminalClosedPayload>('remote-terminal://closed', (event) => {
                updateTerminalStatus(event.payload.terminal_id, 'closed');
                xtermInstancesRef.current
                    .get(event.payload.terminal_id)
                    ?.term.writeln(`\r\n\x1b[33m${t('remote_terminal.session_ended')}\x1b[0m\r\n`);
            }),
        ];

        return () => {
            Promise.all(unlistenPromises).then((unlisteners) => {
                unlisteners.forEach((unlisten) => unlisten());
            });
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    const handleLogin = async () => {
        setError(null);
        setReady(null);
        setLoginStatus('waiting_sso');
        try {
            await invoke('remote_terminal_start_login');
        } catch (e) {
            setError(String(e));
            setLoginStatus('error');
            showToast(t('remote_terminal.toast.login_failed'), 'error');
        }
    };

    const handleCancelLogin = async () => {
        try {
            await invoke('remote_terminal_cancel_login');
        } catch {
            // best-effort
        }
        resetLogin();
    };

    const handleAddFolder = async () => {
        if (!isTauri()) {
            showToast(t('common.tauri_api_not_loaded'), 'error');
            return;
        }
        try {
            const selected = await openDialog({ directory: true, multiple: false, title: t('remote_terminal.empty_folder_hint') });
            if (typeof selected !== 'string' || !selected) return;
            setLastLocalDir(selected);
            const result = await invoke<AddFolderResult>('remote_terminal_add_folder', { localDir: selected });
            addFolder({ tabId: result.tab_id, localDir: selected, slug: result.slug });
        } catch (e) {
            showToast(String(e).includes('local_dir_not_empty_new_project_required')
                ? t('remote_terminal.empty_folder_required') : String(e), 'error');
        }
    };

    const handleCloseFolder = async (tabId: string) => {
        const tab = folders[tabId];
        const hasRunningTerminals = (tab?.terminalIds.length ?? 0) > 0;
        if (hasRunningTerminals && !confirm(t('remote_terminal.confirm_close_folder'))) {
            return;
        }
        try {
            await invoke('remote_terminal_close_folder', { tabId });
        } catch {
            // best-effort
        }
        removeFolder(tabId);
    };

    const checkInFlightRef = useRef(new Map<string, Promise<WorkspaceCheckPayload | null>>());
    const checkWorkspace = useCallback((tabId: string): Promise<WorkspaceCheckPayload | null> => {
        const existing = checkInFlightRef.current.get(tabId);
        if (existing) return existing;
        const current = useRemoteTerminalStore.getState().folders[tabId];
        if (!current || current.syncStatus === 'syncing') return Promise.resolve(null);
        updateFolderSync(tabId, { workspaceStatus: 'checking', workspaceError: null });
        const promise = invoke<WorkspaceCheckPayload>('remote_terminal_check_workspace', { tabId })
            .then((result) => {
                if (useRemoteTerminalStore.getState().folders[tabId]?.syncStatus !== 'syncing') {
                    updateFolderSync(tabId, { workspaceStatus: result.status, workspaceError: null });
                }
                return result;
            })
            .catch((e) => {
                const message = describeRemoteError(e);
                if (useRemoteTerminalStore.getState().folders[tabId]?.syncStatus !== 'syncing') {
                    updateFolderSync(tabId, { workspaceStatus: 'error', workspaceError: message });
                }
                return null;
            })
            .finally(() => checkInFlightRef.current.delete(tabId));
        checkInFlightRef.current.set(tabId, promise);
        return promise;
    }, [updateFolderSync, describeRemoteError]);

    const folderIdsKey = folderOrder.join(',');
    useEffect(() => {
        if (loginStatus !== 'logged_in' || authRequired) return;
        const checkAll = async () => {
            if (document.visibilityState === 'hidden') return;
            for (const id of useRemoteTerminalStore.getState().folderOrder) await checkWorkspace(id);
        };
        void checkAll();
        const interval = setInterval(() => void checkAll(), 60000);
        const onVisible = () => { if (document.visibilityState === 'visible') void checkAll(); };
        window.addEventListener('focus', onVisible);
        document.addEventListener('visibilitychange', onVisible);
        return () => {
            clearInterval(interval);
            window.removeEventListener('focus', onVisible);
            document.removeEventListener('visibilitychange', onVisible);
        };
    }, [loginStatus, authRequired, folderIdsKey, checkWorkspace]);

    const handleSyncFolder = useCallback(async (tabId: string, download = false): Promise<boolean> => {
        await checkInFlightRef.current.get(tabId);
        if (useRemoteTerminalStore.getState().folders[tabId]?.syncStatus === 'syncing') return false;
        updateFolderSync(tabId, { syncStatus: 'syncing', syncMessage: null, syncOutputLine: null });
        let success = false;
        try {
            await invoke(download ? 'remote_terminal_download_folder' : 'remote_terminal_sync_folder', { tabId });
            updateFolderSync(tabId, { syncStatus: 'synced', syncMessage: null, lastSyncedAt: Date.now(), filesSynced: true });
            success = true;
        } catch (e) {
            const message = describeRemoteError(e);
            updateFolderSync(tabId, { syncStatus: 'error', syncMessage: message });
            showToast(message, 'error');
        }
        await checkWorkspace(tabId);
        return success;
    }, [describeRemoteError, updateFolderSync, checkWorkspace]);

    // Measure the real terminal size BEFORE the PTY is opened, using a
    // throwaway xterm.js instance (same font metrics as the real terminals)
    // mounted into the always-present viewport container. Opening the PTY at
    // this size from the start - rather than the previous hardcoded 80x24 -
    // avoids a race where the remote TUI (tmux + codex/claude) paints its
    // first frame at the wrong size and then gets resized out from under
    // itself a moment later, which is what produced garbled/duplicated text
    // right after opening a terminal.
    const measureViewport = useCallback((): { cols: number; rows: number } => {
        const container = viewportRef.current;
        if (!container) return { cols: 80, rows: 24 };

        const probe = document.createElement('div');
        probe.style.position = 'absolute';
        probe.style.inset = '0';
        probe.style.padding = '0.5rem'; // matches the real terminal divs' `p-2`
        probe.style.visibility = 'hidden';
        container.appendChild(probe);

        const probeTerm = new Terminal({
            fontSize: 13,
            fontFamily: 'Menlo, Consolas, "Courier New", monospace',
        });
        const probeFit = new FitAddon();
        probeTerm.loadAddon(probeFit);
        probeTerm.open(probe);
        probeFit.fit();
        const { cols, rows } = probeTerm;

        probeTerm.dispose();
        container.removeChild(probe);

        return { cols, rows };
    }, []);

    const handleOpenTerminal = async (tabId: string, tool: RemoteTool) => {
        const folder = useRemoteTerminalStore.getState().folders[tabId];
        if (!folder || folder.syncStatus === 'syncing' || openingTerminalRef.current) return;
        openingTerminalRef.current = true;
        setIsOpeningTerminal(true);
        try {
            await checkInFlightRef.current.get(tabId);
            const { cols, rows } = measureViewport();
            const terminalId = await invoke<string>('remote_terminal_open_terminal', { tabId, tool, cols, rows });
            updateFolderSync(tabId, {
                filesSynced: true,
                syncStatus: 'synced',
                syncMessage: null,
                workspaceStatus: 'synced',
                workspaceError: null,
                ...(!folder.filesSynced ? { lastSyncedAt: Date.now() } : {}),
            });
            addTerminal(terminalId, tabId, tool);
        } catch (e) {
            const message = describeRemoteError(e);
            showToast(message.includes('workspace_files_not_synced_user_sync_unavailable')
                ? t('remote_terminal.sync.files_required')
                : message.includes('workspace_not_synced:')
                    ? t(`remote_terminal.workspace.${message.split('workspace_not_synced:')[1].trim()}`)
                    : message, 'error');
            await checkWorkspace(tabId);
        } finally {
            openingTerminalRef.current = false;
            setIsOpeningTerminal(false);
        }
    };

    const handleCloseTerminal = async (terminalId: string) => {
        try {
            await invoke('remote_terminal_close_terminal', { terminalId });
        } catch {
            // best-effort
        }
        removeTerminal(terminalId);
    };

    // Shares every terminal currently open under the folder tab that holds
    // `terminalId` as a single link (the page switches between them). The
    // backend writes it straight to the system clipboard (never auto-opened
    // here - the whole point is to hand it off to someone else) - see the doc
    // comment on the Rust `get_share_link` for why the copy happens there
    // instead of via `navigator.clipboard` here. The returned URL powers the QR popup.
    const handleShareTerminal = async (terminalId: string) => {
        if (isSharing) return;
        setIsSharing(true);
        setShareUrl(null);
        try {
            const state = useRemoteTerminalStore.getState();
            const tabId = state.terminals[terminalId]?.tabId;
            const labels: Record<string, string> = {};
            (tabId ? state.folders[tabId]?.terminalIds ?? [] : []).forEach((id, i) => {
                labels[id] = `${state.terminals[id]?.tool ?? 'terminal'} ${i + 1}`;
            });
            const url = await invoke<string>('remote_terminal_get_share_link', { terminalId, labels });
            setShareUrl(url);
            showToast(t('common.copied'), 'success');
        } catch (e) {
            showToast(describeRemoteError(e), 'error');
        } finally {
            setIsSharing(false);
        }
    };

    const syncStatusText = (tab: typeof activeTab) => {
        if (!tab) return '';
        if (tab.syncStatus === 'syncing') {
            // `syncOutputLine` is just rsync's total-transfer percentage now
            // ("45%"), not a file path, so it appends to a short fixed-width
            // status instead of growing the bar. Absent until the first
            // percentage arrives, and for a transfer small enough that rsync
            // never reports one - "syncing" alone is the honest answer then.
            return tab.syncOutputLine
                ? `${t('remote_terminal.sync.syncing')} ${tab.syncOutputLine}`
                : t('remote_terminal.sync.syncing');
        }
        if (tab.syncStatus === 'error') {
            return `${t('remote_terminal.sync.failed')}${tab.syncMessage ? `: ${tab.syncMessage}` : ''}`;
        }
        if (tab.syncStatus === 'synced' && tab.lastSyncedAt) {
            return t('remote_terminal.sync.synced_ago', { time: formatElapsed(tab.lastSyncedAt) });
        }
        if (tab.filesSynced) return t('remote_terminal.sync.synced');
        return t('remote_terminal.sync.idle');
    };

    const allTerminalIds = Object.keys(terminals);

    // One independent "+ <Tool>" button per CLI the SERVER says it offers,
    // instead of the old segmented picker.
    //
    // That picker sat right next to the terminal tabs and looked like it
    // switched between them, but all it did was change which tool the NEXT
    // "+ New terminal" would use - pressing it appeared to do nothing to the
    // terminal you were looking at, which read as the view breaking. A button
    // that opens a codex terminal should say "+ Codex".
    //
    // `availableTools` comes from the server (claude is off there by default),
    // so a tool that isn't actually installed/enabled never gets a button. The
    // server checks the same list again when creating - this only decides what
    // to show.
    const openToolButtons = (variant: 'compact' | 'prominent') => availableTools.map((tool) => (
        <button
            key={tool}
            type="button"
            onClick={() => handleOpenTerminal(activeTabId!, tool)}
            disabled={!activeTab || activeTab.syncStatus === 'syncing' || isOpeningTerminal || authRequired}
            title={!activeTab?.filesSynced ? t('remote_terminal.sync.new_project') : t(`remote_terminal.workspace.${activeTab.workspaceStatus}`)}
            className={'disabled:opacity-40 disabled:cursor-not-allowed ' + (variant === 'prominent'
                ? 'flex items-center gap-2 px-4 py-2 rounded-full text-xs font-semibold bg-gray-900 text-white hover:bg-gray-800 dark:bg-white dark:text-gray-900 dark:hover:bg-gray-200 transition-colors'
                : 'flex items-center gap-1.5 px-3 py-1 rounded-full text-xs font-medium bg-gray-50 text-gray-600 hover:bg-gray-100 dark:bg-base-200/60 dark:text-gray-400 dark:hover:bg-base-200 transition-colors shrink-0')}
        >
            {isOpeningTerminal ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : <Plus className="w-3.5 h-3.5" />}
            {t(`remote_terminal.tool.${tool}`)}
        </button>
    ));

    return (
        <div className="h-full w-full flex flex-col p-5 gap-4">
            {loginStatus !== 'logged_in' && (
                <div className="flex-1 min-h-0 flex flex-col items-center justify-center gap-4 rounded-2xl border border-gray-200 dark:border-base-200 bg-white dark:bg-base-100 px-6 text-center">
                    {(loginStatus === 'idle' || loginStatus === 'error') && (
                        <>
                            <button
                                onClick={handleLogin}
                                className="flex items-center gap-2 px-6 py-3 rounded-full text-sm font-semibold bg-gray-900 text-white hover:bg-gray-800 dark:bg-white dark:text-gray-900 dark:hover:bg-gray-200 transition-colors shadow-sm"
                            >
                                <LogIn className="w-4 h-4" />
                                {t('remote_terminal.login_button')}
                            </button>
                            <p className="text-xs text-gray-500 dark:text-gray-400 max-w-sm">
                                {t('remote_terminal.open_browser_hint')}
                            </p>
                            {loginStatus === 'error' && errorMessage && (
                                <p className="flex items-center gap-1.5 text-xs text-red-500 max-w-sm">
                                    <XCircle className="w-3.5 h-3.5 shrink-0" />
                                    {errorMessage}
                                </p>
                            )}
                        </>
                    )}

                    {loginStatus === 'waiting_sso' && (
                        <>
                            <Loader2 className="w-6 h-6 animate-spin text-gray-500" />
                            <p className="text-sm text-gray-600 dark:text-gray-300">
                                {t('remote_terminal.status.waiting_sso')}
                            </p>
                            <button
                                onClick={handleCancelLogin}
                                className="text-xs text-gray-400 hover:text-gray-600 dark:hover:text-gray-200 underline"
                            >
                                {t('common.cancel')}
                            </button>
                        </>
                    )}
                </div>
            )}

            {loginStatus === 'logged_in' && (
                <>
                    {/* Outer folder tab strip */}
                    <div className="shrink-0 flex items-center justify-between gap-2">
                        <div className="flex items-center gap-2 overflow-x-auto min-w-0">
                            {folderOrder.map((tabId) => {
                                const tab = folders[tabId];
                                if (!tab) return null;
                                const isActive = tabId === activeTabId;
                                return (
                                    <button
                                        key={tabId}
                                        onClick={() => setActiveTab(tabId)}
                                        title={tab.localDir}
                                        className={`flex items-center gap-2 pl-3 pr-2 py-1.5 rounded-full text-xs font-medium whitespace-nowrap transition-colors ${isActive
                                            ? 'bg-gray-900 text-white dark:bg-white dark:text-gray-900'
                                            : 'bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-base-200 dark:text-gray-300 dark:hover:bg-base-300'
                                            }`}
                                    >
                                        <FolderOpen className="w-3.5 h-3.5" />
                                        {basename(tab.localDir) || tab.localDir}
                                        <span
                                            role="button"
                                            title={t('remote_terminal.close_folder_tooltip')}
                                            onClick={(e) => {
                                                e.stopPropagation();
                                                handleCloseFolder(tabId);
                                            }}
                                            className="rounded-full p-0.5 hover:bg-black/10 dark:hover:bg-white/10"
                                        >
                                            <X className="w-3 h-3" />
                                        </span>
                                    </button>
                                );
                            })}
                            <button
                                onClick={handleAddFolder}
                                title={t('remote_terminal.empty_folder_hint')}
                                className="flex items-center gap-1.5 px-3 py-1.5 rounded-full text-xs font-medium bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-base-200 dark:text-gray-300 dark:hover:bg-base-300 transition-colors shrink-0"
                            >
                                <Plus className="w-3.5 h-3.5" />
                                {t('remote_terminal.add_folder_button')}
                            </button>
                        </div>
                        {ready && (
                            <div className="text-xs text-gray-400 dark:text-gray-500 shrink-0">
                                <span className="inline-flex items-center gap-1.5">
                                    <span
                                        role="img"
                                        aria-label={t(remoteConnected ? 'remote_terminal.status.remote_connected' : 'remote_terminal.status.remote_disconnected')}
                                        title={t(remoteConnected ? 'remote_terminal.status.remote_connected' : 'remote_terminal.status.remote_disconnected')}
                                        className={`w-2 h-2 rounded-full ${remoteConnected ? 'bg-green-500' : 'bg-red-500'}`}
                                    />
                                    {t('remote_terminal.principal_label')}: {ready.principal}
                                </span>
                            </div>
                        )}
                    </div>

                    {authRequired && (
                        <div role="alert" className="flex items-center gap-3 rounded-xl bg-amber-50 dark:bg-amber-950/30 px-4 py-3">
                            <p className="flex-1 text-xs text-amber-700 dark:text-amber-300">
                                {t('remote_terminal.auth.required')}
                            </p>
                            <button onClick={handleLogin} className="flex items-center gap-1.5 rounded-full bg-amber-600 px-3 py-1.5 text-xs text-white shrink-0">
                                <LogIn className="w-3.5 h-3.5" />
                                {t('remote_terminal.auth.login_again')}
                            </button>
                        </div>
                    )}

                    {!remoteConnected && (
                        <p role="status" className="text-xs text-red-500">
                            {t('remote_terminal.status.remote_disconnected')}
                        </p>
                    )}

                    {folderOrder.length === 0 && (
                        <div className="flex-1 min-h-0 flex flex-col items-center justify-center gap-4 rounded-2xl border border-gray-200 dark:border-base-200 bg-white dark:bg-base-100 px-6 text-center">
                            <FolderOpen className="w-6 h-6 text-gray-400" />
                            <p className="text-sm text-gray-600 dark:text-gray-300 max-w-sm">
                                {t('remote_terminal.empty_state_add_folder')}
                            </p>
                            <button
                                onClick={handleAddFolder}
                                className="flex items-center gap-2 px-5 py-2.5 rounded-full text-sm font-semibold bg-gray-900 text-white hover:bg-gray-800 dark:bg-white dark:text-gray-900 dark:hover:bg-gray-200 transition-colors"
                            >
                                <Plus className="w-4 h-4" />
                                {t('remote_terminal.add_folder_button')}
                            </button>
                        </div>
                    )}

                    {activeTab && (
                        <>
                            {/* Sync bar, scoped to the active folder tab.
                                Deliberately NOT `flex-wrap`, and the status text
                                below truncates instead of shrink-0: this bar sits
                                directly above the terminal viewport, which is
                                `flex-1`, so ANY change in this bar's height
                                resizes the terminal. The status text grows to
                                "Syncing — <rsync path>" and changed on every
                                line rsync printed, so it wrapped to a second
                                row and back hundreds of times per sync - each
                                one a real SIGWINCH to the remote tmux. codex
                                could not repaint fast enough and the screen
                                ended up blank. Nothing about a status message
                                is allowed to move the terminal. */}
                            <div className="shrink-0 flex items-center gap-3 min-h-16 rounded-xl border border-gray-200 dark:border-base-200 px-4 py-2" role="status" aria-live="polite">
                                <span className={`text-xs flex-1 ${['download_required', 'commit_required', 'conflict', 'error'].includes(activeTab.workspaceStatus) ? 'text-amber-600 dark:text-amber-400' : 'text-gray-500'}`} title={activeTab.workspaceError || undefined}>
                                    {activeTab.workspaceError || (
                                        !activeTab.filesSynced && ['unknown', 'initialization_required', 'upload_required'].includes(activeTab.workspaceStatus) ? t('remote_terminal.sync.new_project')
                                            : t(`remote_terminal.workspace.${activeTab.workspaceStatus}`)
                                    )}
                                </span>
                                {activeTab.workspaceStatus === 'download_required' && (
                                    <button onClick={() => handleSyncFolder(activeTab.tabId, true)} disabled={activeTab.syncStatus === 'syncing'} className="text-xs rounded-full bg-blue-600 text-white px-3 py-1.5 disabled:opacity-40">
                                        {t('remote_terminal.workspace.download_button')}
                                    </button>
                                )}
                                <button onClick={() => checkWorkspace(activeTab.tabId)} disabled={activeTab.workspaceStatus === 'checking' || activeTab.syncStatus === 'syncing'} className="text-xs rounded-full bg-gray-100 dark:bg-base-200 px-3 py-1.5 disabled:opacity-40">
                                    {t('remote_terminal.workspace.check_button')}
                                </button>
                            </div>
                            <div className="shrink-0 flex items-center gap-3 bg-white dark:bg-base-100 border border-gray-200 dark:border-base-200 rounded-xl px-4 py-3">
                                <div className="flex items-center gap-2 flex-1 min-w-0">
                                    <FolderOpen className="w-4 h-4 text-gray-400 shrink-0" />
                                    <span
                                        className="text-sm text-gray-800 dark:text-gray-200 truncate"
                                        title={activeTab.localDir}
                                    >
                                        {activeTab.localDir}
                                    </span>
                                    <span className="text-xs text-gray-400 shrink-0">({activeTab.slug})</span>
                                </div>
                                <button
                                    onClick={() => handleSyncFolder(activeTab.tabId)}
                                    disabled={authRequired || activeTab.syncStatus === 'syncing' || !['synced', 'initialization_required', 'confirmation_required', 'upload_required'].includes(activeTab.workspaceStatus)}
                                    className="flex items-center gap-1.5 px-3 py-1.5 rounded-full text-xs font-medium bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-base-200 dark:text-gray-300 dark:hover:bg-base-300 transition-colors disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
                                >
                                    <RefreshCw className={`w-3.5 h-3.5 ${activeTab.syncStatus === 'syncing' ? 'animate-spin' : ''}`} />
                                    {t('remote_terminal.sync_now_button')}
                                </button>
                                <button
                                    onClick={() => handleShareTerminal(activeTab.activeTerminalId!)}
                                    disabled={!activeTab.activeTerminalId || isSharing}
                                    title={t('remote_terminal.share_terminal_tooltip')}
                                    className="flex items-center gap-1.5 px-3 py-1.5 rounded-full text-xs font-medium bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-base-200 dark:text-gray-300 dark:hover:bg-base-300 transition-colors disabled:opacity-40 disabled:cursor-not-allowed shrink-0"
                                >
                                    {isSharing
                                        ? <Loader2 className="w-3.5 h-3.5 animate-spin" />
                                        : <Share2 className="w-3.5 h-3.5" />}
                                    {t('remote_terminal.share_terminal_button')}
                                </button>
                                <span
                                    title={syncStatusText(activeTab)}
                                    className={`text-xs min-w-0 max-w-[40%] truncate ${activeTab.syncStatus === 'error' ? 'text-red-500' : 'text-gray-400 dark:text-gray-500'
                                        }`}
                                >
                                    {syncStatusText(activeTab)}
                                </span>
                            </div>

                            {/* Inner terminal tab strip, scoped to the active folder tab */}
                            <div className="shrink-0 flex items-center gap-2 overflow-x-auto">
                                {activeTab.terminalIds.map((terminalId, index) => {
                                    const term = terminals[terminalId];
                                    const isActive = terminalId === activeTab.activeTerminalId;
                                    return (
                                        <button
                                            key={terminalId}
                                            onClick={() => setActiveTerminal(activeTab.tabId, terminalId)}
                                            className={`flex items-center gap-2 pl-3 pr-2 py-1 rounded-full text-xs font-medium whitespace-nowrap transition-colors ${isActive
                                                ? 'bg-gray-200 text-gray-900 dark:bg-base-300 dark:text-white'
                                                : 'bg-gray-50 text-gray-600 hover:bg-gray-100 dark:bg-base-200/60 dark:text-gray-400 dark:hover:bg-base-200'
                                                }`}
                                        >
                                            <TerminalIcon className="w-3 h-3" />
                                            {t('remote_terminal.terminal_tab_label', { index: index + 1 })}
                                            {term && (
                                                <span className="text-[10px] px-1.5 py-0.5 rounded-full bg-black/10 dark:bg-white/10 text-gray-600 dark:text-gray-300">
                                                    {t(`remote_terminal.tool.${term.tool}`)}
                                                </span>
                                            )}
                                            {term?.status === 'closed' && (
                                                <span className="text-[10px] text-amber-500">
                                                    {t('remote_terminal.terminal_closed_badge')}
                                                </span>
                                            )}
                                            <span
                                                role="button"
                                                title={t('remote_terminal.close_terminal_tooltip')}
                                                onClick={(e) => {
                                                    e.stopPropagation();
                                                    handleCloseTerminal(terminalId);
                                                }}
                                                className="rounded-full p-0.5 hover:bg-black/10 dark:hover:bg-white/10"
                                            >
                                                <X className="w-3 h-3" />
                                            </span>
                                        </button>
                                    );
                                })}
                                {openToolButtons('compact')}
                            </div>

                            {/* Terminal viewport: every terminal across every tab stays mounted
                                so switching tabs never loses scrollback or requires a fresh PTY.
                                Inactive ones are hidden with `visibility: hidden`, NOT
                                `display: none` - xterm.js (v6) runs its own IntersectionObserver
                                on each terminal's element and PAUSES all rendering the moment it
                                stops intersecting the viewport, which `display: none` triggers
                                (it removes the box entirely). Once paused, writes/refreshes are
                                silently deferred; xterm only auto-catches-up on unpause if it
                                recorded a pending refresh, which never happens for a terminal
                                that's simply sitting idle (not actively producing output) at the
                                moment you switch away - so switching back showed nothing but a
                                blinking cursor until the next keystroke's echo forced a repaint.
                                `visibility: hidden` still occupies its layout box (so it keeps
                                geometrically intersecting the viewport - xterm's renderer is
                                never paused) while remaining invisible and un-hit-testable, so it
                                gets the "no remount" benefit without the pause bug. */}
                            <div ref={viewportRef} className="flex-1 min-h-0 relative rounded-2xl overflow-hidden border border-gray-200 dark:border-base-200 bg-[#0d1117] shadow-sm">
                                {allTerminalIds.map((terminalId) => {
                                    const term = terminals[terminalId];
                                    const isVisible = !!term
                                        && term.tabId === activeTabId
                                        && terminalId === activeTab.activeTerminalId;
                                    return (
                                        <div
                                            key={terminalId}
                                            ref={getTerminalRefCallback(terminalId)}
                                            onMouseDown={() => focusTerminal(terminalId)}
                                            className="absolute inset-0 p-2"
                                            style={{
                                                visibility: isVisible ? 'visible' : 'hidden',
                                                zIndex: isVisible ? 1 : 0,
                                            }}
                                        />
                                    );
                                })}

                                {activeTab.terminalIds.length === 0 && (
                                    <div className="absolute inset-0 flex flex-col items-center justify-center gap-3 bg-white/95 dark:bg-base-100/95 backdrop-blur-sm px-6 text-center">
                                        <TerminalIcon className="w-6 h-6 text-gray-400" />
                                        <p className="text-sm text-gray-600 dark:text-gray-300 max-w-sm">
                                            {t('remote_terminal.no_terminals_yet')}
                                        </p>
                                        <div className="flex items-center gap-2">
                                            {openToolButtons('prominent')}
                                        </div>
                                    </div>
                                )}
                            </div>
                        </>
                    )}
                </>
            )}
            <Modal
                open={shareUrl !== null}
                title={t('remote_terminal.share_qr_title')}
                onCancel={() => setShareUrl(null)}
                footer={null}
                centered
                width={380}
                destroyOnHidden
            >
                {shareUrl && (
                    <div className="flex flex-col items-center gap-4 pt-2">
                        <p className="text-center text-sm text-gray-500">
                            {t('remote_terminal.share_qr_description')}
                        </p>
                        <div className="rounded-xl bg-white p-4">
                            <QRCode value={shareUrl} size={240} type="svg" color="#000000" bgColor="#ffffff" bordered={false} />
                        </div>
                        <input
                            readOnly
                            value={shareUrl}
                            aria-label={t('remote_terminal.share_link_label')}
                            onFocus={event => event.currentTarget.select()}
                            className="w-full rounded-lg border border-gray-200 bg-gray-50 px-3 py-2 text-xs text-gray-700"
                        />
                        <p className="text-center text-xs text-gray-500">
                            {t('remote_terminal.share_qr_copied')}
                        </p>
                    </div>
                )}
            </Modal>
        </div>
    );
}

export default RemoteTerminal;
