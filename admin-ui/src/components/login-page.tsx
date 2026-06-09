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
    <div className="min-h-screen flex items-center justify-center p-4">
      <div className="w-full max-w-[400px] animate-fade-in">
        <div className="rounded-2xl border border-border/60 bg-card/80 backdrop-blur-2xl backdrop-saturate-150 shadow-apple-lg p-8">
          <div className="flex flex-col items-center text-center mb-7">
            <img
              src="/admin/kirors.png"
              alt="Kiro"
              className="mb-4 h-20 w-20 object-contain"
              draggable={false}
            />
            <h1 className="text-[22px] font-semibold tracking-tight">
              Kiro Admin
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              使用登录API密钥登录管理面板
            </p>
          </div>
          <form onSubmit={handleSubmit} className="space-y-3">
            <div className="relative">
              <Lock className="pointer-events-none absolute left-3.5 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
              <Input
                type="password"
                placeholder="登录API密钥"
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setError(null);
                }}
                className="h-11 pl-10"
                disabled={isSubmitting}
                autoFocus
              />
            </div>
            {error && (
              <div className="rounded-xl border border-destructive/30 bg-destructive/10 px-3 py-2.5 text-[13px] text-destructive">
                {error}
              </div>
            )}
            <Button
              type="submit"
              size="lg"
              className="w-full"
              disabled={!apiKey.trim() || isSubmitting}
            >
              {isSubmitting ? "登录中…" : "登录"}
            </Button>
          </form>
        </div>
      </div>
    </div>
  );
}
