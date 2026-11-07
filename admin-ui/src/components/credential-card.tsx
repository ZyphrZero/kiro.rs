import { useState, useEffect, useCallback, memo } from "react";
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
  Wallet,
  Sparkles,
  Flag,
} from "lucide-react";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
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
import type {
  CredentialActivity,
  CredentialDistribution,
  StatsRange,
  CredentialMetadataSchema,
  CredentialStatusItem,
  BalanceResponse,
  ProxyPoolEntry,
} from "@/types/api";
import { maskEmail, maskProxyUrl, extractErrorMessage, overageFailureMessage, formatNumber as formatCompactNumber, cn } from "@/lib/utils";
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useResetSuccessCount,
  useClearThrottle,
  useLoadBalancingMode,
} from "@/hooks/use-credentials";
import { setCredentialOverage, getProxyPool } from "@/api/credentials";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { EditCredentialDialog } from "@/components/edit-credential-dialog";
import { UpdateTokenDialog } from "@/components/update-token-dialog";
import { ReloginDialog } from "@/components/relogin-dialog";
import { CredentialFailuresDialog } from "@/components/credential-failures-dialog";
import { AvailableModelsDialog } from "@/components/available-models-dialog";
import { BalanceDialog } from "@/components/balance-dialog";
import { getDisposition } from "@/components/console/credential-state";
import { railBorderClass } from "@/components/console/rail";
import { PriorityPreview } from "@/components/console/priority-preview";
import { CredentialCacheMetric } from "@/components/credential-cache-stats";
import { CredentialLabel } from "@/components/console/credential-label";

interface CredentialCardProps {
  credential: CredentialStatusItem;
  selected: boolean;
  onToggleSelect: (id: number) => void;
  balance: BalanceResponse | null;
  loadingBalance: boolean;
  onRefreshBalance: (id: number) => void | Promise<void>;
  /** 该凭据的失败分类计数（来自 trace 聚合）；无数据时回退 totalFailureCount */
  failureStats?: { auth: number; throttle: number; other: number };
  activity?: CredentialActivity;
  loadingActivity?: boolean;
  activityError?: boolean;
  cacheStats?: CredentialDistribution;
  cacheRange?: StatsRange;
  loadingCacheStats?: boolean;
  cacheStatsError?: boolean;
  cacheStatsStale?: boolean;
  /** 展示形态：卡片（默认）或紧凑列表行 */
  view?: "card" | "list";
  /** 字段排序开启时禁用拖拽调优先级（隐藏拖拽手柄） */
  dragDisabled?: boolean;
  /** 开发预览卡：仅展示，不发起任何凭据操作。 */
  preview?: boolean;
  privacyMode?: boolean;
  metadataSchema?: CredentialMetadataSchema;
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

/** 添加时间用绝对时刻展示 */
function formatCreatedAt(createdAt: string | null | undefined, includeTime = true): string {
  if (!createdAt) return "未知";
  const date = new Date(createdAt);
  if (Number.isNaN(date.getTime())) return "未知";
  return date.toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: includeTime ? "2-digit" : undefined,
    minute: includeTime ? "2-digit" : undefined,
    hour12: false,
  });
}

function formatCreatedAtFull(createdAt: string | null | undefined): string {
  if (!createdAt) return "添加时间未知";
  const date = new Date(createdAt);
  if (Number.isNaN(date.getTime())) return "添加时间未知";
  return `添加于 ${date.toLocaleString("zh-CN")}`;
}

