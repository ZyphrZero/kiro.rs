import { useState, useEffect, useRef } from "react";
import {
  RefreshCw,
  LogOut,
  Moon,
  Sun,
  Server,
  Plus,
  Upload,
  FileUp,
  FileDown,
  Trash2,
  RotateCcw,
  CheckCircle2,
  Globe,
  LogIn,
  Key,
  Building2,
  Settings,
  UploadCloud,
  MoreHorizontal,
  Activity,
  ChevronLeft,
  ChevronRight,
  AlertTriangle,
  Eye,
  EyeOff,
  Copy,
  Wand2,
  Zap,
  Tags,
  ChevronDown,
  LayoutGrid,
  List,
  Gauge,
  Search,
  X,
} from "lucide-react";

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
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { storage, type CredentialView } from "@/lib/storage";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { CredentialCard } from "@/components/credential-card";
import { ConcurrencyMonitor } from "@/components/concurrency-monitor";
import { AddCredentialDialog } from "@/components/add-credential-dialog";
import { BatchImportDialog } from "@/components/batch-import-dialog";
import { BatchEditCredentialDialog } from "@/components/batch-edit-credential-dialog";
import { IdcLoginDialog } from "@/components/idc-login-dialog";
import { SocialLoginDialog } from "@/components/social-login-dialog";
import { KamImportDialog } from "@/components/kam-import-dialog";
import {
  BatchVerifyDialog,
  type VerifyResult,
} from "@/components/batch-verify-dialog";
import { detectTier, type Tier } from "@/components/subscription-badge";
import { ProxyPoolDialog } from "@/components/proxy-pool-dialog";
import { ImageUpdateDialog } from "@/components/image-update-dialog";
import { useConfirm } from "@/components/ui/confirm-dialog";
import {
  useCredentials,
  useDeleteCredential,
  useResetFailure,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useResetAllSuccessCount,
  useSetPriority,
} from "@/hooks/use-credentials";
import { useUpdateCheck } from "@/hooks/use-update-check";
import { useFailureStats } from "@/hooks/use-traces";
import { useGroupOptions } from "@/hooks/use-groups";
import { useRectSelect } from "@/hooks/use-rect-select";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DndContext,
  PointerSensor,
  useSensor,
  useSensors,
  closestCenter,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  rectSortingStrategy,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import {
  getCredentialBalance,
  forceRefreshToken,
  disableQuotaExceeded,
  enableOverageForAllCapable,
  exportKamCredentials,
  updateAdminKey,
} from "@/api/credentials";
import {
  extractErrorMessage,
  parseError,
  generateApiKey,
  formatNumber,
  overageFailureMessage,
} from "@/lib/utils";
import type { BalanceResponse } from "@/types/api";

interface DashboardProps {
  onLogout: () => void;
  /** 当作为 Tab 嵌入到 App 中时为 true：隐藏自带顶栏与外层布局，由父 App 提供 */
  embedded?: boolean;
}

// 订阅分级筛选的可选项（key 与 detectTier 返回值一致）
const TIER_OPTIONS: { value: Tier; label: string }[] = [
  { value: "free", label: "FREE" },
  { value: "pro", label: "PRO" },
  { value: "pro_plus", label: "PRO+" },
  { value: "power", label: "POWER" },
  { value: "unknown", label: "未知/未查询" },
];
const TIER_LABELS: Record<Tier, string> = {
  free: "FREE",
  pro: "PRO",
  pro_plus: "PRO+",
  power: "POWER",
  unknown: "未知",
};

// 每页数量可选项；另有“全部”（pageSize = 0）由下拉单独追加
const PAGE_SIZE_OPTIONS = [12, 24, 48, 96] as const;

