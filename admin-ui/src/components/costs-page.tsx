import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { useUrlState } from '@/hooks/use-url-state'
import { api } from '@/api/credentials'
import { isAxiosError } from 'axios'

interface Totals {
  paid: string | null
  usageCost: string | null
  reportedCredits: string | null
  unpricedCredits: string | null
  remainingCost: string | null
  expiryLoss: string | null
  pendingExpiryLoss: string | null
  pendingCost: string | null
  attempts: number
  missingMeteringCalls: number
  partialMeteringCalls: number
  unavailableMeteringCalls: number
  invalidMeteringCalls: number
}

interface Lot {
  id: string
  amount: string
  kind: keyof typeof kindNames
  credits: string
  effectiveAt: number
  expiresAt: number | null
  paidAt: number | null
  remainingCredits: string
  remainingCost: string
  state: keyof typeof lotStates
}

interface Call {
  id: string
  startedAt: number
  model: string
  endpoint: string
  attempt: number
  trafficClass: string
  requestId: string | null
  credits: string | null
  cost: string | null
  unpricedCredits: string
  metering: keyof typeof meterStates
  invalidMetering: boolean
  allocations: { lotId: string; credits: string; cost: string }[]
}

interface Report {
  from: string
  to: string
  generatedAt: string
  coverageState: string
  captureErrorsSinceStart: number
  captureErrors: number
  totalsAvailable: boolean
  invalidLots: { id: string; credentialId: number; reason: string }[]
  totals: Totals
  accounts: { credentialId: number; totals: Totals }[]
  activeCredentialIds: number[]
  lots: Lot[]
  recentCalls: Call[]
  latestBalance: { observedAt: number; data: {
    usedCredits: number; totalCredits: number; remainingCredits: number;
    nextResetAt: number | null; overageEnabled: boolean | null;
    buckets: { kind: 'plan' | 'trial' | 'bonus'; usedCredits: number; totalCredits: number; expiresAt: number | null }[];
  } } | null
}

const kindNames = { purchase: '采购包', plan: '套餐', add_on: '加购', bonus: '赠送' }
const lotStates = { active: '生效中', pending: '待生效', expired: '已确认到期', pending_expiry: '待确认到期' }
const meterStates = { complete: '已报告', partial: '部分计量', missing: '缺计量', unavailable: '无独立计量' }
const defaults = { credential: '', period: 'month' }
const selectClass = 'h-9 rounded-md border bg-background px-2 text-sm'
const cell = 'px-3 py-2 text-left whitespace-nowrap'

function showError(error: unknown): string {
  if (isAxiosError(error) && error.code === 'ECONNABORTED') return '成本计算超时，请缩小日期范围或选择单个账号后重试。'
  return isAxiosError(error) && error.response?.data?.error?.message
    ? error.response.data.error.message
    : error instanceof Error ? error.message : '操作失败'
}

function reportWindow(period: string) {
  const now = new Date()
  const to = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1)
  const from = period === 'month'
    ? new Date(now.getFullYear(), now.getMonth(), 1)
    : new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1 - Number(period))
  return { from: from.toISOString(), to: to.toISOString() }
}

function localInputTime() {
  const date = new Date()
  date.setMinutes(date.getMinutes() - date.getTimezoneOffset())
  return date.toISOString().slice(0, 16)
}

function money(value: string | null) {
  return value == null ? '—' : `¥${value}`
}

function time(value: number | null) {
  return value == null ? '—' : new Date(value).toLocaleString()
}

