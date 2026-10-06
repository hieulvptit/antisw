import { useTranslation } from 'react-i18next';

function Settings() {
    const { t } = useTranslation();

    return (
        <div className="h-full w-full overflow-y-auto">
            <div className="p-5 max-w-7xl mx-auto space-y-5">
                <div className="bg-white dark:bg-base-100 rounded-2xl p-6 shadow-sm border border-gray-100 dark:border-base-200">
                    <div className="flex flex-col h-full animate-in fade-in duration-500">
                        <div className="flex-1 flex flex-col justify-center items-center space-y-8">
                            {/* Branding Section */}
                            <div className="text-center space-y-4">
                                <div className="relative inline-block group">
                                    <div className="absolute inset-0 bg-blue-500/20 rounded-3xl blur-xl group-hover:blur-2xl transition-all duration-500"></div>
                                    <img
                                        src="/icon.png"
                                        alt="Antigravity Logo"
                                        className="relative w-24 h-24 rounded-3xl shadow-2xl transform group-hover:scale-105 transition-all duration-500 rotate-3 group-hover:rotate-6 object-cover bg-white dark:bg-black"
                                    />
                                </div>

                                <div>
                                    <h3 className="text-3xl font-black text-gray-900 dark:text-base-content tracking-tight mb-2">{t('common.app_name', 'AntiSwitcher')}</h3>
                                    <div className="flex items-center justify-center gap-2 text-sm">
                                        v5.1.3
                                        <span className="text-gray-400 dark:text-gray-600">•</span>
                                        <span className="text-gray-500 dark:text-gray-400">{t('settings.branding.subtitle')}</span>
                                    </div>
                                </div>
                            </div>
                        </div>

                        <div className="text-center text-[10px] text-gray-300 dark:text-gray-600 mt-auto pb-2">
                            {t('settings.about.copyright')}
                        </div>
                    </div>
                </div>
            </div>
        </div>
    );
}

export default Settings;
