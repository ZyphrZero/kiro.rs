import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { useTraces } from "@/hooks/use-traces";
import type { TraceAttempt, TraceRecord } from "@/types/api";

interface CredentialFailuresDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  credentialId: number;
  email?: string;
}

/** 失败分类 → 中文标签 + Badge 颜色 */
function outcomeStyle(outcome: string | null): {
  label: string;
  variant: "destructive" | "warning" | "outline" | "secondary";
} {
  switch (outcome) {
    case "quota_exhausted":
      return { label: "额度耗尽", variant: "warning" };
    case "account_throttled":
      return { label: "账号风控", variant: "warning" };
    case "auth_failed":
      return { label: "鉴权失败", variant: "destructive" };
    case "transient":
      return { label: "瞬态错误", variant: "outline" };
    case "network_error":
      return { label: "网络错误", variant: "destructive" };
    case "bad_request":
      return { label: "请求错误", variant: "destructive" };
    case "stream_interrupted":
      return { label: "流中断", variant: "warning" };
    default:
      return { label: outcome || "未知", variant: "secondary" };
  }
}

function formatTime(ts: string): string {
  const d = new Date(ts);
  if (isNaN(d.getTime())) return ts;
  return d.toLocaleString("zh-CN", { hour12: false });
}

function keySourceLabel(rec: TraceRecord): string {
  if (rec.keySource === "clientKey") return rec.keyName ?? `#${rec.keyId}`;
  return "管理员API密钥";
}

export function CredentialFailuresDialog({
  open,
  onOpenChange,
  credentialId,
  email,
}: CredentialFailuresDialogProps) {
  const { data, isLoading } = useTraces(
    { failedAttemptCredentialId: credentialId, limit: 50 },
    open,
  );
  const records = data?.records ?? [];
  // 摊平：同一请求里该凭据失败了几跳就显示几条（按时间倒序）
  const failedHops = records.flatMap((rec) =>
    rec.attempts
      .filter((a) => a.credentialId === credentialId && a.outcome !== "success")
      .map((a) => ({ rec, attempt: a })),
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>失败日志详情</DialogTitle>
          <DialogDescription>
            {email || `凭据 #${credentialId}`} 最近的失败记录（最多 50 条请求）
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-[60vh] space-y-2 overflow-y-auto">
          {isLoading ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              加载中…
            </div>
          ) : failedHops.length === 0 ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              该凭据暂无失败记录（trace 关闭或近期无失败）。
            </div>
          ) : (
            failedHops.map(({ rec, attempt }) => (
              <FailureRow
                key={`${rec.traceId}-${attempt.attempt}`}
                rec={rec}
                attempt={attempt}
              />
            ))
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 单跳失败：展示该凭据某次失败的 outcome / HTTP / 错误体 */
function FailureRow({
  rec,
  attempt,
}: {
  rec: TraceRecord;
  attempt: TraceAttempt;
}) {
  const style = outcomeStyle(attempt.outcome);
  // 整条 trace 后续是否成功了（用别的凭据救回）
  const traceRecovered = rec.finalStatus === "success";
  return (
    <div className="rounded-lg border border-border/50 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="tabular-nums text-muted-foreground">
          {formatTime(rec.ts)}
        </span>
        <Badge variant="secondary">{keySourceLabel(rec)}</Badge>
        <Badge variant={style.variant}>{style.label}</Badge>
        {attempt.httpStatus != null && (
          <span className="font-mono text-muted-foreground">
            HTTP {attempt.httpStatus}
          </span>
        )}
        {rec.totalAttempts > 1 && (
          <span className="text-[12px] text-muted-foreground">
            第 {attempt.attempt + 1}/{rec.totalAttempts} 跳
          </span>
        )}
        {traceRecovered && (
          <Badge variant="outline">本次请求最终由其他凭据成功</Badge>
        )}
        {rec.finalStatus === "interrupted" && (
          <Badge variant="warning">中断</Badge>
        )}
        <span className="ml-auto text-[12px] text-muted-foreground">
          {rec.model}
        </span>
      </div>
      {attempt.errorSnippet && (
        <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background/60 p-2 font-mono text-[11px] text-muted-foreground">
          {attempt.errorSnippet}
        </pre>
      )}
    </div>
  );
}
