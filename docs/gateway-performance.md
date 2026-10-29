# Gateway fixes and response latency

Measured locally on October 2, 2026. Baseline: commit `be0c042`. The baseline
and changed code used the same debug build profile and local HTTP fixture;
neither run called Kiro or used real credentials.

## Before and after

The upstream fixture sends assistant text after 20 ms, then waits another
200 ms before sending final usage and closing the response. Each endpoint
receives ten sequential streaming requests through the actual gateway handlers,
provider, HTTP client, and AWS event-stream decoder.

First-content timing includes request conversion and upstream dispatch. It
ignores message-start, role, metadata, and keepalive events. This fixture does
not include incoming HTTP routing, authentication middleware, TLS, or a reverse
proxy. P50 is the upper median of ten observations; P95 is the slowest observation.

| Endpoint | First content P50 before | First content P50 after | First content P95 before | First content P95 after | Total completion P50 before / after |
| --- | ---: | ---: | ---: | ---: | ---: |
| `/cc/v1/messages` | 235.42 ms | 31.53 ms | 247.47 ms | 39.41 ms | 235.45 / 234.55 ms |
| `/v1/chat/completions` | 234.65 ms | 31.17 ms | 246.99 ms | 39.37 ms | 234.65 / 234.58 ms |
| `/v1/messages` | 30.71 ms | 31.33 ms | 31.83 ms | 40.64 ms | 235.47 / 234.96 ms |

CC and Chat first-content latency fell approximately 87% in this fixture.
Completion time stays effectively unchanged: these fixes remove gateway
buffering; they do not accelerate upstream model generation. A slower upstream
will still produce a slower answer. This small controlled run demonstrates the
regression fix; it is not a production load or throughput benchmark.

For every endpoint, ten requests used **ten upstream TCP connections before**
and **one after**. Removing the forced `Connection: close` header allows the
existing reusable HTTP clients to pool connections. Actual savings over HTTPS
depend on upstream keepalive and network conditions.

## Changes

- CC uses the existing live Messages stream instead of collecting the entire
  answer. Initial token usage is estimated; final `message_delta` reports the
  provider usage when available.
- Chat Completions translates incoming Messages events immediately, including
  fragmented tool arguments and final usage. A failed stream emits an error
  without a successful finish or `[DONE]` marker.
- Streaming responses include `X-Accel-Buffering: no` for proxies that honor it.
- Exact upstream `MODEL_TEMPORARILY_UNAVAILABLE` server errors return **503**
  with `Retry-After`, instead of making three internal attempts and returning
  a generic 502. A short per-model cooldown defaults to five seconds and honors
  a valid upstream `Retry-After`. Other models remain available. This change
  exposes upstream overload promptly; it does not repair upstream availability.
- Connection and upstream read deadlines are separate from the existing
  720-second total request budget. Active reads reset the read deadline.
  A timeout while awaiting headers or reading an error response body returns
  **504** without repeating the wait. A timeout after streaming starts emits
  an SSE error, because the HTTP status has already been sent.
- Non-streaming responses decode frames incrementally, fixing the empty-success
  bug when the complete response exceeded the decoder's 16 MB buffer.
- Empty bodies, invalid frames, truncated frames, malformed event payloads,
  upstream error events, and disconnects now produce explicit failures instead
  of normal completion. Full error cause chains are retained in request traces.
- Trace first-token timing starts with emitted text, thinking, or a tool call,
  rather than counting an upstream metadata chunk as content.

## Configuration

Existing configuration files receive these defaults automatically:

```json
{
  "upstreamConnectTimeoutSecs": 15,
  "upstreamReadTimeoutSecs": 120,
  "upstreamTotalTimeoutSecs": 720
}
```

The read setting limits upstream silence while awaiting headers or the next
body read. It does not limit an active answer to 120 seconds. Zero connect/read
settings are clamped to one second. Runtime credentials and configuration files
were not changed.

`upstreamTotalTimeoutSecs` is the deadline for a whole request, body included.
An active stream cannot reset it, so it also caps how long an answer may be: a
request still delivering tokens when it expires dies with a half-written body,
and every token already generated is paid for and lost. It defaults to 0 (disabled),
the value previously hardcoded in the API client, so behaviour is unchanged
unless you set it.

Raise it if long agentic turns are being truncated at twelve minutes, or set it
to 0 to remove the ceiling entirely — a stalled upstream is still caught by
`upstreamReadTimeoutSecs`, which measures silence rather than total work.

## TDD and verification

The initial six regression tests failed on the baseline: buffered CC, buffered
Chat, hidden CC disconnect, forced connection closure, generic overload mapping
with repeated attempts, and an empty response above the decoder buffer limit.
Additional failing tests reproduced damaged/empty streams, lost `Retry-After`
headers, stalled headers/body reads, and metadata being recorded as a first token.
The fixes make these tests pass. Coverage also verifies complete tool arguments,
final usage, and an active stream surviving past its idle timeout.

Final validation: **768 tests passed, zero failed**, with the latency measurement
ignored in the normal suite and run separately afterward. The production
compilation check also passed. This adds 18 regression/configuration tests to
the original 750-test suite, plus the opt-in latency measurement.

Run the full suite and production compilation check:

```powershell
cargo test --locked --offline
cargo check --locked --offline
```

Run the deterministic gateway regressions or repeat the latency measurement:

```powershell
cargo test --locked --offline anthropic::gateway_tests
cargo test --locked --offline gateway_latency_measurement -- --ignored --nocapture
```

Captured local evidence is in `target/gateway-before.txt`,
`target/gateway-after-final.txt`, `target/gateway-final-tests.txt`, and the
`target/gateway-*-red.txt` files. These build-directory logs are not committed.
