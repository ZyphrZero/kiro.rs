import { useState } from 'react'
import { toast } from 'sonner'
import { Plus, Trash2, Zap } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { useModelMappings, useSetModelMappings } from '@/hooks/use-credentials'
import type { ModelMappingRule } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

// 目标模型候选（Claude 系，dashed；与后端 /v1/models 及 map_model 可解析集对齐）
const TARGET_MODELS = [
  'claude-opus-4-8',
  'claude-opus-4-7',
  'claude-opus-4-6',
  'claude-opus-4-5',
  'claude-sonnet-4-8',
  'claude-sonnet-4-6',
  'claude-sonnet-4-5',
  'claude-haiku-4-5',
]

// 常见源模型名候选（GPT/Codex 系，供输入提示）
const SOURCE_MODELS = [
  'gpt-5.5', 'gpt-5.5-pro', 'gpt-5.5-instant',
  'gpt-5.4', 'gpt-5.4-pro', 'gpt-5.4-mini',
  'gpt-5.3-codex', 'gpt-5.3-instant',
  'gpt-5.2', 'gpt-5.2-pro', 'gpt-5.2-codex',
  'gpt-5.1', 'gpt-5.1-pro', 'gpt-5.1-codex', 'gpt-5.1-instant',
]

// 预置 GPT/Codex → Claude 映射（dashed 目标，与本项目 map_model 对齐）
const PRESET_RULES: { source: string; target: string; name: string }[] = [
  { source: 'gpt-5.5', target: 'claude-opus-4-8', name: 'GPT-5.5 → Opus 4.8' },
  { source: 'gpt-5.5-pro', target: 'claude-opus-4-7', name: 'GPT-5.5-pro → Opus 4.7' },
  { source: 'gpt-5.5-instant', target: 'claude-sonnet-4-6', name: 'GPT-5.5-instant → Sonnet 4.6' },
  { source: 'gpt-5.4', target: 'claude-opus-4-6', name: 'GPT-5.4 → Opus 4.6' },
  { source: 'gpt-5.4-mini', target: 'claude-sonnet-4-6', name: 'GPT-5.4-mini → Sonnet 4.6' },
  { source: 'gpt-5.3-codex', target: 'claude-opus-4-5', name: 'GPT-5.3-codex → Opus 4.5' },
  { source: 'gpt-5.3-instant', target: 'claude-sonnet-4-5', name: 'GPT-5.3-instant → Sonnet 4.5' },
  { source: 'gpt-5.2', target: 'claude-opus-4-5', name: 'GPT-5.2 → Opus 4.5' },
  { source: 'gpt-5.1', target: 'claude-sonnet-4-5', name: 'GPT-5.1 → Sonnet 4.5' },
  { source: 'gpt-5.1-codex', target: 'claude-sonnet-4-5', name: 'GPT-5.1-codex → Sonnet 4.5' },
  { source: 'gpt-5.1-instant', target: 'claude-haiku-4-5', name: 'GPT-5.1-instant → Haiku 4.5' },
]

// PLACEHOLDER_PANEL

