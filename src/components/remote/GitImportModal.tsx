import { useEffect, useRef, useState } from 'react';
import { Alert, AutoComplete, Button, Checkbox, Input, Modal, Select, Spin } from 'antd';
import { ExternalLink } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../../utils/request';
import { RemoteTool } from '../../stores/useRemoteTerminalStore';
import { validBranch } from '../../utils/gitBranch';
import { isTauri } from '../../utils/env';

interface GitCredential {
    host: string;
    username: string;
    commit_name: string;
    commit_email: string;
    has_pat: boolean;
}
interface GitInventory {
    allowed_hosts: string[];
    credentials: GitCredential[];
    capabilities?: string[];
}
interface GitBranches {
    branches: string[];
    current_branch: string | null;
}
export interface GitImportResult {
    slug: string;
    terminal_id: string;
    repo_url?: string;
}

export default function GitImportModal({ open, tools, onClose, onImported, describeError, terminalId, initialRepoUrl = '', onPushed, onRepoUrlSaved }: {
    open: boolean;
    tools: RemoteTool[];
    onClose: () => void;
    onImported: (result: GitImportResult) => Promise<void>;
    describeError: (error: unknown) => string;
    terminalId?: string;
    initialRepoUrl?: string;
    onPushed?: () => void;
    onRepoUrlSaved?: (url: string) => void;
}) {
    const { t } = useTranslation();
    const pushing = !!terminalId;
    const [inventory, setInventory] = useState<GitInventory | null>(null);
    const [loading, setLoading] = useState(false);
    const [importing, setImporting] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [repoUrl, setRepoUrl] = useState('');
    const [savedRepoUrl, setSavedRepoUrl] = useState('');
    const [repoUrlEditing, setRepoUrlEditing] = useState(true);
    const repoUrlEdited = useRef(false);
    const [tool, setTool] = useState<RemoteTool>('codex');
    const [username, setUsername] = useState('');
    const [commitName, setCommitName] = useState('');
    const [commitEmail, setCommitEmail] = useState('');
    const [pat, setPat] = useState('');
    const [patMode, setPatMode] = useState<'saved' | 'new'>('new');
    const [branchMode, setBranchMode] = useState<'current' | 'existing' | 'new'>('current');
    const [branch, setBranch] = useState('');
    const [branches, setBranches] = useState<GitBranches | null>(null);
    const [branchesRevision, setBranchesRevision] = useState(0);
    const [branchesLoading, setBranchesLoading] = useState(false);
    const [branchesError, setBranchesError] = useState<string | null>(null);
    const [commitChanges, setCommitChanges] = useState(true);
    const [commitMessage, setCommitMessage] = useState('');
    let host = 'git.vnpay.vn';
    let validUrl = false;
    let cloneRepoUrl = repoUrl.trim();
    try {
        const url = new URL(repoUrl.trim());
        host = url.hostname;
        validUrl = url.protocol === 'https:' && !url.username && !url.password && !url.port && !url.search && !url.hash
            && /^\/[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+\/?$/.test(url.pathname)
            && !/[\s\\\x00-\x1f\x7f]/.test(repoUrl.trim());
        if (validUrl && ['git.vnpay.vn', 'gitlab.com'].includes(host)) {
            const repositoryPath = url.pathname.replace(/\/$/, '');
            url.pathname = repositoryPath.endsWith('.git') ? repositoryPath : `${repositoryPath}.git`;
            cloneRepoUrl = url.href;
        }
    } catch { /* URL is still being entered. */ }
    const saved = inventory?.credentials.find((credential) => credential.host === host);
    const allowed = !!inventory?.allowed_hosts.includes(host);
    const patUrl = validUrl ? ({
        'git.vnpay.vn': 'https://git.vnpay.vn/-/user_settings/personal_access_tokens',
        'gitlab.com': 'https://gitlab.com/-/user_settings/personal_access_tokens',
        'github.com': 'https://github.com/settings/personal-access-tokens/new',
    } as Record<string, string>)[host] : undefined;
    const allowedHosts = [...(inventory?.allowed_hosts || [])].sort((a, b) =>
        a === b ? 0 : a === 'git.vnpay.vn' ? -1 : b === 'git.vnpay.vn' ? 1 : 0);
    const missingFields = [
        !username.trim() && t('remote_terminal.git.username'),
        !commitName.trim() && t('remote_terminal.git.commit_name'),
        !commitEmail.trim() && t('remote_terminal.git.commit_email'),
        !(patMode === 'saved' ? saved?.has_pat : pat.trim()) && t('remote_terminal.git.pat'),
    ].filter(Boolean);
    const blockedReasons = [
        !inventory && t('remote_terminal.git.credentials_load_failed'),
        !validUrl && t('remote_terminal.git.invalid_url'),
        inventory && validUrl && !allowed && t('remote_terminal.git.host_not_allowed', { hosts: allowedHosts.join(', ') }),
        !pushing && !tools.includes(tool) && t('remote_terminal.git.errors.git_import_tool_not_allowed'),
        missingFields.length > 0 && t('remote_terminal.git.required_fields', { fields: missingFields.join(', ') }),
        inventory && ((pushing && (!inventory.capabilities?.includes('push_target')
            || ((commitChanges || branchMode !== 'current') && !inventory.capabilities?.includes('commit_push'))))
            || (!pushing && !!branch.trim() && !inventory.capabilities?.includes('clone_branch')))
            && t('remote_terminal.git.errors.git_protocol_not_supported'),
        !!branch.trim() && !validBranch(branch.trim()) && t('remote_terminal.git.errors.invalid_git_branch'),
        pushing && branchMode !== 'current' && !branch.trim() && t('remote_terminal.git.branch_required'),
        pushing && branchMode === 'new' && branches?.branches.includes(branch.trim()) && t('remote_terminal.git.errors.git_branch_exists'),
        pushing && commitChanges && !commitMessage.trim() && t('remote_terminal.git.commit_message_required'),
        !!commitEmail.trim() && !/^[^\s<>@]+@[^\s<>@]+$/.test(commitEmail.trim()) && t('remote_terminal.git.invalid_commit_email'),
    ].filter((reason): reason is string => typeof reason === 'string');
    const canImport = !loading && !importing && !branchesLoading && blockedReasons.length === 0;

    useEffect(() => {
        if (!open) { setPat(''); return; }
        setRepoUrl(initialRepoUrl); setSavedRepoUrl(initialRepoUrl);
        setRepoUrlEditing(!initialRepoUrl); repoUrlEdited.current = false;
        setInventory(null);
        setBranchMode('current'); setBranch(''); setBranches(null); setBranchesError(null);
        setCommitChanges(true); setCommitMessage('');
        setTool(tools[0] || 'codex');
        // Reset the form when opening.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open]);

    useEffect(() => {
        if (!open || !terminalId || initialRepoUrl) return;
        let cancelled = false;
        invoke<{ repo_url: string | null }>('remote_terminal_get_git_repository', { terminalId })
            .then((result) => {
                if (!cancelled && result.repo_url && !repoUrlEdited.current) {
                    setRepoUrl(result.repo_url); setSavedRepoUrl(result.repo_url);
                    setRepoUrlEditing(false); onRepoUrlSaved?.(result.repo_url);
                }
            })
            .catch(() => { /* A repo without origin can use a URL entered by the user. */ });
        return () => { cancelled = true; };
        // Resolve origin once on opening, without overwriting edits.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open, terminalId]);

    const rememberRepoUrl = () => {
        if (!validUrl || !allowed) return;
        onRepoUrlSaved?.(cloneRepoUrl);
        setSavedRepoUrl(cloneRepoUrl); setRepoUrlEditing(false);
    };

    useEffect(() => {
        if (open && tools.length > 0 && !tools.includes(tool)) setTool(tools[0]);
    }, [open, tools, tool]);

    useEffect(() => {
        if (!open) return;
        let cancelled = false;
        setError(null);
        setLoading(true);
        invoke<GitInventory>('remote_terminal_list_git_credentials')
            .then((result) => { if (!cancelled) setInventory(result); })
            .catch((e) => { if (!cancelled) setError(describeError(e)); })
            .finally(() => { if (!cancelled) setLoading(false); });
        return () => { cancelled = true; };
        // Load on opening, not on each terminal poll.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open]);

    useEffect(() => {
        setUsername(saved?.username || '');
        setCommitName(saved?.commit_name || '');
        setCommitEmail(saved?.commit_email || '');
    }, [host, saved?.username, saved?.commit_name, saved?.commit_email]);

    useEffect(() => { setPat(''); setPatMode(saved?.has_pat ? 'saved' : 'new'); }, [host, open, saved?.has_pat]);

    useEffect(() => {
        if (!open || importing || !validUrl || !allowed || !saved?.has_pat || !inventory?.capabilities?.includes('branches')) { setBranches(null); return; }
        let cancelled = false;
        setBranches(null); setBranchesLoading(true); setBranchesError(null);
        const timer = window.setTimeout(() => {
            invoke<GitBranches>('remote_terminal_list_git_branches', { terminalId: terminalId || null, repoUrl: cloneRepoUrl })
                .then((result) => { if (!cancelled) setBranches(result); })
                .catch((e) => { if (!cancelled) setBranchesError(describeError(e)); })
                .finally(() => { if (!cancelled) setBranchesLoading(false); });
        }, 400);
        return () => { cancelled = true; window.clearTimeout(timer); setBranchesLoading(false); };
        // Keep terminal polling from restarting this request.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open, terminalId, validUrl, allowed, cloneRepoUrl, saved?.has_pat, inventory?.capabilities, branchesRevision, importing]);

    const openPatPage = async () => {
        if (!patUrl || importing || loading) return;
        try {
            if (isTauri()) {
                const { openUrl } = await import('@tauri-apps/plugin-opener');
                await openUrl(patUrl);
            } else {
                window.open(patUrl, '_blank', 'noopener,noreferrer');
            }
        } catch (e) { setError(describeError(e)); }
    };

    const saveCredentials = async () => {
        const newPat = patMode === 'new' ? pat.trim() : '';
        if (newPat || !saved || username.trim() !== saved.username || commitName.trim() !== saved.commit_name || commitEmail.trim() !== saved.commit_email) {
            await invoke('remote_terminal_save_git_credentials', { credential: {
                host, username: username.trim(), commit_name: commitName.trim(), commit_email: commitEmail.trim(),
                ...(newPat ? { pat: newPat } : {}),
            } });
            setInventory((current) => current ? { ...current, credentials: [
                ...current.credentials.filter((credential) => credential.host !== host),
                { host, username: username.trim(), commit_name: commitName.trim(), commit_email: commitEmail.trim(), has_pat: true },
            ] } : current);
            setPat('');
        }
        setPatMode('saved');
    };
    const refreshBranches = async () => {
        setBranchesLoading(true); setBranchesError(null);
        try {
            await saveCredentials();
            setBranchesRevision((revision) => revision + 1);
        } catch (e) { setBranchesError(describeError(e)); setBranchesLoading(false); }
    };

    const importRepository = async () => {
        if (!canImport) return;
        setImporting(true);
        setError(null);
        try {
            await saveCredentials();
            if (pushing) {
                await invoke('remote_terminal_push_git', {
                    terminalId, repoUrl: cloneRepoUrl,
                    branch: branchMode === 'current' ? null : branch.trim(),
                    createBranch: branchMode === 'new',
                    commitMessage: commitChanges ? commitMessage.trim() : null,
                });
                rememberRepoUrl();
                onPushed?.();
                onClose();
                return;
            }
            const result = await invoke<GitImportResult>('remote_terminal_import_git', { repoUrl: cloneRepoUrl, tool, branch: branch.trim() || null });
            // A successful clone must not be submitted again if attaching the
            // viewer fails. The inventory poll can discover it later.
            onClose();
            await onImported({ ...result, repo_url: result.repo_url || cloneRepoUrl });
        } catch (e) {
            const code = String(e).replace(/^(?:Error:\s*)?(?:Remote terminal error:\s*)?/i, '');
            const key = `remote_terminal.git.errors.${code}`;
            setError(t(key, { defaultValue: describeError(e) }));
        } finally { setImporting(false); }
    };

    return (
        <Modal open={open} title={t(pushing ? 'remote_terminal.git.push_title' : 'remote_terminal.git.title')} centered destroyOnHidden
            okText={t(pushing ? commitChanges ? 'remote_terminal.git.push_button' : 'remote_terminal.git.push_only_button' : 'remote_terminal.git.import_button')} cancelText={t('common.cancel')}
            onOk={() => void importRepository()} onCancel={() => { if (!importing) onClose(); }}
            confirmLoading={importing} okButtonProps={{ disabled: !canImport }}
            cancelButtonProps={{ disabled: importing }} closable={!importing} maskClosable={false}>
            <Spin spinning={loading}>
                <div className="flex flex-col gap-4 py-2">
                    <p className="text-sm text-gray-500">{t(pushing ? 'remote_terminal.git.push_description' : 'remote_terminal.git.description')}</p>
                    {error && <Alert type="error" showIcon title={error} />}
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.repo_url')}
                        <div className="flex gap-2">
                            <Input value={repoUrl} onChange={(e) => { repoUrlEdited.current = true; setRepoUrl(e.target.value); }}
                                disabled={importing || loading || (pushing && !repoUrlEditing)}
                                placeholder="https://git.vnpay.vn/group/repository.git" autoFocus={!pushing || repoUrlEditing} maxLength={2048} />
                            {pushing && <Button disabled={importing || loading || (repoUrlEditing && (!validUrl || !allowed))}
                                onClick={() => {
                                    if (repoUrlEditing) rememberRepoUrl();
                                    else { repoUrlEdited.current = true; setRepoUrlEditing(true); }
                                }}>
                                {t(repoUrlEditing ? 'remote_terminal.git.save_repo_url' : 'remote_terminal.git.edit_repo_url')}
                            </Button>}
                        </div>
                    </label>
                    {pushing && !!savedRepoUrl && !repoUrlEditing && <p className="text-xs text-gray-500">
                        {t('remote_terminal.git.repo_url_saved')}
                    </p>}
                    {!pushing && validUrl && cloneRepoUrl !== repoUrl.trim() && <p className="text-xs text-gray-500 break-all">
                        {t('remote_terminal.git.clone_url', { url: cloneRepoUrl })}
                    </p>}
                    {!pushing && <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.tool')}
                        <Select value={tool} onChange={setTool} disabled={importing} options={tools.map((value) => ({ value, label: value === 'codex' ? 'Codex' : 'Claude' }))} />
                    </label>}
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.pat_source')}
                        <Select value={patMode} onChange={(value) => { setPatMode(value); setPat(''); }} disabled={importing || loading}
                            options={[
                                ...(saved?.has_pat ? [{ value: 'saved', label: t('remote_terminal.git.use_saved_pat', { host, username: saved.username }) }] : []),
                                { value: 'new', label: t('remote_terminal.git.enter_new_pat') },
                            ]} />
                    </label>
                    <p className="text-xs text-gray-500">{t(saved?.has_pat ? 'remote_terminal.git.saved_pat' : 'remote_terminal.git.new_pat', { host })}</p>
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.username')}
                        <Input value={username} onChange={(e) => setUsername(e.target.value)} disabled={importing || loading} maxLength={256} autoComplete="off" />
                    </label>
                    <div className="flex flex-col gap-1.5 text-sm">
                        <div className="flex items-center justify-between gap-2">
                            <label htmlFor="git-import-pat">{t('remote_terminal.git.pat')}</label>
                            <Button type="link" size="small" icon={<ExternalLink size={14} />}
                                disabled={!patUrl || importing || loading} onClick={() => void openPatPage()}
                                title={patUrl || t('remote_terminal.git.invalid_url')}>
                                {t('remote_terminal.git.get_pat', { defaultValue: 'Get PAT' })}
                            </Button>
                        </div>
                        {patMode === 'saved' ? <p className="text-xs text-gray-500">{t('remote_terminal.git.saved_pat_selected', { host })}</p> : <Input.Password id="git-import-pat" value={pat} onChange={(e) => setPat(e.target.value)} disabled={importing || loading} maxLength={8192}
                            placeholder={t(saved?.has_pat ? 'remote_terminal.git.pat_keep' : pushing ? 'remote_terminal.git.push_pat_required' : 'remote_terminal.git.pat_required')} autoComplete="new-password" />}
                    </div>
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.branch')}
                        {pushing && <Select value={branchMode} onChange={(value) => { setBranchMode(value); setBranch(''); }} disabled={importing || loading}
                            options={[
                                { value: 'current', label: t('remote_terminal.git.current_branch', { branch: branches?.current_branch || 'HEAD' }) },
                                { value: 'existing', label: t('remote_terminal.git.choose_branch') },
                                { value: 'new', label: t('remote_terminal.git.create_branch') },
                            ]} />}
                        {pushing && branchMode === 'existing' && branches ? <Select showSearch value={branch || undefined}
                            onChange={setBranch} loading={branchesLoading} disabled={importing || loading || branchesLoading}
                            placeholder={t('remote_terminal.git.choose_branch')} options={branches.branches.map((value) => ({ value, label: value }))} />
                            : (!pushing || branchMode !== 'current') && <AutoComplete value={branch} onChange={setBranch}
                                options={!pushing ? branches?.branches.map((value) => ({ value })) : []}
                                filterOption={(input, option) => !!option?.value?.toLowerCase().includes(input.toLowerCase())}
                                disabled={importing || loading}
                                placeholder={t(pushing && branchMode === 'new' ? 'remote_terminal.git.new_branch_placeholder' : pushing ? 'remote_terminal.git.choose_branch' : 'remote_terminal.git.clone_branch_placeholder')} />}
                        <Button size="small" onClick={() => void refreshBranches()} loading={branchesLoading}
                            disabled={importing || loading || !validUrl || !allowed || !inventory?.capabilities?.includes('branches') || missingFields.length > 0 || !/^[^\s<>@]+@[^\s<>@]+$/.test(commitEmail.trim())}>
                            {t('remote_terminal.git.refresh_branches')}
                        </Button>
                        {branchesLoading && <span className="text-xs text-gray-500">{t('remote_terminal.git.loading_branches')}</span>}
                    </label>
                    {branchesError && <Alert type="warning" showIcon title={t('remote_terminal.git.branches_load_failed')} description={branchesError} />}
                    {pushing && <>
                        <Checkbox checked={commitChanges} onChange={(e) => setCommitChanges(e.target.checked)} disabled={importing || loading}>
                            {t('remote_terminal.git.commit_changes')}
                        </Checkbox>
                        {commitChanges && <label className="flex flex-col gap-1.5 text-sm">
                            {t('remote_terminal.git.commit_message')}
                            <Input.TextArea value={commitMessage} onChange={(e) => setCommitMessage(e.target.value)} disabled={importing || loading}
                                maxLength={4096} autoSize={{ minRows: 2, maxRows: 4 }} />
                            <span className="text-xs text-gray-500">{t('remote_terminal.git.commit_changes_hint')}</span>
                        </label>}
                    </>}
                    <div className="grid grid-cols-2 gap-3">
                        <label className="flex flex-col gap-1.5 text-sm">
                            {t('remote_terminal.git.commit_name')}
                            <Input value={commitName} onChange={(e) => setCommitName(e.target.value)} disabled={importing || loading} maxLength={256} />
                        </label>
                        <label className="flex flex-col gap-1.5 text-sm">
                            {t('remote_terminal.git.commit_email')}
                            <Input value={commitEmail} onChange={(e) => setCommitEmail(e.target.value)} disabled={importing || loading} maxLength={320} type="email" />
                        </label>
                    </div>
                    {!loading && !importing && blockedReasons.length > 0 && <Alert type="warning" showIcon
                        title={t(pushing ? 'remote_terminal.git.push_blocked' : 'remote_terminal.git.import_blocked')}
                        description={<ul className="list-disc pl-4">{blockedReasons.map((reason) => <li key={reason}>{reason}</li>)}</ul>} />}
                    {importing && <p role="status" className="text-sm text-gray-500">{t(pushing ? 'remote_terminal.git.pushing' : 'remote_terminal.git.cloning')}</p>}
                </div>
            </Spin>
        </Modal>
    );
}
