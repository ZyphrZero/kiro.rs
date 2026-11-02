import { useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  ShieldAlert,
  ShieldX,
  ShieldCheck,
  ZapOff,
  Boxes,
  ArrowRight,
  Info,
  Check,
  SlidersHorizontal,
  ExternalLink,
} from "lucide-react";
import {
  auditSuspendedCredentials,
  type AuditSuspendedResult,
} from "@/api/credentials";
import { useSelfHealConfig } from "@/hooks/use-credentials";
import type { CredentialStatusItem } from "@/types/api";
import { extractErrorMessage, cn } from "@/lib/utils";
import { toast } from "sonner";

// 跳转到设置页的调度分区中的「账号封禁治理」配置模块
export function navigateToBanSettings() {
  const targetHash = "#/settings?s=dispatch";
  if (window.location.hash !== targetHash) {
    window.location.hash = targetHash;
    window.dispatchEvent(new Event("hashchange"));
  }
  // 等待设置页渲染后平滑滚动到账号封禁治理卡片并给予视觉聚焦
  let attempts = 0;
  const maxAttempts = 25;
  const checkAndScroll = () => {
    const el = document.getElementById("dispatch-ban-detect");
    if (el) {
      const main = document.querySelector("main");
      if (main) {
        const mainRect = main.getBoundingClientRect();
        const elRect = el.getBoundingClientRect();
        const targetScrollTop = main.scrollTop + (elRect.top - mainRect.top) - 20;
        main.scrollTo({
          top: Math.max(0, targetScrollTop),
          behavior: "smooth",
        });
      } else {
        el.scrollIntoView({ behavior: "smooth", block: "start" });
      }
      const targetCard =
        (el.matches('section, [class*="rounded-xl"], [class*="rounded-lg"]')
          ? el
          : el.querySelector<HTMLElement>('section, [class*="rounded-xl"], [class*="rounded-lg"]')) || el;
      document.querySelectorAll('.target-card-highlight').forEach((node) => {
        node.classList.remove('target-card-highlight');
      });
      void targetCard.offsetWidth;
      targetCard.classList.add('target-card-highlight');
      setTimeout(() => {
        targetCard.classList.remove('target-card-highlight');
      }, 2400);
    } else if (attempts < maxAttempts) {
      attempts++;
      setTimeout(checkAndScroll, 50);
    }
  };
  setTimeout(checkAndScroll, 60);
}

export interface BanRulesTableProps {
  customKeywords?: string[];
  isDetectionEnabled?: boolean;
  onNavigateToSettings?: () => void;
  className?: string;
}

