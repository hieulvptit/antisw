import { useEffect, useState } from 'react';
import { Alert, Input, Modal, Select, Spin } from 'antd';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../../utils/request';
import { RemoteTool } from '../../stores/useRemoteTerminalStore';

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
}
export interface GitImportResult {
    slug: string;
    terminal_id: string;
}

export default function GitImportModal({ open, tools, onClose, onImported, describeError }: {
    open: boolean;
    tools: RemoteTool[];
    onClose: () => void;
    onImported: (result: GitImportResult) => Promise<void>;
    describeError: (error: unknown) => string;
}) {
    const { t } = useTranslation();
    const [inventory, setInventory] = useState<GitInventory | null>(null);
    const [loading, setLoading] = useState(false);
    const [importing, setImporting] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [repoUrl, setRepoUrl] = useState('');
    const [tool, setTool] = useState<RemoteTool>('codex');
    const [username, setUsername] = useState('');
    const [commitName, setCommitName] = useState('');
    const [commitEmail, setCommitEmail] = useState('');
    const [pat, setPat] = useState('');
    let host = 'git.vnpay.vn';
    let validUrl = false;
    try {
        const url = new URL(repoUrl.trim());
        host = url.hostname;
        validUrl = url.protocol === 'https:' && !url.username && !url.password && !url.port && !url.search && !url.hash
            && /^\/[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+\/?$/.test(url.pathname)
            && !/[\s\\\x00-\x1f\x7f]/.test(repoUrl.trim());
    } catch { /* URL is still being entered. */ }
    const saved = inventory?.credentials.find((credential) => credential.host === host);
    const allowed = !!inventory?.allowed_hosts.includes(host);
    const canImport = !loading && !importing && validUrl && allowed && tools.includes(tool)
        && !!username.trim() && !!commitName.trim() && /^[^\s<>@]+@[^\s<>@]+$/.test(commitEmail.trim())
        && (!!pat.trim() || !!saved?.has_pat);

    useEffect(() => {
        if (!open) { setPat(''); return; }
        let cancelled = false;
        setInventory(null);
        setRepoUrl('');
        setError(null);
        setLoading(true);
        setTool(tools[0] || 'codex');
        invoke<GitInventory>('remote_terminal_list_git_credentials')
            .then((result) => { if (!cancelled) setInventory(result); })
            .catch((e) => { if (!cancelled) setError(describeError(e)); })
            .finally(() => { if (!cancelled) setLoading(false); });
        return () => { cancelled = true; };
        // Load once per modal opening, not on each inventory poll.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open]);

    useEffect(() => {
        setUsername(saved?.username || '');
        setCommitName(saved?.commit_name || '');
        setCommitEmail(saved?.commit_email || '');
        setPat('');
    }, [host, saved]);

    const importRepository = async () => {
        if (!canImport) return;
        setImporting(true);
        setError(null);
        try {
            if (pat || !saved || username.trim() !== saved.username || commitName.trim() !== saved.commit_name || commitEmail.trim() !== saved.commit_email) {
                await invoke('remote_terminal_save_git_credentials', { credential: {
                    host, username: username.trim(), commit_name: commitName.trim(), commit_email: commitEmail.trim(),
                    ...(pat ? { pat } : {}),
                } });
                setInventory((current) => current ? { ...current, credentials: [
                    ...current.credentials.filter((credential) => credential.host !== host),
                    { host, username: username.trim(), commit_name: commitName.trim(), commit_email: commitEmail.trim(), has_pat: true },
                ] } : current);
                setPat('');
            }
            const result = await invoke<GitImportResult>('remote_terminal_import_git', { repoUrl: repoUrl.trim(), tool });
            // A successful clone must not be submitted again if attaching the
            // viewer fails. The inventory poll can discover it later.
            onClose();
            await onImported(result);
        } catch (e) {
            const code = String(e).replace(/^Error:\s*/, '');
            const key = `remote_terminal.git.errors.${code}`;
            setError(t(key, { defaultValue: describeError(e) }));
        } finally { setImporting(false); }
    };

    return (
        <Modal open={open} title={t('remote_terminal.git.title')} centered destroyOnHidden
            okText={t('remote_terminal.git.import_button')} cancelText={t('common.cancel')}
            onOk={() => void importRepository()} onCancel={() => { if (!importing) onClose(); }}
            confirmLoading={importing} okButtonProps={{ disabled: !canImport }}
            cancelButtonProps={{ disabled: importing }} closable={!importing} maskClosable={false}>
            <Spin spinning={loading}>
                <div className="flex flex-col gap-4 py-2">
                    <p className="text-sm text-gray-500">{t('remote_terminal.git.description')}</p>
                    {error && <Alert type="error" showIcon title={error} />}
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.repo_url')}
                        <Input value={repoUrl} onChange={(e) => setRepoUrl(e.target.value)} disabled={importing || loading}
                            placeholder="https://git.vnpay.vn/group/repository.git" autoFocus maxLength={2048} />
                    </label>
                    {inventory && repoUrl && (!validUrl || !allowed) && <Alert type="warning" showIcon
                        title={t(validUrl ? 'remote_terminal.git.host_not_allowed' : 'remote_terminal.git.invalid_url', { hosts: inventory.allowed_hosts.join(', ') })} />}
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.tool')}
                        <Select value={tool} onChange={setTool} disabled={importing} options={tools.map((value) => ({ value, label: value === 'codex' ? 'Codex' : 'Claude' }))} />
                    </label>
                    <p className="text-xs text-gray-500">{t(saved?.has_pat ? 'remote_terminal.git.saved_pat' : 'remote_terminal.git.new_pat', { host })}</p>
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.username')}
                        <Input value={username} onChange={(e) => setUsername(e.target.value)} disabled={importing || loading} maxLength={256} autoComplete="off" />
                    </label>
                    <label className="flex flex-col gap-1.5 text-sm">
                        {t('remote_terminal.git.pat')}
                        <Input.Password value={pat} onChange={(e) => setPat(e.target.value)} disabled={importing || loading} maxLength={8192}
                            placeholder={t(saved?.has_pat ? 'remote_terminal.git.pat_keep' : 'remote_terminal.git.pat_required')} autoComplete="new-password" />
                    </label>
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
                    {importing && <p role="status" className="text-sm text-gray-500">{t('remote_terminal.git.cloning')}</p>}
                </div>
            </Spin>
        </Modal>
    );
}
