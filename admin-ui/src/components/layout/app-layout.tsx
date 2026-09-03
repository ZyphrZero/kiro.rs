import React, { useState, useEffect } from "react";
import {
  Activity,
  KeyRound,
  Server,
  LogOut,
  ScrollText,
  FolderTree,
  SlidersHorizontal,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  Menu,
  X,
  Gauge,
  Tags,
  Cpu,
  Globe,
  PackageOpen,
  ShieldCheck,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { TopbarTools } from "@/components/topbar-tools";
import { ThemePicker } from "@/components/theme-picker";
import { useConfirm } from "@/components/ui/confirm-dialog";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import type { ThemeId, ThemeMode, ThemeSelection } from "@/lib/theme";

export type TabKey =
  | "overview"
  | "credentials"
  | "keys"
  | "groups"
  | "traces"
  | "settings";

export interface SubMenuItem {
  key: string;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}

export interface TabItem {
  key: TabKey;
  label: string;
  description: string;
  icon: React.ComponentType<{ className?: string }>;
  children?: readonly SubMenuItem[];
}

export const TABS: readonly TabItem[] = [
  {
    key: "overview",
    label: "仪表概览",
    description: "实时调用监控、Token 消耗分布与上游贡献",
    icon: Activity,
  },
  {
    key: "credentials",
    label: "凭据管理",
    description: "上游提供商凭据集群、配额健康与负载均衡",
    icon: Server,
  },
  {
    key: "keys",
    label: "客户端 Key",
    description: "下游入口密钥、模型配额与速率限制",
    icon: KeyRound,
  },
  {
    key: "groups",
    label: "分组管理",
    description: "组织架构隔离、路由组与流量分配策略",
    icon: FolderTree,
  },
  {
    key: "traces",
    label: "请求日志",
    description: "端到端审计追踪、耗时分析与异常排查",
    icon: ScrollText,
  },
  {
    key: "settings",
    label: "系统设置",
    description: "自动自愈、调度熔断、日志治理与元数据规范",
    icon: SlidersHorizontal,
    children: [
      { key: "dispatch", label: "调度策略", icon: Gauge },
      { key: "metadata", label: "凭据字段", icon: Tags },
      { key: "models", label: "模型配置", icon: Cpu },
      { key: "network", label: "网络代理", icon: Globe },
      { key: "log", label: "日志治理", icon: ScrollText },
      { key: "system", label: "系统更新", icon: PackageOpen },
      { key: "security", label: "安全防护", icon: ShieldCheck },
    ],
  },
] as const;

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

interface AppLayoutProps {
  currentTab: TabKey;
  onSelectTab: (tab: TabKey, subKey?: string) => void;
  theme: ThemeSelection;
  isDarkMode: boolean;
  onSelectPalette: (palette: ThemeId) => void;
  onSelectMode: (mode: ThemeMode) => void;
  onLogout: () => void;
  children: React.ReactNode;
}

const SIDEBAR_COLLAPSED_KEY = "kiro_sidebar_collapsed";

export function AppLayout({
  currentTab,
  onSelectTab,
  theme,
  isDarkMode,
  onSelectPalette,
  onSelectMode,
  onLogout,
  children,
}: AppLayoutProps) {
  const [collapsed, setCollapsed] = useState(() => {
    try {
      return localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === "true";
    } catch {
      return false;
    }
  });
  const [mobileOpen, setMobileOpen] = useState(false);

  useEffect(() => {
    try {
      localStorage.setItem(SIDEBAR_COLLAPSED_KEY, String(collapsed));
    } catch {
      // ignore
    }
  }, [collapsed]);

  const [currentSection, setCurrentSection] = useState(() => {
    const hash = window.location.hash;
    const qIndex = hash.indexOf("?");
    if (qIndex >= 0) {
      const p = new URLSearchParams(hash.slice(qIndex + 1));
      return p.get("s") || "dispatch";
    }
    return "dispatch";
  });

  const [settingsExpanded, setSettingsExpanded] = useState(true);

  useEffect(() => {
    const onHash = () => {
      const hash = window.location.hash;
      const qIndex = hash.indexOf("?");
      if (qIndex >= 0) {
        const p = new URLSearchParams(hash.slice(qIndex + 1));
        setCurrentSection(p.get("s") || "dispatch");
      } else {
        setCurrentSection("dispatch");
      }
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const activeTabMeta = TABS.find((t) => t.key === currentTab) ?? TABS[0];

  return (
    <div className="h-screen w-screen overflow-hidden bg-background text-foreground flex">
      {/* 移动端遮罩层 */}
      {mobileOpen && (
        <div
          className="fixed inset-0 z-40 bg-black/60 lg:hidden"
          onClick={() => setMobileOpen(false)}
        />
      )}

      {/* 左侧侧边栏 (Sidebar) - 固定视口高度，绝不跟随页面滚动 */}
      <aside
        className={`fixed inset-y-0 left-0 z-50 flex h-screen flex-col shrink-0 border-r border-sidebar-border bg-sidebar transition-[width,transform] duration-200 ease-out lg:relative lg:translate-x-0 ${
          collapsed ? "w-[68px]" : "w-60"
        } ${
          mobileOpen ? "translate-x-0 w-64 shadow-2xl" : "-translate-x-full"
        }`}
      >
        {/* 顶部 Logo 品牌区 */}
        <div className="flex h-14 items-center justify-between border-b border-sidebar-border px-3.5">
          <div className="flex items-center gap-2.5 overflow-hidden">
            <img
              src="/admin/kirors.png"
              alt="Kiro"
              className="size-7 shrink-0 object-contain"
              draggable={false}
            />
            {(!collapsed || mobileOpen) && (
              <span className="font-semibold tracking-tight text-sm truncate">
                Kiro Gateway
              </span>
            )}
          </div>
          {mobileOpen && (
            <Button
              variant="ghost"
              size="icon"
              className="lg:hidden h-7 w-7"
              onClick={() => setMobileOpen(false)}
            >
              <X className="size-4" />
            </Button>
          )}
        </div>

        {/* 导航菜单列表 */}
        <div className="flex-1 overflow-y-auto px-2.5 py-3 space-y-1">
          {TABS.map((item) => {
            const active = currentTab === item.key;
            const Icon = item.icon;
            const hasChildren = Boolean(item.children && item.children.length > 0);

            const handleItemClick = () => {
              if (hasChildren) {
                if (currentTab === item.key) {
                  setSettingsExpanded(!settingsExpanded);
                } else {
                  setSettingsExpanded(true);
                  onSelectTab(item.key);
                }
              } else {
                onSelectTab(item.key);
              }
              if (!hasChildren) setMobileOpen(false);
            };

            const buttonNode = (
              <button
                key={item.key}
                type="button"
                onClick={handleItemClick}
                className={`group relative flex w-full items-center gap-3 rounded-md px-2.5 py-2 text-xs font-medium transition-colors transition-transform duration-100 ease-out active:scale-[0.97] ${
                  active
                    ? "bg-primary/10 text-primary font-semibold dark:bg-primary/15"
                    : "text-muted-foreground hover:bg-accent hover:text-foreground"
                } ${collapsed && !mobileOpen ? "justify-center px-0" : ""}`}
              >
                {/* 激活指示线 */}
                {active && !hasChildren && (
                  <span className="absolute left-0 top-1.5 bottom-1.5 w-1 rounded-r bg-primary" />
                )}
                <Icon
                  className={`size-4 shrink-0 transition-colors ${
                    active ? "text-primary" : "text-muted-foreground group-hover:text-foreground"
                  }`}
                />
                {(!collapsed || mobileOpen) && (
                  <>
                    <span className="truncate flex-1 text-left">{item.label}</span>
                    {hasChildren && (
                      <ChevronDown
                        className={`size-3.5 shrink-0 text-muted-foreground transition-transform duration-200 ${
                          settingsExpanded ? "rotate-180 text-foreground" : ""
                        }`}
                      />
                    )}
                  </>
                )}
              </button>
            );

            if (collapsed && !mobileOpen) {
              if (hasChildren && item.children) {
                return (
                  <Tooltip key={item.key} delayDuration={100}>
                    <TooltipTrigger asChild>{buttonNode}</TooltipTrigger>
                    <TooltipContent side="right" className="p-1.5 min-w-[140px] space-y-0.5">
                      <div className="px-2 py-1 text-[11px] font-semibold text-foreground border-b border-border/50">
                        {item.label}
                      </div>
                      {item.children.map((child) => {
                        const isChildActive = currentTab === item.key && currentSection === child.key;
                        const ChildIcon = child.icon;
                        return (
                          <button
                            key={child.key}
                            type="button"
                            onClick={() => {
                              onSelectTab(item.key, child.key);
                              setCurrentSection(child.key);
                            }}
                            className={`flex w-full items-center gap-2 rounded px-2 py-1 text-xs text-left transition-colors ${
                              isChildActive
                                ? "bg-primary/15 text-primary font-medium"
                                : "text-muted-foreground hover:bg-accent hover:text-foreground"
                            }`}
                          >
                            <ChildIcon className="size-3 shrink-0" />
                            <span className="truncate">{child.label}</span>
                          </button>
                        );
                      })}
                    </TooltipContent>
                  </Tooltip>
                );
              }

              return (
                <Tooltip key={item.key} delayDuration={100}>
                  <TooltipTrigger asChild>{buttonNode}</TooltipTrigger>
                  <TooltipContent side="right">
                    <div className="font-medium">{item.label}</div>
                    <div className="text-[11px] text-muted-foreground">{item.description}</div>
                  </TooltipContent>
                </Tooltip>
              );
            }

            return (
              <div key={item.key}>
                {buttonNode}
                {hasChildren && settingsExpanded && item.children && (
                  <div className="ml-5 pl-2.5 border-l border-border/70 space-y-0.5 my-1">
                    {item.children.map((child) => {
                      const isChildActive = currentTab === item.key && currentSection === child.key;
                      const ChildIcon = child.icon;
                      return (
                        <button
                          key={child.key}
                          type="button"
                          onClick={() => {
                            onSelectTab(item.key, child.key);
                            setCurrentSection(child.key);
                            setMobileOpen(false);
                          }}
                          className={`group flex w-full items-center gap-2 rounded px-2 py-1.5 text-[11.5px] font-medium transition-colors active:scale-[0.98] ${
                            isChildActive
                              ? "bg-primary/10 text-primary font-semibold dark:bg-primary/15"
                              : "text-muted-foreground hover:bg-accent hover:text-foreground"
                          }`}
                        >
                          <ChildIcon
                            className={`size-3 shrink-0 transition-colors ${
                              isChildActive ? "text-primary" : "text-muted-foreground group-hover:text-foreground"
                            }`}
                          />
                          <span className="truncate">{child.label}</span>
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })}
        </div>

        {/* 侧边栏底部折叠开关 */}
        <div className="border-t border-sidebar-border p-2">
          <div className="flex items-center justify-between gap-1">
            <Button
              variant="ghost"
              size="sm"
              className={`h-8 text-xs text-muted-foreground hover:text-foreground ${
                collapsed && !mobileOpen ? "w-full justify-center px-0" : "flex-1 justify-start"
              }`}
              onClick={() => setCollapsed(!collapsed)}
              title={collapsed ? "展开侧边栏" : "折叠侧边栏"}
            >
              {collapsed && !mobileOpen ? (
                <ChevronRight className="size-4" />
              ) : (
                <>
                  <ChevronLeft className="size-4 mr-1.5" />
                  <span>收起侧栏</span>
                </>
              )}
            </Button>
          </div>
        </div>
      </aside>

      {/* 右侧主工作区容器 - 视口锁定，仅内容区域平滑滚动 */}
      <div className="flex-1 flex flex-col h-screen min-w-0 overflow-hidden">
        {/* 顶部 Header 控制栏 - 固定在顶部 */}
        <header className="h-14 shrink-0 flex items-center justify-between border-b border-border bg-card px-4 sm:px-6 z-20">
          {/* 左侧：移动端汉堡菜单与页面标题 */}
          <div className="flex items-center gap-3 min-w-0">
            <Button
              variant="ghost"
              size="icon"
              className="lg:hidden h-8 w-8 shrink-0 text-muted-foreground"
              onClick={() => setMobileOpen(true)}
              aria-label="打开菜单"
            >
              <Menu className="size-4" />
            </Button>
            <div className="flex items-center gap-2 min-w-0">
              <span className="font-semibold text-foreground text-sm tracking-tight truncate">
                {currentTab === "settings" && activeTabMeta.children
                  ? `${activeTabMeta.label} · ${activeTabMeta.children.find((c) => c.key === currentSection)?.label || "调度策略"}`
                  : activeTabMeta.label}
              </span>
            </div>
          </div>

          {/* 右侧全局操作栏 */}
          <div className="flex items-center gap-2 shrink-0">
            <TopbarTools />
            <span className="h-4 w-px bg-border mx-0.5 hidden sm:inline-block" />
            <Button
              variant="ghost"
              size="icon"
              asChild
              title="GitHub 仓库"
              className="h-8 w-8 text-muted-foreground hover:text-foreground hidden sm:inline-flex"
            >
              <a
                href="https://github.com/ZyphrZero/kiro.rs"
                target="_blank"
                rel="noopener noreferrer"
                aria-label="GitHub 仓库"
              >
                <GithubIcon className="size-3.5" />
              </a>
            </Button>
            <ThemePicker
              theme={theme}
              isDarkMode={isDarkMode}
              onSelectPalette={onSelectPalette}
              onSelectMode={onSelectMode}
            />
            <LogoutButton onLogout={onLogout} />
          </div>
        </header>

        {/* 主数据渲染区 - 独立垂直滚动 */}
        <main className="flex-1 overflow-y-auto overflow-x-hidden p-4 sm:p-6 lg:p-7">
          <div className="max-w-[1440px] w-full mx-auto">
            {children}
          </div>
        </main>
      </div>
    </div>
  );
}

function LogoutButton({ onLogout }: { onLogout: () => void }) {
  const confirm = useConfirm();

  const handleLogout = async () => {
    const confirmed = await confirm({
      title: "退出登录？",
      description: "退出后需要重新输入管理密钥方可进入控制台。",
      confirmText: "退出登录",
      destructive: true,
    });
    if (confirmed) onLogout();
  };

  return (
    <Button
      variant="ghost"
      size="icon"
      className="h-8 w-8 text-muted-foreground hover:text-destructive hover:bg-destructive/10"
      onClick={handleLogout}
      title="退出登录"
    >
      <LogOut className="size-3.5" />
    </Button>
  );
}
