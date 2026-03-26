# wechat

多租户微信通道服务（Rust）。

目标：
- 每个租户独立维护微信 iLink 登录态、轮询状态和 lowcode 转发配置
- 通过扫码登录获取 `bot_token`，再用长轮询接收消息
- 把微信消息按 `channel-gateway` 的统一 inbound 协议转发给 lowcode
- 接收 `channel-gateway` 的 outbound 回调，并用 `contextToken` 回发微信消息
- 保持和 `/Users/c/lowcode/feishu`、`/Users/c/lowcode/qiwei` 相同的 external-module 生命周期风格

说明：
- 当前实现基于 `https://ilinkai.weixin.qq.com/ilink/bot/*` 这条能力链路。
- 这不是仓库内能对应到的微信公开标准开放平台 Bot API，建议先按内测/PoC 使用。
- 当前会话模型只支持私聊：`sessionKey = wechat:dm:<user_id>`。

## 架构

- 配置加载：`config.toml/yaml/json` + 环境变量
- 双端口：
  - 管理 API：租户配置、启停、扫码登录、external-module 生命周期
  - 业务 API：发消息、接收 lowcode outbound 回调
- 内存多租户注册表：`DashMap<tenant_id, TenantContext>`
- 租户凭据持久化：MySQL（启动时自动加载）
- 每租户一个 poll worker：调用 `ilink/bot/getupdates`

## 配置

复制示例配置：

```bash
cp config.example.yaml config.yaml
```

默认端口：
- 管理 API: `3211`
- 业务 API: `3210`

数据库建议使用独立 schema，例如：

```yaml
database:
  url: "mysql://root:password@127.0.0.1:3306/wechat?charset=utf8mb4"
```

`channel_gateway.inbound_token` 用于 lowcode 下行回调鉴权，可选。

## 运行

```bash
cargo run
```

## 管理 API

### 1. 新增或更新租户

先创建一个空租户，后面再扫码登录：

```bash
curl -X PUT http://127.0.0.1:3211/tenants/demo \
  -H 'Content-Type: application/json' \
  -d '{
    "gateway_url":"http://127.0.0.1:4020/channels/wechat",
    "lowcode_forward_enabled":true,
    "enabled": true
  }'
```

如果你已经有现成 `bot_token`，也可以直接写入：

```bash
curl -X PUT http://127.0.0.1:3211/tenants/demo \
  -H 'Content-Type: application/json' \
  -d '{
    "botToken":"replace-with-bot-token",
    "baseUrl":"https://ilinkai.weixin.qq.com",
    "gateway_url":"http://127.0.0.1:4020/channels/wechat",
    "lowcode_forward_enabled":true,
    "enabled": true
  }'
```

### 2. 申请扫码二维码

```bash
curl -X POST http://127.0.0.1:3211/tenants/demo/login/qrcode \
  -H 'Content-Type: application/json' \
  -d '{}'
```

返回：
- `qrcode`：登录票据
- `url`：二维码链接，可直接给前端展示

### 3. 轮询扫码结果

```bash
curl -X POST http://127.0.0.1:3211/tenants/demo/login/status \
  -H 'Content-Type: application/json' \
  -d '{"qrcode":"<上一步返回的 qrcode>","autoStart":true}'
```

当返回 `status=confirmed` 时，服务会自动写入：
- `bot_token`
- `baseUrl`
- `accountId`
- `userId`

若租户已启用且 `autoStart=true`，会自动启动轮询 worker。

### 4. 启动租户连接

```bash
curl -X POST http://127.0.0.1:3211/tenants/demo/connection/start
```

如果还没登录，会返回 `login_required=true`。

### 5. 停止租户连接

```bash
curl -X POST http://127.0.0.1:3211/tenants/demo/connection/stop
```

### 6. 查看最近事件

```bash
curl 'http://127.0.0.1:3211/tenants/demo/events?limit=20'
```

### 7. external-module 兼容 API

- `POST /external-module`
- `GET /external-module/{externalId}`
- `DELETE /external-module/{externalId}`
- `POST /external-module/{externalId}/stop`
- `POST /external-module/updateProperties`
- `GET /external-module/{externalId}/runtimeStatus`

建议把 lowcode 里的 `baseUrl` 配置为：

```text
http://<wechat-host>:3211/external-module
```

`properties/currentProperties` 常用字段：

- `tenantId`
- `botToken`
- `baseUrl`
- `accountId`
- `userId`
- `gatewayUrl`
- `gatewayToken`
- `outboundToken`
- `lowcodeForwardEnabled`
- `autoStart`

## 业务 API

### 1. 按 sessionKey 发消息

```bash
curl -X POST http://127.0.0.1:3210/tenants/demo/messages/session \
  -H 'Content-Type: application/json' \
  -d '{"session_key":"wechat:dm:user_xxx","context_token":"ctx_xxx","text":"你好"}'
```

如果不传 `context_token`，服务会尝试使用该用户最近一条入站消息缓存下来的 token。

### 2. 直接发文本消息

```bash
curl -X POST http://127.0.0.1:3210/tenants/demo/messages/text \
  -H 'Content-Type: application/json' \
  -d '{"to_user_id":"user_xxx","context_token":"ctx_xxx","text":"你好"}'
```

### 3. channel-gateway 下行回调

```bash
curl -X POST http://127.0.0.1:3210/outbound \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer replace-with-wechat-outbound-token' \
  -d '{
    "eventId":"evt_xxx",
    "eventType":"agentMessage",
    "session":{"scope":"dm","userId":"user_xxx","sessionKey":"wechat:dm:user_xxx"},
    "replyTo":{"channel":"wechat","tenantId":"demo","userId":"user_xxx","contextToken":"ctx_xxx"},
    "data":{"content":{"text":"hello"}}
  }'
```

兼容入口 `POST /internal/channel/callback` 仍保留。

## 兼容接口

- `POST /common-tools/api/v1/channel/message/sendBySession`
- `POST /common-tools/api/v1/channel/message/sendText`

请求体继续使用 `tenantId + sdkRequest` 结构。

## lowcode 转发协议

轮询收到微信消息后，服务会调用租户级 `gateway_url + /inbound`，请求体示例：

```json
{
  "requestId": "client_id",
  "sender": {
    "channelUserId": "user_xxx",
    "channelUserType": "wechat_user_id"
  },
  "session": {
    "scope": "dm",
    "userId": "user_xxx",
    "sessionKey": "wechat:dm:user_xxx"
  },
  "replyTo": {
    "channel": "wechat",
    "tenantId": "demo",
    "userId": "user_xxx",
    "contextToken": "ctx_xxx"
  },
  "raw": {},
  "data": {
    "type": "input",
    "source": "wechat",
    "tenantId": "demo",
    "fromUserId": "user_xxx",
    "contextToken": "ctx_xxx",
    "content": "hello"
  }
}
```

## 说明

- 长轮询 `getupdates` 会把 `sync_buf` 落库，服务重启后可继续续接。
- 回消息依赖微信入站消息携带的 `contextToken`。
- 如果 outbound 没显式传 `contextToken`，服务会尝试使用同租户内该用户最近一次入站消息缓存的 token。
- lowcode 转发只使用租户级配置 `gateway_url / gateway_token / lowcode_forward_enabled`。
- 转发失败时，服务会尝试给微信用户回一条兜底提示。
