import { useState, useEffect, lazy, Suspense } from "react";
import { storage } from "@/lib/storage";
import { LoginPage } from "@/components/login-page";
import { Toaster } from "@/components/ui/sonner";
import { ConfirmProvider } from "@/components/ui/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Activity, KeyRound, Server, LogOut, Moon, Sun, ScrollText } from "lucide-react";
import { TopbarTools } from "@/components/topbar-tools";

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

type Tab = "overview" | "credentials" | "keys" | "traces";

const TABS: { key: Tab; label: string; icon: React.ReactNode }[] = [
  {
    key: "overview",
    label: "概览",
    icon: <Activity className="h-3.5 w-3.5" />,
  },
  {
    key: "credentials",
    label: "凭据管理",
    icon: <Server className="h-3.5 w-3.5" />,
  },
  {
    key: "keys",
    label: "客户端 Key",
    icon: <KeyRound className="h-3.5 w-3.5" />,
  },
  {
    key: "traces",
    label: "请求日志",
    icon: <ScrollText className="h-3.5 w-3.5" />,
  },
];

function readTabFromHash(): Tab {
  const h = window.location.hash.replace(/^#\/?/, "");
  if (h === "credentials" || h === "keys" || h === "overview" || h === "traces")
    return h;
  return "overview";
}

function App() {
  const [isLoggedIn, setIsLoggedIn] = useState(false);
  const [tab, setTab] = useState<Tab>(readTabFromHash);
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== "undefined") {
      return document.documentElement.classList.contains("dark");
    }
    return false;
  });

  useEffect(() => {
    if (storage.getApiKey()) setIsLoggedIn(true);
  }, []);

  useEffect(() => {
    const onHash = () => setTab(readTabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const switchTab = (next: Tab) => {
    window.location.hash = `#/${next}`;
    setTab(next);
  };

  const handleLogin = () => setIsLoggedIn(true);
  const handleLogout = () => {
    storage.removeApiKey();
    setIsLoggedIn(false);
  };
  const toggleDarkMode = () => {
    setDarkMode((v) => !v);
    document.documentElement.classList.toggle("dark");
  };

  if (!isLoggedIn) {
    return (
      <>
        <LoginPage onLogin={handleLogin} />
        <Toaster position="top-center" />
      </>
    );
  }

  return (
    <ConfirmProvider>
      {/* 顶部 Tab 导航 */}
      <header className="sticky top-0 z-50 w-full glass">
        <div className="mx-auto max-w-[1400px] flex h-16 items-center justify-between px-4 md:px-8">
          <div className="flex items-center gap-3">
            <img
              src="/admin/kirors.png"
              alt="Kiro"
              className="h-9 w-9 object-contain"
              draggable={false}
            />
            <span className="font-semibold tracking-tight">Kiro Admin</span>
            <div className="ml-4 hidden sm:flex items-center gap-1 rounded-full border border-border/60 p-0.5">
              {TABS.map((t) => (
                <Button
                  key={t.key}
                  size="sm"
                  variant={tab === t.key ? "default" : "ghost"}
                  className="h-7 rounded-full px-3 text-xs"
                  onClick={() => switchTab(t.key)}
                >
                  {t.icon}
                  {t.label}
                </Button>
              ))}
            </div>
          </div>
          <div className="flex items-center gap-1">
            <TopbarTools />
            <span className="mx-1 h-5 w-px bg-border/70" />
            <Button variant="ghost" size="icon" asChild title="GitHub 仓库">
              <a
                href="https://github.com/ZyphrZero/kiro.rs"
                target="_blank"
                rel="noopener noreferrer"
                aria-label="GitHub 仓库"
              >
                <GithubIcon className="h-4 w-4" />
              </a>
            </Button>
            <Button
              variant="ghost"
              size="icon"
              onClick={toggleDarkMode}
              title="切换主题"
            >
              {darkMode ? (
                <Sun className="h-4 w-4" />
              ) : (
                <Moon className="h-4 w-4" />
              )}
            </Button>
            <Button
              variant="ghost"
              size="icon"
              onClick={handleLogout}
              title="退出登录"
            >
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
        </div>
        {/* 移动端 Tab 行 */}
        <div className="sm:hidden mx-auto max-w-[1400px] flex items-center gap-1 px-4 pb-2">
          {TABS.map((t) => (
            <Button
              key={t.key}
              size="sm"
              variant={tab === t.key ? "default" : "ghost"}
              className="h-7 rounded-full px-3 text-xs flex-1"
              onClick={() => switchTab(t.key)}
            >
              {t.icon}
              {t.label}
            </Button>
          ))}
        </div>
      </header>

      <main className="mx-auto max-w-[1400px] px-4 md:px-8 py-8">
        <Suspense
          fallback={
            <div className="text-sm text-muted-foreground">加载中…</div>
          }
        >
          {tab === "overview" && <OverviewPage />}
          {tab === "credentials" && (
            <Dashboard onLogout={handleLogout} embedded />
          )}
          {tab === "keys" && <ClientKeysPage />}
          {tab === "traces" && <TraceLogPage />}
        </Suspense>
      </main>

      <Toaster position="top-center" />
    </ConfirmProvider>
  );
}

export default App;
