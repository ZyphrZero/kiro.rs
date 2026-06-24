const API_KEY_STORAGE_KEY = 'adminApiKey'
const CREDENTIAL_VIEW_KEY = 'credentialView'
const CREDENTIAL_PAGE_SIZE_KEY = 'credentialPageSize'

export type CredentialView = 'card' | 'list'

/** 每页数量：0 视为“全部”（不分页） */
const DEFAULT_PAGE_SIZE = 12

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  // 凭据列表的展示形态（卡片 / 列表），默认卡片。
  // 旧版本的 'monitor' 视图已并入凭据卡片/列表，迁移为 'card'。
  getCredentialView: (): CredentialView => {
    const v = localStorage.getItem(CREDENTIAL_VIEW_KEY)
    return v === 'list' ? 'list' : 'card'
  },
  setCredentialView: (view: CredentialView) =>
    localStorage.setItem(CREDENTIAL_VIEW_KEY, view),

  // 凭据列表每页数量（0 = 全部），默认 12
  getCredentialPageSize: (): number => {
    const raw = localStorage.getItem(CREDENTIAL_PAGE_SIZE_KEY)
    if (raw === null) return DEFAULT_PAGE_SIZE
    const n = Number(raw)
    return Number.isInteger(n) && n >= 0 ? n : DEFAULT_PAGE_SIZE
  },
  setCredentialPageSize: (size: number) =>
    localStorage.setItem(CREDENTIAL_PAGE_SIZE_KEY, String(size)),
}