function downloadReport(report: Report) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `kiro-costs-${report.from.slice(0, 10)}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}

function purchaseId() {
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  bytes[6] = (bytes[6] & 15) | 64
  bytes[8] = (bytes[8] & 63) | 128
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')
  return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20)].join('-')
}

function PurchaseForm({ credentialId }: { credentialId: number }) {
  const client = useQueryClient()
  const [open, setOpen] = useState(false)
  const [opening, setOpening] = useState(false)
  const [id, setId] = useState(purchaseId)
  const save = useMutation({
    mutationFn: async (form: FormData) => {
      const effectiveAt = new Date(String(form.get('effectiveAt'))).toISOString()
      const expiry = String(form.get('expiresAt'))
      const paid = String(form.get('paidAt') ?? '')
      await api.post('/costs/purchases', {
        id, credentialId, effectiveAt,
        expiresAt: expiry ? new Date(expiry).toISOString() : null,
        paidAt: opening ? null : new Date(paid).toISOString(),
        amount: String(form.get('amount')).trim(),
        credits: String(form.get('credits')).trim(),
        kind: form.get('kind'),
      })
    },
    onSuccess: () => {
      setOpen(false)
      setId(purchaseId())
      void client.invalidateQueries({ queryKey: ['cost-ledger'] })
    },
  })
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    save.mutate(new FormData(event.currentTarget))
  }
  if (!open) return <Button onClick={() => setOpen(true)}>新增积分批次</Button>
  return (
    <Card>
      <CardHeader><CardTitle>录入凭据 #{credentialId} 的采购</CardTitle></CardHeader>
      <CardContent>
        <form onSubmit={submit} className="space-y-4">
          <label className="flex items-center gap-2 text-sm">
            <input type="checkbox" checked={opening} onChange={(e) => setOpening(e.target.checked)} />
            期初余额（采购款已发生，当前期间不记现金支出）
          </label>
          <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
            <label className="space-y-1 text-sm">{opening ? '剩余积分对应成本（元）' : '实付金额（元）'}
              <Input name="amount" inputMode="decimal" placeholder="10.00" required />
            </label>
            <label className="space-y-1 text-sm">{opening ? '期初剩余积分' : '到账可用积分'}
              <Input name="credits" inputMode="decimal" placeholder="100" required />
            </label>
            <label className="grid content-start gap-1 text-sm">积分类型
              <select name="kind" className={selectClass}>
                {Object.entries(kindNames).map(([value, name]) => <option key={value} value={value}>{name}</option>)}
              </select>
            </label>
            {!opening && <label className="space-y-1 text-sm">付款时间
              <Input name="paidAt" type="datetime-local" defaultValue={localInputTime()} required />
            </label>}
            <label className="space-y-1 text-sm">开始使用该批积分的时间
              <Input name="effectiveAt" type="datetime-local" defaultValue={localInputTime()} required />
              {opening && <span className="block text-xs text-muted-foreground">填写这份剩余积分对应的余额时点。此前用量不会扣减此余额，请勿将今天的余额直接回填到过去。</span>}
            </label>
            <label className="space-y-1 text-sm">到期时间（可选）
              <Input name="expiresAt" type="datetime-local" />
            </label>
          </div>
          {save.error && <p role="alert" className="text-sm text-destructive">{showError(save.error)}</p>}
          <div className="flex gap-2">
            <Button type="submit" disabled={save.isPending}>{save.isPending ? '保存中…' : '保存批次'}</Button>
            <Button type="button" variant="outline" disabled={save.isPending} onClick={() => {
              setOpen(false)
              setId(purchaseId())
              save.reset()
            }}>取消</Button>
          </div>
        </form>
      </CardContent>
    </Card>
  )
}

function Summary({ report }: { report: Report }) {
  const { totals } = report
  const cards = [
    ['已付采购款', money(totals.paid)],
    ['已分摊使用成本', money(totals.usageCost)],
    ['期末剩余积分成本', money(totals.remainingCost)],
    ['确认到期损失', money(totals.expiryLoss)],
    ['待生效成本余额', money(totals.pendingCost)],
    ['待确认到期损失', money(totals.pendingExpiryLoss)],
  ]
  return (
    <section className="space-y-3" aria-label="成本汇总">
      <div role="status" className="rounded-md border border-amber-500/50 bg-amber-500/10 p-3 text-sm">
        <strong>{report.coverageState === 'partial' ? '部分覆盖：金额不代表完整 AI 成本' : '尚未对账：金额为本地采购成本分摊'}</strong>
        <p className="mt-1">{totals.missingMeteringCalls} 次缺计量，{totals.partialMeteringCalls} 次部分计量，{totals.unavailableMeteringCalls} 次无独立计量，{totals.invalidMeteringCalls} 次含无效数值。待定价 {totals.unpricedCredits ?? '—'} 积分。</p>
        {report.captureErrors > 0 && <p>累计 {report.captureErrors} 次采集异常（当前进程 {report.captureErrorsSinceStart} 次），可能涉及所选范围以外的记录；已恢复的记录也保留异常提示。</p>}
        {!report.totalsAvailable && <p role="alert">汇总数值超出精确计算范围，金额已隐藏，请按账号核对。</p>}
        {report.invalidLots.length > 0 && <p role="alert">{report.invalidLots.length} 个批次的数值无效，未计入金额：{report.invalidLots.map((lot) => `凭据 #${lot.credentialId} / ${lot.id}（${lot.reason}）`).join('；')}</p>}
      </div>
    <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
      {cards.map(([label, value]) => (
        <Card key={label}>
          <CardHeader className="pb-2"><CardTitle className="text-sm font-medium">{label}</CardTitle></CardHeader>
          <CardContent className="break-all text-xl font-semibold tabular-nums">{value}</CardContent>
        </Card>
      ))}
    </div>
      <p className="text-xs text-muted-foreground">采购支出、使用成本和确认损失按所选期间统计；各项余额为期末存量。还需考虑期初余额，因此这些卡片不能直接相加配平。</p>
    </section>
  )
}