export function BanRulesTable({
  customKeywords = [],
  isDetectionEnabled = true,
  onNavigateToSettings,
  className,
}: BanRulesTableProps) {
  return (
    <div className={cn("rounded-lg border border-border/70 bg-muted/20 p-3.5 space-y-2.5", className)}>
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5 font-semibold text-foreground text-xs">
          <Info className="h-3.5 w-3.5 text-muted-foreground" />
          特征匹配与治理规则表
        </div>
        {onNavigateToSettings && (
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-6 px-2 text-[11px] gap-1 text-muted-foreground hover:text-foreground border-border/80 hover:bg-accent/60 cursor-pointer shadow-none"
            onClick={onNavigateToSettings}
          >
            <SlidersHorizontal className="h-3 w-3" />
            <span>管理特征规则</span>
            <ExternalLink className="h-2.5 w-2.5 opacity-60" />
          </Button>
        )}
      </div>

      <div className="rounded-md border border-border/70 overflow-hidden bg-background">
        <table className="w-full text-left text-xs border-collapse">
          <thead>
            <tr className="border-b border-border/60 bg-muted/40 text-muted-foreground text-[10.5px]">
              <th className="py-2 px-2.5 font-medium w-16">来源</th>
              <th className="py-2 px-2.5 font-medium">匹配特征 / 关键词</th>
              <th className="py-2 px-2.5 font-medium w-28 text-right">匹配模式</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border/40 text-[11px]">
            {/* 内置规则 1 */}
            <tr className="hover:bg-muted/20">
              <td className="py-2 px-2.5">
                <Badge variant="outline" className="border-rose-500/30 text-rose-600 dark:text-rose-400 bg-rose-500/10 text-[9.5px] px-1 py-0 font-medium">
                  内置
                </Badge>
              </td>
              <td className="py-2 px-2.5">
                <code className="font-mono text-[10.5px] text-rose-600 dark:text-rose-400 font-semibold truncate block max-w-[220px]">
                  reason = "TEMPORARILY_SUSPENDED"
                </code>
              </td>
              <td className="py-2 px-2.5 text-right text-[10.5px] text-muted-foreground">
                结构化字段精确
              </td>
            </tr>

            {/* 内置规则 2 */}
            <tr className="hover:bg-muted/20">
              <td className="py-2 px-2.5">
                <Badge variant="outline" className="border-rose-500/30 text-rose-600 dark:text-rose-400 bg-rose-500/10 text-[9.5px] px-1 py-0 font-medium">
                  内置
                </Badge>
              </td>
              <td className="py-2 px-2.5 font-mono text-[10.5px] text-foreground">
                "suspended" + ("locked your account" | "locked it")
              </td>
              <td className="py-2 px-2.5 text-right text-[10.5px] text-muted-foreground">
                双短语交叉组合
              </td>
            </tr>

            {/* 自定义关键词 */}
            {customKeywords.length > 0 ? (
              customKeywords.map((kw, idx) => (
                <tr key={idx} className="hover:bg-muted/20">
                  <td className="py-2 px-2.5">
                    <Badge variant="secondary" className="text-[9.5px] px-1 py-0 font-normal">
                      自定义
                    </Badge>
                  </td>
                  <td className="py-2 px-2.5 font-mono text-[10.5px] text-foreground font-medium truncate max-w-[220px]">
                    "{kw}"
                  </td>
                  <td className="py-2 px-2.5 text-right text-[10.5px] text-muted-foreground">
                    响应文本子串
                  </td>
                </tr>
              ))
            ) : (
              <tr className="hover:bg-muted/20">
                <td className="py-2 px-2.5">
                  <Badge variant="secondary" className="text-[9.5px] px-1 py-0 font-normal text-muted-foreground">
                    自定义
                  </Badge>
                </td>
                <td className="py-2 px-2.5 text-muted-foreground text-[10.5px]" colSpan={2}>
                  暂无自定义关键词，
                  {onNavigateToSettings ? (
                    <button
                      type="button"
                      onClick={onNavigateToSettings}
                      className="text-primary hover:underline font-medium cursor-pointer ml-1 inline-flex items-center"
                    >
                      前往设置添加 <ExternalLink className="h-2.5 w-2.5 ml-0.5" />
                    </button>
                  ) : (
                    <span className="ml-1">可在设置页中添加</span>
                  )}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      <div className="text-[11px] text-muted-foreground leading-snug">
        治理动作：命中的账号将被自动标记为「账号封禁」并隔离，绝不参与池全灭时的自愈复活。
      </div>

      {!isDetectionEnabled && (
        <div className="flex items-center justify-between text-[11px] bg-amber-500/10 border border-amber-500/25 text-amber-700 dark:text-amber-400 px-2.5 py-1.5 rounded-md mt-1">
          <span>⚠️ 提示：系统设置中「封号识别」当前已关闭</span>
          {onNavigateToSettings && (
            <button
              type="button"
              onClick={onNavigateToSettings}
              className="font-medium underline hover:text-amber-800 dark:hover:text-amber-300 ml-2 shrink-0 cursor-pointer"
            >
              前往开启
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export interface AuditSuspendedDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  targetIds?: number[];
  stateCounts: {
    total: number;
    dead: number;
    suspended: number;
  };
  credentials?: CredentialStatusItem[];
  onSuccess?: (result: AuditSuspendedResult) => void;
  onViewSuspended?: () => void;
}

export function AuditSuspendedDialog({
  open,
  onOpenChange,
  targetIds,
  stateCounts,
  credentials = [],
  onSuccess,
  onViewSuspended,
}: AuditSuspendedDialogProps) {
  const isTargeted = Boolean(targetIds && targetIds.length > 0);

  // 扫描范围：'disabled' = 仅已禁用 (默认)；'all' = 全量扫描；'selected' = 仅选中
  const [scope, setScope] = useState<"disabled" | "all" | "selected">(
    isTargeted ? "selected" : "disabled",
  );
  const [scanning, setScanning] = useState(false);
  const [result, setResult] = useState<AuditSuspendedResult | null>(null);

  const handleStartScan = async () => {
    setScanning(true);
    setResult(null);
    try {
      const res = await auditSuspendedCredentials({
        onlyDisabled: scope === "disabled",
        credentialIds: scope === "selected" ? targetIds : undefined,
      });
      setResult(res);
      onSuccess?.(res);
      if (res.updatedCount > 0) {
        toast.success(
          `排查完成：扫描 ${res.scannedCount} 个凭据，新标记 ${res.updatedCount} 个封禁账号`,
        );
      } else if (res.bannedCount > 0) {
        toast.info(
          `排查完成：扫描 ${res.scannedCount} 个凭据，已是封禁状态 ${res.bannedCount} 个，无新增`,
        );
      } else {
        toast.info(
          `排查完成：扫描 ${res.scannedCount} 个凭据，未发现封号特征`,
        );
      }
    } catch (err) {
      toast.error("封号排查失败: " + extractErrorMessage(err));
    } finally {
      setScanning(false);
    }
  };

  const handleReset = () => {
    setResult(null);
    setScanning(false);
  };

  const handleClose = () => {
    onOpenChange(false);
    // 延时重置状态，避免动画卡顿
    setTimeout(() => {
      setResult(null);
      setScanning(false);
    }, 200);
  };

  // 自愈与封禁治理配置
  const { data: selfHealConfig } = useSelfHealConfig();
  const customKeywords = selfHealConfig?.suspendedBanKeywords ?? [];
  const isDetectionEnabled = selfHealConfig?.suspendedDetectionEnabled ?? true;

  const handleNavigateToBanSettings = () => {
    handleClose();
    navigateToBanSettings();
  };

  // 根据 ID 查找邮箱或备注
  const findEmail = (id: number) => {
    const found = credentials.find((c) => c.id === id);
    return found?.email || found?.sourceChannel || `凭据 #${id}`;
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-md sm:max-w-lg p-0 gap-0 overflow-hidden">
        {/* Header */}
        <div className="border-b border-border bg-muted/30 px-6 py-5">
          <div className="flex items-start gap-3.5">
            <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-rose-500/10 text-rose-600 dark:text-rose-400 border border-rose-500/20">
              <ShieldAlert className="h-5 w-5" />
            </div>
            <div className="min-w-0 flex-1">
              <DialogTitle className="text-base font-semibold leading-6 text-foreground">
                排查封禁账号
              </DialogTitle>
              <DialogDescription className="mt-1 text-xs text-muted-foreground leading-relaxed">
                扫描历史请求错误日志，精准识别官方封号特征并隔离标记为「账号封禁」，杜绝自愈死循环。
              </DialogDescription>
            </div>
          </div>
        </div>

        {/* Body */}
        <div className="p-6 space-y-5 text-xs max-h-[68vh] overflow-y-auto">
          {scanning ? (
            /* 扫描中动画态 */
            <div className="py-12 flex flex-col items-center justify-center text-center space-y-3">
              <div className="relative">
                <div className="h-12 w-12 rounded-full border-2 border-rose-500/20 border-t-rose-500 animate-spin" />
                <ShieldX className="h-5 w-5 text-rose-500 absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2" />
              </div>
              <div>
                <p className="font-medium text-foreground text-sm">
                  正在扫描日志特征…
                </p>
                <p className="text-muted-foreground mt-1 text-xs">
                  深度匹配 TEMPORARILY_SUSPENDED 状态及自定义关键词
                </p>
              </div>
            </div>
          ) : result ? (
            /* 扫描结果展示态 */
            <div className="space-y-4">
              {/* 指标三栏卡片 */}
              <div className="grid grid-cols-3 gap-2 text-center">
                <div className="rounded-lg border border-border bg-card p-3">
                  <div className="text-[11px] text-muted-foreground">已扫描凭据</div>
                  <div className="mt-1 text-lg font-bold font-mono text-foreground">
                    {result.scannedCount}
                  </div>
                </div>
                <div className="rounded-lg border border-border bg-card p-3">
                  <div className="text-[11px] text-muted-foreground">命中封号特征</div>
                  <div className={cn(
                    "mt-1 text-lg font-bold font-mono",
                    result.bannedCount > 0 ? "text-rose-600 dark:text-rose-400" : "text-foreground"
                  )}>
                    {result.bannedCount}
                  </div>
                </div>
                <div className="rounded-lg border border-border bg-card p-3">
                  <div className="text-[11px] text-muted-foreground">新标记为封禁</div>
                  <div className={cn(
                    "mt-1 text-lg font-bold font-mono",
                    result.updatedCount > 0 ? "text-rose-600 dark:text-rose-400 font-extrabold" : "text-foreground"
                  )}>
                    {result.updatedCount}
                  </div>
                </div>
              </div>

              {/* 命中明细列表 */}
              {result.bannedCount > 0 ? (
                <div className="space-y-2">
                  <div className="flex items-center justify-between text-xs">
                    <span className="font-semibold text-foreground">
                      命中的封禁账号明细
                    </span>
                    <span className="text-muted-foreground text-[11px]">
                      共 {result.bannedCount} 个
                    </span>
                  </div>
                  <div className="rounded-lg border border-border divide-y divide-border max-h-48 overflow-y-auto bg-card">
                    {(result.bannedIds || result.updatedIds).map((id) => {
                      const isNew = result.updatedIds.includes(id);
                      return (
                        <div
                          key={id}
                          className="flex items-center justify-between px-3 py-2 text-xs hover:bg-accent/40"
                        >
                          <div className="flex items-center gap-2 min-w-0">
                            <span className="font-mono text-muted-foreground text-[11px]">
                              #{id}
                            </span>
                            <span className="font-mono truncate max-w-[200px] text-foreground font-medium">
                              {findEmail(id)}
                            </span>
                          </div>
                          <div>
                            {isNew ? (
                              <Badge
                                variant="destructive"
                                className="text-[10px] px-1.5 py-0 h-4 border-rose-500/30 bg-rose-500/15 text-rose-700 dark:text-rose-400 font-semibold"
                              >
                                新标记封禁
                              </Badge>
                            ) : (
                              <Badge
                                variant="secondary"
                                className="text-[10px] px-1.5 py-0 h-4 text-muted-foreground"
                              >
                                此前已封禁
                              </Badge>
                            )}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                  <div className="flex items-center justify-between text-[11px] text-muted-foreground leading-snug pt-1">
                    <span>提示：上述账号已被正式隔离为「账号封禁」，调度器与自愈逻辑将不再放回池中重试。</span>
                    <button
                      type="button"
                      onClick={handleNavigateToBanSettings}
                      className="text-primary hover:underline inline-flex items-center gap-0.5 shrink-0 ml-2 font-medium cursor-pointer"
                    >
                      治理设置
                      <ExternalLink className="h-2.5 w-2.5" />
                    </button>
                  </div>
                </div>
              ) : (
                <div className="rounded-lg border border-emerald-500/30 bg-emerald-500/5 p-4 flex items-center justify-between gap-3">
                  <div className="flex items-center gap-3 min-w-0">
                    <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
                      <ShieldCheck className="h-5 w-5" />
                    </div>
                    <div className="min-w-0">
                      <div className="font-semibold text-emerald-700 dark:text-emerald-400">
                        未发现封号特征
                      </div>
                      <div className="text-muted-foreground text-[11px] mt-0.5 leading-relaxed">
                        本次扫描的历史日志中未匹配到上游封号响应，被排查账号状态正常或仅为临时偶发故障。
                      </div>
                    </div>
                  </div>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="h-7 px-2.5 text-xs text-muted-foreground hover:text-foreground shrink-0 gap-1 border-emerald-500/30 hover:bg-emerald-500/10 cursor-pointer shadow-none"
                    onClick={handleNavigateToBanSettings}
                  >
                    <span>配置规则</span>
                    <ExternalLink className="h-3 w-3" />
                  </Button>
                </div>
              )}
            </div>
          ) : (
            /* 扫描前：选项与规则配置 */
            <div className="space-y-4">
              {/* 扫描范围单选卡片 */}
              <div className="space-y-2">
                <label className="font-semibold text-foreground block">
                  选择排查范围
                </label>
                <div className="grid gap-2">
                  {/* 仅已禁用账号 */}
                  <div
                    onClick={() => setScope("disabled")}
                    className={cn(
                      "flex items-center justify-between p-3 rounded-lg border cursor-pointer transition-all select-none",
                      scope === "disabled"
                        ? "border-rose-500 bg-rose-500/[0.04] shadow-xs"
                        : "border-border hover:bg-muted/40 hover:border-border/80",
                    )}
                  >
                    <div className="flex items-center gap-2.5">
                      <div
                        className={cn(
                          "flex h-7 w-7 items-center justify-center rounded-md",
                          scope === "disabled"
                            ? "bg-rose-500/15 text-rose-600 dark:text-rose-400"
                            : "bg-muted text-muted-foreground",
                        )}
                      >
                        <ZapOff className="h-4 w-4" />
                      </div>
                      <div>
                        <div className="font-medium text-foreground flex items-center gap-1.5">
                          仅排查已禁用账号
                          <span className="text-[10px] rounded bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 px-1 py-0.2 font-medium">
                            推荐
                          </span>
                        </div>
                        <div className="text-[11px] text-muted-foreground mt-0.5">
                          仅扫描目前处于禁用状态的账号，识别封号原因并升格
                        </div>
                      </div>
                    </div>
                    <div className="flex items-center gap-2">
                      <Badge variant="secondary" className="font-mono text-[11px]">
                        {stateCounts.dead} 个
                      </Badge>
                      <div
                        className={cn(
                          "h-4 w-4 rounded-full border flex items-center justify-center transition-colors",
                          scope === "disabled"
                            ? "border-rose-500 bg-rose-500 text-white"
                            : "border-muted-foreground/30",
                        )}
                      >
                        {scope === "disabled" && <Check className="h-2.5 w-2.5 stroke-[3]" />}
                      </div>
                    </div>
                  </div>

                  {/* 全量排查 */}
                  <div
                    onClick={() => setScope("all")}
                    className={cn(
                      "flex items-center justify-between p-3 rounded-lg border cursor-pointer transition-all select-none",
                      scope === "all"
                        ? "border-rose-500 bg-rose-500/[0.04] shadow-xs"
                        : "border-border hover:bg-muted/40 hover:border-border/80",
                    )}
                  >
                    <div className="flex items-center gap-2.5">
                      <div
                        className={cn(
                          "flex h-7 w-7 items-center justify-center rounded-md",
                          scope === "all"
                            ? "bg-rose-500/15 text-rose-600 dark:text-rose-400"
                            : "bg-muted text-muted-foreground",
                        )}
                      >
                        <Boxes className="h-4 w-4" />
                      </div>
                      <div>
                        <div className="font-medium text-foreground">
                          全量排查所有账号
                        </div>
                        <div className="text-[11px] text-muted-foreground mt-0.5">
                          扫描调度池中全部账号（含可用/冷却/超额账号），彻底清查
                        </div>
                      </div>
                    </div>
                    <div className="flex items-center gap-2">
                      <Badge variant="secondary" className="font-mono text-[11px]">
                        {stateCounts.total} 个
                      </Badge>
                      <div
                        className={cn(
                          "h-4 w-4 rounded-full border flex items-center justify-center transition-colors",
                          scope === "all"
                            ? "border-rose-500 bg-rose-500 text-white"
                            : "border-muted-foreground/30",
                        )}
                      >
                        {scope === "all" && <Check className="h-2.5 w-2.5 stroke-[3]" />}
                      </div>
                    </div>
                  </div>

                  {/* 针对选中项 (若有) */}
                  {isTargeted && (
                    <div
                      onClick={() => setScope("selected")}
                      className={cn(
                        "flex items-center justify-between p-3 rounded-lg border cursor-pointer transition-all select-none",
                        scope === "selected"
                          ? "border-rose-500 bg-rose-500/[0.04] shadow-xs"
                          : "border-border hover:bg-muted/40 hover:border-border/80",
                      )}
                    >
                      <div className="flex items-center gap-2.5">
                        <div
                          className={cn(
                            "flex h-7 w-7 items-center justify-center rounded-md",
                            scope === "selected"
                              ? "bg-rose-500/15 text-rose-600 dark:text-rose-400"
                              : "bg-muted text-muted-foreground",
                          )}
                        >
                          <ShieldAlert className="h-4 w-4" />
                        </div>
                        <div>
                          <div className="font-medium text-foreground">
                            仅排查勾选的凭据
                          </div>
                          <div className="text-[11px] text-muted-foreground mt-0.5">
                            仅针对当前列表表格中勾选的凭据进行历史日志特征扫描
                          </div>
                        </div>
                      </div>
                      <div className="flex items-center gap-2">
                        <Badge variant="secondary" className="font-mono text-[11px]">
                          {targetIds?.length} 个
                        </Badge>
                        <div
                          className={cn(
                            "h-4 w-4 rounded-full border flex items-center justify-center transition-colors",
                            scope === "selected"
                              ? "border-rose-500 bg-rose-500 text-white"
                              : "border-muted-foreground/30",
                          )}
                        >
                          {scope === "selected" && (
                            <Check className="h-2.5 w-2.5 stroke-[3]" />
                          )}
                        </div>
                      </div>
                    </div>
                  )}
                </div>
              </div>

              {/* 特征匹配与治理规则表格 */}
              <BanRulesTable
                customKeywords={customKeywords}
                isDetectionEnabled={isDetectionEnabled}
                onNavigateToSettings={handleNavigateToBanSettings}
              />
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="border-t border-border bg-muted/20 px-6 py-3.5 flex items-center justify-between">
          {result ? (
            <>
              <Button
                variant="ghost"
                size="sm"
                className="h-8 px-3 text-xs"
                onClick={handleReset}
              >
                重新排查
              </Button>
              <div className="flex items-center gap-2">
                {result.bannedCount > 0 && onViewSuspended && (
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-8 px-3 text-xs gap-1.5 border-rose-500/30 text-rose-600 dark:text-rose-400 hover:bg-rose-500/10"
                    onClick={() => {
                      handleClose();
                      onViewSuspended();
                    }}
                  >
                    查看封禁列表
                    <ArrowRight className="h-3 w-3" />
                  </Button>
                )}
                <Button
                  size="sm"
                  className="h-8 px-4 text-xs font-medium"
                  onClick={handleClose}
                >
                  完成
                </Button>
              </div>
            </>
          ) : (
            <>
              <Button
                variant="ghost"
                size="sm"
                className="h-8 px-3 text-xs text-muted-foreground"
                onClick={handleClose}
                disabled={scanning}
              >
                取消
              </Button>
              <Button
                size="sm"
                className="h-8 px-4 text-xs font-medium bg-rose-600 hover:bg-rose-700 text-white gap-1.5 shadow-xs"
                onClick={handleStartScan}
                disabled={scanning}
              >
                <ShieldX className="h-3.5 w-3.5" />
                开始排查
              </Button>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