export function ModelMappingPanel() {
  const { data: rules = [], isLoading } = useModelMappings()
  const setMappings = useSetModelMappings()

  const [newSource, setNewSource] = useState('')
  const [newTarget, setNewTarget] = useState('')
  const [newRuleType, setNewRuleType] = useState('replace')

  const persist = async (next: ModelMappingRule[], okMsg: string) => {
    try {
      await setMappings.mutateAsync(next)
      toast.success(okMsg)
    } catch (err) {
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  const handleToggle = (idx: number, checked: boolean) => {
    persist(rules.map((r, i) => (i === idx ? { ...r, enabled: checked } : r)), checked ? '已启用规则' : '已停用规则')
  }

  const handleDelete = (idx: number) => {
    persist(rules.filter((_, i) => i !== idx), '已删除规则')
  }

  const handleAdd = () => {
    const source = newSource.trim()
    const target = newTarget.trim()
    if (!source || !target) return
    if (rules.some((r) => r.sourceModel === source)) {
      toast.error(`源模型 ${source} 已有规则`)
      return
    }
    const rule: ModelMappingRule = {
      id: crypto.randomUUID(),
      name: `${source} → ${target}`,
      enabled: true,
      ruleType: newRuleType,
      sourceModel: source,
      targetModel: target,
    }
    persist([...rules, rule], `已添加映射：${rule.name}`)
    setNewSource('')
    setNewTarget('')
    setNewRuleType('replace')
  }

  const handlePreset = () => {
    const existing = new Set(rules.map((r) => r.sourceModel))
    const adds = PRESET_RULES.filter((p) => !existing.has(p.source)).map((p) => ({
      id: crypto.randomUUID(),
      name: p.name,
      enabled: true,
      ruleType: 'replace',
      sourceModel: p.source,
      targetModel: p.target,
    }))
    if (adds.length === 0) {
      toast.info('预置规则均已存在')
      return
    }
    persist([...rules, ...adds], `已载入 ${adds.length} 条预置映射`)
  }

  return (
    <div className="space-y-3">
      {/* 规则列表 */}
      <div className="overflow-hidden rounded-md border border-border/60">
        {isLoading ? (
          <div className="p-6 text-center text-sm text-muted-foreground">加载中…</div>
        ) : rules.length === 0 ? (
          <div className="p-6 text-center text-sm text-muted-foreground">暂无规则。未命中的模型名原样透传。</div>
        ) : (
          rules.map((rule, idx) => (
            <div
              key={rule.id}
              className={`flex items-center gap-2 border-b border-border/40 p-2.5 last:border-b-0 ${!rule.enabled ? 'opacity-50' : ''}`}
            >
              <Switch checked={rule.enabled} onCheckedChange={(c) => handleToggle(idx, c)} disabled={setMappings.isPending} />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-1.5">
                  <span className="truncate text-sm font-medium">{rule.name || rule.sourceModel}</span>
                  <Badge variant="outline" className="px-1.5 py-0 text-[10px]">
                    {rule.ruleType === 'alias' ? '别名' : '替换'}
                  </Badge>
                </div>
                <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">
                  {rule.sourceModel} → {rule.targetModel}
                </div>
              </div>
              <Button
                size="sm"
                variant="ghost"
                className="h-7 w-7 p-0 text-red-500 hover:bg-red-50 hover:text-red-600 dark:hover:bg-red-950/20"
                onClick={() => handleDelete(idx)}
                disabled={setMappings.isPending}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </Button>
            </div>
          ))
        )}
      </div>

      {/* 添加新规则 */}
      <div className="space-y-2 rounded-md border border-border/60 bg-secondary/20 p-2.5">
        <div className="text-xs font-medium text-muted-foreground">添加新规则</div>
        <div className="grid grid-cols-2 gap-2">
          <Input placeholder="源模型名" className="h-8 text-xs" value={newSource} onChange={(e) => setNewSource(e.target.value)} list="mm-source-list" />
          <datalist id="mm-source-list">{SOURCE_MODELS.map((m) => <option key={m} value={m} />)}</datalist>
          <Input placeholder="目标模型名" className="h-8 text-xs" value={newTarget} onChange={(e) => setNewTarget(e.target.value)} list="mm-target-list" />
          <datalist id="mm-target-list">{TARGET_MODELS.map((m) => <option key={m} value={m} />)}</datalist>
        </div>
        <div className="flex gap-2">
          <Select value={newRuleType} onValueChange={setNewRuleType}>
            <SelectTrigger className="h-8 flex-1 text-xs"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="replace">替换 (replace)</SelectItem>
              <SelectItem value="alias">别名 (alias)</SelectItem>
            </SelectContent>
          </Select>
          <Button size="sm" className="h-8 text-xs" onClick={handleAdd} disabled={!newSource.trim() || !newTarget.trim() || setMappings.isPending}>
            <Plus className="mr-1 h-3.5 w-3.5" />添加
          </Button>
        </div>
      </div>

      <div className="flex items-center justify-between">
        <div className="text-xs text-muted-foreground">快速添加 GPT/Codex 兼容映射</div>
        <Button size="sm" variant="outline" className="h-7 text-xs" onClick={handlePreset} disabled={setMappings.isPending}>
          <Zap className="mr-1 h-3 w-3" />预置 GPT 映射
        </Button>
      </div>
    </div>
  )
}

export default ModelMappingPanel