function AccountDetails({ report }: { report: Report }) {
  const client = useQueryClient()
  const confirm = useConfirm()
  const expiry = useMutation({
    mutationFn: (id: string) => api.post(`/costs/lots/${id}/confirm-expiry`),
    onSuccess: () => client.invalidateQueries({ queryKey: ['cost-ledger'] }),
  })
  return (
    <>
      {report.latestBalance && <Card>
        <CardHeader><CardTitle>最近一次上游余额</CardTitle></CardHeader>
        <CardContent className="space-y-1 text-sm">
          <p>{time(report.latestBalance.observedAt)} · 约 {report.latestBalance.data.remainingCredits.toFixed(4)} / {report.latestBalance.data.totalCredits} 积分剩余</p>
          {report.latestBalance.data.nextResetAt != null && <p>下次重置 {time(report.latestBalance.data.nextResetAt * 1000)}</p>}
          {report.latestBalance.data.buckets.map((bucket, index) => <p key={index} className="text-muted-foreground">
            {{ plan: '套餐', trial: '试用', bonus: '奖励' }[bucket.kind]} · 已用 {bucket.usedCredits} / {bucket.totalCredits}
            {bucket.expiresAt != null && ` · ${time(bucket.expiresAt * 1000)} 到期`}
          </p>)}
          {report.latestBalance.data.overageEnabled && <p className="text-muted-foreground">上游标记已启用超额。请单独核对该账号账单。</p>}
        </CardContent>
      </Card>}
      <Card>
        <CardHeader><CardTitle>采购批次</CardTitle></CardHeader>
        <CardContent className="overflow-x-auto">
          {expiry.error && <p role="alert" className="text-sm text-destructive">{showError(expiry.error)}</p>}
          <p className="mb-3 text-xs text-muted-foreground">未分摊成本按状态分别属于生效中余额、待生效余额或到期损失；已确认到期的残额代表损失，不再属于可用余额。</p>
          {report.lots.length === 0 ? <p className="text-sm text-muted-foreground">所选日期前无采购批次。</p> : (
            <table className="w-full text-sm">
              <thead><tr>{['类型', '金额', '到账积分', '剩余积分', '未分摊成本', '生效', '到期', '状态'].map((h) => <th key={h} className={cell}>{h}</th>)}</tr></thead>
              <tbody>
                {report.lots.map((lot) => (
                  <tr key={lot.id} className="border-t">
                    <td className={cell}>{lot.paidAt == null ? '期初 · ' : ''}{kindNames[lot.kind]}</td>
                    <td className={cell}>{money(lot.amount)}</td><td className={cell}>{lot.credits}</td>
                    <td className={cell}>{lot.remainingCredits}</td><td className={cell}>{money(lot.remainingCost)}</td>
                    <td className={cell}>{time(lot.effectiveAt)}</td><td className={cell}>{time(lot.expiresAt)}</td>
                    <td className={cell}>{lotStates[lot.state]} {lot.state === 'pending_expiry' && (
                      <Button size="sm" variant="outline" disabled={expiry.isPending} onClick={async () => {
                        if (await confirm({ title: '确认批次到期', description: '确认上游已到期且剩余积分无法使用后，将未分摊成本计入当前期间损失。' })) expiry.mutate(lot.id)
                      }}>确认损失</Button>
                    )}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </CardContent>
      </Card>
      <Card>
        <CardHeader><CardTitle>近期上游调用（最多 100 条）</CardTitle></CardHeader>
        <CardContent className="overflow-x-auto">
          {report.recentCalls.length === 0 ? <p className="text-sm text-muted-foreground">所选日期内无调用记录。</p> : (
            <table className="w-full text-sm">
              <thead><tr>{['时间', '模型', '来源', '积分', '已分摊成本', '计量状态', '请求'].map((h) => <th key={h} className={cell}>{h}</th>)}</tr></thead>
              <tbody>{report.recentCalls.map((call) => (
                <tr key={call.id} className="border-t">
                  <td className={cell}>{time(call.startedAt)}</td><td className={cell}>{call.model || '—'}</td>
                  <td className={cell}>{call.trafficClass === 'admin_test' ? '模型测试' : call.trafficClass === 'mcp' ? '搜索' : 'API'} · {call.endpoint} · {call.attempt + 1}</td>
                  <td className={cell}>{call.credits ?? '—'}</td><td className={cell}>{money(call.cost)}</td>
                  <td className={cell}>{meterStates[call.metering]}{call.invalidMetering && ' · 无效数值已隔离'}{call.unpricedCredits !== '0' && ` · ${call.unpricedCredits} 待定价`}</td>
                  <td className={cell} title={call.id}>{call.requestId?.slice(0, 8) ?? '—'}</td>
                </tr>
              ))}</tbody>
            </table>
          )}
        </CardContent>
      </Card>
    </>
  )
}

export function CostsPage() {
  const [filters, setFilters] = useUrlState('costs', defaults)
  const period = ['month', '1', '7', '30', '90'].includes(filters.period) ? filters.period : 'month'
  const id = Number(filters.credential)
  const credential = /^[1-9]\d*$/.test(filters.credential) && Number.isSafeInteger(id) ? id : undefined
  const window = reportWindow(period)
  const query = useQuery({
    queryKey: ['cost-ledger', window, credential],
    queryFn: async () => (await api.get<Report>('/costs/overview', { params: { ...window, credentialId: credential }, timeout: 60000 })).data,
    retry: false,
  })
  const report = query.data
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-2 text-sm">时间范围
          <select className={selectClass} value={period} onChange={(e) => setFilters({ period: e.target.value })}>
            <option value="month">本月</option><option value="1">今日</option><option value="7">近 7 天</option>
            <option value="30">近 30 天</option><option value="90">近 90 天</option>
          </select>
        </label>
        <Button variant="outline" onClick={() => setFilters({ credential: '' })} disabled={!credential}>全部账号</Button>
        {credential && <span className="text-sm">凭据 #{credential}</span>}
        <Button variant="outline" onClick={() => void query.refetch()} disabled={query.isFetching}>刷新</Button>
        {report && <Button variant="outline" onClick={() => downloadReport(report)}>导出报表快照</Button>}
      </div>
      {query.isFetching && <p role="status" className="text-sm text-muted-foreground">正在计算成本，历史记录较多时可能需要更长时间…</p>}
      {query.error && <p role="alert" className="text-sm text-destructive">{showError(query.error)}</p>}
      {report && <>
        <Summary report={report} />
        <p className="text-sm text-muted-foreground">
          已报告 {report.totals.reportedCredits ?? '—'} 积分，{report.totals.unpricedCredits ?? '—'} 待定价，
          {report.totals.missingMeteringCalls} 次缺计量，{report.totals.partialMeteringCalls} 次部分计量，
          {report.totals.unavailableMeteringCalls} 次无独立计量。待确认到期成本 {money(report.totals.pendingExpiryLoss)}。
          {report.captureErrorsSinceStart > 0 && ` 当前进程有 ${report.captureErrorsSinceStart} 次采集异常。`}
          {' '}用量依据上游报告，金额为本地采购成本分摊。
          {' '}导出为当前报表快照，单账号最多含最近 100 条调用，不是完整调用明细。
        </p>
        {credential ? <>
          {report.activeCredentialIds.includes(credential) && <PurchaseForm key={credential} credentialId={credential} />}
          <AccountDetails key={credential} report={report} />
        </> : (
          <Card>
            <CardHeader><CardTitle>按账号查看</CardTitle></CardHeader>
            <CardContent className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead><tr>{['账号', '采购支出', '报告积分', '使用成本', '剩余成本', '待生效成本', '到期损失', '待确认损失', '计量缺口'].map((h) => <th key={h} className={cell}>{h}</th>)}</tr></thead>
                <tbody>{Array.from(new Set([...report.activeCredentialIds, ...report.accounts.map((a) => a.credentialId)]))
                  .sort((a, b) => a - b).map((id) => {
                    const row = report.accounts.find((a) => a.credentialId === id)?.totals
                    return <tr key={id} className="border-t">
                      <td className={cell}><Button variant="link" onClick={() => setFilters({ credential: String(id) })}>凭据 #{id}</Button></td>
                      <td className={cell}>{money(row?.paid ?? null)}</td><td className={cell}>{row?.reportedCredits ?? '—'}</td>
                      <td className={cell}>{money(row?.usageCost ?? null)}</td><td className={cell}>{money(row?.remainingCost ?? null)}</td>
                      <td className={cell}>{money(row?.pendingCost ?? null)}</td>
                      <td className={cell}>{money(row?.expiryLoss ?? null)}</td>
                      <td className={cell}>{money(row?.pendingExpiryLoss ?? null)}</td>
                      <td className={cell}>{row ? `${row.missingMeteringCalls + row.partialMeteringCalls + row.unavailableMeteringCalls} 次 / ${row.unpricedCredits ?? '—'} 待定价积分` : '—'}</td>
                    </tr>
                  })}</tbody>
              </table>
            </CardContent>
          </Card>
        )}
      </>}
    </div>
  )
}
