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
  ChevronRight,
  Activity,
  Key,
  Globe,
  Server,
  Layers,
  Sparkles,
  Flag,
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
import type {
  CredentialMetadataSchema,
  CredentialStatusItem,
  BalanceResponse,
  ProxyPoolEntry,
} from "@/types/api";
import { maskProxyUrl, extractErrorMessage, overageFailureMessage, cn } from "@/lib/utils";
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useResetSuccessCount,
  useClearThrottle,
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
import {
  railBorderClass,
  railChipClass,
} from "@/components/console/rail";
import { PriorityPreview } from "@/components/console/priority-preview";
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
  /** 展示形态：卡片（默认）或紧凑列表行 */
  view?: "card" | "list";
  /** 字段排序开启时禁用拖拽调优先级（隐藏拖拽手柄） */
  dragDisabled?: boolean;
  /** 开发预览卡：仅展示，不发起任何凭据操作。 */
  preview?: boolean;
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
function formatCreatedAt(createdAt: string | null | undefined): string {
  if (!createdAt) return "未知";
  const date = new Date(createdAt);
  if (Number.isNaN(date.getTime())) return "未知";
  return date.toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
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

function proxyDisplayLabel(proxyUrl: string): string {
  try {
    const { host } = new URL(proxyUrl);
    return host || maskProxyUrl(proxyUrl);
  } catch {
    return maskProxyUrl(proxyUrl);
  }
}

function endpointDisplayLabel(endpoint: string): string {
  if (endpoint === "ide") return "IDE 端点";
  if (endpoint === "cli") return "CLI 端点";
  return endpoint;
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



/** 账目行 —— 标签 + 描述在左，值贴右边缘 */
function LedgerRow({
  label,
  hint,
  title,
  description,
  icon: Icon,
  danger,
  children,
}: {
  label: string;
  hint?: string;
  title?: string;
  description?: string;
  icon?: React.ElementType;
  danger?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div className={cn(
      "grid min-w-0 grid-cols-[auto_minmax(0,1fr)] items-baseline gap-x-2.5 gap-y-0.5 py-0.5 -mx-2 px-2 rounded",
      danger && "bg-destructive/10 border border-destructive/30",
    )}>
      <dt className="flex shrink-0 flex-col" title={title}>
        <span className="flex items-center gap-1.5">
          {Icon && <Icon className="h-3.5 w-3.5 text-muted-foreground/70" />}
          <span className="text-[12px] font-normal tracking-normal text-muted-foreground">
            {label}
          </span>
          {hint && (
            <span className="text-[10px] text-muted-foreground/50">{hint}</span>
          )}
        </span>
        {description && (
          <span className="text-[10px] leading-tight text-muted-foreground/50 mt-0.5">
            {description}
          </span>
        )}
      </dt>
      <dd className="min-w-0 break-all text-right text-[12px] leading-4 text-foreground/90 font-medium">
        {children}
      </dd>
    </div>
  );
}

function CardSectionTitle({
  children,
  icon: Icon,
}: {
  children: React.ReactNode;
  icon?: React.ElementType;
}) {
  return (
    <h3 className="flex items-center gap-1.5 text-[11px] font-semibold leading-4 uppercase tracking-wider text-muted-foreground/80">
      {Icon && <Icon className="h-3.5 w-3.5 opacity-70" />}
      {children}
    </h3>
  );
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
      return { label: "失败过多", variant: "destructive" };
    case "Suspended":
      return { label: "账号封禁", variant: "destructive" };
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

function MetadataSummary({
  credential,
}: {
  credential: CredentialStatusItem;
}) {
  const entries = metadataEntries(credential);
  if (entries.length === 0) return null;

  return (
    <div
      className="mt-1 flex min-w-0 items-center gap-1 overflow-hidden"
      aria-label="凭据 Metadata"
    >
      {entries.map((entry) => (
        <span
          key={entry.key}
          className={`inline-flex min-w-0 max-w-full shrink-0 items-center overflow-hidden rounded-md border px-1.5 py-0.5 text-[11px] ${
            entry.emphasized
              ? "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300"
              : "border-border/60 bg-muted/45 text-foreground"
          }`}
          title={entry.description || `${entry.label}: ${entry.value}`}
        >
          <span className="shrink-0 text-muted-foreground">{entry.label}</span>
          <span className="mx-1 text-border">·</span>
          <span className="max-w-40 truncate font-medium">{entry.value}</span>
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
  view = "card",
  dragDisabled = false,
  preview = false,
  metadataSchema,
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
  const [showBalanceDialog, setShowBalanceDialog] = useState(false);
  const [connectionExpanded, setConnectionExpanded] = useState(false);

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
      toast.error("优先级要填 0 或更大的整数，0 最先被使用");
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
        (disposition.action === "clearThrottle" && clearThrottle.isPending) ||
        (disposition.action === "enable" && setDisabled.isPending) ||
        (disposition.action === "refreshToken" && forceRefresh.isPending)
      }
      title={`${disposition.stateLabel} → ${disposition.actionLabel}`}
      className="h-7 whitespace-nowrap px-3 text-xs font-medium border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300 hover:bg-amber-500/20"
    >
      <Sparkles className="mr-1 h-3 w-3" />
      {disposition.actionLabel}
    </Button>
  ) : null;

  const stateClasses = [
    credential.isCurrent ? "ring-2 ring-primary/60 shadow-apple-lg" : "",
    !credential.disabled && isQuotaExceeded ? "ring-1 ring-amber-500/60" : "",
    disabledByQuota
      ? "ring-1 ring-amber-500/70 bg-amber-50/40 dark:bg-amber-500/[0.04]"
      : "",
    isThrottled
      ? "ring-1 ring-orange-500/60 bg-orange-50/40 dark:bg-orange-500/[0.04]"
      : "",
    credential.disabled && !disabledByQuota ? "opacity-75" : "",
  ]
    .filter(Boolean)
    .join(" ");

  // 仅在有异常状态时呈现精简的状态标签，避免常规状态下出现杂乱胶囊
  const statusBadges = (
    <>
      {credential.disabled && reasonStyle && (
        <Badge variant={reasonStyle.variant} className="text-[11px]">
          已禁用 · {reasonStyle.label}
        </Badge>
      )}
      {credential.disabled && !reasonStyle && (
        <Badge variant="destructive" className="text-[11px]">已禁用</Badge>
      )}
      {!credential.disabled && isQuotaExceeded && (
        <Badge variant="warning" className="text-[11px]">已超额</Badge>
      )}
      {isThrottled && (
        <Badge
          variant="warning"
          className="bg-orange-500/15 text-orange-700 dark:text-orange-300 border-orange-500/30 text-[11px]"
          title="账号级风控冷却中"
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

  const metadataItems = metadataEntries(credential);
  const renderMetadataRows = (items: typeof metadataItems) =>
    items.map((entry) => (
      <LedgerRow
        key={entry.key}
        label={entry.label}
        description={entry.description}
        title={entry.description || (entry.key === entry.label ? undefined : `key: ${entry.key}`)}
      >
        <span
          className={`font-medium ${
            entry.emphasized ? "text-amber-600 dark:text-amber-400" : ""
          }`}
        >
          {entry.value}
        </span>
      </LedgerRow>
    ));
  const metadataRows = renderMetadataRows(metadataItems);

  const groups = credential.groups ?? [];
  const groupingBlock =
    groups.length > 0 ? (
      <div className="flex min-w-0 flex-wrap items-center gap-1.5">
        <span className="mr-0.5 text-[11px] text-muted-foreground/80">分组</span>
        {groups.map((g) => (
          <span
            key={g}
            title="账号分组"
            className="inline-flex max-w-full items-center break-all rounded-md bg-secondary/80 px-2 py-0.5 text-[11px] font-medium text-secondary-foreground"
          >
            {g}
          </span>
        ))}
      </div>
    ) : null;

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
            解除风控冷却（{formatThrottleCountdown(throttleRemaining)}）
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
          <DropdownMenuItem onSelect={() => setShowReloginDialog(true)}>
            <LogIn className="mr-2 h-4 w-4" />
            重新登录
          </DropdownMenuItem>
        )}
        {credential.authMethod !== "api_key" && (
          <DropdownMenuItem onSelect={() => setShowUpdateTokenDialog(true)}>
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
      className={`group flex min-w-0 items-center gap-2 rounded-2xl border bg-card/90 px-3 py-2.5 transition-all sm:gap-3.5 sm:px-4 ${railBorderClass(
        disposition.tone,
      )} ${
        isDragging
          ? "shadow-apple-lg opacity-80"
          : "hover:bg-accent/30 hover:shadow-apple-sm"
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
          <CredentialLabel id={credential.id} email={credential.email} />
        </div>
        <div className="mt-1 flex min-w-0 items-center gap-1.5 overflow-hidden [&>*]:shrink-0">
          {statusBadges}
          {groups.map((g) => (
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
              {credential.sourceChannel}
            </span>
          )}
        </div>
        <MetadataSummary credential={credential} />
      </div>

      <div className="hidden shrink-0 items-center gap-6 lg:flex">
        <div className="relative w-16 shrink-0 text-center">
          <div
            className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80"
            title="优先级：数字越小越先被使用，0 最先"
          >
            优先级 ↑
          </div>
          <div className="mt-0.5 flex h-[26px] items-center justify-center">
            {editingPriority ? (
              <div className="absolute left-1/2 top-1/2 z-30 -translate-x-1/2 -translate-y-1/2 rounded-xl border border-border/70 bg-popover/95 p-2 shadow-apple-lg backdrop-blur-md">
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
            className="mt-0.5 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-xs font-semibold tabular-nums transition-colors hover:bg-accent"
            title="鉴权失败 / 风控 / 其他。点击查看日志"
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
          title="编辑"
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
          className={`group flex h-full min-w-0 flex-col overflow-hidden rounded-2xl border-border/70 bg-gradient-to-b from-card via-card/95 to-card/90 shadow-apple transition-all duration-200 backdrop-blur-md ${
            isDragging ? "shadow-apple-lg opacity-80 scale-[1.01]" : "hover:shadow-apple-lg hover:-translate-y-0.5"
          } ${stateClasses}`}
        >
          {/* Card Header: 选择框 + Title + 呼吸指示 + 禁用开关 */}
          <CardHeader className="p-4 pb-3 sm:p-4 sm:pb-3 border-b border-border/40 bg-muted/20">
            <div className="flex min-w-0 items-center justify-between gap-2">
              <div className="flex min-w-0 flex-1 items-center gap-2.5">
                <label
                  data-no-rect-select
                  className="flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded-md transition-colors hover:bg-accent"
                  title={selected ? "取消选择" : "选择凭据"}
                  onClick={(e) => e.stopPropagation()}
                >
                  <Checkbox
                    className="h-4 w-4 [&_svg]:h-3 [&_svg]:w-3"
                    checked={selected}
                    onCheckedChange={handleToggleSelect}
                    disabled={preview}
                  />
                </label>
                <CardTitle className="min-w-0 flex-1 text-sm font-semibold leading-snug">
                  <CredentialLabel
                    id={credential.id}
                    email={credential.email}
                    showId={false}
                    className="flex w-full min-w-0 items-baseline gap-1.5 [&>span:last-child]:min-w-0 [&>span:last-child]:truncate font-mono"
                  />
                </CardTitle>
              </div>

              <div className="flex shrink-0 items-center gap-2">
                <Switch
                  checked={!credential.disabled}
                  onCheckedChange={handleToggleDisabled}
                  disabled={preview || setDisabled.isPending}
                  title={credential.disabled ? "点击启用" : "点击禁用"}
                  className="scale-90"
                />
              </div>
            </div>

        {hasStatusBadges && (
          <div className="mt-2 flex min-w-0 flex-wrap items-center gap-1.5">
            {statusBadges}
          </div>
        )}
      </CardHeader>

          <CardContent className="flex flex-1 flex-col p-4 space-y-3.5">
            {/* 核心指标 (Metrics Grid) */}
            <div className="grid grid-cols-3 divide-x divide-border/30 text-center py-1">
              {/* Priority */}
              <div className="flex flex-col items-center justify-center px-1">
                <span className="text-[10px] font-semibold text-muted-foreground/80 uppercase tracking-wider">
                  优先级
                </span>
                {editingPriority ? (
                  <div className="mt-1 flex items-center justify-center gap-0.5">
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
                      className="h-6 w-12 text-center text-xs font-mono p-0"
                      min="0"
                      autoFocus
                    />
                    <button
                      type="button"
                      onClick={handlePriorityChange}
                      className="text-xs text-emerald-600 font-bold px-1"
                    >
                      ✓
                    </button>
                  </div>
                ) : (
                  <button
                    type="button"
                    onClick={() => {
                      if (!preview) setEditingPriority(true);
                    }}
                    className={`mt-0.5 inline-flex items-center gap-1 rounded-md px-2 py-0.5 font-mono text-xs font-semibold border transition-colors hover:brightness-105 ${railChipClass(disposition.tone)}`}
                    title={credential.isCurrent ? "当前调度优先凭据 · 点击编辑优先级" : "点击编辑优先级（数字越小越先被使用）"}
                  >
                    {credential.isCurrent && (
                      <Flag className="h-3 w-3 fill-emerald-500 text-emerald-500 shrink-0" />
                    )}
                    #{credential.priority}
                    <Pencil className="h-2.5 w-2.5 opacity-60" />
                  </button>
                )}
              </div>

              {/* Success */}
              <div className="flex flex-col items-center justify-center px-1">
                <span className="text-[10px] font-semibold text-muted-foreground/80 uppercase tracking-wider">
                  成功数
                </span>
                <button
                  type="button"
                  onClick={preview ? undefined : handleResetSuccess}
                  disabled={preview}
                  className="mt-0.5 inline-flex items-center gap-1 font-mono text-xs font-semibold text-emerald-600 dark:text-emerald-400 hover:text-emerald-500 transition-colors"
                  title="点击重置成功次数"
                >
                  {credential.successCount}
                  <RotateCcw className="h-2.5 w-2.5 opacity-50" />
                </button>
              </div>

              {/* Failures */}
              <div className="flex flex-col items-center justify-center px-1">
                <span className="text-[10px] font-semibold text-muted-foreground/80 uppercase tracking-wider">
                  失败数
                </span>
                <button
                  type="button"
                  onClick={preview ? undefined : () => setShowFailuresDialog(true)}
                  disabled={preview}
                  className="mt-0.5 inline-flex items-center gap-1 font-mono text-xs font-semibold hover:text-primary transition-colors"
                  title="查看失败日志"
                >
                  {failureStats ? (
                    <span className="text-[11px]">
                      <span className="text-destructive">{failureStats.auth}</span>
                      <span className="text-muted-foreground/40">/</span>
                      <span className="text-amber-600 dark:text-amber-400">{failureStats.throttle}</span>
                      <span className="text-muted-foreground/40">/</span>
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
                  <ScrollText className="h-2.5 w-2.5 opacity-50" />
                </button>
              </div>
            </div>

            {/* Status & Ledger Section */}
            <div className="space-y-1 border-t border-border/40 pt-3 px-3 text-[12px]">
              <CardSectionTitle icon={Activity}>运行与账号信息</CardSectionTitle>
              {groupingBlock && <div className="py-1">{groupingBlock}</div>}
              <LedgerRow label="凭据类型">
                <span className="font-semibold">{authLabel}</span>
              </LedgerRow>
              <LedgerRow label="最近调用">
                <span className="font-mono text-muted-foreground">
                  {formatLastUsed(credential.lastUsedAt)}
                </span>
              </LedgerRow>
              <LedgerRow label="添加时间">
                <span
                  className="font-mono text-muted-foreground/80"
                  title={formatCreatedAtFull(credential.createdAt)}
                >
                  {formatCreatedAt(credential.createdAt)}
                </span>
              </LedgerRow>
              {metadataRows}
            </div>

            {/* Usage & Quota Card (余额与额度) */}
            <div
              className={`rounded-xl border p-3 transition-all space-y-2 ${
                isQuotaExceeded || disabledByQuota
                  ? "border-amber-500/50 bg-amber-500/[0.04]"
                  : "border-border/60 bg-secondary/20"
              }`}
            >
              <div className="flex items-center justify-between gap-2">
                <CardSectionTitle icon={Wallet}>额度与用量</CardSectionTitle>
                <div className="flex items-center gap-1.5">
                  {subscriptionBadge}
                  {balance && <OverageStatusPill balance={balance} />}
                </div>
              </div>

              {loadingBalance ? (
                <div className="flex items-center justify-center gap-2 py-3 text-xs text-muted-foreground">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  正在查询最新余额…
                </div>
              ) : balance ? (
                <div className="space-y-2">
                  <div className="flex items-baseline justify-between gap-2">
                    <div>
                      <div className="text-[10px] font-semibold text-muted-foreground uppercase">
                        剩余可用
                      </div>
                      <div
                        className={`console-num font-mono text-xl font-semibold tracking-tight ${
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
                    <div className="text-right font-mono">
                      <div className="text-[10px] font-semibold text-muted-foreground uppercase">
                        已用比例
                      </div>
                      <div className="text-sm font-semibold text-foreground">
                        {balance.usagePercentage.toFixed(1)}%
                      </div>
                    </div>
                  </div>

                  <Progress value={balance.usagePercentage} className="h-1.5 bg-muted" />

                  <div className="grid grid-cols-2 gap-1 text-[11px] font-mono text-muted-foreground pt-1 border-t border-border/30">
                    <div>已用: ${formatNumber(balance.currentUsage)}</div>
                    <div className="text-right">上限: ${formatNumber(balance.usageLimit)}</div>
                  </div>

                  {balance.nextResetAt && (
                    <div className="flex items-center justify-between text-[11px] font-mono text-muted-foreground/80 pt-0.5">
                      <span>下次重置</span>
                      <span>{formatResetDate(balance.nextResetAt)}</span>
                    </div>
                  )}
                </div>
              ) : (
                <div className="flex items-center justify-between py-1 text-xs">
                  <span className="text-muted-foreground">尚未获取余额数据</span>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 px-2.5 text-xs font-medium"
                    onClick={handleRefreshBalanceClick}
                  >
                    <Wallet className="mr-1.5 h-3.5 w-3.5" />
                    查询余额
                  </Button>
                </div>
              )}
            </div>

            {/* Connection Details 手风琴展开面板 */}
            <div className="rounded-xl border border-border/40 bg-card/40 overflow-hidden">
              <button
                type="button"
                className="flex w-full items-center justify-between px-3 py-2 text-left hover:bg-accent/40 transition-colors"
                onClick={() => setConnectionExpanded((expanded) => !expanded)}
                aria-expanded={connectionExpanded}
              >
                <CardSectionTitle icon={Server}>连接与代理详情</CardSectionTitle>
                <ChevronRight
                  className={`h-4 w-4 text-muted-foreground transition-transform duration-200 ${
                    connectionExpanded ? "rotate-90" : ""
                  }`}
                />
              </button>
              {connectionExpanded && (
                <div className="px-3 pb-2.5 pt-1 space-y-1 border-t border-border/30 text-[12px]">
                  {credential.endpoint && (
                    <LedgerRow label="端点" icon={Server}>
                      <span>{endpointDisplayLabel(credential.endpoint)}</span>
                    </LedgerRow>
                  )}
                  {credential.hasProfileArn && (
                    <LedgerRow label="Profile ARN" icon={Layers}>
                      <span className="text-emerald-600 dark:text-emerald-400">已配置</span>
                    </LedgerRow>
                  )}
                  {credential.sourceChannel && (
                    <LedgerRow label="来源" icon={Globe}>
                      <span>{credential.sourceChannel}</span>
                    </LedgerRow>
                  )}
                  {credential.maskedApiKey && (
                    <LedgerRow label="API Key" icon={Key}>
                      <span className="font-mono text-xs text-muted-foreground">{credential.maskedApiKey}</span>
                    </LedgerRow>
                  )}
                  {credential.hasProxy && (
                    <LedgerRow label="代理地址" icon={Globe} danger={proxyUnhealthy}>
                      <span
                        className="font-mono text-xs"
                        title={maskProxyUrl(credential.proxyUrl ?? "")}
                      >
                        {proxyDisplayLabel(credential.proxyUrl ?? "")}
                      </span>
                      {proxyEntry && proxyUnhealthy && (
                        <span className="ml-1.5 text-[11px] font-medium text-destructive">
                          {proxyEntry.autoDisabled ? '已自动禁用' : `异常 ×${proxyEntry.consecutiveFailures}`}
                        </span>
                      )}
                    </LedgerRow>
                  )}
                </div>
              )}
            </div>

            {/* 底栏 ToolBar */}
            {preview ? (
              <div className="mt-auto flex items-center justify-end gap-2 pt-2.5 border-t border-border/40">
                <Button size="icon" variant="ghost" className="h-8 w-8" disabled title="预览">
                  <RefreshCw className="h-4 w-4" />
                </Button>
                <Button size="icon" variant="ghost" className="h-8 w-8" disabled title="预览">
                  <Wallet className="h-4 w-4" />
                </Button>
                <Button size="icon" variant="ghost" className="h-8 w-8" disabled title="预览">
                  <Pencil className="h-4 w-4" />
                </Button>
                <Button size="icon" variant="ghost" className="h-8 w-8" disabled title="预览">
                  <MoreHorizontal className="h-4 w-4" />
                </Button>
              </div>
            ) : (
              <div className="mt-auto flex min-w-0 items-center gap-2 pt-2.5 border-t border-border/40">
                {!dragDisabled && (
                  <Button
                    ref={setActivatorNodeRef}
                    size="icon"
                    variant="ghost"
                    data-no-rect-select
                    className="h-8 w-8 shrink-0 cursor-grab touch-none active:cursor-grabbing hover:bg-accent"
                    title="拖拽排序"
                    {...attributes}
                    {...listeners}
                  >
                    <GripVertical className="h-4 w-4 text-muted-foreground/70" />
                  </Button>
                )}
                {dispositionButton}
                <span className="min-w-0 flex-1" />
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 shrink-0 hover:bg-accent"
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
                    className={`h-4 w-4 ${forceRefresh.isPending ? "animate-spin" : ""}`}
                  />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 shrink-0 hover:bg-accent"
                  onClick={handleRefreshBalanceClick}
                  disabled={loadingBalance || credential.disabled}
                  title={credential.disabled ? "已禁用" : "刷新余额"}
                >
                  {loadingBalance ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Wallet className="h-4 w-4" />
                  )}
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 shrink-0 hover:bg-accent"
                  onClick={() => setShowEditDialog(true)}
                  title="编辑"
                >
                  <Pencil className="h-4 w-4" />
                </Button>
                {moreMenu}
              </div>
            )}
          </CardContent>
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

      {showEditDialog && (
        <EditCredentialDialog
          open={showEditDialog}
          onOpenChange={setShowEditDialog}
          credential={credential}
          metadataSchema={metadataSchema}
        />
      )}
      {showUpdateTokenDialog && (
        <UpdateTokenDialog
          open={showUpdateTokenDialog}
          onOpenChange={setShowUpdateTokenDialog}
          credential={credential}
        />
      )}
      {showReloginDialog && (
        <ReloginDialog
          open={showReloginDialog}
          onOpenChange={setShowReloginDialog}
          credential={credential}
        />
      )}
      {showFailuresDialog && (
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
