import { useState, useEffect, useCallback } from "react";
import { toast } from "sonner";
import {
  RefreshCw,
  GripVertical,
  Trash2,
  Loader2,
  Pencil,
  LogIn,
  MoreHorizontal,
  RotateCcw,
  Zap,
  ZapOff,
  Clock,
  ScrollText,
  Boxes,
} from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Checkbox } from "@/components/ui/checkbox";
import { Progress } from "@/components/ui/progress";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { SubscriptionBadge } from "@/components/subscription-badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { CredentialStatusItem, BalanceResponse } from "@/types/api";
import { maskProxyUrl, extractErrorMessage, overageFailureMessage } from "@/lib/utils";
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useResetSuccessCount,
  useClearThrottle,
} from "@/hooks/use-credentials";
import { setCredentialOverage } from "@/api/credentials";
import { useQueryClient } from "@tanstack/react-query";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { EditCredentialDialog } from "@/components/edit-credential-dialog";
import { UpdateTokenDialog } from "@/components/update-token-dialog";
import { ReloginDialog } from "@/components/relogin-dialog";
import { CredentialFailuresDialog } from "@/components/credential-failures-dialog";
import { AvailableModelsDialog } from "@/components/available-models-dialog";

interface CredentialCardProps {
  credential: CredentialStatusItem;
  selected: boolean;
  onToggleSelect: () => void;
  balance: BalanceResponse | null;
  loadingBalance: boolean;
  onRefreshBalance: () => void;
  /** 该凭据的失败分类计数（来自 trace 聚合）；无数据时回退 totalFailureCount */
  failureStats?: { auth: number; throttle: number; other: number };
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return "从未使用";
  const date = new Date(lastUsedAt);
  const diff = Date.now() - date.getTime();
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s} 秒前`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时前`;
  return `${Math.floor(h / 24)} 天前`;
}

function formatNumber(n: number): string {
  return n.toLocaleString("zh-CN", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

function formatResetDate(ts: number | null): string {
  if (!ts) return "未知";
  return new Date(ts * 1000).toLocaleString("zh-CN");
}

/** 把秒数格式化为 `mm:ss` 或 `hh:mm:ss` */
function formatThrottleCountdown(secs: number): string {
  const total = Math.max(0, Math.floor(secs));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

/**
 * 紧凑超额状态胶囊 — 与订阅徽章并列展示，不占整行
 * 三态：已开（绿色实色）/ 未开（中性细描边）/ 不支持（灰色虚边小字）
 */
function OverageStatusPill({ balance }: { balance: BalanceResponse }) {
  const cap = balance.overageCapable;
  const on = balance.overageEnabled === true;

  // 不支持的订阅：极弱化
  if (cap === false) return null;

  if (on) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full bg-emerald-500/15 px-2 h-6 text-[11px] font-medium text-emerald-700 dark:text-emerald-400"
        title="此账号已开启超额"
      >
        <Zap className="h-3 w-3" />
        超额
      </span>
    );
  }

  if (cap === true) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 bg-transparent px-2 h-6 text-[11px] font-medium text-amber-600 dark:text-amber-400"
        title="此账号支持超额但当前未开启"
      >
        <ZapOff className="h-3 w-3" />
        未开
      </span>
    );
  }

  // 未知：低调灰色，hover 看原始值
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full border border-dashed border-border/60 bg-transparent px-2 h-6 text-[11px] text-muted-foreground"
      title={
        balance.overageCapabilityRaw
          ? `overageCapability = ${balance.overageCapabilityRaw}`
          : "上游未返回 overageCapability"
      }
    >
      <ZapOff className="h-3 w-3" />
      未知
    </span>
  );
}

/**
 * 把后端返回的 disabledReason 字符串映射为更直观的中文徽标
 * （颜色/文案/排序权重，越靠前越显眼）
 */
function getDisabledReasonStyle(reason?: string | null): {
  label: string;
  variant: "destructive" | "warning" | "outline" | "secondary";
} | null {
  if (!reason) return null;
  switch (reason) {
    case "QuotaExceeded":
      return { label: "已超额", variant: "warning" };
    case "TooManyFailures":
      return { label: "失败过多", variant: "destructive" };
    case "TooManyRefreshFailures":
      return { label: "刷新失败过多", variant: "destructive" };
    case "InvalidRefreshToken":
      return { label: "Token 失效", variant: "destructive" };
    case "InvalidConfig":
      return { label: "配置无效", variant: "destructive" };
    case "Manual":
      return { label: "手动禁用", variant: "secondary" };
    default:
      return { label: reason, variant: "outline" };
  }
}

