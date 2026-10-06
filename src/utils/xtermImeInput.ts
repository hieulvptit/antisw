import type { Terminal } from '@xterm/xterm';
import { debugState, escapeForLog, record } from './remoteTerminalDebug';

/**
 * Make xterm.js forward text that an input method inserts, and make what the
 * input method actually does observable.
 *
 * xterm.js only reliably turns a keystroke into terminal input through the
 * `keypress` event. A key that the OS input method swallows never produces
 * one - the IME instead edits the hidden textarea directly - and both of
 * xterm 6.0.0's fallbacks for that case lose characters:
 *
 *   * `_inputEvent` handles an `input` event only when
 *     `(!ev.composed || !this._keyDownSeen)`. An `input` event is always
 *     `composed`, and `_keyDownSeen` stays true from `keydown` until `keyup`,
 *     so any insertion the IME makes while the key that triggered it is still
 *     physically down is dropped.
 *   * `_handleAnyTextareaChanges` (the `keyCode === 229` path) diffs the whole
 *     textarea one tick later with `newValue.replace(oldValue, '')` and, when
 *     the value got SHORTER, sends a single DEL no matter how many characters
 *     were actually removed - and when it stayed the same length but changed,
 *     re-sends the entire textarea. Vietnamese Telex does exactly that: the
 *     tone mark in "Nhưngx" makes the IME delete "ưng" and insert "ững".
 *
 * So both are replaced with `beforeinput`, which states the edit the IME is
 * about to make before the textarea changes, with no diffing or guessing.
 *
 * That is a THEORY about which of several possible faults is the real one,
 * which is why it is a switch rather than a rewrite: `__rtIme(false)` in the
 * console puts xterm's own handlers back, live, and `__rtRaw(true)` prints
 * what the IME actually does in this webview. If `beforeinput` turns out not
 * to fire here at all, the trace says so outright instead of leaving the
 * terminal quietly worse than before.
 *
 * Deliberately NOT touched: `keypress`, which still handles ordinary typing,
 * and xterm's composition handling (`compositionstart`/`end`) used by IMEs
 * that mark text instead of rewriting it - composition input types are skipped
 * here so those are never sent twice.
 *
 * Returns a disposer; call it before `term.dispose()`.
 */

/** Input types that add text, whose `data` is what should reach the pty. */
const INSERT_TYPES = new Set(['insertText', 'insertReplacementText']);

/** Input types that remove text backwards from the caret. */
const DELETE_TYPES = new Set([
    'deleteContentBackward',
    'deleteWordBackward',
    'deleteSoftLineBackward',
    'deleteHardLineBackward',
]);

/**
 * Whether xterm is about to send only the FIRST character of a `keypress`
 * whose `key` holds several.
 *
 * This is the actual fault, caught in a trace of EVKey typing "Nhuwngx":
 *
 *   keydown     key="ững" code=KeyA kc=65
 *   keypress    key="ững" code=KeyA kc=7919
 *   beforeinput insertText data="ững" cancelable=true
 *   queue       "ữ"            <- what reached the pty
 *
 * The IME replaces a whole syllable at once and reports it in `key`, but a
 * `keypress` carries a single `charCode` - here 7919, which is `ữ`. xterm's
 * `_keyPress` reads that one code and does `String.fromCharCode` on it, so
 * "ng" is dropped before anything of ours is reached. That is "Những"
 * arriving as "nhữ", and no amount of care further down the pipe can put back
 * a character that was never emitted.
 *
 * The same keystroke also produces a `beforeinput` carrying the COMPLETE
 * "ững", so the repair is to let that one through instead - which means
 * stopping xterm from sending its truncated version first.
 *
 * The test is deliberately narrow: `charCode` must decode to exactly the
 * first character of `key`, i.e. xterm is about to send a genuine prefix of
 * what the user typed. A named key such as Enter also has a multi-character
 * `key`, but its `charCode` (13) is not "E", so it is left alone - as is
 * every ordinary single-character keystroke.
 */
function keypressIsTruncated(ev: KeyboardEvent): boolean {
    const key = ev.key ?? '';
    if (key.length <= 1) return false;
    const code = ev.charCode || (ev.which !== 0 ? ev.which : ev.keyCode);
    if (!code) return false;
    return String.fromCharCode(code) === key[0];
}

interface XtermCore {
    /** True once `keypress` has already turned this key into terminal input. */
    _keyPressHandled: boolean;
    /** xterm's own `input` event handler - wrapped, see above. */
    _inputEvent: (ev: InputEvent) => boolean;
    _compositionHelper?: { _handleAnyTextareaChanges?: () => void };
}

