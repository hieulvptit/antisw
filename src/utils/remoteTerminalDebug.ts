/**
 * Diagnostics for Remote Terminal input, driven from the devtools console.
 *
 * Input that arrives wrong has too many suspects to reason about from a
 * screenshot: the IME may never reach xterm, xterm may drop what it does
 * reach, or the transport may mangle it. These make each layer observable
 * and, more importantly, switchable WITHOUT a rebuild.
 *
 * Everything is recorded into a ring buffer ALL THE TIME, and `__rtDump()`
 * prints it. That ordering matters: a trace you have to switch on before the
 * bug happens is a trace you find out you needed one keystroke too late. Type
 * the word that breaks first, ask questions after.
 *
 * Type `__rtHelp()` for the list.
 */
import { invoke } from '@tauri-apps/api/core';
export interface RemoteTerminalDebugState {
    /** Take IME text from `beforeinput` instead of xterm's own handling. */
    ime: boolean;
    /** Also echo every recorded entry to the console as it happens. */
    live: boolean;
    /**
     * Mirror the trace to `~/.antisw/remote_terminal/input-trace.log`, so the
     * evidence can be read off disk instead of copied out of the console by
     * hand - the console route needs the trace switched on BEFORE the bug,
     * which is always noticed one keystroke too late.
     *
     * This records keystrokes, so it defaults to a DEV build only, stays on
     * this machine, and `__rtFile(false)` stops it immediately.
     */
    file: boolean;
    /**
     * How long to hold typed text before sending, in ms. 0 sends as soon as
     * the previous request is done, which is the behaviour that does not
     * stutter. A non-zero hold batches an IME's delete-and-retype into one
     * request, at the cost of the echo arriving in visible bursts.
     */
    holdMs: number;
}

export const debugState: RemoteTerminalDebugState = {
    ime: true,
    live: false,
    file: import.meta.env.DEV,
    holdMs: 0,
};

/** Render a string as printable text plus escapes, for a readable log line. */
export function escapeForLog(data: string): string {
    return [...data]
        .map((c) => {
            const code = c.codePointAt(0)!;
            if (code < 0x20 || code === 0x7f) return `\\x${code.toString(16).padStart(2, '0')}`;
            return c;
        })
        .join('');
}

interface TraceEntry {
    at: number;
    kind: string;
    detail: string;
}

/**
 * Bounded so an app left open all day cannot grow this without limit. 600
 * entries is far more than one word of typing produces, which is all anyone
 * needs to look at.
 */
const RING_MAX = 600;
const ring: TraceEntry[] = [];

/**
 * Lines waiting to go to disk. Batched on a timer rather than written per
 * event: one IPC round trip per keystroke would itself perturb the timing this
 * is meant to measure.
 */
const fileQueue: string[] = [];
let fileTimer: number | undefined;

function flushToFile(): void {
    fileTimer = undefined;
    if (fileQueue.length === 0) return;
    const lines = fileQueue.splice(0, fileQueue.length);
    invoke('remote_terminal_trace', { lines }).catch(() => {
        // Never let a failed debug write disturb the thing being debugged.
        debugState.file = false;
    });
}

function stamp(at: number): string {
    const d = new Date(at);
    return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
        + `:${String(d.getSeconds()).padStart(2, '0')}.${String(d.getMilliseconds()).padStart(3, '0')}`;
}

/** Record one event. Cheap enough to leave on permanently. */
export function record(kind: string, detail: string): void {
    const at = Date.now();
    ring.push({ at, kind, detail });
    if (ring.length > RING_MAX) ring.shift();
    if (debugState.live) console.log(`[rt] ${kind.padEnd(14)} ${detail}`);
    if (debugState.file) {
        fileQueue.push(`${stamp(at)}  ${kind.padEnd(14)} ${detail}`);
        if (fileTimer === undefined) fileTimer = window.setTimeout(flushToFile, 400);
    }
}

function formatRing(sinceMs: number): string {
    const cutoff = Date.now() - sinceMs;
    const rows = ring.filter((e) => e.at >= cutoff);
    if (rows.length === 0) return '(nothing recorded in that window)';
    const first = rows[0].at;
    return rows
        .map((e) => `+${String(e.at - first).padStart(5)}ms  ${e.kind.padEnd(14)} ${e.detail}`)
        .join('\n');
}

let installed = false;

/** Publishes the switches on `window`. Safe to call more than once. */
export function installDebugSwitches(): void {
    if (installed) return;
    installed = true;
    const w = window as unknown as Record<string, unknown>;

    // A marker so separate runs are told apart in a file that outlives them.
    record('---- session', `${new Date().toISOString()} ime=${debugState.ime} hold=${debugState.holdMs}ms`);

    w.__rtDump = (seconds = 30) => {
        console.log(formatRing(Math.max(1, Number(seconds) || 30) * 1000));
        return `--- ${ring.length} entries held; printed the last ${seconds}s above ---`;
    };
    w.__rtClear = () => {
        ring.length = 0;
        return 'trace buffer cleared - now type the word that breaks, then __rtDump()';
    };
    w.__rtFile = (on = true) => {
        debugState.file = !!on;
        return debugState.file
            ? 'ALSO WRITING KEYSTROKES to ~/.antisw/remote_terminal/input-trace.log'
            : 'file trace OFF';
    };
    w.__rtLive = (on = true) => {
        debugState.live = !!on;
        return on ? 'live echo ON' : 'live echo OFF (still recording)';
    };
    w.__rtIme = (on = true) => {
        debugState.ime = !!on;
        return `IME input taken from beforeinput: ${debugState.ime ? 'ON (our handling)' : 'OFF (xterm default)'}`;
    };
    w.__rtHold = (ms = 0) => {
        debugState.holdMs = Math.max(0, Number(ms) || 0);
        return `input hold: ${debugState.holdMs}ms${debugState.holdMs === 0 ? ' (send immediately)' : ''}`;
    };
    w.__rtHelp = () => [
        'Everything is ALWAYS recorded. The usual run is:',
        '  __rtClear()     empty the buffer',
        '  ...type the word that breaks...',
        '  __rtDump()      print what happened, copy it out',
        '',
        'In a dev build the same trace is ALSO written to',
        '  ~/.antisw/remote_terminal/input-trace.log   (keystrokes; __rtFile(false) stops it)',
        '',
        '__rtIme(false)  turn OFF our beforeinput handling, back to xterm default',
        '__rtHold(ms)    hold typed text before sending; 0 = immediately (default)',
        '__rtLive(true)  also echo each entry as it happens',
        '__rt()          what each xterm instance currently holds',
    ].join('\n');
}
