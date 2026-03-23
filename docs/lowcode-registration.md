# lowcode 接入模板（external-managed）

本文件用于把 `wechat` 服务挂到 lowcode 的 `external-managed` 生命周期链路。

结论：
- 只想后台手工调用接口，不一定需要市场模块。
- 想让用户在 lowcode 里自助安装微信通道，就需要：
  - 一个市场模块
  - 一个 external-manage-service 注册
  - 模块管理配置绑定到 `external-managed`
  - 当前 lowcode 已补 `wechat` 渠道枚举识别

## 1. 注册 external-manage-service

```bash
curl -X POST "$LOWCODE_BASE/api/external-manage-service/create" \
  -H "Content-Type: application/json" \
  -H "userId: $USER_ID" \
  -d '{
    "name": "wechat-external-module",
    "baseUrl": "http://127.0.0.1:3211/external-module",
    "authToken": "",
    "description": "wechat external module adapter"
  }'
```

返回 `data.id` 即 `externalManageServiceId`。

## 2. 绑定到目标 type5 模块项目

```bash
curl -X POST "$LOWCODE_BASE/api/module-manage-config/create" \
  -H "Content-Type: application/json" \
  -H "userId: $USER_ID" \
  -d '{
    "projectId": 12345,
    "externalManageServiceId": 67890,
    "lifecyclePluginId": "external-managed"
  }'
```

其中：
- `projectId`: 微信模块项目 ID
- `externalManageServiceId`: 第一步返回 ID

## 3. 模块 propertyDefinition 建议

参考：`docs/module-property-definition.json`

如果要直接写数据库里的 `module.propertyDefinition` 字段，可先生成单行 JSON：

```bash
cat docs/module-property-definition.json | jq -c .
```

## 4. external-module create/update 建议 properties

工作区安装时建议至少透传：

```json
{
  "tenantId": "wechat-1106",
  "gatewayUrl": "http://127.0.0.1:4020/channels/wechat",
  "lowcodeForwardEnabled": true,
  "autoStart": true
}
```

说明：
- `tenantId`：external-module 实例 ID，也会作为租户 ID
- `gatewayUrl`：channel-gateway 渠道根地址，服务内部会自动拼 `/inbound`
- `autoStart=true`：若该租户已经扫码登录，创建后自动启动轮询
- `autoStart=true` 但尚未登录：实例会创建成功，但状态仍是 `PENDING`

## 5. 市场模块建议

如果你要让工作区渠道安装页自动找到微信模块，市场模块建议：

- 名称带上 `wechat` 或 `微信`
- 默认服务名建议为 `wechat`
- `managedMode` 设为 `external-managed`
- `propertyDefinition` 里至少包含：
  - `serviceBaseUrl`
  - `tenantId`
  - `botToken`
  - `gatewayUrl`
  - `gatewayToken`
  - `lowcodeForwardEnabled`
  - `autoStart`

参考模板：`docs/lowcode-module-definition.template.json`

## 6. 联调建议

先本地启动服务：

```bash
cd /Users/c/lowcode/wechat
cargo run -- --config config.example.yaml
```

然后：

1. 在 lowcode 里安装微信市场模块
2. 创建工作区微信渠道实例
3. 调 `POST /tenants/{tenantId}/login/qrcode`
4. 前端展示二维码，扫码
5. 调 `POST /tenants/{tenantId}/login/status`
6. 确认 worker 启动、`channel-gateway` 能收到 inbound
