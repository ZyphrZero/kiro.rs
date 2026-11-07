// Run after the normal production build. Requires Playwright + Chromium.
// Serves dist locally and intercepts every API request; never contacts a backend.
const assert = require('node:assert/strict')
const fs = require('node:fs/promises')
const path = require('node:path')
const http = require('node:http')
const { chromium } = require('playwright')

const dist = path.resolve(__dirname, '../dist')
const output = process.env.CACHE_REVIEW_OUTPUT
  ? path.resolve(process.env.CACHE_REVIEW_OUTPUT)
  : path.resolve(__dirname, '../../target/account-cache-review')
const checks = []
const pageErrors = []
const unexpectedRequests = []
const credentials = [101, 102, 103].map(id => ({
  id, email: `review.${id}@example.test`, priority: 0, loadFactor: 10,
  disabled: false, failureCount: 0, totalFailureCount: 0, isCurrent: false,
  expiresAt: null, authMethod: 'social', hasProfileArn: true,
  profileArn: 'arn:aws:codewhisperer:us-east-1:000000000000:profile/review',
  successCount: 10, lastUsedAt: null, hasProxy: false, refreshFailureCount: 0,
  endpoint: 'ide', groups: [], metadata: {}, subscriptionTitle: 'KIRO PRO',
  balance: { id, subscriptionTitle: 'KIRO PRO', currentUsage: 0, usageLimit: 500,
    remaining: 500, usagePercentage: 0, nextResetAt: null, overageEnabled: false,
    overageCapable: false },
}))
const rows = [
  { credentialId: 101, email: credentials[0].email, calls: 56, errors: 2,
    inputTokens: 24000, outputTokens: 8600, cacheCreationTokens: 6000, cacheReadTokens: 70000 },
  { credentialId: 102, email: credentials[1].email, calls: 12, errors: 0,
    inputTokens: 40000, outputTokens: 2400, cacheCreationTokens: 10000, cacheReadTokens: 0 },
]

