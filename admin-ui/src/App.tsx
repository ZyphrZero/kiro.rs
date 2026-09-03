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
import { ConfirmProvider, useConfirm } from "@/components/ui/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Activity, KeyRound, Server, LogOut, ScrollText, FolderTree, SlidersHorizontal } from "lucide-react";
import { TopbarTools } from "@/components/topbar-tools";
import { ThemePicker } from "@/components/theme-picker";
import { TooltipProvider } from "@/components/ui/tooltip";
import { tabFromHash } from "@/hooks/use-url-state";

function GithubIcon({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      className={className}
      aria-hidden="true"
    >
      <path d="M12 .5C5.65.5.5 5.65.5 12.02c0 5.1 3.29 9.42 7.86 10.95.58.11.79-.25.79-.55 0-.27-.01-.99-.02-1.95-3.2.7-3.87-1.54-3.87-1.54-.52-1.32-1.27-1.67-1.27-1.67-1.04-.71.08-.7.08-.7 1.15.08 1.76 1.18 1.76 1.18 1.02 1.76 2.69 1.25 3.34.95.1-.74.4-1.25.72-1.54-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.18-3.09-.12-.29-.51-1.46.11-3.05 0 0 .96-.31 3.16 1.18a10.95 10.95 0 0 1 5.75 0c2.2-1.49 3.16-1.18 3.16-1.18.62 1.59.23 2.76.12 3.05.74.8 1.18 1.83 1.18 3.09 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14 0 1.55-.01 2.79-.01 3.17 0 .31.21.67.8.55A11.51 11.51 0 0 0 23.5 12.02C23.5 5.65 18.35.5 12 .5Z" />
    </svg>
  );
}

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

type Tab =
  | "overview"
  | "credentials"
  | "keys"
  | "groups"
  | "traces"
  | "settings";

const TABS: {
  key: Tab;
  label: string;
  mobileLabel: string;
  icon: React.ReactNode;
}[] = [
  {
    key: "overview",
    label: "概览",
    mobileLabel: "概览",
    icon: <Activity className="h-3.5 w-3.5" />,
  },
  {
    key: "credentials",
    label: "凭据管理",
    mobileLabel: "凭据",
    icon: <Server className="h-3.5 w-3.5" />,
  },
  {
    key: "keys",
    label: "客户端 Key",
    mobileLabel: "Key",
    icon: <KeyRound className="h-3.5 w-3.5" />,
  },
  {
    key: "groups",
    label: "分组管理",
    mobileLabel: "分组",
    icon: <FolderTree className="h-3.5 w-3.5" />,
  },
  {
    key: "traces",
    label: "请求日志",
    mobileLabel: "日志",
    icon: <ScrollText className="h-3.5 w-3.5" />,
  },
  {
    key: "settings",
    label: "设置",
    mobileLabel: "设置",
    icon: <SlidersHorizontal className="h-3.5 w-3.5" />,
  },
];

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
  onSwitchTab: (next: Tab) => void;
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

  const switchTab = (next: Tab) => {
    window.location.hash = `#/${next}`;
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
        <AppHeader
          theme={theme}
          isDarkMode={isDarkMode}
          tab={tab}
          onLogout={onLogout}
          onSwitchTab={onSwitchTab}
          onSelectPalette={onSelectPalette}
          onSelectMode={onSelectMode}
        />
        <AppMain tab={tab} onLogout={onLogout} />
        <Toaster position="top-center" />
      </ConfirmProvider>
    </TooltipProvider>
  );
}

function AppHeader({
  theme,
  isDarkMode,
  onLogout,
  onSwitchTab,
  onSelectPalette,
  onSelectMode,
  tab,
}: AppHeaderProps) {
  return (
    <header className="sticky top-0 z-50 w-full glass">
      <div className="mx-auto flex h-14 max-w-[1400px] min-w-0 items-center gap-2 px-3 sm:h-16 sm:px-4 xl:px-8">
        <HeaderBrand tab={tab} onSwitchTab={onSwitchTab} />
        <HeaderActions
          theme={theme}
          isDarkMode={isDarkMode}
          onLogout={onLogout}
          onSelectPalette={onSelectPalette}
          onSelectMode={onSelectMode}
        />
      </div>
      <MobileTabs tab={tab} onSwitchTab={onSwitchTab} />
    </header>
  );
}

function HeaderBrand({
  onSwitchTab,
  tab,
}: {
  onSwitchTab: (next: Tab) => void;
  tab: Tab;
}) {
  return (
    <div className="flex min-w-0 flex-1 items-center gap-2 xl:gap-3">
      <img
        src="/admin/kirors.png"
        alt="Kiro"
        className="size-8 shrink-0 object-contain xl:size-9"
        draggable={false}
      />
      <span className="min-w-0 truncate text-sm font-semibold tracking-tight min-[380px]:text-base">
        Kiro Admin
      </span>
      <DesktopTabs tab={tab} onSwitchTab={onSwitchTab} />
    </div>
  );
}