export function installImeInput(term: Terminal, send: (data: string) => void): () => void {
    const core = (term as unknown as { _core?: XtermCore })._core;
    const textarea = term.textarea;
    const element = term.element;
    if (!core || !textarea || !element) {
        record('ime.install', 'FAILED - terminal not opened yet, IME input left to xterm');
        return () => { };
    }
    record('ime.install', `ok (beforeinput supported: ${typeof InputEvent !== 'undefined'
        && 'inputType' in InputEvent.prototype})`);

    // ---- raw event trace -------------------------------------------------
    // Recorded regardless of whether our handling is on, so `__rtIme(false)`
    // can be compared against `__rtIme(true)` with the same instrumentation.
    const traceKey = (ev: KeyboardEvent) => {
        record(ev.type, `key=${JSON.stringify(ev.key)} code=${ev.code} kc=${ev.keyCode}`
            + ` composing=${ev.isComposing} prevented=${ev.defaultPrevented}`);
    };
    const traceInput = (ev: InputEvent) => {
        record(ev.type, `${ev.inputType} data=${ev.data === null ? 'null' : `"${escapeForLog(ev.data)}"`}`
            + ` composing=${ev.isComposing} cancelable=${ev.cancelable}`
            + ` sel=${textarea.selectionStart},${textarea.selectionEnd}`
            + ` ta="${escapeForLog(textarea.value)}"`);
    };
    const traceComposition = (ev: CompositionEvent) => {
        record(ev.type, `data=${JSON.stringify(ev.data)} ta="${escapeForLog(textarea.value)}"`);
    };

    for (const type of ['keydown', 'keypress', 'keyup'] as const) {
        textarea.addEventListener(type, traceKey as EventListener);
    }
    textarea.addEventListener('input', traceInput as EventListener);
    for (const type of ['compositionstart', 'compositionupdate', 'compositionend'] as const) {
        textarea.addEventListener(type, traceComposition as EventListener);
    }

    // ---- the override ----------------------------------------------------
    // Instance properties shadow the prototype methods; xterm calls both
    // through `this`, so these take effect for this terminal only - and fall
    // straight back to the originals when the switch is off.
    const originalInputEvent = core._inputEvent;
    core._inputEvent = function (this: XtermCore, ev: InputEvent) {
        if (!debugState.ime) return originalInputEvent.call(this, ev);
        return false;
    };

    const helper = core._compositionHelper;
    const originalTextareaChanges = helper?._handleAnyTextareaChanges;
    if (helper && typeof originalTextareaChanges === 'function') {
        helper._handleAnyTextareaChanges = function (this: object) {
            if (!debugState.ime) originalTextareaChanges.call(this);
        };
    }

    // `_keyPressHandled` is the one signal that says "this key was already
    // sent by `keypress`, don't send it again". xterm only clears it on
    // `keyup`, so with overlapping keystrokes it is still true when the NEXT
    // key's `beforeinput` arrives - which would make this drop exactly the
    // IME insertions it exists to rescue. Clearing it as each key goes down
    // scopes it to a single key, which is what it is meant to mean.
    //
    // Registered on the terminal's root element in the CAPTURE phase on
    // purpose: xterm's own `keydown` listener sits on the textarea, i.e. the
    // event target, where capture and bubble listeners run in registration
    // order - and xterm registered first. Capturing on an ancestor is what
    // guarantees this runs before xterm's handler for the same event.
    const onKeyDownCapture = () => {
        if (!debugState.ime) return;
        core._keyPressHandled = false;
    };

    // The one thing that stops xterm sending a truncated syllable. `_keyPress`
    // bails when the custom key handler returns false, which also leaves
    // `_keyPressHandled` false - so `onBeforeInput` below then sees the key as
    // unhandled and forwards the whole of it.
    term.attachCustomKeyEventHandler((ev) => {
        if (!debugState.ime || ev.type !== 'keypress' || !keypressIsTruncated(ev)) return true;
        record('ime.truncated', `keypress key="${escapeForLog(ev.key)}" would send only`
            + ` "${escapeForLog(String.fromCharCode(ev.charCode || ev.which || ev.keyCode))}"`
            + ' - handing it to beforeinput');
        return false;
    });

    const onBeforeInput = (ev: InputEvent) => {
        traceInput(ev);
        if (!debugState.ime) {
            record('ime.skip', 'our handling is switched off (__rtIme(false))');
            return;
        }
        // Already sent by `keypress` - ordinary typing.
        if (core._keyPressHandled) {
            record('ime.skip', 'keypress already sent this key');
            return;
        }

        const isInsert = INSERT_TYPES.has(ev.inputType);
        if (!isInsert && !DELETE_TYPES.has(ev.inputType)) {
            // Composition, paste, cut, undo, formatting: xterm's own handlers
            // (`compositionend`, the `paste` listener) still own these.
            record('ime.skip', `inputType ${ev.inputType} left to xterm`);
            return;
        }

        // `getTargetRanges()` is empty for a <textarea> by spec, so the extent
        // of the edit has to come from the selection, which at `beforeinput`
        // time still describes what the edit is about to replace. A collapsed
        // selection on a delete means the single character before the caret.
        const start = textarea.selectionStart ?? 0;
        const end = textarea.selectionEnd ?? start;
        let removed = Math.max(0, end - start);
        if (!isInsert && removed === 0) removed = 1;

        const payload = '\x7f'.repeat(removed) + (isInsert ? ev.data ?? '' : '');
        record('ime.take', `"${escapeForLog(payload)}" (${removed} deleted)`);
        if (payload) send(payload);
    };

    element.addEventListener('keydown', onKeyDownCapture, true);
    textarea.addEventListener('beforeinput', onBeforeInput as EventListener);

    return () => {
        element.removeEventListener('keydown', onKeyDownCapture, true);
        textarea.removeEventListener('beforeinput', onBeforeInput as EventListener);
        textarea.removeEventListener('input', traceInput as EventListener);
        for (const type of ['keydown', 'keypress', 'keyup'] as const) {
            textarea.removeEventListener(type, traceKey as EventListener);
        }
        for (const type of ['compositionstart', 'compositionupdate', 'compositionend'] as const) {
            textarea.removeEventListener(type, traceComposition as EventListener);
        }
        core._inputEvent = originalInputEvent;
        if (helper && typeof originalTextareaChanges === 'function') {
            helper._handleAnyTextareaChanges = originalTextareaChanges;
        }
    };
}