function formatNumber(n: number): string {
  return n.toLocaleString("zh-CN", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

function formatResetDate(ts: number | null): string {
  if (!ts) return "未知";
  return new Date(ts * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
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

function endpointDisplayLabel(endpoint: string): string {
  if (endpoint === "ide") return "IDE 端点";
  if (endpoint === "cli") return "CLI 端点";
  return endpoint;
}

/** Profile ARN 的短标识：`arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK` → `EHGA3GRVQMUK` */
export function profileShortId(arn: string): string {
  const idx = arn.lastIndexOf("/");
  return idx >= 0 ? arn.slice(idx + 1) : arn;
}

function metadataValueLabel(value: unknown): string {
  if (typeof value === "boolean") return value ? "是" : "否";
  if (typeof value === "string" || typeof value === "number") {
    return String(value);
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function metadataEntries(
  credential: CredentialStatusItem,
) {
  const metadata = credential.metadata ?? {};
  return Object.entries(metadata).flatMap(([key, detail]) => {
    const rawValue = detail.value;
    if (rawValue == null || rawValue === "") return [];
    return [
      {
        key,
        label: detail.title?.trim() || key,
        description: detail.description,
        value:
          key === "salePrice" && typeof rawValue === "number"
            ? `¥${(rawValue as number).toLocaleString("zh-CN", {
                minimumFractionDigits: 2,
                maximumFractionDigits: 2,
              })}`
            : detail.valueLabel ?? metadataValueLabel(rawValue),
        emphasized: key === "type" && rawValue === "boom",
      },
    ];
  });
}



function OverageStatusPill({ balance }: { balance: BalanceResponse }) {
  const cap = balance.overageCapable;
  const on = balance.overageEnabled === true;

  if (cap === false) return null;

  if (on) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full bg-emerald-500/15 px-2 py-0.5 text-[10px] font-semibold text-emerald-700 dark:text-emerald-400 border border-emerald-500/30"
        title="此账号已开启超额"
      >
        <Zap className="h-2.5 w-2.5 fill-current" />
        超额开启
      </span>
    );
  }

  if (cap === true) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 text-[10px] font-medium text-amber-600 dark:text-amber-400"
        title="此账号支持超额但当前未开启"
      >
        <ZapOff className="h-2.5 w-2.5" />
        超额未开
      </span>
    );
  }

  return (
    <span
      className="inline-flex items-center gap-1 rounded-full border border-dashed border-border/60 bg-transparent px-2 py-0.5 text-[10px] text-muted-foreground"
      title="上游未返回超额能力状态"
    >
      <ZapOff className="h-2.5 w-2.5 opacity-60" />
      未知
    </span>
  );
}

function getDisabledReasonStyle(reason?: string | null): {
  label: string;
  variant: "destructive" | "warning" | "outline" | "secondary";
} | null {
  if (!reason) return null;
  switch (reason) {
    case "QuotaExceeded":
      return { label: "已超额", variant: "warning" };
    case "TooManyFailures":
      return { label: "失败过多", variant: "secondary" };
    case "Suspended":
      return { label: "账号封禁", variant: "destructive" };
    case "TooManyRefreshFailures":
      return { label: "刷新失败过多", variant: "secondary" };
    case "InvalidRefreshToken":
      return { label: "Token 失效", variant: "secondary" };
    case "InvalidConfig":
      return { label: "配置无效", variant: "secondary" };
    case "Manual":
      return { label: "手动禁用", variant: "secondary" };
    default:
      return { label: reason, variant: "outline" };
  }
}

function MetadataSummary({
  credential,
  wrap = false,
  privacyMode = false,
}: {
  credential: CredentialStatusItem;
  wrap?: boolean;
  privacyMode?: boolean;
}) {
  const entries = metadataEntries(credential);
  if (entries.length === 0) return null;
  if (privacyMode) return <Badge variant="outline" className="self-start">备注已隐藏</Badge>;

  return (
    <div
      className={cn("flex min-w-0 items-center gap-1", wrap ? "flex-wrap" : "mt-1 overflow-hidden")}
      aria-label="凭据 Metadata"
    >
      {entries.map((entry) => (
        <span
          key={entry.key}
          className={cn(
            "inline-flex min-w-0 max-w-full items-center overflow-hidden rounded-md border px-1.5 py-0.5 text-[11px]",
            !wrap && "shrink-0",
            entry.emphasized
              ? "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300"
              : "border-border/60 bg-muted/45 text-foreground",
          )}
          title={[`${entry.label}: ${entry.value}`, entry.description].filter(Boolean).join("\n")}
        >
          <span className={cn("shrink-0 text-muted-foreground", wrap && "max-w-32 truncate")}>{entry.label}</span>
          <span className="mx-1 text-border">·</span>
          <span className="min-w-0 max-w-40 truncate font-medium">{entry.value}</span>
        </span>
      ))}
    </div>
  );
}

function CredentialCardImpl({
  credential,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onRefreshBalance,
  failureStats,
  activity,
  loadingActivity = false,
  activityError = false,
  cacheStats,
  cacheRange = '24h',
  loadingCacheStats = false,
  cacheStatsError = false,
  cacheStatsStale = false,
  view = "card",
  dragDisabled = false,
  preview = false,
  privacyMode = false,
  metadataSchema,
}: CredentialCardProps) {
  const displayEmail = privacyMode ? maskEmail(credential.email) : credential.email;
  const displayApiKey = privacyMode ? "••••••••" : credential.maskedApiKey;
  const displayProxyUrl = privacyMode ? "地址已隐藏" : maskProxyUrl(credential.proxyUrl ?? "");
  const displaySourceChannel = privacyMode ? "已隐藏" : credential.sourceChannel;
  const privacyActionHint = "隐私防护已开启，请先关闭防护再查看或编辑账号明文信息";
  const displayedActivity = preview || activityError ? undefined : activity;
  const activityUnavailable = preview
    ? "开发预览不参与请求统计"
    : activityError
      ? "请求统计获取失败，等待重试"
      : loadingActivity
        ? "正在加载请求统计"
        : "暂无请求统计数据";
  const activityStats = (
    <>
      <span title={displayedActivity
        ? `今日请求数：${displayedActivity.todayCalls.toLocaleString("zh-CN")}（服务端本地零点起，完成时记账，含成功和失败）`
        : activityUnavailable}>
        今日 <span className={cn("tabular-nums text-foreground/80", activityError && "text-destructive")}>
          {displayedActivity ? formatCompactNumber(displayedActivity.todayCalls) : "—"}
        </span>
      </span>
      <span title={displayedActivity
        ? `实时 RPM：最近 60 秒完成 ${displayedActivity.rpm.toLocaleString("zh-CN")} 次请求（含成功和失败，每 5 秒刷新）`
        : activityUnavailable}>
        实时 <span className={cn("tabular-nums text-foreground/80", activityError && "text-destructive")}>
          {displayedActivity ? formatCompactNumber(displayedActivity.rpm) : "—"} RPM
        </span>
      </span>
    </>
  );
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
  const [showBalanceDialog, setShowBalanceDialog] = useState(false);

  useEffect(() => {
    if (privacyMode) {
      setShowEditDialog(false);
      setShowUpdateTokenDialog(false);
      setShowReloginDialog(false);
      setShowFailuresDialog(false);
    }
  }, [privacyMode]);

  const { data: lbData } = useLoadBalancingMode();
  const balanced = lbData?.mode === "balanced";
  const invertPriority = lbData?.invertPriority ?? false;

  const setDisabled = useSetDisabled();
  const setPriority = useSetPriority();
  const resetFailure = useResetFailure();
  const deleteCredential = useDeleteCredential();
  const forceRefresh = useForceRefreshToken();
  const resetSuccess = useResetSuccessCount();
  const clearThrottle = useClearThrottle();
  const queryClient = useQueryClient();

  // 代理池健康数据，用于标记凭据专属代理是否异常（仅在凭据配置了代理时按需查询）
  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    staleTime: 30_000,
    enabled: Boolean(credential.hasProxy && credential.proxyUrl),
  });

  /** 凭据专属代理在代理池中的健康信息（仅当 proxyUrl 匹配到池内条目时有效） */
  const proxyEntry: ProxyPoolEntry | undefined = (() => {
    if (!credential.proxyUrl || !proxyPool?.proxies) return undefined;
    return proxyPool.proxies.find((p) => p.url === credential.proxyUrl);
  })();

  const proxyUnhealthy = proxyEntry && (proxyEntry.health === 'unhealthy' || proxyEntry.autoDisabled);

  const {
    attributes,
    listeners,
    setNodeRef,
    setActivatorNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: credential.id, disabled: dragDisabled });
  const dragStyle: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition: isDragging ? "none" : transition,
    zIndex: isDragging ? 20 : undefined,
  };

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

  const handleToggleSelect = useCallback(() => {
    onToggleSelect(credential.id);
  }, [onToggleSelect, credential.id]);

  const handleRefreshBalanceClick = useCallback(() => {
    onRefreshBalance(credential.id);
  }, [onRefreshBalance, credential.id]);

  const handleToggleDisabled = () => {
    const willEnable = credential.disabled;
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message);
          if (willEnable) onRefreshBalance(credential.id);
        },
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  const handlePriorityChange = () => {
    const np = parseInt(priorityValue, 10);
    if (isNaN(np) || np < 0) {
      toast.error(
        invertPriority
          ? "优先级要填 0 或更大的整数，数值大的最先被使用"
          : "优先级要填 0 或更大的整数，0 最先被使用",
      );
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
    if (credential.authMethod === "external_idp") {
      if (provider === "azuread") return "Entra ID";
      return "企业 SSO";
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

  const disposition = getDisposition(credential, balance, throttleRemaining);

  const runDisposition = () => {
    switch (disposition.action) {
      case "clearThrottle":
        handleClearThrottle();
        break;
      case "viewBalance":
        setShowBalanceDialog(true);
        break;
      case "relogin":
        setShowReloginDialog(true);
        break;
      case "enable":
        handleToggleDisabled();
        break;
      case "refreshToken":
        handleForceRefresh();
        break;
      case "viewFailures":
        setShowFailuresDialog(true);
        break;
      case "none":
        break;
    }
  };

  const dispositionButton = disposition.actionLabel ? (
    <Button
      size="sm"
      variant="outline"
      onClick={runDisposition}
      disabled={
        (privacyMode && (disposition.action === "relogin" || disposition.action === "viewFailures")) ||
        (disposition.action === "clearThrottle" && clearThrottle.isPending) ||
        (disposition.action === "enable" && setDisabled.isPending) ||
        (disposition.action === "refreshToken" && forceRefresh.isPending)
      }
      title={privacyMode && (disposition.action === "relogin" || disposition.action === "viewFailures")
        ? privacyActionHint
        : `${disposition.stateLabel} → ${disposition.actionLabel}`}
      className="h-7 whitespace-nowrap px-3 text-xs font-medium border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300 hover:bg-amber-500/20"
    >
      <Sparkles className="mr-1 h-3 w-3" />
      {disposition.actionLabel}
    </Button>
  ) : null;

  const stateClasses = [
    credential.isCurrent ? "ring-1 ring-primary shadow-xs" : "",
    !credential.disabled && isQuotaExceeded ? "ring-1 ring-amber-500/60" : "",
    disabledByQuota
      ? "ring-1 ring-amber-500/70 bg-amber-50/40 dark:bg-amber-500/[0.04]"
      : "",
    isThrottled
      ? "ring-1 ring-orange-500/60 bg-orange-50/40 dark:bg-orange-500/[0.04]"
      : "",
    credential.disabled && credential.disabledReason === "Suspended"
      ? "hover:border-rose-500/40"
      : "",
  ]
    .filter(Boolean)
    .join(" ");

  // 仅在有异常状态时呈现精简的状态标签，避免常规状态下出现杂乱胶囊
  const statusBadges = (
    <>
      {credential.disabled && reasonStyle && (
        <Badge
          variant={reasonStyle.variant}
          className={cn(
            "text-[11px]",
            credential.disabledReason === "Suspended" &&
              "border-rose-500/40 bg-rose-500/15 text-rose-700 dark:text-rose-400 font-semibold shadow-xs"
          )}
        >
          {credential.disabledReason === "Suspended"
            ? "已封禁 · 账号锁定"
            : `已禁用 · ${reasonStyle.label}`}
        </Badge>
      )}
      {credential.disabled && !reasonStyle && (
        <Badge variant="secondary" className="text-[11px]">已禁用</Badge>
      )}
      {!credential.disabled && isQuotaExceeded && (
        <Badge variant="warning" className="text-[11px]">已超额</Badge>
      )}
      {isThrottled && (
        <Badge
          variant="warning"
          className="bg-orange-500/15 text-orange-700 dark:text-orange-300 border-orange-500/30 text-[11px]"
          title="账号级限流冷却中"
        >
          <Clock className="mr-1 h-3 w-3 inline" />
          冷却 {formatThrottleCountdown(throttleRemaining)}
        </Badge>
      )}
    </>
  );

  const hasStatusBadges =
    credential.disabled || isQuotaExceeded || isThrottled;

  const subscriptionTitle = balance?.subscriptionTitle ?? credential.subscriptionTitle;
  const subscriptionBadge = subscriptionTitle ? (
    <SubscriptionBadge title={subscriptionTitle} />
  ) : null;

  const groups = credential.groups ?? [];

  const moreMenu = (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button size="icon" variant="ghost" className="h-8 w-8 hover:bg-accent" title="更多操作">
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-48">
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
          <RotateCcw className="mr-2 h-4 w-4" />
          重置失败计数
        </DropdownMenuItem>
        <DropdownMenuItem
          onSelect={() => setShowModelsDialog(true)}
          disabled={credential.disabled}
          title={credential.disabled ? "已禁用凭据无法查询" : undefined}
        >
          <Boxes className="mr-2 h-4 w-4" />
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
            <Clock className="mr-2 h-4 w-4" />
            解除限流冷却（{formatThrottleCountdown(throttleRemaining)}）
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
              <ZapOff className="mr-2 h-4 w-4 text-amber-500" />
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
              <Zap className="mr-2 h-4 w-4 text-emerald-500" />
              开启超额
            </DropdownMenuItem>
          ))}
        {credential.authMethod !== "api_key" && <DropdownMenuSeparator />}
        {credential.authMethod !== "api_key" && (
          <DropdownMenuItem
            onSelect={() => setShowReloginDialog(true)}
            disabled={privacyMode}
            title={privacyMode ? privacyActionHint : undefined}
          >
            <LogIn className="mr-2 h-4 w-4" />
            重新登录
          </DropdownMenuItem>
        )}
        {credential.authMethod !== "api_key" && (
          <DropdownMenuItem
            onSelect={() => setShowUpdateTokenDialog(true)}
            disabled={privacyMode}
            title={privacyMode ? privacyActionHint : undefined}
          >
            <RefreshCw className="mr-2 h-4 w-4" />
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
          <Trash2 className="mr-2 h-4 w-4" />
          删除凭据
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );

  // 紧凑列表行 View
  const listView = (
    <div
      ref={setNodeRef}
      style={dragStyle}
      data-credential-id={credential.id}
      className={`group flex min-w-0 items-center gap-2 rounded-lg border border-border bg-card px-3 py-2 transition-colors sm:gap-3 sm:px-3.5 [transform:translateZ(0)] ${railBorderClass(
        disposition.tone,
      )} ${
        isDragging
          ? "shadow-md opacity-80"
          : "hover:bg-accent/40 hover:border-border"
      } ${stateClasses}`}
    >
      {/* 拖拽手柄（字段排序开启时隐藏，此时拖拽无意义） */}
      {!dragDisabled && (
        <Button
          ref={setActivatorNodeRef}
          size="icon"
          variant="ghost"
          data-no-rect-select
          className="h-8 w-8 shrink-0 cursor-grab touch-none active:cursor-grabbing hover:bg-accent/60"
          title="拖拽排序 · 越靠上越先被使用"
          {...attributes}
          {...listeners}
        >
          <GripVertical className="h-4 w-4 text-muted-foreground/70" />
        </Button>
      )}

      <label
        data-no-rect-select
        className="flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-md transition-colors hover:bg-accent"
        onClick={(e) => e.stopPropagation()}
      >
        <Checkbox
          className="h-4 w-4 [&_svg]:h-3 [&_svg]:w-3"
          checked={selected}
          onCheckedChange={handleToggleSelect}
        />
      </label>

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm font-medium leading-5">
          <CredentialLabel id={credential.id} email={displayEmail} />
        </div>
        <div className="mt-1 flex min-w-0 items-center gap-1.5 overflow-hidden [&>*]:shrink-0">
          {statusBadges}
          {privacyMode && groups.length > 0 ? <Badge variant="secondary">分组已隐藏</Badge> : groups.map((g) => (
            <span
              key={g}
              title="账号分组"
              className="inline-flex items-center rounded-md bg-secondary/80 px-1.5 py-0.5 text-[11px] font-medium text-secondary-foreground"
            >
              {g}
            </span>
          ))}
          {credential.sourceChannel && (
            <span
              title="账号来源渠道"
              className="text-[11px] text-muted-foreground/80"
            >
              {displaySourceChannel}
            </span>
          )}
        </div>
        <MetadataSummary credential={credential} privacyMode={privacyMode} />
        <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
          {activityStats}
        </div>
        <div className="mt-2 select-text"><CredentialCacheMetric stats={cacheStats} range={cacheRange} loading={loadingCacheStats} failed={cacheStatsError} stale={cacheStatsStale} compact /></div>
      </div>

      <div className="hidden shrink-0 items-center gap-6 lg:flex">
        <div className="relative w-16 shrink-0 text-center">
          <div
            className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80"
            title={
              balanced
                ? "均衡负载权重：数值越大被调度频次越高"
                : invertPriority
                  ? "优先级：数字越大越先被使用"
                  : "优先级：数字越小越先被使用，0 最先"
            }
          >
            {balanced ? "权重" : "优先级 ↑"}
          </div>
          <div className="mt-0.5 flex h-[26px] items-center justify-center">
            {balanced ? (
              <span
                className="inline-flex items-center gap-1 rounded-md px-2 py-0.5 font-mono text-xs font-semibold tabular-nums"
                title="均衡负载权重（可在编辑凭据中修改）"
              >
                ×{credential.loadFactor}
              </span>
            ) : editingPriority ? (
              <div className="absolute left-1/2 top-1/2 z-30 -translate-x-1/2 -translate-y-1/2 rounded-md border border-border bg-popover p-1.5 shadow-md">
                <div className="inline-flex items-center gap-1">
                  <Input
                    type="number"
                    value={priorityValue}
                    onChange={(e) => setPriorityValue(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") handlePriorityChange();
                      if (e.key === "Escape") {
                        setEditingPriority(false);
                        setPriorityValue(String(credential.priority));
                      }
                    }}
                    className="h-7 w-16 rounded-md text-sm font-mono"
                    min="0"
                    autoFocus
                  />
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7 text-emerald-600"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                    title="确认"
                  >
                    ✓
                  </Button>
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7 text-muted-foreground"
                    onClick={() => {
                      setEditingPriority(false);
                      setPriorityValue(String(credential.priority));
                    }}
                    title="取消"
                  >
                    ✕
                  </Button>
                </div>
                <div className="mt-1 whitespace-nowrap px-1 text-center">
                  <PriorityPreview
                    credentialId={credential.id}
                    draft={priorityValue}
                    disabled={credential.disabled}
                  />
                </div>
              </div>
            ) : (
              <button
                type="button"
                className="inline-flex items-center gap-1 rounded-md px-2 py-0.5 font-mono text-xs font-semibold tabular-nums transition-colors hover:bg-accent hover:text-primary"
                onClick={() => setEditingPriority(true)}
                title={credential.isCurrent ? "当前调度优先凭据 · 点击编辑" : "点击编辑优先级"}
              >
                {credential.isCurrent && (
                  <Flag className="h-3 w-3 fill-emerald-500 text-emerald-500 shrink-0" />
                )}
                #{credential.priority}
                <Pencil className="h-3 w-3 opacity-60" />
              </button>
            )}
          </div>
        </div>

        <div className="w-20 text-center">
          <div className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80">
            失败
          </div>
          <button
            type="button"
            onClick={() => setShowFailuresDialog(true)}
            disabled={privacyMode}
            className="mt-0.5 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-xs font-semibold tabular-nums transition-colors hover:bg-accent"
            title={privacyMode ? privacyActionHint : "鉴权失败 / 风控 / 其他。点击查看日志"}
          >
            {failureStats ? (
              <span className="tabular-nums font-mono">
                <span className="text-destructive font-semibold">{failureStats.auth}</span>
                <span className="text-muted-foreground/40">/</span>
                <span className="text-amber-600 dark:text-amber-400">{failureStats.throttle}</span>
                <span className="text-muted-foreground/40">/</span>
                <span className="text-muted-foreground">{failureStats.other}</span>
              </span>
            ) : (
              <span
                className={
                  credential.totalFailureCount > 0
                    ? "font-mono font-semibold text-destructive"
                    : "font-mono text-muted-foreground"
                }
              >
                {credential.totalFailureCount}
              </span>
            )}
            <ScrollText className="h-3 w-3 opacity-60" />
          </button>
        </div>

        <div className="w-16 text-center">
          <div className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80">
            成功
          </div>
          <button
            type="button"
            onClick={handleResetSuccess}
            className="mt-0.5 inline-flex items-center gap-1 rounded px-1.5 py-0.5 font-mono text-xs font-semibold tabular-nums transition-colors hover:bg-accent hover:text-primary"
            title="点击重置成功次数"
          >
            {credential.successCount}
            <RotateCcw className="h-3 w-3 opacity-50" />
          </button>
        </div>
      </div>

      <div className="hidden w-44 shrink-0 xl:block">
        {loadingBalance ? (
          <div className="flex items-center justify-center gap-1.5 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            查询余额…
          </div>
        ) : balance ? (
          <div>
            <div className="flex items-baseline justify-between gap-2 text-xs tabular-nums font-mono">
              <span
                className={`font-semibold ${
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
              </span>
              <span className="text-muted-foreground text-[11px]">
                {balance.usagePercentage.toFixed(0)}%
              </span>
            </div>
            <Progress value={balance.usagePercentage} className="mt-1 h-1.5" />
          </div>
        ) : (
          <div className="text-center text-[11px] text-muted-foreground/70">
            余额未查询
          </div>
        )}
      </div>

      <div className="hidden w-28 shrink-0 truncate text-right text-xs md:block">
        <div className="truncate font-medium text-foreground/90">
          {formatLastUsed(credential.lastUsedAt)}
        </div>
        <div
          className="truncate text-[10px] tabular-nums font-mono text-muted-foreground/60"
          title={formatCreatedAtFull(credential.createdAt)}
        >
          {formatCreatedAt(credential.createdAt)}
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-1">
        {dispositionButton}
        <Button
          size="icon"
          variant="ghost"
          className={`h-8 w-8 ${dispositionButton ? "hidden" : "hidden sm:inline-flex"}`}
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
        </Button>
        <Button
          size="icon"
          variant="ghost"
          className={`h-8 w-8 ${dispositionButton ? "hidden" : "hidden sm:inline-flex"}`}
          onClick={handleRefreshBalanceClick}
          disabled={loadingBalance || credential.disabled}
          title={credential.disabled ? "已禁用" : "刷新余额"}
        >
          {loadingBalance ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <Wallet className="h-3.5 w-3.5" />
          )}
        </Button>
        <Switch
          checked={!credential.disabled}
          onCheckedChange={handleToggleDisabled}
          disabled={setDisabled.isPending}
          title={credential.disabled ? "启用" : "禁用"}
          className="scale-90"
        />
        <Button
          size="icon"
          variant="ghost"
          className="h-8 w-8"
          onClick={() => setShowEditDialog(true)}
          disabled={privacyMode}
          title={privacyMode ? privacyActionHint : "编辑"}
          aria-label="编辑凭据"
        >
          <Pencil className="h-3.5 w-3.5" />
        </Button>
        {moreMenu}
      </div>
    </div>
  );

  // 标准卡片 View (Default)
  return (
    <>
      {view === "list" ? (
        listView
      ) : (
        <Card
          ref={setNodeRef}
          style={dragStyle}
          data-credential-id={credential.id}
          className={cn(
            "group flex h-full min-w-0 flex-col rounded-2xl border-border/60 transition-[border-color,box-shadow] duration-150 [transform:translateZ(0)]",
            isDragging ? "shadow-md opacity-80" : "hover:border-primary/30 hover:shadow-md",
            selected && "border-primary/60 ring-1 ring-primary/30",
            stateClasses,
          )}
        >
          <CardHeader className="px-4 pb-2 pt-3">
            <div className="flex min-w-0 items-start justify-between gap-3">
              <div className="flex min-w-0 flex-1 items-start gap-2">
                <label
                  data-no-rect-select
                  className="-ml-1 flex size-6 shrink-0 cursor-pointer items-center justify-center rounded-full transition-colors hover:bg-accent"
                  title={selected ? "取消选择" : "选择凭据"}
                  onClick={(e) => e.stopPropagation()}
                >
                  <Checkbox
                    className="size-3.5 rounded-full border-primary/50 [&_svg]:size-2.5"
                    checked={selected}
                    onCheckedChange={handleToggleSelect}
                    disabled={preview}
                    aria-label={`选择凭据 ${credential.id}`}
                  />
                </label>
                <div className="min-w-0 flex-1">
                  <CardTitle title={`凭据 #${credential.id}${displayEmail ? ` · ${displayEmail}` : ""}`}>
                    <CredentialLabel
                      id={credential.id}
                      email={displayEmail}
                      showId={false}
                      className="flex min-w-0 items-baseline gap-1.5 text-sm leading-6 [&>span:last-child]:min-w-0 [&>span:last-child]:truncate"
                    />
                  </CardTitle>
                </div>
              </div>
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={preview || setDisabled.isPending}
                title={credential.disabled ? "点击启用" : "点击禁用"}
                aria-label={credential.disabled ? "启用凭据" : "禁用凭据"}
                className="mt-0.5 shrink-0"
              />
            </div>
            <div className="flex min-w-0 flex-wrap items-center gap-1">
              {subscriptionBadge}
              {authLabel && <Badge variant="secondary">{authLabel}</Badge>}
              {credential.endpoint && (
                <Badge
                  variant="outline"
                  title={[
                    endpointDisplayLabel(credential.endpoint),
                    credential.hasProfileArn
                      ? `${privacyMode ? "Profile ARN 已隐藏" : credential.profileArn ?? "已配置 Profile ARN"}\n同 profile 的账号共享上游 prompt cache`
                      : "",
                  ].filter(Boolean).join("\n")}
                >
                  {credential.endpoint}{credential.hasProfileArn ? " · ARN" : ""}
                </Badge>
              )}
              {credential.sourceChannel && (
                <span className="max-w-24 truncate text-[10px] text-muted-foreground" title={`来源：${displaySourceChannel}`}>
                  来源 {displaySourceChannel}
                </span>
              )}
              <span className="ml-auto text-[10px] tabular-nums text-muted-foreground" title={`凭据 #${credential.id} · ${formatCreatedAtFull(credential.createdAt)}`}>
                添加 {formatCreatedAt(credential.createdAt, false)}
              </span>
            </div>
            {hasStatusBadges && (
              <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                {statusBadges}
              </div>
            )}
          </CardHeader>

          <CardContent className="flex flex-1 flex-col gap-2 px-4 pb-3 pt-0">
            <div className="grid min-w-0 grid-cols-[0.85fr_1.3fr_1fr_1fr] gap-1 py-1">
              <div className="flex min-w-0 flex-col items-start gap-1">
                <span className="text-[11px] text-muted-foreground">
                  {balanced ? "权重" : "优先级"}
                </span>
                {balanced ? (
                  <span
                    className="inline-flex h-5 items-center text-xs font-medium tabular-nums"
                    title="均衡负载权重（可在编辑凭据中修改）"
                  >
                    ×{credential.loadFactor}
                  </span>
                ) : (
                  <button
                    type="button"
                    onClick={() => setEditingPriority(true)}
                    disabled={preview}
                    className="inline-flex h-5 items-center gap-1 rounded text-xs font-medium tabular-nums transition-colors hover:text-primary"
                    title={credential.isCurrent ? "当前调度优先凭据 · 点击编辑优先级" : invertPriority ? "点击编辑优先级（数字越大越先被使用）" : "点击编辑优先级（数字越小越先被使用）"}
                  >
                    {credential.isCurrent && (
                      <Flag className="size-3 shrink-0 fill-primary text-primary" />
                    )}
                    {credential.priority}
                    <Pencil className="size-2.5 text-muted-foreground" />
                  </button>
                )}
              </div>
              <div className="flex min-w-0 flex-col items-start gap-1">
                <span className="text-[11px] text-muted-foreground">
                  失败次数
                </span>
                <button
                  type="button"
                  onClick={() => setShowFailuresDialog(true)}
                  disabled={preview || privacyMode}
                  className="inline-flex min-h-5 max-w-full items-center gap-1 rounded text-xs font-medium tabular-nums transition-colors hover:text-primary"
                  title={privacyMode ? privacyActionHint : failureStats ? `鉴权失败 ${failureStats.auth} / 风控 ${failureStats.throttle} / 其他 ${failureStats.other} · 点击查看失败日志` : `累计失败 ${credential.totalFailureCount} 次 · 点击查看失败日志`}
                >
                  {failureStats ? (
                    <span className="truncate text-[11px]">
                      <span className="text-destructive">{failureStats.auth}</span>
                      <span className="text-muted-foreground/50">/</span>
                      <span className="text-[hsl(var(--rail-warn))]">{failureStats.throttle}</span>
                      <span className="text-muted-foreground/50">/</span>
                      <span className="text-muted-foreground">{failureStats.other}</span>
                    </span>
                  ) : (
                    <span className={cn("truncate", credential.totalFailureCount > 0 && "text-destructive")}>
                      {credential.totalFailureCount}
                    </span>
                  )}
                  <ScrollText className="size-3 shrink-0 text-muted-foreground" />
                </button>
              </div>
              <div className="flex min-w-0 flex-col items-start gap-1">
                <span className="text-[11px] text-muted-foreground">
                  刷新失败
                </span>
                <span className={cn(
                  "inline-flex min-h-5 items-center break-all text-xs font-medium tabular-nums",
                  credential.refreshFailureCount > 0 && "text-destructive",
                )}>
                  {credential.refreshFailureCount}
                </span>
              </div>
              <div className="flex min-w-0 flex-col items-end gap-1">
                <span className="text-[11px] text-muted-foreground">
                  成功次数
                </span>
                <button
                  type="button"
                  onClick={handleResetSuccess}
                  disabled={preview || resetSuccess.isPending}
                  className="inline-flex min-h-5 max-w-full items-center gap-1 rounded text-xs font-medium tabular-nums transition-colors hover:text-primary"
                  title={`成功 ${credential.successCount} 次 · 点击重置成功次数`}
                >
                  <span className="truncate">{credential.successCount}</span>
                  <RotateCcw className="size-3 shrink-0 text-muted-foreground" />
                </button>
              </div>
            </div>

            {editingPriority && (
              <div className="flex flex-wrap items-center gap-2 rounded-lg bg-muted/60 p-2">
                <Input
                  type="number"
                  value={priorityValue}
                  onChange={(event) => setPriorityValue(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") handlePriorityChange();
                    if (event.key === "Escape") {
                      setEditingPriority(false);
                      setPriorityValue(String(credential.priority));
                    }
                  }}
                  className="h-7 w-20"
                  aria-label="优先级"
                  min="0"
                  autoFocus
                />
                <Button size="sm" variant="outline" onClick={handlePriorityChange} disabled={setPriority.isPending}>
                  保存
                </Button>
                <Button size="sm" variant="ghost" onClick={() => {
                  setEditingPriority(false);
                  setPriorityValue(String(credential.priority));
                }}>
                  取消
                </Button>
                <PriorityPreview credentialId={credential.id} draft={priorityValue} disabled={credential.disabled} />
              </div>
            )}

            <div className="flex min-w-0 flex-col gap-1 text-[11px]">
              <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1 text-muted-foreground">
                {activityStats}
                <span title="最近一次 API 调用">
                  最近 <span className="tabular-nums text-foreground/80">{formatLastUsed(credential.lastUsedAt)}</span>
                </span>
              </div>
              {credential.maskedApiKey && (
                <div className="flex min-w-0 items-center justify-between gap-3">
                  <span className="shrink-0 text-muted-foreground">API Key</span>
                  <span className="truncate font-mono text-foreground/80" title={displayApiKey}>
                    {displayApiKey}
                  </span>
                </div>
              )}
              <div className="mt-1 select-text"><CredentialCacheMetric stats={cacheStats} range={cacheRange} loading={loadingCacheStats} failed={cacheStatsError} stale={cacheStatsStale} /></div>
              <div className={cn(
                "flex min-w-0 items-center justify-between gap-3",
                (credential.proxyTemporaryDirect || proxyUnhealthy) && "text-destructive",
              )}>
                <span className="shrink-0 text-muted-foreground">代理</span>
                <span
                  className="truncate text-right font-mono"
                  title={credential.proxyTemporaryDirect
                    ? "临时直连（等待代理恢复）"
                    : credential.hasProxy
                      ? displayProxyUrl
                      : "未配置凭据级代理，使用全局代理设置"}
                >
                  {credential.proxyTemporaryDirect
                    ? "临时直连（等待代理恢复）"
                    : credential.hasProxy
                      ? displayProxyUrl || "已配置"
                      : "跟随全局"}
                </span>
                {proxyEntry && proxyUnhealthy && !credential.proxyTemporaryDirect && (
                  <span className="shrink-0">
                    {proxyEntry.autoDisabled ? "已自动禁用" : `异常 ×${proxyEntry.consecutiveFailures}`}
                  </span>
                )}
              </div>
            </div>

            {(groups.length > 0 || metadataEntries(credential).length > 0) && (
              <div className="flex min-w-0 flex-col gap-1">
                {groups.length > 0 && (
                  <div className="flex min-w-0 flex-wrap items-center gap-1">
                    {privacyMode ? <Badge variant="secondary">分组已隐藏</Badge> : groups.map((group) => (
                      <Badge key={group} variant="secondary" title={`分组：${group}`} className="min-w-0 max-w-full">
                        <span className="truncate">{group}</span>
                      </Badge>
                    ))}
                  </div>
                )}
                <MetadataSummary credential={credential} wrap privacyMode={privacyMode} />
              </div>
            )}

            <div className="mt-auto flex flex-col gap-2 border-t border-border/40 pt-2.5">

              {loadingBalance ? (
                <div role="status" className="flex min-h-16 items-center justify-center gap-2 text-xs text-muted-foreground">
                  <Loader2 className="size-4 animate-spin" />
                  正在查询最新余额…
                </div>
              ) : balance ? (
                <>
                  <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1">
                    <div className="flex min-w-0 flex-wrap items-baseline gap-x-1.5 gap-y-1">
                      <span className="text-[11px] text-muted-foreground">余额</span>
                      <span
                        className={cn(
                          "console-num break-all text-xl font-semibold tracking-tight tabular-nums",
                          balance.remaining < 0
                            ? "text-destructive"
                            : balance.remaining === 0
                              ? "text-[hsl(var(--rail-warn))]"
                              : "text-[hsl(var(--rail-ok))]",
                        )}
                      >
                        {balance.remaining < 0
                          ? `-$${formatNumber(Math.abs(balance.remaining))}`
                          : `$${formatNumber(balance.remaining)}`}
                      </span>
                    </div>
                    <OverageStatusPill balance={balance} />
                  </div>
                  <Progress value={balance.usagePercentage} className="h-1" aria-label="额度已用比例" />
                  <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1 text-[10px] leading-4 tabular-nums text-muted-foreground">
                    <span>已用 ${formatNumber(balance.currentUsage)} <span className="ml-1">{balance.usagePercentage.toFixed(1)}%</span></span>
                    <span>额度 ${formatNumber(balance.usageLimit)}</span>
                    <span title={balance.nextResetAt ? new Date(balance.nextResetAt * 1000).toLocaleString("zh-CN") : undefined}>
                      重置 {formatResetDate(balance.nextResetAt)}
                    </span>
                  </div>
                </>
              ) : (
                <div className="flex min-h-16 flex-wrap items-center justify-between gap-2 text-xs">
                  <span className="text-muted-foreground">尚未获取余额数据</span>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={handleRefreshBalanceClick}
                    disabled={preview || credential.disabled}
                  >
                    <Wallet data-icon="inline-start" />
                    查询余额
                  </Button>
                </div>
              )}
            </div>
          </CardContent>
          <CardFooter className="mx-4 mt-auto min-h-11 flex-wrap gap-1 border-t border-border/40 px-0 py-1.5">
            {preview ? (
              <>
                <Button size="icon" variant="ghost" className="size-8" disabled title="预览 · 拖拽排序">
                  <GripVertical />
                </Button>
                <Button size="icon" variant="ghost" className="size-8" disabled title="预览 · 刷新 Token">
                  <RefreshCw />
                </Button>
                <Button size="icon" variant="ghost" className="size-8" disabled title="预览 · 刷新余额">
                  <Wallet />
                </Button>
                <span className="flex-1" />
                <Button size="icon" variant="outline" className="size-8 rounded-full" disabled title="预览 · 编辑">
                  <Pencil />
                </Button>
                <Button size="icon" variant="ghost" className="size-8" disabled title="预览 · 更多操作">
                  <MoreHorizontal />
                </Button>
              </>
            ) : (
              <>
                {!dragDisabled && (
                  <Button
                    ref={setActivatorNodeRef}
                    size="icon"
                    variant="ghost"
                    data-no-rect-select
                    className="size-8 shrink-0 cursor-grab touch-none active:cursor-grabbing"
                    title="拖拽排序"
                    aria-label="拖拽排序"
                    {...attributes}
                    {...listeners}
                  >
                    <GripVertical />
                  </Button>
                )}
                <Button
                  size="icon"
                  variant="ghost"
                  className={cn("size-8 shrink-0", forceRefresh.isPending && "[&_svg]:animate-spin")}
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
                  aria-label="强制刷新 Token"
                >
                  <RefreshCw />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className={cn("size-8 shrink-0", loadingBalance && "[&_svg]:animate-spin")}
                  onClick={handleRefreshBalanceClick}
                  disabled={loadingBalance || credential.disabled}
                  title={credential.disabled ? "已禁用" : "刷新余额"}
                  aria-label="刷新余额"
                >
                  {loadingBalance ? <Loader2 /> : <Wallet />}
                </Button>
                <span className="min-w-0 flex-1" />
                {dispositionButton}
                <Button
                  size="icon"
                  variant="outline"
                  className="size-8 shrink-0 rounded-full"
                  onClick={() => setShowEditDialog(true)}
                  disabled={privacyMode}
                  title={privacyMode ? privacyActionHint : "编辑"}
                  aria-label="编辑凭据"
                >
                  <Pencil />
                </Button>
                {moreMenu}
              </>
            )}
          </CardFooter>
        </Card>
      )}

      {/* 弹窗 Dialog 组件按需惰性挂载，避免关闭时无谓实例化 */}
      {showDeleteDialog && (
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
                disabled={deleteCredential.isPending}
              >
                确认删除
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}

      {showEditDialog && !privacyMode && (
        <EditCredentialDialog
          open={showEditDialog}
          onOpenChange={setShowEditDialog}
          credential={credential}
          metadataSchema={metadataSchema}
        />
      )}
      {showUpdateTokenDialog && !privacyMode && (
        <UpdateTokenDialog
          open={showUpdateTokenDialog}
          onOpenChange={setShowUpdateTokenDialog}
          credential={credential}
        />
      )}
      {showReloginDialog && !privacyMode && (
        <ReloginDialog
          open={showReloginDialog}
          onOpenChange={setShowReloginDialog}
          credential={credential}
        />
      )}
      {showFailuresDialog && !privacyMode && (
        <CredentialFailuresDialog
          open={showFailuresDialog}
          onOpenChange={setShowFailuresDialog}
          credentialId={credential.id}
          email={credential.email}
        />
      )}
      {showModelsDialog && (
        <AvailableModelsDialog
          open={showModelsDialog}
          onOpenChange={setShowModelsDialog}
          credentialId={credential.id}
        />
      )}
      {showBalanceDialog && (
        <BalanceDialog
          open={showBalanceDialog}
          onOpenChange={setShowBalanceDialog}
          credentialId={showBalanceDialog ? credential.id : null}
        />
      )}
    </>
  );
}

export const CredentialCard = memo(CredentialCardImpl);