function DesktopTabs({
  onSwitchTab,
  tab,
}: {
  onSwitchTab: (next: Tab) => void;
  tab: Tab;
}) {
  return (
    <div className="ml-4 hidden items-center gap-1 rounded-full border border-border/60 p-0.5 xl:flex">
      {TABS.map((t) => (
        <TabButton
          key={t.key}
          active={tab === t.key}
          tab={t}
          onSwitchTab={onSwitchTab}
        />
      ))}
    </div>
  );
}

function HeaderActions({
  theme,
  isDarkMode,
  onLogout,
  onSelectPalette,
  onSelectMode,
}: {
  theme: ThemeSelection;
  isDarkMode: boolean;
  onLogout: () => void;
  onSelectPalette: (palette: ThemeId) => void;
  onSelectMode: (mode: ThemeMode) => void;
}) {
  const confirm = useConfirm();

  const handleLogout = async () => {
    const confirmed = await confirm({
      title: "退出登录？",
      description: "退出后需要重新输入管理面板密钥才能继续使用。",
      confirmText: "退出登录",
      destructive: true,
    });
    if (confirmed) onLogout();
  };

  return (
    <div className="flex shrink-0 items-center gap-1">
      <div className="xl:hidden">
        <TopbarTools compact />
      </div>
      <div className="hidden items-center gap-1 xl:flex">
        <TopbarTools />
      </div>
      <span className="mx-1 hidden h-5 w-px bg-border/70 xl:inline-block" />
      <GithubButton />
      <ThemePicker
        theme={theme}
        isDarkMode={isDarkMode}
        onSelectPalette={onSelectPalette}
        onSelectMode={onSelectMode}
      />
      <Button variant="ghost" size="icon" onClick={handleLogout} title="退出登录">
        <LogOut className="h-4 w-4" />
      </Button>
    </div>
  );
}

function GithubButton() {
  return (
    <Button
      variant="ghost"
      size="icon"
      asChild
      title="GitHub 仓库"
      className="hidden xl:inline-flex"
    >
      <a
        href="https://github.com/ZyphrZero/kiro.rs"
        target="_blank"
        rel="noopener noreferrer"
        aria-label="GitHub 仓库"
      >
        <GithubIcon className="h-4 w-4" />
      </a>
    </Button>
  );
}

function MobileTabs({
  onSwitchTab,
  tab,
}: {
  onSwitchTab: (next: Tab) => void;
  tab: Tab;
}) {
  return (
    <div className="mx-auto grid w-full max-w-[1400px] grid-cols-6 items-center gap-0.5 overflow-hidden px-2 pb-2 xl:hidden [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
      {TABS.map((t) => (
        <TabButton
          key={t.key}
          active={tab === t.key}
          mobile
          tab={t}
          onSwitchTab={onSwitchTab}
        />
      ))}
    </div>
  );
}

function TabButton({
  active,
  mobile = false,
  onSwitchTab,
  tab,
}: {
  active: boolean;
  mobile?: boolean;
  onSwitchTab: (next: Tab) => void;
  tab: (typeof TABS)[number];
}) {
  const className = mobile
    ? "h-8 w-full min-w-0 overflow-hidden rounded-full px-0.5 text-[10px] min-[360px]:px-1 min-[360px]:text-[11px] min-[390px]:px-1.5 min-[390px]:text-xs md:w-auto md:min-w-0 md:px-3"
    : "h-7 rounded-full px-3 text-xs";
  const label = mobile ? tab.mobileLabel : tab.label;

  return (
    <Button
      size="sm"
      variant={active ? "default" : "ghost"}
      className={className}
      onClick={() => onSwitchTab(tab.key)}
    >
      {tab.icon}
      <span className={mobile ? "min-w-0 truncate" : undefined}>
        {label}
      </span>
    </Button>
  );
}

function AppMain({ onLogout, tab }: { onLogout: () => void; tab: Tab }) {
  return (
    <main className="mx-auto max-w-[1400px] px-4 md:px-8 py-8">
      <Suspense fallback={<div className="text-sm text-muted-foreground">加载中…</div>}>
        {tab === "overview" && <OverviewPage />}
        {tab === "credentials" && <Dashboard onLogout={onLogout} embedded />}
        {tab === "keys" && <ClientKeysPage />}
        {tab === "groups" && <GroupsPage />}
        {tab === "traces" && <TraceLogPage />}
        {tab === "settings" && <SettingsPage />}
      </Suspense>
    </main>
  );
}

export default App;
