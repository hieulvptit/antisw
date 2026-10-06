import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom';

import Layout from './components/layout/Layout';
import Accounts from './pages/Accounts';
import Settings from './pages/Settings';
import RemoteTerminal from './pages/RemoteTerminal';
import ThemeManager from './components/common/ThemeManager';
import { UpdateNotification } from './components/UpdateNotification';
import DebugConsole from './components/debug/DebugConsole';
import { useEffect } from 'react';
import { useViewStore } from './stores/useViewStore';
import { useConfigStore } from './stores/useConfigStore';
import { useAccountStore } from './stores/useAccountStore';
import { useTranslation } from 'react-i18next';
import { listen } from '@tauri-apps/api/event';
import { isTauri } from './utils/env';
import { AdminAuthGuard } from './components/common/AdminAuthGuard';
import { trackEvent, TrackingEvents } from './utils/tracking';



const router = createBrowserRouter([
  {
    path: '/',
    element: <Layout />,
    children: [
      {
        index: true,
        element: <Navigate to="/remote-terminal" replace />,
      },
      {
        path: 'accounts',
        element: <Accounts />,
      },
      {
        path: 'settings',
        element: <Settings />,
      },
      {
        path: 'remote-terminal',
        element: <RemoteTerminal />,
      },
    ],
  },
]);

function App() {
  const { config, loadConfig } = useConfigStore();
  const { fetchCurrentAccount, fetchAccounts } = useAccountStore();
  const { i18n } = useTranslation();

  useEffect(() => {
    loadConfig();

    // Track app open event
    trackEvent(TrackingEvents.APP_OPEN, undefined, {
      screen: 'app',
      button: null,
    });
  }, [loadConfig]);

  // Sync language from config
  useEffect(() => {
    if (config?.language) {
      i18n.changeLanguage(config.language);
      // Support RTL
      if (config.language === 'ar') {
        document.documentElement.dir = 'rtl';
      } else {
        document.documentElement.dir = 'ltr';
      }
    }
  }, [config?.language, i18n]);

  // Listen for tray navigation and backend account refresh events
  useEffect(() => {
    if (!isTauri()) return;
    const unlistenPromises: Promise<() => void>[] = [];

    unlistenPromises.push(
      listen('tray://open-remote-terminal', () => {
        useViewStore.getState().setMiniView(false);
        void router.navigate('/remote-terminal');
      })
    );

    // 监听后端全量刷新事件 (Command / Scheduler)
    unlistenPromises.push(
      listen('accounts://refreshed', () => {
        console.log('[App] Backend triggered quota refresh, syncing UI...');
        fetchCurrentAccount();
        fetchAccounts();
      })
    );

    // Cleanup
    return () => {
      Promise.all(unlistenPromises).then(unlisteners => {
        unlisteners.forEach(unlisten => unlisten());
      });
    };
  }, [fetchCurrentAccount, fetchAccounts]);

  // Update notification state

  // Check for updates on startup
  useEffect(() => {
    // const checkUpdates = async () => {
    //   try {
    //     console.log('[App] Checking if we should check for updates...');
    //     const shouldCheck = await invoke<boolean>('should_check_updates');
    //     console.log('[App] Should check updates:', shouldCheck);

    //     if (shouldCheck) {
    //       setShowUpdateNotification(true);
    //       // 我们这里只负责显示通知组件，通知组件内部会去调用 check_for_updates
    //       // 我们在显示组件后，标记已经检查过了（即便失败或无更新，组件内部也会处理）
    //       await invoke('update_last_check_time');
    //       console.log('[App] Update check cycle initiated and last check time updated.');
    //     }
    //   } catch (error) {
    //     console.error('Failed to check update settings:', error);
    //   }
    // };

    // Delay check to avoid blocking initial render
    // const timer = setTimeout(checkUpdates, 2000);
    // return () => clearTimeout(timer);
  }, []);

  return (
    <AdminAuthGuard>
      <ThemeManager />
      <DebugConsole />
      {true && (
        <UpdateNotification onClose={() => { }} />
      )}
      <RouterProvider router={router} />
    </AdminAuthGuard>
  );
}

export default App;