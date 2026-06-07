import { useState, useEffect, useRef } from 'react'
import { toast } from 'sonner'
import { ExternalLink, Copy, Loader2, CheckCircle, Check } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { startIdcLogin, pollIdcLogin } from '@/api/credentials'
import type { StartIdcLoginResponse } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

/** 预设 SSO 区域（分组 + 显示名），与 AWS 常用区域一致 */
const SSO_REGION_GROUPS: { group: string; items: [string, string][] }[] = [
  {
    group: 'US',
    items: [
      ['us-east-1', 'us-east-1 (N. Virginia)'],
      ['us-east-2', 'us-east-2 (Ohio)'],
      ['us-west-1', 'us-west-1 (N. California)'],
      ['us-west-2', 'us-west-2 (Oregon)'],
    ],
  },
  {
    group: 'Europe',
    items: [
      ['eu-west-1', 'eu-west-1 (Ireland)'],
      ['eu-west-2', 'eu-west-2 (London)'],
      ['eu-west-3', 'eu-west-3 (Paris)'],
      ['eu-central-1', 'eu-central-1 (Frankfurt)'],
      ['eu-north-1', 'eu-north-1 (Stockholm)'],
      ['eu-south-1', 'eu-south-1 (Milan)'],
    ],
  },
  {
    group: 'Asia Pacific',
    items: [
      ['ap-northeast-1', 'ap-northeast-1 (Tokyo)'],
      ['ap-northeast-2', 'ap-northeast-2 (Seoul)'],
      ['ap-northeast-3', 'ap-northeast-3 (Osaka)'],
      ['ap-southeast-1', 'ap-southeast-1 (Singapore)'],
      ['ap-southeast-2', 'ap-southeast-2 (Sydney)'],
      ['ap-south-1', 'ap-south-1 (Mumbai)'],
      ['ap-east-1', 'ap-east-1 (Hong Kong)'],
    ],
  },
  {
    group: 'Other',
    items: [
      ['ca-central-1', 'ca-central-1 (Canada)'],
      ['sa-east-1', 'sa-east-1 (São Paulo)'],
      ['me-south-1', 'me-south-1 (Bahrain)'],
      ['af-south-1', 'af-south-1 (Cape Town)'],
    ],
  },
]

const KNOWN_SSO_REGIONS = SSO_REGION_GROUPS.flatMap((g) => g.items.map(([v]) => v))

/** SSO 区域选择：下拉预设区域 + 始终可输入的自定义文本框 */
function RegionSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const selectValue = KNOWN_SSO_REGIONS.includes(value) ? value : 'custom'
  return (
    <div className="flex gap-2">
      <select
        value={selectValue}
        onChange={(e) => {
          if (e.target.value !== 'custom') onChange(e.target.value)
        }}
        className="flex-1 h-10 rounded-md border border-input bg-background px-3 text-sm"
      >
        {SSO_REGION_GROUPS.map((g) => (
          <optgroup key={g.group} label={g.group}>
            {g.items.map(([v, label]) => (
              <option key={v} value={v}>
                {label}
              </option>
            ))}
          </optgroup>
        ))}
        <optgroup label="自定义">
          <option value="custom">-- 自定义输入 --</option>
        </optgroup>
      </select>
      <Input
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="例如: cn-north-1"
        className="w-36"
      />
    </div>
  )
}

interface IdcLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess: () => void
  /** 登录模式：'builder-id' 为 AWS Builder ID；'enterprise' 为企业 IAM Identity Center SSO */
  mode?: 'builder-id' | 'enterprise'
}

type Step = 'form' | 'waiting' | 'done'

