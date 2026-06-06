import { useState } from "react";
import { toast } from "sonner";
import { Pencil, Power, RefreshCw, CircleDot } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { SubscriptionBadge } from "@/components/subscription-badge";
import { EditCredentialDialog } from "@/components/edit-credential-dialog";
import { useSetDisabled } from "@/hooks/use-credentials";
import { cn, extractErrorMessage, type GroupColor } from "@/lib/utils";
import type { CredentialStatusItem, BalanceResponse } from "@/types/api";

interface CredentialRowProps {
  credential: CredentialStatusItem;
  selected: boolean;
  onToggleSelect: () => void;
  balance: BalanceResponse | null;
  loadingBalance: boolean;
  onRefreshBalance: () => void;
  /** 该行是否为所在分组当前在用的账号 */
  groupCurrent?: boolean;
  /** 所在分组的配色（用于高亮区分） */
  highlightColor?: GroupColor;
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return "从未";
  const diff = Date.now() - new Date(lastUsedAt).getTime();
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}秒前`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}小时前`;
  return `${Math.floor(h / 24)}天前`;
}

export function CredentialRow({
  credential,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onRefreshBalance,
  groupCurrent,
  highlightColor,
}: CredentialRowProps) {
  const [showEdit, setShowEdit] = useState(false);
  const setDisabled = useSetDisabled();

  const toggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: () =>
          toast.success(credential.disabled ? "已启用" : "已禁用"),
        onError: (e) => toast.error(`操作失败: ${extractErrorMessage(e)}`),
      },
    );
  };

  const bal = balance || credential.balance || null;
  const throttled = (credential.throttledRemainingSecs ?? 0) > 0;

  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-lg border border-border/50 bg-background/40 px-3 py-2 text-[13px] transition-colors hover:bg-accent/30",
        groupCurrent && highlightColor
          ? `ring-2 ${highlightColor.ring}`
          : "",
        credential.disabled ? "opacity-60" : "",
      )}
    >
      <Checkbox
        checked={selected}
        onCheckedChange={onToggleSelect}
        data-no-rect-select
      />

      {/* 邮箱 + 徽章 */}
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          {groupCurrent && highlightColor && (
            <span
              title="该分组当前在用"
              className={cn("inline-flex items-center gap-1 text-[11px] font-medium", highlightColor.text)}
            >
              <CircleDot className="h-3 w-3" />
              当前
            </span>
          )}
          <span className="truncate font-medium">
            {credential.email || `凭据 #${credential.id}`}
          </span>
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-1">
          {bal?.subscriptionTitle && (
            <SubscriptionBadge title={bal.subscriptionTitle} />
          )}
          {(credential.groups ?? []).map((g) => (
            <Badge key={g} variant="outline" className="text-[10px]">
              {g}
            </Badge>
          ))}
          {credential.sourceChannel && (
            <Badge variant="outline" className="text-[10px]" title="来源渠道">
              来源: {credential.sourceChannel}
            </Badge>
          )}
        </div>
      </div>

      {/* 成功/失败 */}
      <div className="hidden w-20 shrink-0 text-right text-muted-foreground sm:block">
        <div className="tabular-nums">成功 {credential.successCount}</div>
        <div className="tabular-nums">失败 {credential.failureCount}</div>
      </div>

      {/* 余额 */}
      <div className="hidden w-28 shrink-0 text-right md:block">
        {bal ? (
          <>
            <div className="font-medium tabular-nums">
              ${bal.remaining.toFixed(0)}
            </div>
            <div className="text-[11px] text-muted-foreground tabular-nums">
              {bal.usagePercentage.toFixed(0)}% 已用
            </div>
          </>
        ) : (
          <button
            className="text-[11px] text-muted-foreground underline-offset-2 hover:underline disabled:opacity-50"
            onClick={onRefreshBalance}
            disabled={loadingBalance}
          >
            {loadingBalance ? "查询中…" : "查余额"}
          </button>
        )}
      </div>

      {/* 最后调用 */}
      <div className="hidden w-16 shrink-0 text-right text-[11px] text-muted-foreground lg:block">
        {formatLastUsed(credential.lastUsedAt)}
      </div>

      {/* 状态 */}
      <div className="w-16 shrink-0 text-center">
        {credential.disabled ? (
          <Badge variant="destructive">已禁用</Badge>
        ) : throttled ? (
          <Badge variant="warning">冷却</Badge>
        ) : (
          <Badge variant="success">可用</Badge>
        )}
      </div>

      {/* 操作 */}
      <div className="flex shrink-0 items-center gap-0.5">
        <Button size="icon" variant="ghost" className="h-7 w-7" onClick={() => setShowEdit(true)} title="编辑">
          <Pencil className="h-3.5 w-3.5" />
        </Button>
        <Button size="icon" variant="ghost" className="h-7 w-7" onClick={onRefreshBalance} disabled={loadingBalance} title="刷新余额">
          <RefreshCw className={cn("h-3.5 w-3.5", loadingBalance && "animate-spin")} />
        </Button>
        <Button size="icon" variant="ghost" className="h-7 w-7" onClick={toggleDisabled} title={credential.disabled ? "启用" : "禁用"}>
          <Power className={cn("h-3.5 w-3.5", credential.disabled ? "text-emerald-500" : "text-amber-500")} />
        </Button>
      </div>

      {showEdit && (
        <EditCredentialDialog open={showEdit} onOpenChange={setShowEdit} credential={credential} />
      )}
    </div>
  );
}
