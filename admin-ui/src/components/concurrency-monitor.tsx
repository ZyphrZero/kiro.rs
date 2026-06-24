import { useMemo, useState } from "react";
import { toast } from "sonner";
import { Clock, Pause, Activity, Pencil } from "lucide-react";
import type { CredentialStatusItem } from "@/types/api";
import { useSetConcurrency } from "@/hooks/use-credentials";

/** 耗时 EWMA（毫秒）格式化 */
function formatEwmaMs(ms: number): string {
  if (ms <= 0) return "—";
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

/** 把秒数格式化为 `mm:ss` 或 `hh:mm:ss` */
function formatCountdown(secs: number): string {
  const total = Math.max(0, Math.floor(secs));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

type Status = "active" | "idle" | "throttled" | "disabled";

function statusOf(c: CredentialStatusItem): Status {
  if (c.disabled) return "disabled";
  if ((c.throttledRemainingSecs ?? 0) > 0) return "throttled";
  if ((c.inFlight ?? 0) > 0) return "active";
  return "idle";
}

/** 排序：活跃(在途多者优先) → 冷却 → 空闲 → 禁用 */
function sortKey(c: CredentialStatusItem): [number, number, number] {
  const st = statusOf(c);
  const group = st === "active" ? 0 : st === "throttled" ? 1 : st === "idle" ? 2 : 3;
  const inFlight = c.inFlight ?? 0;
  const cap = c.maxConcurrency ?? 1;
  const load = cap > 0 ? inFlight / cap : 0;
  // group 升序、在途降序、负载降序
  return [group, -inFlight, -load];
}

function cmp(a: [number, number, number], b: [number, number, number]): number {
  return a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
}

/** 单账号监控行 */
function MonitorRow({ c }: { c: CredentialStatusItem }) {
  const st = statusOf(c);
  const inFlight = c.inFlight ?? 0;
  const cap = c.maxConcurrency ?? 0;
  const pct = cap > 0 ? Math.min(100, Math.round((inFlight / cap) * 100)) : 0;
  const errRate = c.recentErrorRate ?? 0;
  const name = c.email || `凭据 #${c.id}`;

  // 单账号并发上限编辑（留空 = 回退全局值）
  const setConcurrency = useSetConcurrency();
  const [editing, setEditing] = useState(false);
  const [capValue, setCapValue] = useState(
    c.maxConcurrencyOverride != null ? String(c.maxConcurrencyOverride) : "",
  );
  const commitCap = () => {
    const trimmed = capValue.trim();
    const val = trimmed === "" ? null : parseInt(trimmed, 10);
    if (val !== null && (isNaN(val) || val < 0)) {
      toast.error("并发上限必须是非负整数（留空为清除覆盖）");
      return;
    }
    setConcurrency.mutate(
      { id: c.id, maxConcurrency: val },
      {
        onSuccess: (res) => {
          toast.success(res.message);
          setEditing(false);
        },
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  // 状态点颜色
  const dot =
    st === "active"
      ? "bg-emerald-500"
      : st === "throttled"
        ? "bg-orange-500"
        : st === "disabled"
          ? "bg-muted-foreground/40"
          : "bg-muted-foreground/30";

  // 进度条颜色：满载橙、有错红、正常绿
  const barColor =
    errRate >= 20
      ? "bg-red-500"
      : pct >= 100
        ? "bg-amber-500"
        : "bg-emerald-500";

  const dim = st === "disabled";

  return (
    <div
      className={`flex items-center gap-3 rounded-xl border px-3 py-2.5 transition-colors ${
        st === "active"
          ? "border-primary/50 bg-primary/[0.03]"
          : "border-border/60 bg-card"
      } ${dim ? "opacity-55" : ""}`}
      title={name}
    >
      {/* 状态点 */}
      <span
        className={`h-2.5 w-2.5 shrink-0 rounded-full ${dot} ${st === "active" ? "animate-pulse" : ""}`}
      />

      {/* 名称 */}
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-medium leading-5">{name}</div>
        {/* 进度条 */}
        <div className="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-secondary">
          <div
            className={`h-full rounded-full transition-all ${barColor}`}
            style={{ width: `${pct}%` }}
          />
        </div>
      </div>

      {/* 在途/上限（上限可点编辑：单账号并发覆盖） */}
      <div className="w-[88px] shrink-0 text-right">
        {dim ? (
          <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
            <Pause className="h-3 w-3" />
            禁用
          </span>
        ) : st === "throttled" ? (
          <span className="inline-flex items-center gap-1 text-xs text-orange-600 dark:text-orange-400">
            <Clock className="h-3 w-3" />
            {formatCountdown(c.throttledRemainingSecs ?? 0)}
          </span>
        ) : editing ? (
          <span className="inline-flex items-center gap-0.5 text-sm tabular-nums">
            <span className="text-muted-foreground/60">{inFlight}/</span>
            <input
              autoFocus
              type="number"
              min={0}
              value={capValue}
              onChange={(e) => setCapValue(e.target.value)}
              onBlur={commitCap}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitCap();
                if (e.key === "Escape") {
                  setCapValue(
                    c.maxConcurrencyOverride != null
                      ? String(c.maxConcurrencyOverride)
                      : "",
                  );
                  setEditing(false);
                }
              }}
              placeholder="全局"
              className="w-12 rounded border border-input bg-background px-1 py-0.5 text-right text-sm tabular-nums"
            />
          </span>
        ) : (
          <button
            type="button"
            onClick={() => setEditing(true)}
            title="点击编辑该账号并发上限（留空=用全局值）"
            className="group/cap inline-flex items-center gap-1 rounded px-1 text-sm font-semibold tabular-nums transition-colors hover:bg-accent hover:text-primary"
          >
            <span className={pct >= 100 ? "text-amber-600 dark:text-amber-400" : ""}>
              {inFlight}
            </span>
            <span className="text-muted-foreground/60">/{cap}</span>
            {c.maxConcurrencyOverride != null && (
              <span className="text-[10px] text-primary">覆盖</span>
            )}
            <Pencil className="h-3 w-3 opacity-0 transition-opacity group-hover/cap:opacity-60" />
          </button>
        )}
      </div>

      {/* 错误率 */}
      <div className="hidden w-12 shrink-0 text-right text-xs tabular-nums sm:block">
        {errRate > 0 ? (
          <span className={errRate >= 20 ? "font-semibold text-destructive" : "text-muted-foreground"}>
            {errRate}%
          </span>
        ) : (
          <span className="text-muted-foreground/40">0%</span>
        )}
      </div>

      {/* 平均耗时 */}
      <div className="hidden w-14 shrink-0 text-right text-xs tabular-nums text-muted-foreground sm:block">
        {formatEwmaMs(c.ewmaDurationMs ?? 0)}
      </div>
    </div>
  );
}

/** 顶部汇总小块 */
function SummaryStat({
  label,
  value,
  accent,
}: {
  label: string;
  value: string;
  accent?: string;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[11px] uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <span className={`text-lg font-semibold tabular-nums ${accent ?? ""}`}>
        {value}
      </span>
    </div>
  );
}

/**
 * 并发监控视图：紧凑、近实时地展示每账号的调度负载。
 * 按 活跃(在途多者优先) → 冷却 → 空闲 → 禁用 排序，一屏扫完全部账号。
 */
export function ConcurrencyMonitor({
  credentials,
}: {
  credentials: CredentialStatusItem[];
}) {
  const sorted = useMemo(
    () => [...credentials].sort((a, b) => cmp(sortKey(a), sortKey(b))),
    [credentials],
  );

  const summary = useMemo(() => {
    let inFlight = 0;
    let capacity = 0;
    let active = 0;
    let usable = 0; // 未禁用且未冷却 = 可调度
    for (const c of credentials) {
      const disabled = c.disabled;
      const throttled = (c.throttledRemainingSecs ?? 0) > 0;
      inFlight += c.inFlight ?? 0;
      if (!disabled && !throttled) {
        capacity += c.maxConcurrency ?? 0;
        usable += 1;
      }
      if (!disabled && (c.inFlight ?? 0) > 0) active += 1;
    }
    const pct = capacity > 0 ? Math.round((inFlight / capacity) * 100) : 0;
    return { inFlight, capacity, active, usable, pct };
  }, [credentials]);

  if (credentials.length === 0) return null;

  return (
    <div className="space-y-4">
      {/* 汇总条 */}
      <div className="flex flex-wrap items-center gap-x-8 gap-y-3 rounded-2xl border border-border/60 bg-card/60 px-5 py-4 backdrop-blur">
        <div className="flex items-center gap-2">
          <Activity className="h-4 w-4 text-emerald-500" />
          <span className="text-sm font-medium">实时调度</span>
        </div>
        <SummaryStat
          label="总在途"
          value={String(summary.inFlight)}
          accent={summary.inFlight > 0 ? "text-emerald-600 dark:text-emerald-400" : ""}
        />
        <SummaryStat label="合并容量" value={String(summary.capacity)} />
        <SummaryStat
          label="整体负载"
          value={`${summary.pct}%`}
          accent={
            summary.pct >= 90
              ? "text-amber-600 dark:text-amber-400"
              : summary.pct > 0
                ? "text-emerald-600 dark:text-emerald-400"
                : ""
          }
        />
        <SummaryStat label="活跃账号" value={`${summary.active} / ${summary.usable}`} />
        {/* 整体负载条 */}
        <div className="min-w-[120px] flex-1">
          <div className="h-2 w-full overflow-hidden rounded-full bg-secondary">
            <div
              className={`h-full rounded-full transition-all ${
                summary.pct >= 90 ? "bg-amber-500" : "bg-emerald-500"
              }`}
              style={{ width: `${Math.min(100, summary.pct)}%` }}
            />
          </div>
        </div>
      </div>

      {/* 列头（中屏以上） */}
      <div className="hidden items-center gap-3 px-3 text-[11px] uppercase tracking-wider text-muted-foreground sm:flex">
        <span className="w-2.5 shrink-0" />
        <span className="flex-1">账号 / 负载</span>
        <span className="w-[88px] shrink-0 text-right">在途/上限</span>
        <span className="w-12 shrink-0 text-right">错误率</span>
        <span className="w-14 shrink-0 text-right">耗时</span>
      </div>

      {/* 账号行：紧凑网格 */}
      <div className="grid grid-cols-1 gap-2 lg:grid-cols-2">
        {sorted.map((c) => (
          <MonitorRow key={c.id} c={c} />
        ))}
      </div>
    </div>
  );
}

