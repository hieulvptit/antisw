import { useEffect, useState } from 'react';
import { Alert, Button, Input, Modal, Select, Spin } from 'antd';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../../utils/request';
import { validBranch } from '../../utils/gitBranch';

interface Repository {
    repo_url: string | null;
    branches: string[];
    current_branch: string | null;
    capabilities?: string[];
}
export default function GitBranchModal({ open, terminalId, onClose, onChanged, describeError }: {
    open: boolean;
    terminalId?: string;
    onClose: () => void;
    onChanged: (branch: string) => void;
    describeError: (error: unknown) => string;
}) {
    const { t } = useTranslation();
    const [repository, setRepository] = useState<Repository | null>(null);
    const [mode, setMode] = useState<'existing' | 'new'>('new');
    const [branch, setBranch] = useState('');
    const [loading, setLoading] = useState(false);
    const [switching, setSwitching] = useState(false);
    const [remoteLoaded, setRemoteLoaded] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const errorMessage = (e: unknown) => {
        const code = String(e).replace(/^(?:Error:\s*)?(?:Remote terminal error:\s*)?/i, '');
        return t(`remote_terminal.git.errors.${code}`, { defaultValue: describeError(e) });
    };
    useEffect(() => {
        if (!open || !terminalId) return;
        let cancelled = false;
        setLoading(true); setError(null); setRepository(null); setRemoteLoaded(false);
        setMode('new'); setBranch('');
        invoke<Repository>('remote_terminal_get_git_repository', { terminalId })
            .then((result) => { if (!cancelled) setRepository(result); })
            .catch((e) => { if (!cancelled) setError(errorMessage(e)); })
            .finally(() => { if (!cancelled) setLoading(false); });
        return () => { cancelled = true; };
        // Load once for this terminal, independently of terminal polling.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [open, terminalId]);
    const supported = !!repository?.capabilities?.includes('switch_branch');
    const branches = repository?.branches || [];
    const duplicate = mode === 'new' && (branches.includes(branch.trim()) || branch.trim() === repository?.current_branch);
    const invalid = !!branch && !validBranch(branch.trim());
    const canSwitch = supported && !!terminalId && validBranch(branch.trim()) && !duplicate && !loading && !switching;
    const loadRemote = async () => {
        if (!repository?.repo_url || !terminalId) return;
        setLoading(true); setError(null);
        try {
            const result = await invoke<{ branches: string[] }>('remote_terminal_list_git_branches', { terminalId, repoUrl: repository.repo_url });
            setRepository((current) => current ? { ...current, branches: result.branches } : current);
            setRemoteLoaded(true);
        } catch (e) { setError(errorMessage(e)); }
        finally { setLoading(false); }
    };
    const changeBranch = async () => {
        if (!canSwitch) return;
        setSwitching(true); setError(null);
        try {
            await invoke('remote_terminal_switch_git_branch', {
                terminalId, branch: branch.trim(), createBranch: mode === 'new',
                // Local branch changes need no PAT or network access.
                repoUrl: mode === 'existing' && remoteLoaded ? repository?.repo_url : null,
            });
            onChanged(branch.trim()); onClose();
        } catch (e) { setError(errorMessage(e)); }
        finally { setSwitching(false); }
    };
    return <Modal open={open} title={t('remote_terminal.git.branch_title')} centered destroyOnHidden
        onCancel={() => { if (!switching && !loading) onClose(); }} onOk={() => void changeBranch()}
        okText={t(mode === 'new' ? 'remote_terminal.git.create_branch_button' : 'remote_terminal.git.switch_branch_button')}
        cancelText={t('common.cancel')} okButtonProps={{ disabled: !canSwitch }} confirmLoading={switching}
        cancelButtonProps={{ disabled: switching || loading }} closable={!switching && !loading} maskClosable={false}>
        <Spin spinning={loading}>
            <div className="flex flex-col gap-4 py-2">
                <p className="text-sm text-gray-500">{t('remote_terminal.git.branch_description')}</p>
                {error && <Alert type="error" showIcon title={error} />}
                {repository && !supported && <Alert type="warning" showIcon title={t('remote_terminal.git.branch_server_update')} />}
                {repository && <p className="text-sm">{t('remote_terminal.git.current_branch', { branch: repository.current_branch || 'HEAD' })}</p>}
                <Select value={mode} disabled={loading || switching || !supported} onChange={(value) => { setMode(value); setBranch(''); }}
                    options={[
                        { value: 'new', label: t('remote_terminal.git.create_branch') },
                        { value: 'existing', label: t('remote_terminal.git.choose_branch') },
                    ]} />
                <label className="flex flex-col gap-1.5 text-sm">
                    {t('remote_terminal.git.branch')}
                    {mode === 'new' ? <Input value={branch} onChange={(e) => setBranch(e.target.value)} maxLength={256}
                        disabled={loading || switching || !supported} placeholder={t('remote_terminal.git.new_branch_placeholder')} />
                        : <Select showSearch value={branch || undefined} onChange={setBranch} disabled={loading || switching || !supported}
                            options={branches.map((value) => ({ value, label: value }))} placeholder={t('remote_terminal.git.choose_branch')} />}
                </label>
                {duplicate && <Alert type="warning" showIcon title={t('remote_terminal.git.errors.git_branch_exists')} />}
                {invalid && <Alert type="warning" showIcon title={t('remote_terminal.git.errors.invalid_git_branch')} />}
                {repository?.repo_url && <Button onClick={() => void loadRemote()} disabled={loading || switching || !supported}>
                    {t('remote_terminal.git.load_remote_branches')}
                </Button>}
            </div>
        </Spin>
    </Modal>;
}