async function main() {
  await fs.mkdir(output, { recursive: true })
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://localhost')
      const relative = url.pathname.replace(/^\/admin\//, '')
      const file = path.resolve(dist, relative || 'index.html')
      if (!file.startsWith(dist + path.sep)) { res.writeHead(404).end(); return }
      const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' }
      res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream')
      res.end(await fs.readFile(file))
    } catch { res.writeHead(404).end() }
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  const origin = `http://127.0.0.1:${server.address().port}`
  let browser
  try {
    browser = await chromium.launch({ headless: true })
    async function scenario(options = {}) {
      const state = { mode: 'ok', rangeDelay: null, noCredentials: false, cacheRequests: [], ...options }
      const context = await browser.newContext({ viewport: { width: 1440, height: 1100 } })
      await context.addInitScript(mode => {
        localStorage.setItem('adminApiKey', 'synthetic-review-key')
        localStorage.setItem('adminTheme', JSON.stringify({ palette: 'graphite', mode }))
      }, options.theme || 'dark')
      await context.route('**/*', async route => {
        const request = route.request()
        const url = new URL(request.url())
        if (url.origin === 'https://fonts.googleapis.com' && url.pathname === '/css2') {
          return route.fulfill({ contentType: 'text/css', body: '/* offline browser test */' })
        }
        if (url.origin !== origin) {
          unexpectedRequests.push(request.url()); return route.abort()
        }
        if (!url.pathname.startsWith('/api/')) return route.continue()
        if (request.method() !== 'GET') {
          unexpectedRequests.push(`${request.method()} ${url.pathname}`); return route.abort()
        }
        const send = (json, status = 200) => route.fulfill({ status, json })
        switch (url.pathname) {
          case '/api/admin/credentials': {
            const search = url.searchParams.get('search') || ''
            const status = url.searchParams.get('status')
            const filtered = state.noCredentials ? [] : state.legacyPagination ? credentials : credentials.filter(c =>
              c.email.includes(search) && (!status || status === 'healthy'))
            return send({ total: state.noCredentials ? 0 : 3, filteredTotal: state.legacyPagination ? undefined : filtered.length,
              available: state.noCredentials ? 0 : 3, currentId: 0, page: 1, pageSize: 12,
              stateCounts: { healthy: state.noCredentials ? 0 : 3, throttled: 0, quota: 0, dead: 0, suspended: 0, total: state.noCredentials ? 0 : 3 },
              metadataSchema: { type: 'object', properties: {}, additionalProperties: true }, credentials: filtered })
          }
          case '/api/admin/stats/by-credential': {
            state.cacheRequests.push(url.search)
            if (state.rangeDelay) await state.rangeDelay
            if (state.mode === 'error') return send({ error: 'synthetic outage' }, 503)
            if (state.mode === 'missing') return send(rows.map(({ cacheCreationTokens, cacheReadTokens, ...row }) => row))
            if (state.mode === 'empty') return send([])
            if (state.mode === 'boundary') return send([
              { ...rows[0], inputTokens: 9996, cacheCreationTokens: 0, cacheReadTokens: 4 },
              { ...rows[1], inputTokens: 4, cacheCreationTokens: 0, cacheReadTokens: 9996 },
            ])
            return send(rows.map(row => ({ ...row, cacheReadTokens: url.searchParams.get('range') === '7d' ? 50000 : row.cacheReadTokens })))
          }
          case '/api/admin/config/load-balancing': return send({ mode: 'balanced' })
          case '/api/admin/config/account-throttle': return send({ failover: true, cooldownSecs: 60 })
          case '/api/admin/config/self-heal': return send({ enabled: false, rules: [] })
          case '/api/admin/system/update/check': return send({ currentVersion: '0.9.7', latestVersion: '0.9.7', hasUpdate: false })
          case '/api/admin/traces/failure-stats': return send({})
          case '/api/admin/stats/credential-activity': return send({})
          case '/api/admin/groups': return send({ total: 0, groups: [] })
          default:
            unexpectedRequests.push(url.pathname)
            return send({ error: 'unmocked endpoint' }, 404)
        }
      })
      const page = await context.newPage()
      page.on('pageerror', error => pageErrors.push(error.message))
      await page.goto(`${origin}/admin/#/credentials`)
      await page.getByPlaceholder('搜索来源渠道 / 备注 / 邮箱').waitFor()
      const summary = page.getByRole('region', { name: '账号缓存统计', exact: true })
      return { page, context, state, summary }
    }

    const { page, context, state, summary } = await scenario()
    const assertText = async (locator, expected) => {
      await locator.filter({ hasText: expected }).waitFor()
      assert.match((await locator.innerText()).replace(/\s+/g, ' '), expected)
    }
    await assertText(summary, /46\.7%/)
    assert.equal(await summary.count(), 1)
    assert.equal(state.cacheRequests.length, 1, 'all cards must share one cache query')
    await assertText(summary, /含 2 次失败/)
    const search = page.getByPlaceholder('搜索来源渠道 / 备注 / 邮箱')
    await search.fill('no-such-account')
    await page.getByRole('button', { name: '显示全部凭据', exact: true }).waitFor()
    await page.getByText('当前筛选条件下没有凭据', { exact: true }).waitFor()
    await assertText(summary, /46\.7%/)
    await summary.getByRole('button', { name: '展开明细' }).click()
    await assertText(summary, /review\.101@example\.test/)
    assert.equal(state.cacheRequests.length, 1, 'credential filters must not refetch cache stats')
    await page.screenshot({ path: path.join(output, 'dashboard-filter-empty.png'), fullPage: true })
    checks.push('server-paginated search with zero matches retains one complete summary')

    await search.fill('')
    await page.getByRole('button', { name: /0.*封禁/ }).click()
    await page.getByRole('button', { name: '显示全部凭据', exact: true }).waitFor()
    await assertText(summary, /46\.7%/)
    assert.equal(await summary.count(), 1)
    await page.getByRole('button', { name: '显示全部凭据', exact: true }).click()
    checks.push('empty suspended-status filter retains the summary')
    await page.locator('details summary').first().waitFor()
    const metrics = page.locator('details').filter({ has: page.locator('summary', { hasText: '缓存命中率' }) })
    assert.equal(await metrics.count(), 3)
    assert.match(await metrics.nth(0).locator('summary').innerText(), /70\.0%/)
    assert.match(await metrics.nth(1).locator('summary').innerText(), /0\.0%/)
    assert.match(await metrics.nth(2).locator('summary').innerText(), /—/)
    await metrics.first().locator('summary').focus()
    await page.keyboard.press('Enter')
    await metrics.first().getByText(/缺少上游用量的失败请求/).waitFor()
    checks.push('card zero/no-usage distinction and keyboard-accessible accounting explanation')

    state.mode = 'error'
    await summary.getByRole('button', { name: '刷新缓存统计' }).click()
    await summary.getByText('刷新失败，显示上次数据', { exact: true }).waitFor()
    await assertText(summary, /46\.7%/)
    await assertText(metrics.first().locator('summary'), /70\.0%.*上次数据/)
    await page.screenshot({ path: path.join(output, 'dashboard-refresh-failed.png'), fullPage: true })
    state.mode = 'ok'
    await summary.getByRole('button', { name: '刷新缓存统计' }).click()
    await summary.getByText('刷新失败，显示上次数据', { exact: true }).waitFor({ state: 'hidden' })
    checks.push('refresh failure retains numbers with stale notice; successful retry clears it')

    let releaseRange
    state.rangeDelay = new Promise(resolve => { releaseRange = resolve })
    await summary.getByRole('button', { name: '7天', exact: true }).click()
    await assertText(summary, /正在加载统计/)
    assert.equal(await summary.locator('strong').innerText(), '—')
    assert.match((await metrics.first().locator('summary').innerText()).replace(/\s+/g, ' '), /—.*近7天/)
    state.rangeDelay = null
    releaseRange()
    await assertText(summary, /55\.6%/)
    checks.push('new range clears prior range numbers during loading')

    await summary.getByRole('button', { name: '30天', exact: true }).click()
    await assertText(summary, /46\.7%/)
    assert.ok(state.cacheRequests.some(query => new URLSearchParams(query).get('range') === '30d' && new URLSearchParams(query).get('granularity') === 'day'))
    assert.ok(state.cacheRequests.every(query => !/[?&](group|keyId|search)=/.test(query)))
    checks.push('30-day range uses daily buckets; all cache queries are unfiltered')

    state.mode = 'missing'
    await summary.getByRole('button', { name: '刷新缓存统计' }).click()
    await assertText(summary, /缓存用量缺失或无效/)
    assert.equal(await summary.locator('strong').innerText(), '—')
    await assertText(metrics.first(), /缓存用量缺失或无效/)
    checks.push('old backend missing fields explains unavailable totals and card rates')

    state.mode = 'boundary'
    await summary.getByRole('button', { name: '刷新缓存统计' }).click()
    await assertText(metrics.nth(0).locator('summary'), /<0\.1%/)
    await assertText(metrics.nth(1).locator('summary'), />99\.9%/)
    checks.push('small/near-complete hits preserve nonzero/partial semantics')

    await page.getByRole('button', { name: '隐私防护', exact: true }).click()
    assert.equal((await summary.innerText()).includes('review.101@example.test'), false)
    assert.equal(await summary.locator('[title="review.101@example.test"]').count(), 0)
    await page.getByTitle('列表视图', { exact: true }).click()
    await assertText(metrics.nth(0).locator('summary'), /<0\.1%/)
    checks.push('privacy removes full email/title; list mode uses same metric')

    for (const width of [1440, 390, 320]) {
      await page.setViewportSize({ width, height: 1000 })
      const bounds = await summary.boundingBox()
      assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= width, `summary fits ${width}px`)
      assert.equal(await summary.evaluate(el => el.scrollWidth > el.clientWidth), false)
      await page.screenshot({ path: path.join(output, `dashboard-${width}.png`), fullPage: true })
    }
    assert.equal(await summary.getByRole('group', { name: '缓存统计时间范围' }).count(), 1)
    const toggle = summary.getByRole('button', { name: '收起明细' })
    const panelId = await toggle.getAttribute('aria-controls')
    assert.ok(await page.evaluate(id => !!document.getElementById(id), panelId))
    await toggle.click()
    assert.equal(await summary.getByRole('button', { name: '展开明细' }).getAttribute('aria-controls'), null)
    checks.push('summary fits desktop/390/320 widths; disclosure references and range group valid')

    state.mode = 'empty'
    await summary.getByRole('button', { name: '刷新缓存统计' }).click()
    await assertText(summary, /所选时间内暂无调用/)
    assert.equal(await summary.locator('strong').innerText(), '—')
    checks.push('empty usage shows no-calls state')
    await context.close()

    const initialError = await scenario({ mode: 'error' })
    await assertText(initialError.summary, /加载失败，暂不显示统计/)
    assert.equal(await initialError.summary.locator('strong').innerText(), '—')
    await initialError.context.close()
    checks.push('initial failure does not fabricate zero usage')

    const legacy = await scenario({ legacyPagination: true, theme: 'light' })
    await assertText(legacy.summary, /46\.7%/)
    await legacy.page.getByPlaceholder('搜索来源渠道 / 备注 / 邮箱').fill('no-such-account')
    await legacy.page.getByRole('button', { name: '显示全部凭据', exact: true }).waitFor()
    await legacy.page.getByText('当前筛选条件下没有凭据', { exact: true }).waitFor()
    await assertText(legacy.summary, /46\.7%/)
    assert.equal(await legacy.page.locator('html').evaluate(el => el.classList.contains('dark')), false)
    await legacy.page.screenshot({ path: path.join(output, 'dashboard-light-legacy-filter.png'), fullPage: true })
    await legacy.context.close()
    checks.push('client-side filtering with legacy pagination also retains summary; light theme renders')

    const emptyPool = await scenario({ noCredentials: true })
    await emptyPool.page.getByText('暂无凭据', { exact: false }).first().waitFor()
    assert.equal(await emptyPool.summary.count(), 0)
    await emptyPool.context.close()
    checks.push('zero configured credentials hides summary')

    assert.deepEqual(pageErrors, [])
    assert.deepEqual(unexpectedRequests, [])
    const result = { checks, pageErrors, unexpectedRequests }
    await fs.writeFile(path.join(output, 'result.json'), JSON.stringify(result, null, 2))
    console.log(JSON.stringify(result, null, 2))
  } finally {
    if (browser) await browser.close()
    await new Promise(resolve => server.close(resolve))
  }
}

main().catch(error => { console.error(error); process.exitCode = 1 })
