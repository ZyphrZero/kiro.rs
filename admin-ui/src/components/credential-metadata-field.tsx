import { memo } from 'react'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import type { CredentialMetadataSchema } from '@/types/api'
import type { CredentialMetadata } from '@/types/api'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'

interface CredentialMetadataFieldProps {
  schema?: CredentialMetadataSchema
  fieldKey: string
  value: string
  onValueChange: (value: string) => void
  disabled?: boolean
  className?: string
}

/** 按后端 JSON Schema 渲染 metadata 枚举字段，避免在各个表单重复硬编码 key/value。 */
export function CredentialMetadataField({
  schema,
  fieldKey,
  value,
  onValueChange,
  disabled,
  className,
}: CredentialMetadataFieldProps) {
  const field = schema?.properties[fieldKey]
  if (!field?.oneOf?.length) return null

  return (
    <div className="space-y-2">
      <label htmlFor={`metadata-${fieldKey}`} className="text-sm font-medium">
        {field.title}
      </label>
      <Select value={value} onValueChange={onValueChange} disabled={disabled}>
        <SelectTrigger
          id={`metadata-${fieldKey}`}
          className={className ?? 'h-10 rounded-xl px-3.5'}
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {field.oneOf.map((option) => (
            <SelectItem key={String(option.const)} value={String(option.const)}>
              {option.title}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {field.description && (
        <p className="text-xs text-muted-foreground">{field.description}</p>
      )}
    </div>
  )
}

export function metadataFieldDefault(
  schema: CredentialMetadataSchema | undefined,
  fieldKey: string,
  fallback: string,
): string {
  const value = schema?.properties[fieldKey]?.default
  return typeof value === 'string' ? value : fallback
}

export function metadataFieldValues(
  schema: CredentialMetadataSchema | undefined,
  fieldKey: string,
): string[] {
  return schema?.properties[fieldKey]?.oneOf?.map((option) => String(option.const)) ?? []
}

export function metadataDefaults(
  schema: CredentialMetadataSchema | undefined,
): CredentialMetadata {
  const values: Record<string, unknown> = {}
  for (const [key, field] of Object.entries(schema?.properties ?? {})) {
    if (field.default !== undefined) values[key] = field.default
  }
  return {
    ...values,
    type: typeof values.type === 'string' ? values.type : 'normal',
    saleStatus: typeof values.saleStatus === 'string' ? values.saleStatus : 'not_for_sale',
  } as CredentialMetadata
}

interface CredentialMetadataEditorProps {
  schema?: CredentialMetadataSchema
  value: CredentialMetadata
  onChange: (value: CredentialMetadata) => void
  disabled?: boolean
}

/** 按 schema 渲染全部已登记字段；schema 外扩展键仍留在 value 中，不会被覆盖。 */
export const CredentialMetadataEditor = memo(function CredentialMetadataEditor({
  schema,
  value,
  onChange,
  disabled,
}: CredentialMetadataEditorProps) {
  if (!schema) return null

  const setField = (key: string, next: unknown) => {
    const updated = { ...value }
    if (next === undefined || next === '') delete updated[key]
    else updated[key] = next
    onChange(updated)
  }

  return (
    <div className="space-y-4">
      {Object.entries(schema.properties).map(([key, field]) => {
        const current = value[key] ?? field.default
        if (field.oneOf?.length) {
          return (
            <CredentialMetadataField
              key={key}
              schema={schema}
              fieldKey={key}
              value={typeof current === 'string' ? current : ''}
              onValueChange={(next) => {
                if (field.type === 'boolean') setField(key, next === 'true')
                else if (field.type === 'number' || field.type === 'integer') setField(key, Number(next))
                else setField(key, next)
              }}
              disabled={disabled}
            />
          )
        }
        if (field.type === 'boolean') {
          return (
            <div key={key} className="space-y-1.5">
              <label className="flex items-center justify-between gap-3 text-sm font-medium">
                <span>{field.title}</span>
                <Switch
                  checked={Boolean(current)}
                  onCheckedChange={(next) => setField(key, next)}
                  disabled={disabled}
                />
              </label>
              {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
            </div>
          )
        }
        return (
          <div key={key} className="space-y-2">
            <label htmlFor={`metadata-${key}`} className="text-sm font-medium">{field.title}</label>
            <Input
              id={`metadata-${key}`}
              type={field.type === 'string' ? 'text' : 'number'}
              step={field.type === 'integer' ? 1 : 'any'}
              min={field.minimum}
              value={current == null ? '' : String(current)}
              onChange={(event) => {
                const raw = event.target.value
                const parsed = Number(raw)
                setField(
                  key,
                  field.type === 'string' || raw === ''
                    ? raw
                    : Number.isFinite(parsed) ? parsed : undefined,
                )
              }}
              disabled={disabled}
            />
            {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
          </div>
        )
      })}
    </div>
  )
})
