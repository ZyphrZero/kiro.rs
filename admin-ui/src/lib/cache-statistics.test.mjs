import test from 'node:test'
import assert from 'node:assert/strict'
import {
  cacheHitRate,
  formatCacheHitRate,
  sumCacheUsage,
} from './cache-statistics.ts'

test('cache creation belongs to the input denominator', () => {
  assert.equal(
    cacheHitRate({
      inputTokens: 20,
      cacheCreationTokens: 30,
      cacheReadTokens: 50,
    }),
    50,
  )
})

test('aggregate rate weights tokens instead of equally weighting accounts', () => {
  const rows = [
    { inputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 100 },
    { inputTokens: 9000, cacheCreationTokens: 0, cacheReadTokens: 0 },
  ]
  assert.equal(cacheHitRate(sumCacheUsage(rows)), (100 / 9100) * 100)
  assert.notEqual(cacheHitRate(sumCacheUsage(rows)), 50)
})

test('zero input, zero hits, and unavailable data are distinct', () => {
  assert.equal(
    cacheHitRate({
      inputTokens: 0,
      cacheCreationTokens: 0,
      cacheReadTokens: 0,
    }),
    null,
  )
  assert.equal(
    cacheHitRate({
      inputTokens: 10,
      cacheCreationTokens: 0,
      cacheReadTokens: 0,
    }),
    0,
  )
  assert.equal(cacheHitRate({ inputTokens: 10 }), null)
  assert.equal(cacheHitRate(undefined), null)
})

test('unknown or invalid counters cannot produce a partial, misleading aggregate', () => {
  for (const invalid of [
    undefined,
    NaN,
    Infinity,
    -1,
    Number.MAX_SAFE_INTEGER + 1,
  ]) {
    assert.equal(
      sumCacheUsage([
        { inputTokens: 10, cacheCreationTokens: 0, cacheReadTokens: 90 },
        { inputTokens: 1, cacheCreationTokens: 0, cacheReadTokens: invalid },
      ]),
      undefined,
    )
  }
  assert.equal(
    cacheHitRate({
      inputTokens: Number.MAX_SAFE_INTEGER,
      cacheCreationTokens: 1,
      cacheReadTokens: 1,
    }),
    null,
  )
  assert.equal(
    sumCacheUsage([
      {
        inputTokens: Number.MAX_SAFE_INTEGER,
        cacheCreationTokens: 0,
        cacheReadTokens: 0,
      },
      { inputTokens: 1, cacheCreationTokens: 0, cacheReadTokens: 0 },
    ]),
    undefined,
  )
})

test('display rounding never turns partial hits into zero or complete hits', () => {
  for (const [rate, label] of [
    [null, '—'],
    [NaN, '—'],
    [Infinity, '—'],
    [-1, '—'],
    [101, '—'],
    [0, '0.0%'],
    [0.04, '<0.1%'],
    [0.1, '0.1%'],
    [46.666, '46.7%'],
    [99.9, '99.9%'],
    [99.96, '>99.9%'],
    [100, '100.0%'],
  ])
    assert.equal(formatCacheHitRate(rate), label)
})
