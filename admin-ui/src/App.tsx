import { useState, useEffect, lazy, Suspense } from "react";
import { storage } from "@/lib/storage";
import {
  applyTheme,
  applyThemeWithTransition,
  resolveDarkMode,
  type ThemeId,
  type ThemeMode,
  type ThemeSelection,
} from "@/lib/theme";
import { LoginPage } from "@/components/login-page";
import { Toaster } from "@/components/ui/sonner";
import { ConfirmProvider } from "@/components/ui/confirm-dialog";
import { TooltipProvider } from "@/components/ui/tooltip";
import { tabFromHash } from "@/hooks/use-url-state";
import { AppLayout, type TabKey as Tab } from "@/components/layout/app-layout";

const Dashboard = lazy(() =>
  import("@/components/dashboard").then((m) => ({ default: m.Dashboard })),
);
const OverviewPage = lazy(() =>
  import("@/components/overview-page").then((m) => ({
    default: m.OverviewPage,
  })),
);
const ClientKeysPage = lazy(() =>
  import("@/components/client-keys-page").then((m) => ({
    default: m.ClientKeysPage,
  })),
);
const TraceLogPage = lazy(() =>
  import("@/components/trace-log-page").then((m) => ({
    default: m.TraceLogPage,
  })),
);
const GroupsPage = lazy(() =>
  import("@/components/groups-page").then((m) => ({
    default: m.GroupsPage,
  })),
);
const SettingsPage = lazy(() =>
  import("@/components/settings-page").then((m) => ({
    default: m.SettingsPage,
  })),
);


function readTabFromHash(): Tab {
  // 走共享解析：hash 里现在可能带筛选查询串（#/traces?status=error），
  // 直接全等比较会认不出 Tab。
  const h = tabFromHash();
  if (
    h === "credentials" ||
    h === "keys" ||
    h === "groups" ||
    h === "overview" ||
    h === "traces" ||
    h === "settings"
  )
    return h;
  return "overview";
}

interface AppHeaderProps {
  theme: ThemeSelection;
  isDarkMode: boolean;
  tab: Tab;
  onLogout: () => void;
  onSwitchTab: (next: Tab, subKey?: string) => void;
  onSelectPalette: (palette: ThemeId) => void;
  onSelectMode: (mode: ThemeMode) => void;
}

function App() {
  const app = useAppShell();

  if (!app.isLoggedIn) {
    return <LoggedOutApp onLogin={app.handleLogin} />;
  }

  return (
    <LoggedInApp
      theme={app.theme}
      isDarkMode={app.isDarkMode}
      tab={app.tab}
      onLogout={app.handleLogout}
      onSwitchTab={app.switchTab}
      onSelectPalette={app.selectPalette}
      onSelectMode={app.selectMode}
    />
  );
}

function useAppShell() {
  const [isLoggedIn, setIsLoggedIn] = useState(false);
  const [tab, setTab] = useState<Tab>(readTabFromHash);
  const [theme, setTheme] = useState<ThemeSelection>(() => storage.getThemeSelection());
  const [isDarkMode, setIsDarkMode] = useState(() => resolveDarkMode(theme));

  useEffect(() => {
    if (storage.getApiKey()) setIsLoggedIn(true);
  }, []);

  useEffect(() => {
    const onHash = () => setTab(readTabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    storage.setThemeSelection(theme);
    const resolved = resolveDarkMode(theme);
    const root = document.documentElement;
    const alreadyApplied =
      root.dataset.theme === theme.palette && root.classList.contains("dark") === resolved;
    setIsDarkMode(alreadyApplied ? resolved : applyThemeWithTransition(theme, resolved));
    if (alreadyApplied) applyTheme(theme, resolved);
  }, [theme]);

  useEffect(() => {
    if (theme.mode !== "system" || typeof window.matchMedia !== "function") return;

    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onSystemThemeChange = (event: MediaQueryListEvent) => {
      setIsDarkMode(applyThemeWithTransition(theme, event.matches));
    };
    media.addEventListener("change", onSystemThemeChange);
    return () => media.removeEventListener("change", onSystemThemeChange);
  }, [theme]);

  const switchTab = (next: Tab, subKey?: string) => {
    const targetHash = subKey ? `#/${next}?s=${subKey}` : `#/${next}`;
    if (window.location.hash !== targetHash) {
      window.location.hash = targetHash;
      window.dispatchEvent(new Event("hashchange"));
    }
    setTab(next);
  };

  const handleLogin = () => setIsLoggedIn(true);
  const handleLogout = () => {
    storage.removeApiKey();
    setIsLoggedIn(false);
  };
  const selectPalette = (palette: ThemeId) => {
    setTheme((current) => ({ ...current, palette }));
  };
  const selectMode = (mode: ThemeMode) => {
    setTheme((current) => ({ ...current, mode }));
  };

  return {
    handleLogin,
    handleLogout,
    isLoggedIn,
    isDarkMode,
    selectMode,
    selectPalette,
    switchTab,
    tab,
    theme,
  };
}

function LoggedOutApp({ onLogin }: { onLogin: () => void }) {
  return (
    <>
      <LoginPage onLogin={onLogin} />
      <Toaster position="top-center" />
    </>
  );
}

function LoggedInApp({
  theme,
  isDarkMode,
  onLogout,
  onSwitchTab,
  onSelectPalette,
  onSelectMode,
  tab,
}: AppHeaderProps) {
  return (
    <TooltipProvider delayDuration={150}>
      <ConfirmProvider>
        <AppLayout
          currentTab={tab}
          onSelectTab={onSwitchTab}
          theme={theme}
          isDarkMode={isDarkMode}
          onSelectPalette={onSelectPalette}
          onSelectMode={onSelectMode}
          onLogout={onLogout}
        >
          <AppMain tab={tab} onLogout={onLogout} />
        </AppLayout>
        <Toaster position="top-center" />
      </ConfirmProvider>
    </TooltipProvider>
  );
}

function AppMain({ onLogout, tab }: { onLogout: () => void; tab: Tab }) {
  return (
    <Suspense
      fallback={
        <div className="flex h-64 items-center justify-center text-xs font-mono text-muted-foreground">
          Loading Console Module...
        </div>
      }
    >
      {tab === "overview" && <OverviewPage />}
      {tab === "credentials" && <Dashboard onLogout={onLogout} embedded />}
      {tab === "keys" && <ClientKeysPage />}
      {tab === "groups" && <GroupsPage />}
      {tab === "traces" && <TraceLogPage />}
      {tab === "settings" && <SettingsPage />}
    </Suspense>
  );
}

export default App;
