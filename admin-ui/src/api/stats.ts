import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialActivityResponse,
  CredentialDistribution,
  KeyDistribution,
  ModelDistribution,
  OverviewStats,
  StatsFilter,
  StatsTimeFilter,
  TimeSeriesPoint,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export async function getOverview(): Promise<OverviewStats> {
  const { data } = await api.get<OverviewStats>('/stats/overview')
  return data
}

export async function getCredentialActivity(): Promise<CredentialActivityResponse> {
  const { data } = await api.get<CredentialActivityResponse>('/stats/credential-activity')
  if (!data || typeof data !== 'object' || Array.isArray(data) || Object.values(data).some((activity) =>
    !activity || typeof activity !== 'object' ||
    !Number.isSafeInteger(activity.todayCalls) || activity.todayCalls < 0 ||
    !Number.isSafeInteger(activity.rpm) || activity.rpm < 0
  )) {
    throw new Error('请求统计响应格式无效')
  }
  return data
}

function statsParams(time: StatsTimeFilter, filter?: StatsFilter) {
  return {
    ...time,
    ...(filter?.keyId !== undefined ? { keyId: filter.keyId } : {}),
    ...(filter?.group ? { group: filter.group } : {}),
  }
}

export async function getTimeSeries(time: StatsTimeFilter, filter?: StatsFilter): Promise<TimeSeriesPoint[]> {
  const { data } = await api.get<TimeSeriesPoint[]>('/stats/timeseries', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByModel(time: StatsTimeFilter, filter?: StatsFilter): Promise<ModelDistribution[]> {
  const { data } = await api.get<ModelDistribution[]>('/stats/by-model', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByCredential(time: StatsTimeFilter, filter?: StatsFilter): Promise<CredentialDistribution[]> {
  const { data } = await api.get<CredentialDistribution[]>('/stats/by-credential', {
    params: statsParams(time, filter),
  })
  return data
}

export async function getByKey(time: StatsTimeFilter, filter?: StatsFilter): Promise<KeyDistribution[]> {
  // by-key 是横向对比所有 Key，忽略 keyId 过滤；仅透传时间窗与分组
  const { data } = await api.get<KeyDistribution[]>('/stats/by-key', {
    params: { ...time, ...(filter?.group ? { group: filter.group } : {}) },
  })
  return data
}
