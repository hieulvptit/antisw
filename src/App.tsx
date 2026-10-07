import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom';

import Layout from './components/layout/Layout';
import Accounts from './pages/Accounts';
import Settings from './pages/Settings';
import RemoteTerminal from './pages/RemoteTerminal';
import ThemeManager from './components/common/ThemeManager';
import DebugConsole from './components/debug/DebugConsole';
import { useEffect } from 'react';
import { useViewStore } from './stores/useViewStore';
import { useConfigStore } from './stores/useConfigStore';
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

  // Listen for tray navigation
  useEffect(() => {
    if (!isTauri()) return;
    const unlistenPromises: Promise<() => void>[] = [];

    unlistenPromises.push(
      listen('tray://open-remote-terminal', () => {
        useViewStore.getState().setMiniView(false);
        void router.navigate('/remote-terminal');
      })
    );

    // Cleanup
    return () => {
      Promise.all(unlistenPromises).then(unlisteners => {
        unlisteners.forEach(unlisten => unlisten());
      });
    };
  }, []);

  return (
    <AdminAuthGuard>
      <ThemeManager />
      <DebugConsole />
      <RouterProvider router={router} />
    </AdminAuthGuard>
  );
}

export default App;