export function CredentialCard({
  credential,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onRefreshBalance,
  failureStats,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false);
  const [priorityValue, setPriorityValue] = useState(
    String(credential.priority),
  );
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [showEditDialog, setShowEditDialog] = useState(false);
  const [showUpdateTokenDialog, setShowUpdateTokenDialog] = useState(false);
  const [showReloginDialog, setShowReloginDialog] = useState(false);
  const [showFailuresDialog, setShowFailuresDialog] = useState(false);
  const [showModelsDialog, setShowModelsDialog] = useState(false);

  const setDisabled = useSetDisabled();
  const setPriority = useSetPriority();
  const resetFailure = useResetFailure();
  const deleteCredential = useDeleteCredential();
  const forceRefresh = useForceRefreshToken();
  const resetSuccess = useResetSuccessCount();
  const clearThrottle = useClearThrottle();
  const queryClient = useQueryClient();

  // 拖拽排序：手柄触发，整卡随拖动位移
  const {
    attributes,
    listeners,
    setNodeRef,
    setActivatorNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: credential.id });
  const dragStyle: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    // 拖拽中关掉过渡，避免 Card 基类的 transition-all 把每帧 transform 动画化导致"不跟手"；
    // 非拖拽态保留 dnd-kit 的归位过渡。
    transition: isDragging ? "none" : transition,
    zIndex: isDragging ? 20 : undefined,
  };

  // 后端冷却剩余秒数会在 30s 拉取间隔之间过时，本地用 setInterval 自然递减以让倒计时连续。
  const [throttleRemaining, setThrottleRemaining] = useState<number>(
    credential.throttledRemainingSecs ?? 0,
  );
  useEffect(() => {
    setThrottleRemaining(credential.throttledRemainingSecs ?? 0);
  }, [credential.throttledRemainingSecs]);
  useEffect(() => {
    if (throttleRemaining <= 0) return;
    const t = window.setInterval(() => {
      setThrottleRemaining((v) => (v > 0 ? v - 1 : 0));
    }, 1000);
    return () => window.clearInterval(t);
  }, [throttleRemaining]);
  const handleClearThrottle = useCallback(() => {
    clearThrottle.mutate(credential.id, {
      onSuccess: (res) => {
        setThrottleRemaining(0);
        toast.success(res.message);
      },
      onError: (err) => toast.error("解除失败: " + extractErrorMessage(err)),
    });
  }, [clearThrottle, credential.id]);
  const [overageBusy, setOverageBusy] = useState(false);
  const handleSetOverage = async (enabled: boolean) => {
    setOverageBusy(true);
    try {
      await setCredentialOverage(credential.id, enabled);
      toast.success(enabled ? "已开启超额" : "已关闭超额");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error(
        (enabled ? "开启" : "关闭") +
          "超额失败: " +
          overageFailureMessage(extractErrorMessage(err)),
      );
    } finally {
      setOverageBusy(false);
    }
  };

  const handleToggleDisabled = () => {
    // 当前为禁用态 → 这次操作是“启用”，启用成功后顺带刷新一次余额
    const willEnable = credential.disabled;
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message);
          if (willEnable) onRefreshBalance();
        },
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  const handlePriorityChange = () => {
    const np = parseInt(priorityValue, 10);
    if (isNaN(np) || np < 0) {
      toast.error("优先级必须是非负整数");
      return;
    }
    setPriority.mutate(
      { id: credential.id, priority: np },
      {
        onSuccess: (res) => {
          toast.success(res.message);
          setEditingPriority(false);
        },
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  const handleReset = () =>
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("操作失败: " + (err as Error).message),
    });

  const handleForceRefresh = () =>
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("刷新失败: " + extractErrorMessage(err)),
    });

  const handleResetSuccess = () =>
    resetSuccess.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("重置失败: " + (err as Error).message),
    });

  const handleDelete = () => {
    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message);
        setShowDeleteDialog(false);
      },
      onError: (err) => toast.error("删除失败: " + (err as Error).message),
    });
  };

  const authLabel = (() => {
    if (credential.authMethod === "api_key") return "API Key";
    const provider = credential.provider?.toLowerCase();
    if (credential.authMethod === "social") {
      if (provider === "github") return "GitHub";
      if (provider === "google") return "Google";
      return "Social";
    }
    if (credential.authMethod === "idc") {
      if (provider === "enterprise") return "Enterprise";
      if (provider === "iam_sso") return "IAM SSO";
      if (provider === "builderid") return "Builder ID";
      return "IdC";
    }
    return credential.authMethod;
  })();

  const isQuotaExceeded = balance
    ? balance.remaining <= 0 || balance.usagePercentage >= 100
    : false;

  const disabledByQuota =
    credential.disabled && credential.disabledReason === "QuotaExceeded";
  const reasonStyle = getDisabledReasonStyle(credential.disabledReason);
  const isThrottled = !credential.disabled && throttleRemaining > 0;

  return (
    <>
      <Card
        ref={setNodeRef}
        style={dragStyle}
        data-credential-id={credential.id}
        className={`group flex h-full min-w-0 flex-col ${
          isDragging
            ? "shadow-apple-lg opacity-80"
            : "hover:-translate-y-0.5 hover:shadow-apple-lg"
        } ${
          credential.isCurrent ? "ring-2 ring-primary/60 shadow-apple-lg" : ""
        } ${
          // 未禁用但已超额：琥珀色提醒边
          !credential.disabled && isQuotaExceeded
            ? "ring-1 ring-amber-500/60"
            : ""
        } ${
          // 已因超额被禁用：琥珀色实色边 + 不灰化（保留可读性，方便审视）
          disabledByQuota
            ? "ring-1 ring-amber-500/70 bg-amber-50/40 dark:bg-amber-500/[0.04]"
            : ""
        } ${
          // 账号级风控冷却中：橙红色提示边
          isThrottled
            ? "ring-1 ring-orange-500/60 bg-orange-50/40 dark:bg-orange-500/[0.04]"
            : ""
        } ${
          // 其他原因被禁用：常规灰化
          credential.disabled && !disabledByQuota ? "opacity-70" : ""
        }`}
      >
        <CardHeader className="p-4 pb-3 sm:p-5 sm:pb-3">
          <div className="flex min-w-0 items-start gap-2.5 sm:gap-3">
            <label
              data-no-rect-select
              className="mt-0.5 flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-md transition-colors hover:bg-accent sm:h-7 sm:w-7"
              onClick={(e) => {
                // label + Checkbox 双击事件去重，避免触发两次 onCheckedChange
                e.stopPropagation();
              }}
            >
              <Checkbox
                className="h-5 w-5 [&_svg]:h-4 [&_svg]:w-4"
                checked={selected}
                onCheckedChange={onToggleSelect}
              />
            </label>
            <div className="min-w-0 flex-1">
              <CardTitle className="truncate text-[15px] leading-5">
                {credential.email || `凭据 #${credential.id}`}
              </CardTitle>
              <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-1 overflow-hidden">
                {balance?.subscriptionTitle && (
                  <SubscriptionBadge
                    title={balance.subscriptionTitle}
                    className="max-w-full"
                  />
                )}
                {credential.isCurrent && <Badge variant="success">活跃</Badge>}
                {/* 禁用状态：合并 "已禁用" + 中文化的原因，单个 Badge 更醒目 */}
                {credential.disabled && reasonStyle && (
                  <Badge variant={reasonStyle.variant}>
                    已禁用 · {reasonStyle.label}
                  </Badge>
                )}
                {credential.disabled && !reasonStyle && (
                  <Badge variant="destructive">已禁用</Badge>
                )}
                {/* 仍启用但已经达到上限：黄色"已超额"徽章 */}
                {!credential.disabled && isQuotaExceeded && (
                  <Badge variant="warning">已超额</Badge>
                )}
                {isThrottled && (
                  <Badge
                    variant="warning"
                    className="bg-orange-500/15 text-orange-700 dark:text-orange-300 border-orange-500/30"
                    title="账号级风控冷却中（429 + suspicious activity），到期或手动解除后恢复调度"
                  >
                    <Clock className="mr-1 h-3 w-3" />
                    冷却 {formatThrottleCountdown(throttleRemaining)}
                  </Badge>
                )}
                {credential.authMethod && (
                  <Badge variant="secondary">{authLabel}</Badge>
                )}
                {/* 配置元信息合并为单个徽章，减少换行：endpoint · ARN */}
                {(credential.endpoint || credential.hasProfileArn) && (
                  <Badge
                    variant="outline"
                    className="max-w-full truncate"
                    title={
                      credential.hasProfileArn
                        ? "endpoint / 已配置 Profile ARN"
                        : "endpoint"
                    }
                  >
                    {[
                      credential.endpoint,
                      credential.hasProfileArn ? "ARN" : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </Badge>
                )}
                {/* 账号所属分组 */}
                {(credential.groups ?? []).map((g) => (
                  <Badge key={g} variant="outline" title="账号分组">
                    {g}
                  </Badge>
                ))}
                {/* 账号来源渠道 */}
                {credential.sourceChannel && (
                  <Badge variant="outline" title="账号来源渠道">
                    来源: {credential.sourceChannel}
                  </Badge>
                )}
              </div>
            </div>
            <Switch
              className="mt-0.5"
              checked={!credential.disabled}
              onCheckedChange={handleToggleDisabled}
              disabled={setDisabled.isPending}
              title={credential.disabled ? "启用" : "禁用"}
            />
          </div>
        </CardHeader>

        <CardContent className="flex flex-1 flex-col space-y-3 px-4 pb-4 sm:space-y-4 sm:px-5 sm:pb-5">
          {/* 信息行 */}
          <dl className="grid grid-cols-1 gap-2 text-[13px] min-[420px]:grid-cols-2 min-[420px]:gap-x-4">
            <div className="flex min-w-0 items-center justify-between gap-2">
              <dt className="shrink-0 text-muted-foreground">优先级</dt>
              <dd className="min-w-0">
                {editingPriority ? (
                  <div className="inline-flex max-w-full items-center gap-1">
                    <Input
                      type="number"
                      value={priorityValue}
                      onChange={(e) => setPriorityValue(e.target.value)}
                      className="w-16 h-7 text-sm rounded-md"
                      min="0"
                    />
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      onClick={handlePriorityChange}
                      disabled={setPriority.isPending}
                    >
                      ✓
                    </Button>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      onClick={() => {
                        setEditingPriority(false);
                        setPriorityValue(String(credential.priority));
                      }}
                    >
                      ✕
                    </Button>
                  </div>
                ) : (
                  <button
                    type="button"
                    className="inline-flex cursor-pointer items-center gap-1 rounded px-1.5 py-0.5 font-medium tabular-nums transition-colors hover:bg-accent hover:text-primary"
                    onClick={() => setEditingPriority(true)}
                    title="点击编辑优先级"
                  >
                    {credential.priority}
                    <Pencil className="h-3 w-3 opacity-70" />
                  </button>
                )}
              </dd>
            </div>
            <div className="flex min-w-0 items-center justify-between gap-2">
              <dt className="shrink-0 text-muted-foreground">失败次数</dt>
              <dd className="min-w-0">
                <button
                  type="button"
                  onClick={() => setShowFailuresDialog(true)}
                  className="inline-flex cursor-pointer items-center gap-1 rounded px-1.5 py-0.5 font-medium tabular-nums transition-colors hover:bg-accent"
                  title="鉴权失败 / 账号风控 / 其他（额度·瞬态·网络等）。点击查看失败日志详情"
                >
                  {failureStats ? (
                    <span className="tabular-nums">
                      <span className="text-destructive">{failureStats.auth}</span>
                      <span className="text-muted-foreground/50">/</span>
                      <span className="text-amber-600 dark:text-amber-400">
                        {failureStats.throttle}
                      </span>
                      <span className="text-muted-foreground/50">/</span>
                      <span className="text-muted-foreground">{failureStats.other}</span>
                    </span>
                  ) : (
                    <span
                      className={
                        credential.totalFailureCount > 0
                          ? "text-destructive"
                          : "text-muted-foreground"
                      }
                    >
                      {credential.totalFailureCount}
                    </span>
                  )}
                  <ScrollText className="h-3.5 w-3.5 opacity-70" />
                </button>
              </dd>
            </div>
            <div className="flex min-w-0 items-center justify-between gap-2">
              <dt className="shrink-0 text-muted-foreground">刷新失败</dt>
              <dd
                className={`tabular-nums font-medium ${credential.refreshFailureCount > 0 ? "text-destructive" : ""}`}
              >
                {credential.refreshFailureCount}
              </dd>
            </div>
            <div className="flex min-w-0 items-center justify-between gap-2">
              <dt className="shrink-0 text-muted-foreground">成功次数</dt>
              <dd className="min-w-0">
                <button
                  type="button"
                  onClick={handleResetSuccess}
                  className="inline-flex cursor-pointer items-center gap-1 rounded px-1.5 py-0.5 font-medium tabular-nums transition-colors hover:bg-accent hover:text-primary"
                  title="点击重置成功次数"
                >
                  {credential.successCount}
                  <RotateCcw className="h-3 w-3 opacity-70" />
                </button>
              </dd>
            </div>
            <div className="flex min-w-0 items-center justify-between gap-2 border-t border-border/50 pt-2 min-[420px]:col-span-2">
              <dt className="shrink-0 text-muted-foreground">最后调用</dt>
              <dd className="min-w-0 truncate text-right font-medium">
                {formatLastUsed(credential.lastUsedAt)}
              </dd>
            </div>
            {credential.maskedApiKey && (
              <div className="flex min-w-0 items-center justify-between gap-2 min-[420px]:col-span-2">
                <dt className="shrink-0 text-muted-foreground">API Key</dt>
                <dd className="min-w-0 truncate text-right font-mono text-xs">
                  {credential.maskedApiKey}
                </dd>
              </div>
            )}
            {credential.hasProxy && (
              <div className="flex min-w-0 items-center justify-between gap-2 min-[420px]:col-span-2">
                <dt className="shrink-0 text-muted-foreground">代理</dt>
                <dd className="min-w-0 truncate text-right font-mono text-xs">
                  {maskProxyUrl(credential.proxyUrl ?? "")}
                </dd>
              </div>
            )}
          </dl>

          {/* 余额面板 */}
          <div
            className={`flex min-h-[138px] flex-col rounded-xl border p-3 transition-colors sm:min-h-[150px] sm:p-4 ${
              isQuotaExceeded || disabledByQuota
                ? "border-amber-500/40 bg-amber-50/60 dark:bg-amber-500/[0.06]"
                : "border-border/60 bg-secondary/40"
            }`}
          >
            {loadingBalance ? (
              <div className="flex flex-1 items-center justify-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                正在查询余额…
              </div>
            ) : balance ? (
              <div className="space-y-3">
                <div className="flex min-w-0 items-end justify-between gap-3">
                  <div className="min-w-0">
                    <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
                      {balance.remaining < 0 ? "超额" : "余额"}
                    </div>
                    <div
                      className={`mt-0.5 text-xl font-semibold tabular-nums ${
                        balance.remaining < 0
                          ? "text-red-600 dark:text-red-400"
                          : balance.remaining === 0
                            ? "text-amber-600 dark:text-amber-400"
                            : "text-emerald-600 dark:text-emerald-400"
                      }`}
                    >
                      {balance.remaining < 0
                        ? `-$${formatNumber(Math.abs(balance.remaining))}`
                        : `$${formatNumber(balance.remaining)}`}
                    </div>
                  </div>
                  <div className="min-w-0 shrink-0 text-right">
                    <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
                      超额
                    </div>
                    <div className="mt-1 flex items-center justify-end">
                      <OverageStatusPill balance={balance} />
                    </div>
                  </div>
                </div>
                <div className="space-y-1.5">
                  <Progress value={balance.usagePercentage} />
                  <div className="grid grid-cols-3 gap-1 text-[11px] tabular-nums text-muted-foreground">
                    <span className="min-w-0 truncate">
                      已用 ${formatNumber(balance.currentUsage)}
                    </span>
                    <span className="text-center">
                      {balance.usagePercentage.toFixed(1)}%
                    </span>
                    <span className="min-w-0 truncate text-right">
                      额度 ${formatNumber(balance.usageLimit)}
                    </span>
                  </div>
                </div>
                <div className="break-words border-t border-border/50 pt-2 text-[11px] text-muted-foreground">
                  下次重置：
                  <span className="font-medium text-foreground">
                    {formatResetDate(balance.nextResetAt)}
                  </span>
                </div>
              </div>
            ) : (
              <div className="flex flex-1 items-center justify-center text-center text-[13px] text-muted-foreground">
                余额未查询，点击顶部"刷新当前页余额"即可加载。
              </div>
            )}
          </div>

          {/* 操作区 */}
          <div className="mt-auto flex flex-col gap-2 border-t border-border/50 pt-3 min-[420px]:flex-row min-[420px]:items-center min-[420px]:justify-between">
            <div className="grid grid-cols-3 gap-1 min-[420px]:flex min-[420px]:items-center">
              <Button
                ref={setActivatorNodeRef}
                size="icon"
                variant="ghost"
                data-no-rect-select
                className="w-full cursor-grab touch-none active:cursor-grabbing min-[420px]:w-9"
                title="拖拽调整优先级"
                {...attributes}
                {...listeners}
              >
                <GripVertical className="h-4 w-4 text-muted-foreground" />
              </Button>
              <span className="mx-1 hidden h-5 w-px bg-border/70 min-[420px]:inline-block" />
              <Button
                size="sm"
                variant="ghost"
                className="w-full px-2 min-[420px]:w-auto min-[420px]:px-3"
                onClick={handleForceRefresh}
                disabled={
                  forceRefresh.isPending ||
                  credential.disabled ||
                  credential.authMethod === "api_key"
                }
                title={
                  credential.authMethod === "api_key"
                    ? "API Key 无需刷新"
                    : credential.disabled
                      ? "已禁用"
                      : "强制刷新 Token"
                }
              >
                <RefreshCw
                  className={`h-3.5 w-3.5 ${forceRefresh.isPending ? "animate-spin" : ""}`}
                />
                <span className="hidden sm:inline">刷新 Token</span>
              </Button>
              <Button
                size="sm"
                variant="ghost"
                className="w-full px-2 min-[420px]:w-auto min-[420px]:px-3"
                onClick={onRefreshBalance}
                disabled={loadingBalance || credential.disabled}
                title={credential.disabled ? "已禁用" : "刷新余额"}
              >
                <RefreshCw
                  className={`h-3.5 w-3.5 ${loadingBalance ? "animate-spin" : ""}`}
                />
                <span className="hidden sm:inline">刷新余额</span>
              </Button>
            </div>

            <div className="grid grid-cols-[1fr_auto] gap-1 min-[420px]:flex min-[420px]:items-center">
              <Button
                size="sm"
                variant="outline"
                className="w-full min-[420px]:w-auto"
                onClick={() => setShowEditDialog(true)}
              >
                <Pencil className="h-3.5 w-3.5" />
                编辑
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="icon" variant="ghost" title="更多操作">
                    <MoreHorizontal className="h-4 w-4" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleReset();
                    }}
                    disabled={
                      resetFailure.isPending ||
                      (credential.failureCount === 0 &&
                        credential.refreshFailureCount === 0)
                    }
                  >
                    <RotateCcw />
                    重置失败计数
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => setShowModelsDialog(true)}
                    disabled={credential.disabled}
                    title={
                      credential.disabled ? "已禁用凭据无法查询" : undefined
                    }
                  >
                    <Boxes />
                    查看可用模型
                  </DropdownMenuItem>
                  {throttleRemaining > 0 && (
                    <DropdownMenuItem
                      onSelect={(e) => {
                        e.preventDefault();
                        handleClearThrottle();
                      }}
                      disabled={clearThrottle.isPending}
                    >
                      <Clock />
                      解除风控冷却（{formatThrottleCountdown(throttleRemaining)}
                      ）
                    </DropdownMenuItem>
                  )}
                  {balance?.overageCapable === true &&
                    (balance.overageEnabled ? (
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          handleSetOverage(false);
                        }}
                        disabled={overageBusy}
                      >
                        <ZapOff />
                        关闭超额
                      </DropdownMenuItem>
                    ) : (
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          handleSetOverage(true);
                        }}
                        disabled={overageBusy}
                      >
                        <Zap className="text-emerald-500" />
                        开启超额
                      </DropdownMenuItem>
                    ))}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuSeparator />
                  )}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuItem
                      onSelect={() => setShowReloginDialog(true)}
                    >
                      <LogIn />
                      重新登录
                    </DropdownMenuItem>
                  )}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuItem
                      onSelect={() => setShowUpdateTokenDialog(true)}
                    >
                      <RefreshCw />
                      重新导入 Token
                    </DropdownMenuItem>
                  )}
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    destructive
                    onSelect={(e) => {
                      e.preventDefault();
                      setShowDeleteDialog(true);
                    }}
                  >
                    <Trash2 />
                    删除凭据
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        </CardContent>
      </Card>

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <EditCredentialDialog
        open={showEditDialog}
        onOpenChange={setShowEditDialog}
        credential={credential}
      />
      <UpdateTokenDialog
        open={showUpdateTokenDialog}
        onOpenChange={setShowUpdateTokenDialog}
        credential={credential}
      />
      <ReloginDialog
        open={showReloginDialog}
        onOpenChange={setShowReloginDialog}
        credential={credential}
      />
      <CredentialFailuresDialog
        open={showFailuresDialog}
        onOpenChange={setShowFailuresDialog}
        credentialId={credential.id}
        email={credential.email}
      />
      <AvailableModelsDialog
        open={showModelsDialog}
        onOpenChange={setShowModelsDialog}
        credentialId={showModelsDialog ? credential.id : null}
      />
    </>
  );
}
