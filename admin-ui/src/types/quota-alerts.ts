export interface QuotaAlertMode {
  enabled: boolean
  thresholds: number[]
}

export interface QuotaAlertSettings {
  account: QuotaAlertMode
  pool: QuotaAlertMode
  poolCooldownSecs: number
  maxDataAgeSecs: number
}

export interface QuotaAccountUsage {
  id: number
  name: string
  status: 'valid' | 'missing' | 'failed' | 'stale' | 'invalid'
  queriedAt: number | null
  used: number | null
  limit: number | null
  percentage: number | null
  overageEnabled: boolean | null
  nextResetAt: number | null
}

export interface QuotaAlertsResponse {
  settings: QuotaAlertSettings
  webhookConfigured: boolean
  signingSecretConfigured: boolean
  notifierEnabled: boolean
  refreshIntervalSecs: number
  sending: boolean
  error: string | null
  lastDelivery: {
    at: number
    success: boolean
    target: string
    message: string
  } | null
  overview: {
    completedAt: number | null
    enabledAccounts: number
    validAccounts: number
    overageEnabled: number
    overageUnknown: number
    missingAccounts: number
    failedAccounts: number
    staleAccounts: number
    invalidAccounts: number
    unknownCycleAccounts: number
    complete: boolean
    used: number
    limit: number
    percentage: number | null
    thresholdCounts: Record<string, number>
    accounts: QuotaAccountUsage[]
  }
}

export interface UpdateQuotaAlertsRequest {
  settings: QuotaAlertSettings
  webhook?: string
  signingSecret?: string
}