export function IdcLoginDialog({ open, onOpenChange, onSuccess, mode = 'builder-id' }: IdcLoginDialogProps) {
  const isEnterprise = mode === 'enterprise'
  const [step, setStep] = useState<Step>('form')
  const [region, setRegion] = useState('us-east-1')
  const [startUrl, setStartUrl] = useState('')
  const [email, setEmail] = useState('')
  const [incognito, setIncognito] = useState(false)
  const [linkCopied, setLinkCopied] = useState(false)
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<StartIdcLoginResponse | null>(null)
  const [credentialId, setCredentialId] = useState<number | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // 清理轮询定时器
  useEffect(() => {
    return () => {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
    }
  }, [])

  // 对话框关闭时重置状态
  const handleOpenChange = (v: boolean) => {
    if (!v) {
      if (pollTimerRef.current) clearTimeout(pollTimerRef.current)
      setStep('form')
      setSession(null)
      setCredentialId(null)
      setIsStarting(false)
      setLinkCopied(false)
    }
    onOpenChange(v)
  }

  /** 复制验证链接到剪贴板（无痕模式下让用户手动在无痕窗口打开） */
  const copyVerificationUrl = async (resp: StartIdcLoginResponse) => {
    const url = resp.verificationUriComplete ?? resp.verificationUri
    try {
      await navigator.clipboard.writeText(url)
      setLinkCopied(true)
      setTimeout(() => setLinkCopied(false), 2000)
      toast.success('登录链接已复制，请在无痕窗口粘贴打开')
    } catch {
      toast.error('复制失败，请手动复制链接')
    }
  }

  const handleStart = async () => {
    if (!region.trim()) {
      toast.error('请填写 SSO 区域')
      return
    }
    if (isEnterprise && !startUrl.trim()) {
      toast.error('请填写 SSO Start URL')
      return
    }
    setIsStarting(true)
    try {
      const resp = await startIdcLogin({
        region: region.trim(),
        startUrl: startUrl.trim() || undefined,
        email: email.trim() || undefined,
      })
      setSession(resp)
      setStep('waiting')
      if (incognito) {
        await copyVerificationUrl(resp)
      }
      schedulePoll(resp.sessionId, resp.pollInterval)
    } catch (e) {
      toast.error('发起登录失败：' + extractErrorMessage(e))
    } finally {
      setIsStarting(false)
    }
  }

  const schedulePoll = (sessionId: string, interval: number) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollIdcLogin(sessionId)
        if (result.status === 'pending') {
          schedulePoll(sessionId, interval)
        } else if (result.status === 'success') {
          setCredentialId(result.credentialId)
          setStep('done')
          onSuccess()
          toast.success(`登录成功，已添加凭据 #${result.credentialId}`)
        } else {
          toast.error('授权已过期，请重新发起登录')
          setStep('form')
          setSession(null)
        }
      } catch (e) {
        toast.error('轮询状态失败：' + extractErrorMessage(e))
        schedulePoll(sessionId, interval)
      }
    }, interval * 1000)
  }

  const copyCode = () => {
    if (!session) return
    navigator.clipboard.writeText(session.userCode)
    toast.success('验证码已复制')
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            {isEnterprise ? 'Enterprise IAM Identity Center SSO 登录' : 'AWS SSO / Builder ID 登录'}
          </DialogTitle>
          <DialogDescription>
            {isEnterprise
              ? '填写组织的 SSO Start URL 与区域，通过设备授权流程添加企业凭据。'
              : '通过 AWS Identity Center 设备授权流程添加凭据，无需手动导出 refreshToken。'}
          </DialogDescription>
        </DialogHeader>

        {step === 'form' && isEnterprise && (
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <label htmlFor="idc-start-url" className="text-sm font-medium">
                SSO Start URL
              </label>
              <Input
                id="idc-start-url"
                placeholder="https://your-org.awsapps.com/start"
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <label htmlFor="idc-region" className="text-sm font-medium">SSO 区域</label>
              <RegionSelect value={region} onChange={setRegion} />
            </div>
          </div>
        )}

        {step === 'form' && !isEnterprise && (
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <label htmlFor="idc-region" className="text-sm font-medium">AWS Region</label>
              <RegionSelect value={region} onChange={setRegion} />
            </div>
            <div className="space-y-1.5">
              <label htmlFor="idc-start-url" className="text-sm font-medium">
                SSO Start URL
                <span className="ml-1 text-xs text-muted-foreground">
                  （留空使用 AWS Builder ID）
                </span>
              </label>
              <Input
                id="idc-start-url"
                placeholder="https://view.awsapps.com/start"
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <label htmlFor="idc-email" className="text-sm font-medium">邮箱（可选）</label>
              <Input
                id="idc-email"
                placeholder="user@example.com"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>
          </div>
        )}

        {step === 'form' && (
          <label className="flex items-start gap-2 rounded-lg border bg-muted/40 p-3 cursor-pointer">
            <input
              type="checkbox"
              checked={incognito}
              onChange={(e) => setIncognito(e.target.checked)}
              className="mt-0.5 h-4 w-4 shrink-0 accent-primary"
            />
            <span className="text-sm">
              <span className="font-medium">使用无痕窗口登录</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">
                发起后复制验证链接，自行用浏览器无痕/隐身窗口（Ctrl+Shift+N）打开，
                避免与当前已登录的 AWS 账号串号。
              </span>
            </span>
          </label>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <div className="rounded-lg border bg-muted/50 p-4 text-center space-y-3">
              {incognito ? (
                <>
                  <p className="text-sm text-muted-foreground">
                    验证链接已复制。请新开一个
                    <span className="font-medium text-foreground">无痕 / 隐身窗口</span>
                    （Ctrl+Shift+N，Safari 为 ⌘+Shift+N），粘贴打开并完成授权。
                  </p>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => copyVerificationUrl(session)}
                  >
                    {linkCopied ? (
                      <Check className="h-3.5 w-3.5" />
                    ) : (
                      <Copy className="h-3.5 w-3.5" />
                    )}
                    {linkCopied ? '已复制' : '复制验证链接'}
                  </Button>
                </>
              ) : (
                <>
                  <p className="text-sm text-muted-foreground">在浏览器中访问以下地址并输入验证码</p>
                  <a
                    href={session.verificationUriComplete ?? session.verificationUri}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="inline-flex items-center gap-1.5 text-sm font-medium text-primary hover:underline"
                  >
                    {session.verificationUri}
                    <ExternalLink className="h-3.5 w-3.5" />
                  </a>
                </>
              )}
              <div className="flex items-center justify-center gap-2">
                <span className="font-mono text-2xl font-bold tracking-widest">
                  {session.userCode}
                </span>
                <Button variant="ghost" size="icon" className="h-7 w-7" onClick={copyCode}>
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              正在等待授权，请在浏览器中完成登录…
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="flex flex-col items-center gap-3 py-4">
            <CheckCircle className="h-10 w-10 text-green-500" />
            <p className="text-sm font-medium">登录成功</p>
            <p className="text-xs text-muted-foreground">
              凭据 #{credentialId} 已添加并启用
            </p>
          </div>
        )}

        <DialogFooter>
          {step === 'form' && (
            <Button onClick={handleStart} disabled={isStarting}>
              {isStarting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              发起登录
            </Button>
          )}
          {step === 'waiting' && (
            <Button variant="outline" onClick={() => handleOpenChange(false)}>
              取消
            </Button>
          )}
          {step === 'done' && (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
