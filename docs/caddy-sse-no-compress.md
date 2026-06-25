# Caddy 修复：不要压缩 SSE 流（消除 ZstdDecompressionError）

## 问题

生产 Caddyfile 对整站启用 `encode zstd gzip`，会把 `Content-Type: text/event-stream`
的流式响应也用 zstd/gzip 压缩。Claude Code（`Accept-Encoding: ...zstd`）解流式 zstd 帧
失败 → 客户端报 `Decompression error: ZstdDecompressionError` → 流中断、重试。

实证（生产 Caddy 日志）：
- 客户端 `Accept-Encoding: gzip, deflate, br, zstd` 出现 ~3800 次
- 响应 `Content-Encoding: zstd` 出现 3084 次，且 `Content-Type` 为 `text/event-stream`

## 修复

`encode` 加 `match`，**排除 `text/event-stream`** 不压缩（其余响应仍压缩，省带宽）。
SSE 本就是逐 token 小块、压缩收益极低，且压缩会破坏流式语义。

### 修改后的站点块（`ceshi.asdfdsa123.com`）

```caddyfile
ceshi.asdfdsa123.com {
	tls {
		protocols tls1.2 tls1.3
	}

	# 只对非 SSE 响应压缩；text/event-stream 绝不压缩（否则 Claude Code 解 zstd 流失败断流）
	encode zstd gzip {
		match {
			not header Content-Type text/event-stream*
		}
	}

	reverse_proxy 127.0.0.1:8990 {
		flush_interval -1
		transport http {
			keepalive 120s
			keepalive_idle_conns 256
		}
	}

	request_body {
		max_size 100MB
	}

	log {
		output file /var/log/caddy/kiro-rs.log {
			roll_size 50mb
			roll_keep 10
			roll_keep_for 720h
		}
		format json
	}
}
```

关键改动：`encode zstd gzip` → `encode zstd gzip { match { not header Content-Type text/event-stream* } }`。
`text/event-stream*` 末尾通配兼容带 `; charset=...` 的变体。

## 应用步骤（仅在测试服 / 经批准后的生产）

```bash
# 1. 备份
cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.bak-$(date +%Y%m%d-%H%M%S)
# 2. 编辑 /etc/caddy/Caddyfile，按上面替换 encode 行
# 3. 校验语法（不通过则不要 reload）
caddy validate --config /etc/caddy/Caddyfile
# 4. 热重载（不断连）
caddy reload --config /etc/caddy/Caddyfile
# 5. 验证：带 zstd 请求 SSE，响应不应再有 Content-Encoding: zstd
curl -s -D - -o /dev/null -H "Accept-Encoding: zstd" -H "x-api-key: <KEY>" \
  -H "content-type: application/json" -H "anthropic-version: 2023-06-01" \
  -X POST https://<域名>/v1/messages \
  -d '{"model":"claude-sonnet-4-6","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}' \
  | grep -i "content-encoding\|content-type"
# 期望：content-type: text/event-stream，且无 content-encoding 行
```

## 回退

```bash
cp /etc/caddy/Caddyfile.bak-<TS> /etc/caddy/Caddyfile && caddy reload --config /etc/caddy/Caddyfile
```
