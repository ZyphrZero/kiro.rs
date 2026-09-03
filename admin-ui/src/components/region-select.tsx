import { memo, useRef } from 'react'
import {
  Select,
  SelectGroup,
  SelectLabel,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select'
import { Input } from '@/components/ui/input'

/** 预设 AWS 区域（分组 + 显示名） */
export const AWS_REGION_GROUPS: { group: string; items: [string, string][] }[] = [
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

export const KNOWN_AWS_REGIONS = AWS_REGION_GROUPS.flatMap((g) => g.items.map(([v]) => v))

// shadcn Select 不允许 SelectItem 用空字符串作 value，故用哨兵值代表两种非预设状态
const CUSTOM = 'custom'
const INHERIT = 'inherit'

/**
 * AWS 区域选择：下拉预设区域 + 始终可输入的自定义文本框。
 */
export const RegionSelect = memo(function RegionSelect({
  value,
  onChange,
  allowInherit = false,
  inheritLabel = '跟随全局配置',
  placeholder = '例如: cn-north-1',
  disabled,
}: {
  value: string
  onChange: (v: string) => void
  allowInherit?: boolean
  inheritLabel?: string
  placeholder?: string
  disabled?: boolean
}) {
  const inputRef = useRef<HTMLInputElement>(null)
  const isInherit = allowInherit && value === ''
  const selectValue = KNOWN_AWS_REGIONS.includes(value)
    ? value
    : isInherit
      ? INHERIT
      : CUSTOM

  const handleSelectChange = (v: string) => {
    if (v === INHERIT) {
      onChange('')
      return
    }
    if (v !== CUSTOM) {
      onChange(v)
      return
    }
    // 从预设切到自定义：清空并聚焦，避免残留的预设值被误当成手填内容
    if (KNOWN_AWS_REGIONS.includes(value)) onChange('')
    requestAnimationFrame(() => inputRef.current?.focus())
  }

  return (
    <div className="flex gap-2">
      <Select value={selectValue} onValueChange={handleSelectChange} disabled={disabled}>
        <SelectTrigger className="flex-1 h-10">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {allowInherit && (
            <SelectGroup>
              <SelectItem value={INHERIT}>{inheritLabel}</SelectItem>
            </SelectGroup>
          )}
          {AWS_REGION_GROUPS.map((g) => (
            <SelectGroup key={g.group}>
              <SelectLabel>{g.group}</SelectLabel>
              {g.items.map(([v, label]) => (
                <SelectItem key={v} value={v}>
                  {label}
                </SelectItem>
              ))}
            </SelectGroup>
          ))}
          <SelectGroup>
            <SelectLabel>自定义</SelectLabel>
            <SelectItem value={CUSTOM}>-- 自定义输入 --</SelectItem>
          </SelectGroup>
        </SelectContent>
      </Select>
      <Input
        ref={inputRef}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={isInherit ? '' : placeholder}
        className="w-36"
        disabled={disabled}
      />
    </div>
  )
})
