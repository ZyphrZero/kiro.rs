import { useState, useEffect } from "react";
import { Lock } from "lucide-react";
import { storage } from "@/lib/storage";
import { getCredentials } from "@/api/credentials";
import { extractErrorMessage } from "@/lib/utils";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";

interface LoginPageProps {
  onLogin: (apiKey: string) => void;
}

export function LoginPage({ onLogin }: LoginPageProps) {
  const [apiKey, setApiKey] = useState("");
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const savedKey = storage.getApiKey();
    if (savedKey) setApiKey(savedKey);
  }, []);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const key = apiKey.trim();
    if (!key || isSubmitting) return;
    setIsSubmitting(true);
    setError(null);
    storage.setApiKey(key);
    try {
      await getCredentials();
      onLogin(key);
    } catch (err) {
      storage.removeApiKey();
      setError(extractErrorMessage(err));
    } finally {
      setIsSubmitting(false);
    }
  };

  return (
    <div className="min-h-screen flex flex-col items-center justify-center p-4 bg-background">
      <div className="w-full max-w-[380px] animate-fade-in">
        <div className="rounded-lg border border-border bg-card p-6 shadow-xs">
          <div className="flex flex-col items-center text-center mb-6">
            <img
              src="/admin/kirors.png"
              alt="Kiro"
              className="mb-3 h-12 w-12 object-contain"
              draggable={false}
            />
            <div className="flex items-center gap-1.5">
              <h1 className="text-base font-semibold tracking-tight">
                Kiro Gateway
              </h1>
              <span className="rounded border border-border bg-secondary px-1.5 py-0.2 text-[10px] font-mono text-muted-foreground">
                Console
              </span>
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              API 凭据与上游集群管理控制台
            </p>
          </div>
          <form onSubmit={handleSubmit} className="space-y-3.5">
            <div>
              <label className="text-[11px] font-medium text-muted-foreground mb-1.5">
                管理员密钥 (Admin API Key)
              </label>
              <div className="relative">
                <Lock className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground" />
                <Input
                  type="password"
                  placeholder="输入 adminApiKey"
                  value={apiKey}
                  onChange={(e) => {
                    setApiKey(e.target.value);
                    setError(null);
                  }}
                  className="h-9 pl-9 text-xs font-mono"
                  disabled={isSubmitting}
                  autoFocus
                />
              </div>
            </div>
            {error && (
              <div className="rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                {error}
              </div>
            )}
            <Button
              type="submit"
              className="w-full h-8 text-xs font-medium"
              disabled={!apiKey.trim() || isSubmitting}
            >
              {isSubmitting ? "正在验证…" : "验证并进入控制台"}
            </Button>
          </form>
          <div className="mt-5 pt-3 border-t border-border flex items-center justify-between text-[10px] font-mono text-muted-foreground">
            <span>Core: Kiro.rs</span>
            <span className="inline-flex items-center gap-1">
              <span className="inline-block size-1.5 rounded-full bg-emerald-500" />
              Ready
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
