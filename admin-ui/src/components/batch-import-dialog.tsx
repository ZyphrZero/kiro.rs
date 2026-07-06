import { useRef, useState } from 'react'
import { toast } from 'sonner'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { CheckCircle2, XCircle, AlertCircle, Loader2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials } from '@/hooks/use-credentials'
import {
  batchImportCredentials,
  getProxyPool,
  type BatchImportItemEvent,
  type BatchImportSummary,
} from '@/api/credentials'
import type { AddCredentialRequest } from '@/types/api'
import { extractErrorMessage, sha256Hex, normalizeImportAuthMethod } from '@/lib/utils'

interface BatchImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface CredentialInput {
  refreshToken?: string
  clientId?: string
  clientSecret?: string
  region?: string
  authRegion?: string
  apiRegion?: string
  priority?: number
  machineId?: string
  kiroApiKey?: string
  authMethod?: string
  endpoint?: string
  email?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  // 企业 SSO (external_idp)
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'imported' | 'duplicate' | 'failed'
  error?: string
  usage?: string
  email?: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}



export function BatchImportDialog({ open, onOpenChange }: BatchImportDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])
  // 进行中的 AbortController，用于"停止导入"：abort 会让 fetch 流中断，
  // 服务端在下次写回事件时检测到接收端关闭即停止处理剩余凭据。
  const abortRef = useRef<AbortController | null>(null)

  const { data: existingCredentials } = useCredentials()
  const queryClient = useQueryClient()
  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  const resetForm = () => {
    setJsonInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
  }

  // 按原始下标局部更新单行结果（避免每条全量拷贝之外的额外复杂度）
  const updateResult = (i: number, patch: Partial<VerificationResult>) => {
    setResults(prev => {
      const next = [...prev]
      next[i] = { ...next[i], ...patch }
      return next
    })
  }

  const handleBatchImport = async (verify: boolean) => {
    // 先单独解析 JSON，给出精准的错误提示
    let credentials: CredentialInput[]
    try {
      const parsed = JSON.parse(jsonInput)
      credentials = Array.isArray(parsed) ? parsed : [parsed]
    } catch (error) {
      toast.error('JSON 格式错误: ' + extractErrorMessage(error))
      return
    }

    if (credentials.length === 0) {
      toast.error('没有可导入的凭据')
      return
    }

    try {
      setImporting(true)
      setProgress({ current: 0, total: credentials.length })

      // 初始化结果
      const initialResults: VerificationResult[] = credentials.map((_, i) => ({
        index: i + 1,
        status: 'pending'
      }))
      setResults(initialResults)

      // 客户端去重：OAuth 与 API Key 分别使用对应的 hash 集合
      const existingOauthHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.refreshTokenHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )
      const existingApiKeyHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.apiKeyHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )

      // 可用的代理池条目（用于无代理凭据的随机分配）
      const enabledProxies = proxyPool?.proxies.filter(p => p.enabled) ?? []

      // 本地预处理：代理分配 + 去重 + 校验 + 构造请求。
      // 不通过的行直接标终态；通过的收集进 toImport，记录其原始下标，
      // 以便把服务端 SSE 事件（按 toImport 内位置返回 index）映射回对应行。
      const toImport: { index: number; req: AddCredentialRequest }[] = []

      for (let i = 0; i < credentials.length; i++) {
        const cred = credentials[i]

        // 若凭据未指定代理且代理池有可用代理，随机分配一个
        if (!cred.proxyUrl?.trim() && enabledProxies.length > 0) {
          const picked = enabledProxies[Math.floor(Math.random() * enabledProxies.length)]
          cred.proxyUrl = picked.url
        }
        const isApiKeyCred = !!(cred.kiroApiKey?.trim()) || cred.authMethod === 'api_key'

        updateResult(i, { status: 'checking' })

        if (isApiKeyCred) {
          const apiKey = cred.kiroApiKey?.trim() || ''
          if (!apiKey) {
            updateResult(i, { status: 'failed', error: '缺少 kiroApiKey' })
            continue
          }
          const credHash = await sha256Hex(apiKey)
          if (existingApiKeyHashes.has(credHash)) {
            const existingCred = existingCredentials?.credentials.find(c => c.apiKeyHash === credHash)
            updateResult(i, {
              status: 'duplicate',
              error: '该凭据已存在',
              email: existingCred?.email || undefined
            })
            continue
          }
          existingApiKeyHashes.add(credHash)
          toImport.push({
            index: i,
            req: {
              authMethod: 'api_key',
              kiroApiKey: apiKey,
              priority: cred.priority || 0,
              authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
              apiRegion: cred.apiRegion?.trim() || undefined,
              machineId: cred.machineId?.trim() || undefined,
              endpoint: cred.endpoint?.trim() || undefined,
              email: cred.email?.trim() || undefined,
              proxyUrl: cred.proxyUrl?.trim() || undefined,
              proxyUsername: cred.proxyUsername?.trim() || undefined,
              proxyPassword: cred.proxyPassword?.trim() || undefined,
            },
          })
        } else {
          const token = cred.refreshToken?.trim() || ''
          if (!token) {
            updateResult(i, { status: 'failed', error: '缺少 refreshToken' })
            continue
          }
          const credHash = await sha256Hex(token)
          if (existingOauthHashes.has(credHash)) {
            const existingCred = existingCredentials?.credentials.find(c => c.refreshTokenHash === credHash)
            updateResult(i, {
              status: 'duplicate',
              error: '该凭据已存在',
              email: existingCred?.email || undefined
            })
            continue
          }
          existingOauthHashes.add(credHash)

          const clientId = cred.clientId?.trim() || undefined
          const clientSecret = cred.clientSecret?.trim() || undefined
          const tokenEndpoint = cred.tokenEndpoint?.trim() || undefined
          const { authMethod, error: authError } = normalizeImportAuthMethod(cred.authMethod, {
            tokenEndpoint,
            clientId,
            clientSecret,
          })
          if (authError) {
            updateResult(i, { status: 'failed', error: authError })
            continue
          }
          const isExternalIdp = authMethod === 'external_idp'

          toImport.push({
            index: i,
            req: {
              refreshToken: token,
              authMethod,
              provider: isExternalIdp ? 'AzureAD' : undefined,
              authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
              apiRegion: cred.apiRegion?.trim() || undefined,
              clientId,
              // external_idp 为公共客户端，不携带 clientSecret
              clientSecret: isExternalIdp ? undefined : clientSecret,
              tokenEndpoint,
              issuerUrl: cred.issuerUrl?.trim() || undefined,
              scopes: cred.scopes?.trim() || undefined,
              priority: cred.priority || 0,
              machineId: cred.machineId?.trim() || undefined,
              endpoint: cred.endpoint?.trim() || undefined,
              email: cred.email?.trim() || undefined,
              proxyUrl: cred.proxyUrl?.trim() || undefined,
              proxyUsername: cred.proxyUsername?.trim() || undefined,
              proxyPassword: cred.proxyPassword?.trim() || undefined,
            },
          })
        }
      }

      // 待上传的行标记为验活中
      for (const item of toImport) {
        updateResult(item.index, { status: 'verifying' })
      }

      if (toImport.length === 0) {
        setCurrentProcessing('没有需要上传的凭据（全部重复或校验失败）')
      } else {
        setCurrentProcessing(
          `${verify ? '批量验活' : '直接导入'}中（${toImport.length} 个）…`,
        )
        // 一次性 POST，服务端有界并发处理，逐条通过 SSE 回传结果。
        // 事件 ev.index 是 toImport 内的位置，需映射回原始凭据下标。
        const controller = new AbortController()
        abortRef.current = controller
        await batchImportCredentials(
          { credentials: toImport.map(t => t.req), concurrency: 8, verify },
          (ev: BatchImportItemEvent) => {
            const orig = toImport[ev.index]?.index ?? -1
            if (orig < 0) return
            if (ev.status === 'verified') {
              updateResult(orig, {
                status: 'verified',
                usage: ev.usage,
                email: ev.email,
                credentialId: ev.credentialId,
              })
              setCurrentProcessing(ev.email ? `验活成功: ${ev.email}` : '验活成功')
            } else if (ev.status === 'imported') {
              updateResult(orig, {
                status: 'imported',
                email: ev.email,
                credentialId: ev.credentialId,
              })
              setCurrentProcessing(ev.email ? `已导入: ${ev.email}` : '已导入')
            } else if (ev.status === 'duplicate') {
              updateResult(orig, { status: 'duplicate', error: ev.error || '该凭据已存在' })
            } else {
              updateResult(orig, {
                status: 'failed',
                error: ev.error,
                rollbackStatus: ev.rolledBack ? 'success' : undefined,
              })
            }
          },
          (s: BatchImportSummary) => {
            const importedTotal = s.imported + s.verified
            if (verify) {
              if (s.failed === 0 && s.duplicate === 0) {
                toast.success(`成功导入并验活 ${s.verified} 个凭据`)
              } else {
                toast.info(
                  `验活完成：成功 ${s.verified} 个，重复 ${s.duplicate} 个，失败 ${s.failed} 个（已排除 ${s.rolledBack}）`
                )
                if (s.rolledBack < s.failed) {
                  toast.warning(`有 ${s.failed - s.rolledBack} 个失败凭据回滚未完成，请手动处理`)
                }
              }
            } else {
              if (s.failed === 0 && s.duplicate === 0) {
                toast.success(`直接导入 ${importedTotal} 个凭据（未验活）`)
              } else {
                toast.info(
                  `导入完成：成功 ${importedTotal} 个，重复 ${s.duplicate} 个，失败 ${s.failed} 个`
                )
              }
            }
          },
          controller.signal,
        )
      }

      // 刷新凭据列表，让新导入的立即可见
      await queryClient.invalidateQueries({ queryKey: ['credentials'] })
    } catch (error) {
      // 用户点击"停止"→ AbortError，服务端会停止处理剩余凭据；已完成的保留。
      if (error instanceof DOMException && error.name === 'AbortError') {
        toast.info('已停止导入（已完成的凭据保留）')
        await queryClient.invalidateQueries({ queryKey: ['credentials'] })
      } else {
        toast.error('导入失败: ' + extractErrorMessage(error))
      }
    } finally {
      abortRef.current = null
      setImporting(false)
    }
  }

  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking':
      case 'verifying':
        return <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
      case 'verified':
        return <CheckCircle2 className="w-5 h-5 text-green-500" />
      case 'imported':
        return <CheckCircle2 className="w-5 h-5 text-sky-500" />
      case 'duplicate':
        return <AlertCircle className="w-5 h-5 text-yellow-500" />
      case 'failed':
        return <XCircle className="w-5 h-5 text-red-500" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending':
        return '等待中'
      case 'checking':
        return '检查重复...'
      case 'verifying':
        return '处理中...'
      case 'verified':
        return '验活成功'
      case 'imported':
        return '已导入（未验活）'
      case 'duplicate':
        return '重复凭据'
      case 'failed':
        if (result.rollbackStatus === 'success') return '验活失败（已排除）'
        if (result.rollbackStatus === 'failed') return '验活失败（未排除）'
        return '处理失败（未创建）'
    }
  }

  // 已终结（verified/imported/duplicate/failed）的行数，驱动进度条；客户端去重/校验在
  // 上传前即完成，故这些行在 SSE 流开始前就已计入。
  const finalizedCount = results.filter(
    r =>
      r.status === 'verified' ||
      r.status === 'imported' ||
      r.status === 'duplicate' ||
      r.status === 'failed'
  ).length

  return (
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        if (!newOpen) {
          if (importing) {
            // 导入过程中关闭 = 停止导入（abort 服务端流）
            abortRef.current?.abort()
          } else {
            resetForm()
          }
        }
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>批量导入凭据</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">
              JSON 格式凭据
            </label>
            <textarea
              placeholder={'粘贴 JSON 格式的凭据（支持单个对象或数组）\n\nOAuth: [{"refreshToken":"...","clientId":"...","clientSecret":"..."}]\nAPI Key: [{"kiroApiKey":"ksk_xxx"}]\n企业 SSO: [{"authMethod":"external_idp","refreshToken":"...","clientId":"...","tokenEndpoint":"https://login.microsoftonline.com/<tenant>/oauth2/v2.0/token","scopes":"...","region":"eu-central-1"}]\n\n支持 region 字段自动映射为 authRegion'}
              value={jsonInput}
              onChange={(e) => setJsonInput(e.target.value)}
              disabled={importing}
              className="flex min-h-[200px] w-full rounded-xl border border-input bg-background/60 px-3.5 py-2.5 text-sm transition-[border-color,background-color,box-shadow] duration-150 ease-apple placeholder:text-muted-foreground/70 hover:border-border focus-visible:outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/30 focus-visible:bg-background disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
            <p className="text-xs text-muted-foreground">
              💡 "开始导入并验活"会校验余额、失败自动排除；"直接导入"只落库不验活（更快）。两种模式均支持中途"停止"。
            </p>
          </div>

          {(importing || results.length > 0) && (
            <>
              {/* 进度条 */}
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? '验活进度' : '验活完成'}</span>
                  <span>{finalizedCount} / {progress.total}</span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div
                    className="bg-primary h-2 rounded-full transition-all"
                    style={{ width: `${progress.total > 0 ? (finalizedCount / progress.total) * 100 : 0}%` }}
                  />
                </div>
                {importing && currentProcessing && (
                  <div className="text-xs text-muted-foreground">
                    {currentProcessing}
                  </div>
                )}
              </div>

              {/* 统计 */}
              <div className="flex gap-4 text-sm">
                <span className="text-green-600 dark:text-green-400">
                  ✓ 验活成功: {results.filter(r => r.status === 'verified').length}
                </span>
                <span className="text-sky-600 dark:text-sky-400">
                  ✓ 已导入: {results.filter(r => r.status === 'imported').length}
                </span>
                <span className="text-yellow-600 dark:text-yellow-400">
                  ⚠ 重复: {results.filter(r => r.status === 'duplicate').length}
                </span>
                <span className="text-red-600 dark:text-red-400">
                  ✗ 失败: {results.filter(r => r.status === 'failed').length}
                </span>
              </div>

              {/* 结果列表 */}
              <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
                {results.map((result) => (
                  <div key={result.index} className="p-3">
                    <div className="flex items-start gap-3">
                      {getStatusIcon(result.status)}
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="text-sm font-medium">
                            {result.email || `凭据 #${result.index}`}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {getStatusText(result)}
                          </span>
                        </div>
                        {result.usage && (
                          <div className="text-xs text-muted-foreground mt-1">
                            用量: {result.usage}
                          </div>
                        )}
                        {result.error && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            {result.error}
                          </div>
                        )}
                        {result.rollbackError && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            回滚失败: {result.rollbackError}
                          </div>
                        )}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <DialogFooter>
          {importing ? (
            <Button
              type="button"
              variant="destructive"
              onClick={() => abortRef.current?.abort()}
            >
              停止导入
            </Button>
          ) : (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  onOpenChange(false)
                  resetForm()
                }}
              >
                {results.length > 0 ? '关闭' : '取消'}
              </Button>
              {results.length === 0 && (
                <>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => handleBatchImport(false)}
                    disabled={!jsonInput.trim()}
                  >
                    直接导入（不验活）
                  </Button>
                  <Button
                    type="button"
                    onClick={() => handleBatchImport(true)}
                    disabled={!jsonInput.trim()}
                  >
                    开始导入并验活
                  </Button>
                </>
              )}
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
