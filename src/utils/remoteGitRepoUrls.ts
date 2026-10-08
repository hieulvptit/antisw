import type { RemoteTerminalReady } from '../stores/useRemoteTerminalStore';

function key(ready: RemoteTerminalReady, slug: string) {
    return `antisw_remote_git_repo:${JSON.stringify([ready.principal, ready.host, ready.ssh_user, slug])}`;
}
function safeUrl(value: string) {
    try {
        const url = new URL(value);
        return url.protocol === 'https:' && !url.username && !url.password && !url.port && !url.search && !url.hash
            && !/[\s\\\x00-\x1f\x7f]/.test(value) && value.length <= 2048
            && /^\/[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+\/?$/.test(url.pathname);
    } catch { return false; }
}
export function readGitRepoUrl(ready: RemoteTerminalReady | null, slug: string) {
    if (!ready || !slug) return '';
    try {
        const value = localStorage.getItem(key(ready, slug)) || '';
        return safeUrl(value) ? value : '';
    } catch { return ''; }
}
export function saveGitRepoUrl(ready: RemoteTerminalReady | null, slug: string, value: string) {
    if (!ready || !slug || !safeUrl(value)) return;
    try { localStorage.setItem(key(ready, slug), value); } catch { /* Storage may be disabled. */ }
}
