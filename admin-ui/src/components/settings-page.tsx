import {
  Cpu,
  Gauge,
  Globe,
  ScrollText,
  PackageOpen,
  ShieldCheck,
  Tags,
} from 'lucide-react'
import { PageHeader } from '@/components/console/page-header'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent } from '@/components/ui/card'
import { useUrlState } from '@/hooks/use-url-state'
import { cn } from '@/lib/utils'
import { DispatchSection } from '@/components/settings/dispatch-section'
import { NetworkSection } from '@/components/settings/network-section'
import { LogSection } from '@/components/settings/log-section'
import { SystemSection } from '@/components/settings/system-section'
import { SecuritySection } from '@/components/settings/security-section'
import { MetadataSection } from '@/components/settings/metadata-section'
import { ModelsSection } from '@/components/settings/models-section'

/**
 * 设置页 —— 把此前散在三处的 7 个配置端点收拢到一处。
 *
 * 改造前它们分别住在：顶栏按钮（负载均衡）、顶栏两个下拉（风控故障转移、自愈）、
 * 顶栏设置菜单（登录密钥）、日志页下拉（日志治理）、代理池弹窗内（全局代理）、
 * 镜像更新弹窗内（更新配置）。同一类东西分在六个地方，找一个配置得先记住它藏在哪。
 *
 * 顶栏**保留**三个快捷开关（负载均衡 / 故障转移 / 自愈），因为它们是运维高频动作，
 * 一次点击就该切换完；但参数（冷却时长、连续上限、保留天数这些）全部移到这里 ——
 * 下拉菜单里塞数字输入框本来就不是它该干的事。
 */
type SectionKey = 'dispatch' | 'metadata' | 'network' | 'log' | 'models' | 'system' | 'security'

const SECTIONS: {
  key: SectionKey
  label: string
  icon: React.ReactNode
}[] = [
  {
    key: 'dispatch',
    label: '调度',
    icon: <Gauge className="h-4 w-4" />,
  },
  {
    key: 'metadata',
    label: '凭据字段',
    icon: <Tags className="h-4 w-4" />,
  },
  {
    key: 'network',
    label: '网络',
    icon: <Globe className="h-4 w-4" />,
  },
  {
    key: 'log',
    label: '日志',
    icon: <ScrollText className="h-4 w-4" />,
  },
  {
    key: 'models',
    label: '模型',
    icon: <Cpu className="h-4 w-4" />,
  },
  {
    key: 'system',
    label: '系统',
    icon: <PackageOpen className="h-4 w-4" />,
  },
  {
    key: 'security',
    label: '安全',
    icon: <ShieldCheck className="h-4 w-4" />,
  },
]

export function SettingsPage() {
  const [urlState, patchUrl] = useUrlState('settings', { s: 'dispatch' })
  const active = (SECTIONS.some((x) => x.key === urlState.s)
    ? urlState.s
    : 'dispatch') as SectionKey

  const activeMeta = SECTIONS.find((s) => s.key === active) ?? SECTIONS[0]

  return (
    <div className="console-scope space-y-4">
      <PageHeader
        breadcrumbs={[
          { label: '控制台' },
          { label: '系统设置', onClick: () => patchUrl({ s: 'dispatch' }) },
          { label: activeMeta.label, active: true },
        ]}
        icon={activeMeta.icon}
        title={`设置 · ${activeMeta.label}`}
        description="所有改动即时生效并直接落盘写入 config.json，无需重启服务进程。"
        badge={
          <Badge variant="outline" className="font-mono text-xs">
            热重载就绪
          </Badge>
        }
      />

      {/* 窄屏设备横向滚动的子菜单导航栏（桌面端由左侧全局侧边栏接管） */}
      <nav
        className="flex lg:hidden shrink-0 gap-1 overflow-x-auto px-1 py-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        aria-label="设置分区"
      >
        {SECTIONS.map((s) => (
          <button
            key={s.key}
            type="button"
            onClick={() => patchUrl({ s: s.key })}
            aria-current={active === s.key ? 'page' : undefined}
            className={cn(
              'inline-flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors',
              'focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring',
              active === s.key
                ? 'bg-primary/10 text-primary font-semibold'
                : 'text-muted-foreground hover:bg-accent hover:text-foreground',
            )}
          >
            {s.icon}
            <span className="whitespace-nowrap">{s.label}</span>
          </button>
        ))}
      </nav>

      <div className="flex flex-col gap-4">

        <Card className="min-w-0 flex-1">
          <CardContent className="p-4 sm:p-5">
            {active === 'dispatch' && <DispatchSection />}
            {active === 'metadata' && <MetadataSection />}
            {active === 'models' && <ModelsSection />}
            {active === 'network' && <NetworkSection />}
            {active === 'log' && <LogSection />}
            {active === 'system' && <SystemSection />}
            {active === 'security' && <SecuritySection />}
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