export function Dashboard({ onLogout, embedded = false }: DashboardProps) {
  const confirm = useConfirm();
  const [addDialogOpen, setAddDialogOpen] = useState(false);
  const [batchImportDialogOpen, setBatchImportDialogOpen] = useState(false);
  const [batchEditDialogOpen, setBatchEditDialogOpen] = useState(false);
  const [idcLoginDialogOpen, setIdcLoginDialogOpen] = useState(false);
  const [enterpriseLoginDialogOpen, setEnterpriseLoginDialogOpen] =
    useState(false);
  const [socialLoginDialogOpen, setSocialLoginDialogOpen] = useState(false);
  const [kamImportDialogOpen, setKamImportDialogOpen] = useState(false);
  const [proxyPoolDialogOpen, setProxyPoolDialogOpen] = useState(false);
  const [imageUpdateDialogOpen, setImageUpdateDialogOpen] = useState(false);
  const [adminKeyDialogOpen, setAdminKeyDialogOpen] = useState(false);
  const [newAdminKey, setNewAdminKey] = useState("");
  const [updatingAdminKey, setUpdatingAdminKey] = useState(false);
  const [showAdminKeyPlain, setShowAdminKeyPlain] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set());
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [verifyDeleting, setVerifyDeleting] = useState(false);
  const [verifyProgress, setVerifyProgress] = useState({
    current: 0,
    total: 0,
  });
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(
    new Map(),
  );
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(
    new Map(),
  );
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(
    new Set(),
  );
  const [queryingInfo, setQueryingInfo] = useState(false);
  const [queryInfoProgress, setQueryInfoProgress] = useState({
    current: 0,
    total: 0,
  });
  const [batchRefreshing, setBatchRefreshing] = useState(false);
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({
    current: 0,
    total: 0,
  });
  const cancelVerifyRef = useRef(false);
  const [currentPage, setCurrentPage] = useState(1);
  // 展示形态（卡片 / 列表）与每页数量，均持久化到 localStorage
  const [viewMode, setViewMode] = useState<CredentialView>(() =>
    storage.getCredentialView(),
  );
  const [pageSize, setPageSize] = useState<number>(() =>
    storage.getCredentialPageSize(),
  );
  const changeViewMode = (v: CredentialView) => {
    setViewMode(v);
    storage.setCredentialView(v);
  };
  const changePageSize = (n: number) => {
    setPageSize(n);
    storage.setCredentialPageSize(n);
    setCurrentPage(1);
  };
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== "undefined") {
      return document.documentElement.classList.contains("dark");
    }
    return false;
  });

  const queryClient = useQueryClient();
  // 监控视图需要近实时数据，缩短轮询到 3s；其余视图维持 30s。
  const { data, isLoading, error, refetch } = useCredentials(
    viewMode === "monitor" ? 3000 : 30000,
  );
  const { mutate: deleteCredential } = useDeleteCredential();
  const { mutate: resetFailure } = useResetFailure();
  const { data: loadBalancingData, isLoading: isLoadingMode } =
    useLoadBalancingMode();
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } =
    useSetLoadBalancingMode();
  const resetAllSuccess = useResetAllSuccessCount();
  const setPriority = useSetPriority();
  const { data: updateCheck } = useUpdateCheck();
  const { data: failureStatsMap } = useFailureStats();
  const groupOptions = useGroupOptions();

  // 分组筛选：'' = 全部；'__none__' = 仅显示未分组；其他 = 按分组名筛选
  const [groupFilter, setGroupFilter] = useState<string>("");
  // 订阅分级筛选（多选）：空集合 = 全部分级；否则只显示集合内的分级
  const [tierFilter, setTierFilter] = useState<Set<Tier>>(new Set());
  // 模糊搜索：按来源渠道（备注）/ 邮箱做大小写不敏感的子串匹配；空串 = 不限
  const [searchQuery, setSearchQuery] = useState("");
  const toggleTier = (t: Tier) => {
    setTierFilter((prev) => {
      const next = new Set(prev);
      if (next.has(t)) next.delete(t);
      else next.add(t);
      return next;
    });
  };

  // 应用分组 + 分级筛选后的凭据全集（分页前先过滤，确保翻页粒度正确）
  const filteredCredentials = (() => {
    const all = data?.credentials ?? [];
    let out = all;
    if (groupFilter) {
      out =
        groupFilter === "__none__"
          ? out.filter((c) => !c.groups || c.groups.length === 0)
          : out.filter((c) => c.groups?.includes(groupFilter));
    }
    if (tierFilter.size > 0) {
      out = out.filter((c) =>
        tierFilter.has(detectTier(c.balance?.subscriptionTitle)),
      );
    }
    const q = searchQuery.trim().toLowerCase();
    if (q) {
      out = out.filter(
        (c) =>
          (c.sourceChannel ?? "").toLowerCase().includes(q) ||
          (c.email ?? "").toLowerCase().includes(q),
      );
    }
    return out;
  })();

  // 切换分组 / 分级筛选 / 搜索时复位到第 1 页，避免空页
  useEffect(() => {
    setCurrentPage(1);
  }, [groupFilter, tierFilter, searchQuery]);

  // pageSize === 0 表示“全部”：单页容纳全部已筛选凭据
  const effectivePageSize =
    pageSize === 0 ? Math.max(filteredCredentials.length, 1) : pageSize;
  const totalPages = Math.max(
    1,
    Math.ceil(filteredCredentials.length / effectivePageSize),
  );
  const startIndex = (currentPage - 1) * effectivePageSize;
  const endIndex = startIndex + effectivePageSize;
  const serverPageCreds = filteredCredentials.slice(startIndex, endIndex);
  // 拖拽排序的本地乐观顺序：仅当 id 集合与当前页一致时生效，否则回落到服务端顺序，
  // 避免翻页 / 数据变更后顺序错乱。
  const [pageOrder, setPageOrder] = useState<number[] | null>(null);
  const currentCredentials = (() => {
    if (!pageOrder) return serverPageCreds;
    const serverIds = new Set(serverPageCreds.map((c) => c.id));
    const orderIds = new Set(pageOrder);
    if (
      serverIds.size !== orderIds.size ||
      ![...serverIds].every((id) => orderIds.has(id))
    ) {
      return serverPageCreds;
    }
    const byId = new Map(serverPageCreds.map((c) => [c.id, c]));
    return pageOrder.map((id) => byId.get(id)!).filter(Boolean);
  })();
  const currentPageIds = currentCredentials.map((c) => c.id);
  const currentPageAllSelected =
    currentPageIds.length > 0 &&
    currentPageIds.every((id) => selectedIds.has(id));
  const allFilteredIds = filteredCredentials.map((c) => c.id);
  const allFilteredSelected =
    allFilteredIds.length > 0 &&
    allFilteredIds.every((id) => selectedIds.has(id));

  // 翻页时清掉本地排序覆盖，回到服务端顺序
  useEffect(() => {
    setPageOrder(null);
  }, [currentPage]);

  const dragSensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
  );

  const handleDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const ids = currentCredentials.map((c) => c.id);
    const oldIndex = ids.indexOf(Number(active.id));
    const newIndex = ids.indexOf(Number(over.id));
    if (oldIndex < 0 || newIndex < 0) return;

    const newOrder = arrayMove(ids, oldIndex, newIndex);
    setPageOrder(newOrder);

    // 按新视觉顺序赋连续递增的 priority（全局位置 = startIndex + 页内索引）。
    // 不依赖原有 priority 值域：即使原值全为默认 0 / 相同，也能保证数字更新、排序持久化；
    // 跨页也不冲突（第 1 页 0..11、第 2 页 12..23）。只对实际变化的卡片发请求。
    const prevPriority = new Map(
      currentCredentials.map((c) => [c.id, c.priority]),
    );
    const updates = newOrder
      .map((id, i) => ({ id, priority: startIndex + i }))
      .filter((u) => prevPriority.get(u.id) !== u.priority);
    if (updates.length === 0) return;

    Promise.all(
      updates.map((u) =>
        setPriority.mutateAsync({ id: u.id, priority: u.priority }),
      ),
    )
      .then(() => {
        toast.success("优先级顺序已更新");
        queryClient.invalidateQueries({ queryKey: ["credentials"] });
      })
      .catch((err) => {
        toast.error("更新优先级失败: " + (err as Error).message);
        setPageOrder(null);
      });
  };

  const gridRef = useRef<HTMLElement | null>(null);
  const rectSelection = useRectSelect({
    containerRef: gridRef,
    itemSelector: "[data-credential-id]",
    idAttribute: "credential-id",
    enabled: currentCredentials.length > 0,
    onSelectionChange: (hits, additive) => {
      setSelectedIds((prev) => {
        if (!additive) return new Set(hits);
        const next = new Set(prev);
        hits.forEach((id) => next.add(id));
        return next;
      });
    },
  });
  const disabledCredentialCount =
    data?.credentials.filter((c) => c.disabled).length || 0;

  // 实时并发调度态：多账号并发模型下没有单一"当前账号"，
  // 用真实在途计数衡量——activeAccounts=此刻有请求在途的账号数，
  // inFlightTotal=全部账号在途请求之和。与「并发监控」页同语义。
  const { activeAccounts, inFlightTotal } = (() => {
    let activeAccounts = 0;
    let inFlightTotal = 0;
    for (const c of data?.credentials || []) {
      const n = c.inFlight ?? 0;
      if (n > 0) {
        activeAccounts += 1;
        inFlightTotal += n;
      }
    }
    return { activeAccounts, inFlightTotal };
  })();

  // 已超额且尚未禁用的数量（用于一键超额按钮）
  const quotaExceededCount = (data?.credentials || []).filter((c) => {
    if (c.disabled) return false;
    const b = balanceMap.get(c.id) || c.balance;
    if (!b) return false;
    return b.remaining <= 0 || b.usagePercentage >= 100;
  }).length;

  // 超额统计：分别计算"已开 / 未开 / 待确定"三类，便于按钮文案与决策
  const overageStats = (() => {
    let enabled = 0;
    let disabledOff = 0;
    let unknown = 0;
    let total = 0;
    for (const c of data?.credentials || []) {
      if (c.disabled) continue;
      total += 1;
      const b = balanceMap.get(c.id) || c.balance;
      if (!b) {
        // 还没拉到余额，无法判断 — 视为待定
        unknown += 1;
        continue;
      }
      // 不可开启的订阅（FREE）不参与统计
      if (b.overageCapable === false) continue;
      if (b.overageEnabled === true) enabled += 1;
      else if (b.overageCapable === true) disabledOff += 1;
      else unknown += 1;
    }
    return { enabled, disabledOff, unknown, total };
  })();
  const overageEnableableCount = overageStats.disabledOff;
  const overageRetryableCount = overageStats.disabledOff + overageStats.unknown;

  useEffect(() => {
    setCurrentPage(1);
  }, [data?.credentials.length]);

  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map());
      setLoadingBalanceIds(new Set());
      return;
    }
    const validIds = new Set(data.credentials.map((c) => c.id));
    setBalanceMap((prev) => {
      const next = new Map<number, BalanceResponse>();
      prev.forEach((v, id) => {
        if (validIds.has(id)) next.set(id, v);
      });
      return next.size === prev.size ? prev : next;
    });
    setLoadingBalanceIds((prev) => {
      if (prev.size === 0) return prev;
      const next = new Set<number>();
      prev.forEach((id) => {
        if (validIds.has(id)) next.add(id);
      });
      return next.size === prev.size ? prev : next;
    });
  }, [data?.credentials]);

  const toggleDarkMode = () => {
    setDarkMode(!darkMode);
    document.documentElement.classList.toggle("dark");
  };

  const handleRefresh = () => {
    refetch();
    toast.success("已刷新凭据列表");
  };

  const handleLogout = () => {
    storage.removeApiKey();
    queryClient.clear();
    onLogout();
  };

  useEffect(() => {
    if (!error) return;
    const parsed = parseError(error);
    if (parsed.type === "authentication_error") {
      toast.error("登录已失效，请重新登录");
      handleLogout();
    }
  }, [error]);

  const toggleSelect = (id: number) => {
    const next = new Set(selectedIds);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelectedIds(next);
  };
  const deselectAll = () => setSelectedIds(new Set());

  /** 全选 / 取消全选当前页凭据。已选中其他页的不会被清除。 */
  const toggleSelectCurrentPage = () => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (currentPageAllSelected) {
        currentPageIds.forEach((id) => next.delete(id));
      } else {
        currentPageIds.forEach((id) => next.add(id));
      }
      return next;
    });
  };

  /** 全选 / 取消全选所有筛选后的凭据（跨页） */
  const toggleSelectAllFiltered = () => {
    if (allFilteredSelected) {
      // 取消：仅清除筛选范围内的，保留筛选范围外的已选项
      setSelectedIds((prev) => {
        const next = new Set(prev);
        allFilteredIds.forEach((id) => next.delete(id));
        return next;
      });
    } else {
      setSelectedIds(new Set(allFilteredIds));
    }
  };

  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) {
      toast.error("请先选择要删除的凭据");
      return;
    }
    const ids = Array.from(selectedIds);
    if (
      !(await confirm({
        title: "批量删除凭据",
        description: `确定要删除 ${ids.length} 个凭据吗？此操作无法撤销。`,
        confirmText: "删除",
        destructive: true,
      }))
    )
      return;
    let s = 0,
      f = 0;
    for (const id of ids) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功删除 ${s} 个凭据`);
    else toast.warning(`删除凭据：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error("请先选择要恢复的凭据");
      return;
    }
    const failedIds = Array.from(selectedIds).filter((id) => {
      const c = data?.credentials.find((x) => x.id === id);
      return c && c.failureCount > 0;
    });
    if (failedIds.length === 0) {
      toast.error("选中的凭据中没有失败的凭据");
      return;
    }
    let s = 0,
      f = 0;
    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功恢复 ${s} 个凭据`);
    else toast.warning(`成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error("请先选择要刷新的凭据");
      return;
    }
    const enabledIds = Array.from(selectedIds).filter((id) => {
      const c = data?.credentials.find((x) => x.id === id);
      return c && !c.disabled;
    });
    if (enabledIds.length === 0) {
      toast.error("选中的凭据中没有启用的凭据");
      return;
    }
    setBatchRefreshing(true);
    setBatchRefreshProgress({ current: 0, total: enabledIds.length });
    let s = 0,
      f = 0;
    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i]);
        s++;
      } catch {
        f++;
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length });
    }
    setBatchRefreshing(false);
    queryClient.invalidateQueries({ queryKey: ["credentials"] });
    if (f === 0) toast.success(`成功刷新 ${s} 个凭据的 Token`);
    else toast.warning(`刷新 Token：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error("没有可清除的凭据");
      return;
    }
    const disabled = data.credentials.filter((c) => c.disabled);
    if (disabled.length === 0) {
      toast.error("没有可清除的已禁用凭据");
      return;
    }
    if (
      !(await confirm({
        title: "清除已禁用凭据",
        description: `确定要清除所有 ${disabled.length} 个已禁用凭据吗？此操作无法撤销。`,
        confirmText: "清除",
        destructive: true,
      }))
    )
      return;
    let s = 0,
      f = 0;
    for (const c of disabled) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(c.id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功清除所有 ${s} 个已禁用凭据`);
    else toast.warning(`清除已禁用凭据：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error("当前页没有可查询的凭据");
      return;
    }
    const ids = currentCredentials.filter((c) => !c.disabled).map((c) => c.id);
    if (ids.length === 0) {
      toast.error("当前页没有可查询的启用凭据");
      return;
    }
    setQueryingInfo(true);
    setQueryInfoProgress({ current: 0, total: ids.length });
    // 有界并发（worker pool，与批量验活一致），逐条更新余额与进度
    let s = 0;
    let f = 0;
    let finalized = 0;
    let next = 0;
    const CONCURRENCY = 8;
    const worker = async () => {
      while (true) {
        const i = next++;
        if (i >= ids.length) return;
        const id = ids[i];
        setLoadingBalanceIds((prev) => {
          const n = new Set(prev);
          n.add(id);
          return n;
        });
        try {
          const balance = await getCredentialBalance(id);
          s++;
          setBalanceMap((prev) => {
            const n = new Map(prev);
            n.set(id, balance);
            return n;
          });
        } catch {
          f++;
        } finally {
          setLoadingBalanceIds((prev) => {
            const n = new Set(prev);
            n.delete(id);
            return n;
          });
        }
        finalized++;
        setQueryInfoProgress({ current: finalized, total: ids.length });
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, ids.length) }, () => worker()),
    );
    setQueryingInfo(false);
    if (f === 0) toast.success(`查询完成：成功 ${s}/${ids.length}`);
    else toast.warning(`查询完成：成功 ${s} 个，失败 ${f} 个`);
  };

  const handleRefreshBalance = async (id: number) => {
    setLoadingBalanceIds((prev) => {
      const n = new Set(prev);
      n.add(id);
      return n;
    });
    try {
      const balance = await getCredentialBalance(id);
      setBalanceMap((prev) => {
        const n = new Map(prev);
        n.set(id, balance);
        return n;
      });
      toast.success("余额已刷新");
    } catch (err) {
      toast.error("刷新余额失败: " + (err as Error).message);
    } finally {
      setLoadingBalanceIds((prev) => {
        const n = new Set(prev);
        n.delete(id);
        return n;
      });
    }
  };

  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) {
      toast.error("请先选择要验活的凭据");
      return;
    }
    setVerifying(true);
    cancelVerifyRef.current = false;
    const ids = Array.from(selectedIds);
    setVerifyProgress({ current: 0, total: ids.length });

    // id → email，便于结果列表直接看到是哪个账号
    const emailById = new Map<number, string | undefined>();
    for (const c of data?.credentials ?? []) emailById.set(c.id, c.email);

    const init = new Map<number, VerifyResult>();
    ids.forEach((id) =>
      init.set(id, { id, status: "pending", email: emailById.get(id) }),
    );
    setVerifyResults(init);
    setVerifyDialogOpen(true);

    // 有界并发（无 2s 间隔）。worker pool 领取下一个 id，逐条更新结果。
    let successCount = 0;
    let finalized = 0;
    let next = 0;
    const CONCURRENCY = 8;
    const worker = async () => {
      while (true) {
        if (cancelVerifyRef.current) return;
        const i = next++;
        if (i >= ids.length) return;
        const id = ids[i];
        setVerifyResults((prev) => {
          const n = new Map(prev);
          n.set(id, { id, status: "verifying", email: emailById.get(id) });
          return n;
        });
        try {
          const balance = await getCredentialBalance(id);
          successCount++;
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.set(id, {
              id,
              status: "success",
              usage: `${balance.currentUsage}/${balance.usageLimit}`,
              email: emailById.get(id),
            });
            return n;
          });
        } catch (err) {
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.set(id, {
              id,
              status: "failed",
              error: extractErrorMessage(err),
              email: emailById.get(id),
            });
            return n;
          });
        }
        finalized++;
        setVerifyProgress({ current: finalized, total: ids.length });
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, ids.length) }, () => worker()),
    );
    setVerifying(false);
    if (!cancelVerifyRef.current)
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`);
  };

  const handleCancelVerify = () => {
    cancelVerifyRef.current = true;
    setVerifying(false);
  };

  // 在批量验活窗口删除单个失败凭据
  const handleDeleteVerifyResult = (id: number) => {
    deleteCredential(id, {
      onSuccess: () => {
        setVerifyResults((prev) => {
          const n = new Map(prev);
          n.delete(id);
          return n;
        });
        toast.success(`凭据 #${id} 已删除`);
      },
      onError: (err) => toast.error("删除失败: " + extractErrorMessage(err)),
    });
  };

  // 一键删除批量验活窗口里全部失败凭据（并发删除）
  const handleDeleteFailedVerify = () => {
    const failedIds = Array.from(verifyResults.values())
      .filter((r) => r.status === "failed")
      .map((r) => r.id);
    if (failedIds.length === 0) return;
    setVerifyDeleting(true);
    let remaining = failedIds.length;
    let ok = 0;
    failedIds.forEach((id) => {
      deleteCredential(id, {
        onSuccess: () => {
          ok++;
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.delete(id);
            return n;
          });
        },
        onError: (err) =>
          toast.error(`删除 #${id} 失败: ` + extractErrorMessage(err)),
        onSettled: () => {
          remaining--;
          if (remaining === 0) {
            setVerifyDeleting(false);
            toast.success(`已删除 ${ok}/${failedIds.length} 个失败凭据`);
          }
        },
      });
    });
  };

  // 一键超额：把所有已超额（未禁用）凭据标记为 QuotaExceeded 并禁用
  const [disablingQuota, setDisablingQuota] = useState(false);
  const handleDisableQuotaExceeded = async () => {
    if (quotaExceededCount === 0) {
      toast.info('当前没有已超额的凭据，可先点击"刷新当前页余额"');
      return;
    }
    if (
      !(await confirm({
        title: "禁用已超额凭据",
        description: `确定要把 ${quotaExceededCount} 个已超额的凭据全部禁用吗？`,
        confirmText: "禁用",
        destructive: true,
      }))
    )
      return;
    setDisablingQuota(true);
    try {
      const res = await disableQuotaExceeded();
      const ok = res.disabledIds?.length || 0;
      const skip = res.skippedIds?.length || 0;
      if (ok > 0)
        toast.success(
          `已禁用 ${ok} 个已超额凭据${skip > 0 ? `，跳过 ${skip} 个` : ""}`,
        );
      else toast.warning("未找到已超额凭据（缓存可能已失效）");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error("一键超额失败: " + extractErrorMessage(err));
    } finally {
      setDisablingQuota(false);
    }
  };

  // 一键开启超额：调用上游 setUserPreference 把所有"可开启且未开启"的凭据开启
  const [enablingOverage, setEnablingOverage] = useState(false);
  const handleEnableOverageAll = async () => {
    if (overageEnableableCount === 0) {
      toast.info("当前没有明确「未开启超额」的凭据");
      return;
    }
    if (
      !(await confirm({
        title: "开启超额",
        description: `确定要为 ${overageEnableableCount} 个凭据开启超额吗？开启后超出额度将按 overageRate 计费。`,
        confirmText: "开启",
      }))
    )
      return;
    setEnablingOverage(true);
    try {
      const res = await enableOverageForAllCapable();
      const ok = res.enabledIds?.length || 0;
      const fail = res.failedIds?.length || 0;
      if (ok > 0 && fail === 0) toast.success(`已为 ${ok} 个凭据开启超额`);
      else if (ok > 0 && fail > 0)
        toast.warning(
          `成功 ${ok} 个，失败 ${fail} 个：${overageFailureMessage(res.failureMessages?.[0])}`,
        );
      else if (fail > 0)
        toast.error(
          `全部失败：${overageFailureMessage(res.failureMessages?.[0])}`,
        );
      else toast.info("没有需要操作的凭据");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error("一键开启超额失败: " + extractErrorMessage(err));
    } finally {
      setEnablingOverage(false);
    }
  };

  // 重试拉取超额状态：仅针对状态待确定的凭据批量查余额（只读，安全）。
  // 区分于「一键开启超额」——后者会调用写接口 setUserPreference，FREE 订阅会 403。
  const [refreshingOverage, setRefreshingOverage] = useState(false);
  const [refreshingOverageProgress, setRefreshingOverageProgress] = useState({
    current: 0,
    total: 0,
  });
  const handleRefreshOverageStatus = async () => {
    const targets = (data?.credentials || [])
      .filter((c) => {
        if (c.disabled) return false;
        const b = balanceMap.get(c.id) || c.balance;
        if (!b) return true;
        return b.overageCapable === undefined || b.overageCapable === null;
      })
      .map((c) => c.id);
    if (targets.length === 0) {
      toast.info("没有状态待确定的凭据");
      return;
    }
    setRefreshingOverage(true);
    setRefreshingOverageProgress({ current: 0, total: targets.length });
    let s = 0,
      f = 0;
    for (let i = 0; i < targets.length; i++) {
      const id = targets[i];
      setLoadingBalanceIds((prev) => {
        const n = new Set(prev);
        n.add(id);
        return n;
      });
      try {
        const balance = await getCredentialBalance(id);
        s++;
        setBalanceMap((prev) => {
          const n = new Map(prev);
          n.set(id, balance);
          return n;
        });
      } catch {
        f++;
      } finally {
        setLoadingBalanceIds((prev) => {
          const n = new Set(prev);
          n.delete(id);
          return n;
        });
      }
      setRefreshingOverageProgress({ current: i + 1, total: targets.length });
    }
    setRefreshingOverage(false);
    if (f === 0) toast.success(`刷新完成：成功 ${s}/${targets.length}`);
    else toast.warning(`刷新完成：成功 ${s} 个，失败 ${f} 个`);
  };

  const [exportingKam, setExportingKam] = useState(false);

  const handleUpdateAdminKey = async (e: React.FormEvent) => {
    e.preventDefault();
    const key = newAdminKey.trim();
    if (!key) {
      toast.error("新登录API密钥不能为空");
      return;
    }
    setUpdatingAdminKey(true);
    try {
      await updateAdminKey({ newKey: key });
      storage.setApiKey(key);
      toast.success("登录API密钥已更新，已自动切换到新 Key");
      setAdminKeyDialogOpen(false);
      setNewAdminKey("");
    } catch (error) {
      toast.error(`更新失败: ${extractErrorMessage(error)}`);
    } finally {
      setUpdatingAdminKey(false);
    }
  };

  const handleExportKam = async () => {
    if (selectedIds.size === 0) {
      toast.info("请先勾选要导出的凭据");
      return;
    }
    const ids = Array.from(selectedIds);
    setExportingKam(true);
    try {
      const exportData = await exportKamCredentials(ids);
      const accountCount = exportData.accounts?.length ?? 0;
      if (accountCount === 0) {
        toast.warning("勾选的凭据中没有可导出的（缺少 refreshToken）");
        return;
      }
      const json = JSON.stringify(exportData, null, 2);
      const blob = new Blob([json], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const ts = new Date()
        .toISOString()
        .replace(/[:.]/g, "-")
        .replace("T", "_")
        .slice(0, 19);
      const a = document.createElement("a");
      a.href = url;
      a.download = `kiro-account-manager-export-${ts}.json`;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
      const skipped = ids.length - accountCount;
      toast.success(
        skipped > 0
          ? `已导出 ${accountCount} 个账号，${skipped} 个无效已跳过`
          : `已导出 ${accountCount} 个账号`,
      );
    } catch (err) {
      toast.error("导出失败: " + extractErrorMessage(err));
    } finally {
      setExportingKam(false);
    }
  };

  const handleToggleLoadBalancing = () => {
    const cur = loadBalancingData?.mode || "priority";
    const next = cur === "priority" ? "balanced" : "priority";
    setLoadBalancingMode(next, {
      onSuccess: () =>
        toast.success(
          `已切换到${next === "priority" ? "优先级模式" : "均衡负载模式"}`,
        ),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    });
  };

  if (isLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center">
        <div className="text-center">
          <div className="animate-spin rounded-full h-10 w-10 border-2 border-primary/20 border-t-primary mx-auto mb-4"></div>
          <p className="text-sm text-muted-foreground">加载中…</p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="min-h-screen flex items-center justify-center p-4">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-destructive font-semibold mb-2">加载失败</div>
            <p className="text-sm text-muted-foreground mb-4">
              {extractErrorMessage(error)}
            </p>
            <div className="flex gap-2 justify-center">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={handleLogout}>
                重新登录
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <div className={embedded ? "" : "min-h-screen"}>
      {/* 顶部毛玻璃导航条（仅独立模式渲染；嵌入模式由外层 App 提供顶栏） */}
      {!embedded && (
        <header className="sticky top-0 z-40 w-full glass">
          <div className="mx-auto max-w-[1400px] flex h-16 items-center justify-between px-4 md:px-8">
            <div className="flex items-center gap-2.5">
              <img
                src="/admin/kirors.png"
                alt="Kiro"
                className="h-10 w-10 object-contain"
                draggable={false}
              />
              <span className="font-semibold tracking-tight">Kiro Admin</span>
            </div>
            <div className="flex items-center gap-1.5">
              <Button
                variant="outline"
                size="sm"
                onClick={handleToggleLoadBalancing}
                disabled={isLoadingMode || isSettingMode}
                title="切换负载均衡模式"
              >
                <Activity className="h-3.5 w-3.5" />
                {isLoadingMode
                  ? "加载中…"
                  : loadBalancingData?.mode === "priority"
                    ? "优先级"
                    : "均衡负载"}
              </Button>
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
                onClick={handleRefresh}
                title="刷新"
              >
                <RefreshCw className="h-4 w-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                onClick={() => setImageUpdateDialogOpen(true)}
                title={
                  updateCheck?.hasUpdate
                    ? `发现新版本 v${updateCheck.latestVersion}（当前 v${updateCheck.currentVersion}）`
                    : "镜像在线更新"
                }
                className="relative"
              >
                <UploadCloud className="h-4 w-4" />
                {updateCheck?.hasUpdate && (
                  <span className="absolute right-1 top-1 inline-flex h-2 w-2 items-center justify-center">
                    <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
                    <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
                  </span>
                )}
              </Button>
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button variant="ghost" size="icon" title="设置">
                    <Settings className="h-4 w-4" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={() => {
                      setNewAdminKey("");
                      setShowAdminKeyPlain(false);
                      setAdminKeyDialogOpen(true);
                    }}
                  >
                    <Key />
                    修改登录API密钥（管理面板登录）
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
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
        </header>
      )}

      {/* 主内容 */}
      <main
        ref={gridRef}
        className={embedded ? "" : "mx-auto max-w-[1400px] px-4 md:px-8 py-8"}
      >
        {/* 大标题 */}
        <div className="mb-5 flex items-end justify-between gap-4 sm:mb-6">
          <div>
            <h1 className="text-2xl font-semibold tracking-tight leading-tight sm:text-[28px]">
              凭据管理
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              管理 Kiro 的所有访问凭据、负载均衡与登录信息
            </p>
          </div>
        </div>

        {/* 统计卡片 */}
        <div className="mb-5 grid grid-cols-3 gap-2 sm:mb-6 sm:gap-4">
          <Card className="hover:shadow-apple-lg hover:-translate-y-0.5">
            <CardContent className="p-3 sm:p-5">
              <div className="text-[11px] font-medium text-muted-foreground sm:text-[13px]">
                凭据总数
              </div>
              <div className="mt-1.5 text-2xl font-semibold tracking-tight tabular-nums sm:mt-2 sm:text-3xl">
                {formatNumber(data?.total)}
              </div>
            </CardContent>
          </Card>
          <Card className="hover:shadow-apple-lg hover:-translate-y-0.5">
            <CardContent className="p-3 sm:p-5">
              <div className="text-[11px] font-medium text-muted-foreground sm:text-[13px]">
                可用凭据
              </div>
              <div className="mt-1.5 text-2xl font-semibold tracking-tight tabular-nums text-emerald-600 dark:text-emerald-400 sm:mt-2 sm:text-3xl">
                {formatNumber(data?.available)}
              </div>
            </CardContent>
          </Card>
          <Card className="hover:shadow-apple-lg hover:-translate-y-0.5">
            <CardContent className="p-3 sm:p-5">
              <div className="text-[11px] font-medium text-muted-foreground sm:text-[13px]">
                活跃账号
              </div>
              <div className="mt-1.5 flex min-w-0 flex-wrap items-baseline gap-1.5 sm:mt-2 sm:gap-2">
                <span
                  className={`truncate text-2xl font-semibold tracking-tight tabular-nums sm:text-3xl ${
                    activeAccounts > 0
                      ? "text-emerald-600 dark:text-emerald-400"
                      : ""
                  }`}
                >
                  {formatNumber(activeAccounts)}
                </span>
                <span
                  className="text-xs text-muted-foreground"
                  title="此刻有请求在途的账号数 · 全部账号在途请求总数（实时并发调度态见「并发监控」页）"
                >
                  {inFlightTotal > 0 ? `${inFlightTotal} 在途` : "空闲"}
                </span>
              </div>
            </CardContent>
          </Card>
        </div>

        {/* 工具栏 */}
        <div className="mb-5 flex flex-col gap-3">
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <h2 className="text-lg font-semibold tracking-tight">凭据列表</h2>
            {data?.credentials && data.credentials.length > 0 && (
              <Badge variant="secondary">
                {groupFilter || tierFilter.size > 0
                  ? `${filteredCredentials.length} / ${data.credentials.length}`
                  : data.credentials.length}
              </Badge>
            )}
            {groupFilter && (
              <Badge variant="outline" className="gap-1">
                筛选：{groupFilter === "__none__" ? "未分组" : groupFilter}
                <button
                  type="button"
                  className="ml-1 text-muted-foreground hover:text-foreground"
                  onClick={() => setGroupFilter("")}
                  title="清除筛选"
                >
                  ×
                </button>
              </Badge>
            )}
            {tierFilter.size > 0 && (
              <Badge variant="outline" className="gap-1">
                分级：
                {Array.from(tierFilter)
                  .map((t) => TIER_LABELS[t])
                  .join("、")}
                <button
                  type="button"
                  className="ml-1 text-muted-foreground hover:text-foreground"
                  onClick={() => setTierFilter(new Set())}
                  title="清除分级筛选"
                >
                  ×
                </button>
              </Badge>
            )}

            {currentCredentials.length > 0 && (
              <Button
                size="sm"
                variant="ghost"
                className="px-2 sm:px-3"
                onClick={toggleSelectCurrentPage}
                title={currentPageAllSelected ? "取消选择当前页" : "全选当前页"}
              >
                {currentPageAllSelected ? "取消全选" : "全选当前页"}
              </Button>
            )}
            {filteredCredentials.length > currentCredentials.length && (
              <Button
                size="sm"
                variant="ghost"
                className="px-2 sm:px-3"
                onClick={toggleSelectAllFiltered}
                title={
                  allFilteredSelected
                    ? "取消选择全部筛选结果"
                    : `全选所有 ${filteredCredentials.length} 个筛选结果`
                }
              >
                {allFilteredSelected
                  ? "取消全选所有页"
                  : `全选所有页 (${filteredCredentials.length})`}
              </Button>
            )}
            {selectedIds.size > 0 && (
              <>
                <Badge variant="default">已选 {selectedIds.size}</Badge>
                <Button
                  onClick={deselectAll}
                  size="sm"
                  variant="ghost"
                  className="px-2 sm:px-3"
                >
                  取消选择
                </Button>
              </>
            )}
            {verifying && !verifyDialogOpen && (
              <Button
                onClick={() => setVerifyDialogOpen(true)}
                size="sm"
                variant="secondary"
              >
                <CheckCircle2 className="h-3.5 w-3.5 animate-spin" />
                验活中… {verifyProgress.current}/{verifyProgress.total}
              </Button>
            )}
          </div>

          {/* 第二行：筛选（左） + 操作（右） */}
          <div className="flex w-full flex-col gap-2 sm:flex-row sm:flex-wrap sm:items-center">
            {/* 筛选器 — 左（移动端两列网格并排，桌面端内联） */}
            <div className="grid w-full grid-cols-2 gap-2 sm:flex sm:w-auto sm:flex-wrap sm:items-center">
              {/* 模糊搜索：来源渠道（备注）/ 邮箱；移动端整行、桌面端 200px */}
              <div className="relative col-span-2 sm:col-span-1 sm:w-[200px]">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                <input
                  type="text"
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  placeholder="搜索来源渠道 / 备注 / 邮箱"
                  className="h-8 w-full rounded-full border border-border bg-card/60 pl-5 pr-5 text-base backdrop-blur placeholder:text-muted-foreground/70 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring sm:text-sm"
                />
                {searchQuery && (
                  <button
                    type="button"
                    onClick={() => setSearchQuery("")}
                    className="absolute right-2 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                    title="清除搜索"
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                )}
              </div>
              <Select
                value={groupFilter || "all"}
                onValueChange={(v) => setGroupFilter(v === "all" ? "" : v)}
              >
                <SelectTrigger
                  className="h-8 w-full rounded-full border-border bg-card/60 px-3 backdrop-blur sm:w-[140px]"
                  title="按分组筛选凭据"
                >
                  <SelectValue placeholder="全部分组" />
                </SelectTrigger>
                <SelectContent align="end">
                  <SelectItem value="all">全部分组</SelectItem>
                  <SelectItem value="__none__">未分组</SelectItem>
                  {groupOptions.map((g) => (
                    <SelectItem key={g} value={g}>
                      {g}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              {/* 订阅分级筛选（多选） */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    title="按订阅分级筛选凭据（可多选，依据最近一次余额缓存）"
                    className="inline-flex h-8 w-full items-center justify-between gap-1 rounded-full border border-border bg-card/60 px-3 text-sm backdrop-blur hover:bg-accent sm:w-[136px]"
                  >
                    <span className="truncate">
                      {tierFilter.size > 0
                        ? `分级 ·${tierFilter.size}`
                        : "全部分级"}
                    </span>
                    <ChevronDown className="h-3.5 w-3.5 opacity-60" />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="min-w-[10rem]">
                  <DropdownMenuLabel>订阅分级</DropdownMenuLabel>
                  {TIER_OPTIONS.map((t) => (
                    <DropdownMenuItem
                      key={t.value}
                      onSelect={(e) => {
                        e.preventDefault();
                        toggleTier(t.value);
                      }}
                      className="gap-2"
                    >
                      <Checkbox checked={tierFilter.has(t.value)} />
                      <span>{t.label}</span>
                    </DropdownMenuItem>
                  ))}
                  {tierFilter.size > 0 && (
                    <>
                      <DropdownMenuSeparator />
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          setTierFilter(new Set());
                        }}
                        className="text-muted-foreground"
                      >
                        清除分级筛选
                      </DropdownMenuItem>
                    </>
                  )}
                </DropdownMenuContent>
              </DropdownMenu>

              {/* 卡片 / 列表 视图切换（iOS 分段控件） */}
              <div className="col-span-2 inline-flex h-8 shrink-0 items-center justify-self-start rounded-full border border-border bg-card/60 p-0.5 backdrop-blur sm:col-span-1">
                <button
                  type="button"
                  onClick={() => changeViewMode("card")}
                  aria-pressed={viewMode === "card"}
                  title="卡片视图"
                  className={`inline-flex h-7 items-center gap-1 rounded-full px-2.5 text-[13px] transition-colors ${
                    viewMode === "card"
                      ? "bg-background text-foreground shadow-apple-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <LayoutGrid className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">卡片</span>
                </button>
                <button
                  type="button"
                  onClick={() => changeViewMode("list")}
                  aria-pressed={viewMode === "list"}
                  title="列表视图"
                  className={`inline-flex h-7 items-center gap-1 rounded-full px-2.5 text-[13px] transition-colors ${
                    viewMode === "list"
                      ? "bg-background text-foreground shadow-apple-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <List className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">列表</span>
                </button>
                <button
                  type="button"
                  onClick={() => changeViewMode("monitor")}
                  aria-pressed={viewMode === "monitor"}
                  title="并发监控视图"
                  className={`inline-flex h-7 items-center gap-1 rounded-full px-2.5 text-[13px] transition-colors ${
                    viewMode === "monitor"
                      ? "bg-background text-foreground shadow-apple-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <Gauge className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">监控</span>
                </button>
              </div>
            </div>

            {/* 操作 — 右（移动端整宽两列网格，桌面端右对齐内联） */}
            <div className="ml-auto grid w-full grid-cols-2 gap-2 sm:flex sm:w-auto sm:flex-wrap sm:items-center">
              {selectedIds.size > 0 && (
                <>
                  <Button
                    onClick={() => setBatchEditDialogOpen(true)}
                    size="sm"
                    variant="outline"
                    title="批量编辑分组 / 来源渠道"
                  >
                    <Tags className="h-3.5 w-3.5" />
                    分组/来源
                  </Button>
                  <Button
                    onClick={handleBatchDelete}
                    size="sm"
                    variant="destructive"
                    className="w-full sm:w-auto"
                    disabled={selectedIds.size === 0}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                    删除
                  </Button>
                  <span className="mx-1 hidden h-5 w-px bg-border/70 sm:inline-block" />
                </>
              )}

              {/* 主操作 */}
              <Button
                onClick={() => setAddDialogOpen(true)}
                size="sm"
                className="w-full sm:w-auto"
              >
                <Plus className="h-3.5 w-3.5" />
                添加凭据
              </Button>

              {/* 导入 / 登录折叠菜单 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant="outline"
                    className="w-full sm:w-auto"
                  >
                    <Upload className="h-3.5 w-3.5" />
                    登录 / 导入 / 导出
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>登录</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={() => setSocialLoginDialogOpen(true)}
                  >
                    <LogIn />
                    Kiro 账号登录
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => setIdcLoginDialogOpen(true)}
                  >
                    <Key />
                    AWS SSO (IdC) 登录
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => setEnterpriseLoginDialogOpen(true)}
                  >
                    <Building2 />
                    Enterprise (IAM Identity Center) 登录
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>导入</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={() => setBatchImportDialogOpen(true)}
                  >
                    <Upload />
                    批量导入
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => setKamImportDialogOpen(true)}
                  >
                    <FileUp />
                    Kiro Account Manager 导入
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={handleExportKam}
                    disabled={exportingKam}
                  >
                    <FileDown />
                    {exportingKam ? "导出中…" : "Kiro Account Manager 导出"}
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>

              {/* 维护 / 危险操作折叠菜单 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant="outline"
                    title="更多操作"
                    className="w-full sm:w-auto"
                  >
                    <MoreHorizontal className="h-3.5 w-3.5" />
                    <span className="sm:hidden">更多</span>
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>批量操作</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={handleBatchVerify}
                    disabled={selectedIds.size === 0}
                  >
                    <CheckCircle2 />
                    批量验活
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleBatchForceRefresh();
                    }}
                    disabled={selectedIds.size === 0 || batchRefreshing}
                  >
                    <RefreshCw
                      className={batchRefreshing ? "animate-spin" : ""}
                    />
                    {batchRefreshing
                      ? `刷新中… ${batchRefreshProgress.current}/${batchRefreshProgress.total}`
                      : "刷新 Token"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={handleBatchResetFailure}
                    disabled={selectedIds.size === 0}
                  >
                    <RotateCcw />
                    恢复异常
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>维护</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleQueryCurrentPageInfo();
                    }}
                    disabled={queryingInfo || !data?.credentials?.length}
                  >
                    <RefreshCw className={queryingInfo ? "animate-spin" : ""} />
                    {queryingInfo
                      ? `刷新中… ${queryInfoProgress.current}/${queryInfoProgress.total}`
                      : "刷新当前页余额"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => setProxyPoolDialogOpen(true)}
                  >
                    <Globe />
                    IP 代理池管理
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={
                      resetAllSuccess.isPending || !data?.credentials?.length
                    }
                    onSelect={(e) => {
                      e.preventDefault();
                      resetAllSuccess.mutate(undefined, {
                        onSuccess: (res) => toast.success(res.message),
                        onError: (err) =>
                          toast.error("重置失败: " + (err as Error).message),
                      });
                    }}
                  >
                    <RotateCcw
                      className={
                        resetAllSuccess.isPending ? "animate-spin" : ""
                      }
                    />
                    重置成功次数
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={
                      enablingOverage ||
                      refreshingOverage ||
                      overageRetryableCount === 0
                    }
                    onSelect={(e) => {
                      e.preventDefault();
                      if (overageEnableableCount > 0) {
                        handleEnableOverageAll();
                      } else {
                        handleRefreshOverageStatus();
                      }
                    }}
                    title={
                      overageRetryableCount === 0
                        ? `全部 ${overageStats.enabled} 个 PRO/ENTERPRISE 凭据均已开启超额`
                        : `已开 ${overageStats.enabled} 个 / 未开 ${overageStats.disabledOff} 个 / 待确定 ${overageStats.unknown} 个`
                    }
                  >
                    <Zap
                      className={
                        enablingOverage || refreshingOverage
                          ? "animate-pulse text-emerald-500"
                          : "text-emerald-500"
                      }
                    />
                    {refreshingOverage
                      ? `刷新中… ${refreshingOverageProgress.current}/${refreshingOverageProgress.total}`
                      : overageRetryableCount === 0
                        ? `全部已开启超额（${overageStats.enabled}）`
                        : overageEnableableCount > 0
                          ? `一键开启超额（${overageEnableableCount}）`
                          : `重试拉取超额状态（${overageStats.unknown}）`}
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    destructive
                    disabled={disablingQuota || quotaExceededCount === 0}
                    onSelect={(e) => {
                      e.preventDefault();
                      handleDisableQuotaExceeded();
                    }}
                  >
                    <AlertTriangle />
                    一键超额禁用 ({quotaExceededCount})
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    destructive
                    disabled={disabledCredentialCount === 0}
                    onSelect={(e) => {
                      e.preventDefault();
                      handleClearAll();
                    }}
                  >
                    <Trash2 />
                    清除已禁用 ({disabledCredentialCount})
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        </div>

        {/* 列表 */}
        {data?.credentials.length === 0 ? (
          <Card>
            <CardContent className="py-16 text-center">
              <div className="mx-auto mb-3 flex h-12 w-12 items-center justify-center rounded-2xl bg-secondary text-muted-foreground">
                <Server className="h-5 w-5" />
              </div>
              <p className="text-sm text-muted-foreground">
                暂无凭据，点击右上角“添加凭据”开始
              </p>
            </CardContent>
          </Card>
        ) : viewMode === "monitor" ? (
          <ConcurrencyMonitor credentials={filteredCredentials} />
        ) : (
          <>
            <DndContext
              sensors={dragSensors}
              collisionDetection={closestCenter}
              onDragEnd={handleDragEnd}
            >
              <SortableContext
                items={currentPageIds}
                strategy={
                  viewMode === "list"
                    ? verticalListSortingStrategy
                    : rectSortingStrategy
                }
              >
                <div
                  className={
                    viewMode === "list"
                      ? "flex select-none flex-col gap-2"
                      : "grid select-none gap-3 sm:gap-4 md:grid-cols-2 lg:grid-cols-3"
                  }
                >
                  {currentCredentials.map((credential) => (
                    <CredentialCard
                      key={credential.id}
                      credential={credential}
                      view={viewMode}
                      selected={selectedIds.has(credential.id)}
                      onToggleSelect={() => toggleSelect(credential.id)}
                      balance={
                        balanceMap.get(credential.id) ||
                        credential.balance ||
                        null
                      }
                      loadingBalance={loadingBalanceIds.has(credential.id)}
                      onRefreshBalance={() =>
                        handleRefreshBalance(credential.id)
                      }
                      failureStats={failureStatsMap?.[String(credential.id)]}
                    />
                  ))}
                </div>
              </SortableContext>
            </DndContext>

            {filteredCredentials.length > 0 && (
              <div className="mt-6 flex flex-col items-center justify-center gap-3 sm:mt-8 sm:flex-row sm:gap-5">
                {/* 每页数量 */}
                <div className="flex items-center gap-2 text-sm text-muted-foreground">
                  <span className="whitespace-nowrap">每页</span>
                  <Select
                    value={String(pageSize)}
                    onValueChange={(v) => changePageSize(Number(v))}
                  >
                    <SelectTrigger
                      className="h-8 w-[92px] rounded-full border-border bg-card/60 px-3 backdrop-blur"
                      title="设置每页显示数量"
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent align="center">
                      {PAGE_SIZE_OPTIONS.map((n) => (
                        <SelectItem key={n} value={String(n)}>
                          {n} 个
                        </SelectItem>
                      ))}
                      <SelectItem value="0">全部</SelectItem>
                    </SelectContent>
                  </Select>
                </div>

                {/* 翻页控件（仅多页时显示） */}
                {totalPages > 1 && (
                  <div className="flex flex-wrap items-center justify-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => setCurrentPage((p) => Math.max(1, p - 1))}
                      disabled={currentPage === 1}
                    >
                      <ChevronLeft className="h-3.5 w-3.5" />
                      上一页
                    </Button>
                    <div className="order-first w-full px-3 text-center text-sm tabular-nums text-muted-foreground sm:order-none sm:w-auto">
                      第{" "}
                      <span className="font-medium text-foreground">
                        {currentPage}
                      </span>{" "}
                      / {totalPages} 页
                      <span className="mx-1.5 text-muted-foreground/50">·</span>
                      共 {filteredCredentials.length} 个
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() =>
                        setCurrentPage((p) => Math.min(totalPages, p + 1))
                      }
                      disabled={currentPage === totalPages}
                    >
                      下一页
                      <ChevronRight className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </main>

      {/* 弹窗们 */}
      <AddCredentialDialog
        open={addDialogOpen}
        onOpenChange={setAddDialogOpen}
      />
      <BatchImportDialog
        open={batchImportDialogOpen}
        onOpenChange={setBatchImportDialogOpen}
      />
      <BatchEditCredentialDialog
        open={batchEditDialogOpen}
        onOpenChange={setBatchEditDialogOpen}
        credentials={(data?.credentials ?? []).filter((c) =>
          selectedIds.has(c.id),
        )}
        groupOptions={groupOptions}
        onDone={deselectAll}
      />
      <SocialLoginDialog
        open={socialLoginDialogOpen}
        onOpenChange={setSocialLoginDialogOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <IdcLoginDialog
        open={idcLoginDialogOpen}
        onOpenChange={setIdcLoginDialogOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <IdcLoginDialog
        mode="enterprise"
        open={enterpriseLoginDialogOpen}
        onOpenChange={setEnterpriseLoginDialogOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <KamImportDialog
        open={kamImportDialogOpen}
        onOpenChange={setKamImportDialogOpen}
      />
      <ProxyPoolDialog
        open={proxyPoolDialogOpen}
        onOpenChange={setProxyPoolDialogOpen}
      />
      <ImageUpdateDialog
        open={imageUpdateDialogOpen}
        onOpenChange={setImageUpdateDialogOpen}
      />

      {/* 修改登录API密钥对话框（adminApiKey —— 管理面板登录密钥） */}
      <Dialog
        open={adminKeyDialogOpen}
        onOpenChange={(open) => {
          if (!updatingAdminKey) setAdminKeyDialogOpen(open);
        }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Key className="h-4 w-4" />
              修改登录API密钥
            </DialogTitle>
            <DialogDescription>
              用于登录此管理面板。修改后将自动更新本地存储的 Key，无需重新登录。
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleUpdateAdminKey} className="space-y-4 py-2">
            <div className="relative">
              <Input
                type={showAdminKeyPlain ? "text" : "password"}
                placeholder="输入或生成新的登录API密钥"
                value={newAdminKey}
                onChange={(e) => setNewAdminKey(e.target.value)}
                disabled={updatingAdminKey}
                autoFocus
                className="pr-20 font-mono text-[13px]"
              />
              <div className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-1.5">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={() => setShowAdminKeyPlain((v) => !v)}
                  disabled={updatingAdminKey}
                  title={showAdminKeyPlain ? "隐藏" : "显示"}
                >
                  {showAdminKeyPlain ? (
                    <EyeOff className="h-3.5 w-3.5" />
                  ) : (
                    <Eye className="h-3.5 w-3.5" />
                  )}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={async () => {
                    if (!newAdminKey.trim()) {
                      toast.error("请先输入或生成 Key 再复制");
                      return;
                    }
                    try {
                      await navigator.clipboard.writeText(newAdminKey);
                      toast.success("已复制到剪贴板");
                    } catch {
                      toast.error("复制失败，请手动选择文本");
                    }
                  }}
                  disabled={updatingAdminKey}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  const key = generateApiKey("sk-admin-");
                  setNewAdminKey(key);
                  setShowAdminKeyPlain(true);
                }}
                disabled={updatingAdminKey}
              >
                <Wand2 className="h-3.5 w-3.5" />
                生成随机 Key
              </Button>
              <p className="text-[11px] text-muted-foreground">
                建议生成后立即复制保存，确认更新后即生效。
              </p>
            </div>
            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => setAdminKeyDialogOpen(false)}
                disabled={updatingAdminKey}
              >
                取消
              </Button>
              <Button
                type="submit"
                disabled={updatingAdminKey || !newAdminKey.trim()}
              >
                {updatingAdminKey ? "更新中…" : "确认更新"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {rectSelection.active && rectSelection.rect && (
        <div
          className="pointer-events-none fixed z-50 rounded-sm border border-primary/70 bg-primary/15"
          style={{
            left: rectSelection.rect.left,
            top: rectSelection.rect.top,
            width: rectSelection.rect.width,
            height: rectSelection.rect.height,
          }}
        />
      )}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
        onDelete={handleDeleteVerifyResult}
        onDeleteFailed={handleDeleteFailedVerify}
        deleting={verifyDeleting}
      />
    </div>
  );
}
