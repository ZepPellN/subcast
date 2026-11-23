# SubCast HTTPS 手机访问

手机保留当前 VPN，用浏览器打开自己的 HTTPS 域名。Cloudflare Access 先验证邮箱，SubCast 再用一次性配对码授权这台设备。运行 SubCast 和 `cloudflared` 的 Mac 需要保持开机、联网。HTTPS 和局域网模式一次只开启一种。

## 配置 Cloudflare Access

1. 在 Cloudflare Zero Trust 的 **Access controls → Applications** 创建自托管应用。添加公开主机名，例如 `phone.example.com`，留空路径，使 Access 覆盖整站，包括静态文件、`/pair`、`/rpc/*` 和 `/events`。 [自托管应用文档](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/self-hosted-public-app/)
2. 只添加一条 **Allow** 策略，`Include → Emails` 填写唯一的所有者邮箱。可使用邮件一次性 PIN。保持 **Authenticate with Cloudflare One Client** 关闭，让手机通过浏览器登录。移除覆盖该主机的宽泛 Allow、Bypass 或 Service Auth 策略。 [邮件 PIN 登录文档](https://developers.cloudflare.com/cloudflare-one/integrations/identity-providers/one-time-pin/)
3. 为应用设置较短的 Access 会话，例如 1 小时。Allow 策略使用应用的时长，或显式设为同样短的时长。应用令牌到期后，若 Access 全局或身份提供商的会话仍有效，刷新时可能自动续签。 [Access 会话文档](https://developers.cloudflare.com/cloudflare-one/access-controls/access-settings/session-management/)
4. 从应用 **Additional settings** 复制 **Application Audience (AUD) Tag**，并记下 Access team 名称。例如团队域名为 `https://example-team.cloudflareaccess.com` 时，team 名称是 `example-team`。 [AUD 和团队域名](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/)
5. 在该 Cloudflare zone 的 **Caching → Cache Rules** 新建规则，表达式为 `(http.host eq "phone.example.com")`，设置 **Cache eligibility → Bypass cache**，覆盖全部路径。保证该主机没有后续匹配的规则重新启用缓存。 [缓存设置](https://developers.cloudflare.com/cache/how-to/cache-rules/settings/) · [规则优先级](https://developers.cloudflare.com/cache/how-to/cache-rules/order/)

## 建立命名 Tunnel

在运行 SubCast 的 Mac 上安装 `cloudflared`。以下用本地管理的命名 Tunnel 演示。 [命名 Tunnel 文档](https://developers.cloudflare.com/tunnel/features/locally-managed-tunnels/create-local-tunnel/)

```sh
cloudflared tunnel login
cloudflared tunnel create subcast-phone
```

在仓库外创建 `~/.cloudflared/subcast-phone.yml`，替换其中的 UUID、凭据文件绝对路径、公开主机名、team 名称和 AUD。这里的 `httpHostHeader` 必须是公开 HTTPS 网址的主机名，回环地址用于连接本机源站。开启 **Protect with Access**，仅接受这个应用的 AUD。 [Tunnel 源站参数](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/origin-parameters/)

```yaml
tunnel: "<Tunnel UUID>"
credentials-file: "/Users/<mac-user>/.cloudflared/<Tunnel UUID>.json"
ingress:
  - hostname: phone.example.com
    service: http://127.0.0.1:14347
    originRequest:
      httpHostHeader: phone.example.com
      access:
        required: true
        teamName: example-team
        audTag:
          - "<Access application AUD>"
  - service: http_status:404
```

Access 和缓存规则保存后，验证 ingress、配置 DNS 并启动 Tunnel。若使用控制台管理的命名 Tunnel，在同一公开主机的路由填写上述源站和 **HTTP Host Header**，将 **Protect with Access** 设为开启，并使用同一 team 和 AUD。

```sh
cloudflared tunnel --config ~/.cloudflared/subcast-phone.yml ingress validate
cloudflared tunnel route dns subcast-phone phone.example.com
cloudflared tunnel --config ~/.cloudflared/subcast-phone.yml run subcast-phone
```

保留回环监听，路由器无需开放入站端口。将 `cert.pem`、Tunnel 凭据 JSON 或 Tunnel token 保存在当前操作者的受限目录。配对码、设备令牌、Access JWT 和会话 Cookie 也须留在仓库与日志之外。

## 开启并使用

在桌面 SubCast 的 **设置 → 通用 → 手机访问** 中选择 **跨网络 HTTPS**。填写公开 HTTPS 地址（例如 `https://phone.example.com`，只填站点根地址）、Access 团队名称、应用 AUD 和同一个所有者邮箱。勾选 **允许手机通过 HTTPS 访问** 并确认。这四项非凭据配置保存到已有的本地 SQLite 中。

在手机浏览器打开 `https://phone.example.com`，用所有者邮箱登录 Access，再输入桌面显示的 6 位配对码。公开网址和配对码分开分享。配对码 5 分钟内有效且只能使用一次，设备令牌从配对起有效 7 天，也可在桌面提前撤销。

HTTPS 源站在提供页面、资源、配对、RPC 和 SSE 之前验证 Access JWT 的 RS256 签名、签发方、应用受众、时间声明（`exp`、存在时的 `nbf`）和所有者邮箱。RPC 和 SSE 还需要设备令牌。Access 过期时，浏览器里的请求可能收到登录重定向或源站的 `401 {"error":"access_required"}`。按 **刷新并登录**，继续用同一域名和浏览器。有效的设备配对和待同步进度会保留，重连后按原有进度同步规则提交。Access 有效时收到设备 `401 {"error":"unauthorized"}` 则需要重新配对。

HTTPS 手机页直接播放手机可访问的 HTTPS 原始音频。旧的 HTTP 直链会显示提示，仍可阅读字幕和使用问答。电脑上的下载文件不会经 Tunnel 传给手机。

## 验收

### 本地

- [x] 本地 Chrome 夹具在 `/pair`、`/rpc/*` 和 `/events` 模拟 Access 源站 401、登录重定向和设备 401。确认各自的页面提示。同一浏览器完成模拟登录后，用原设备令牌回写待同步进度并恢复播放位置。
- [x] HTTPS 音频可以播放，HTTP 直链在 HTTPS 页显示提示且没有媒体请求，字幕和分段 SSE 问答仍可用。HTTP 页面通过原局域网配对、播放和设备失效流程。
- [x] 本文的示例配置通过本地 `cloudflared tunnel ingress validate`。用 `ingress rule` 确认公开主机的 `/events` 命中回环端口，其余主机命中 404。
- [ ] 集成 Rust HTTPS 源站后，在 Mac 运行 `lsof -nP -iTCP:14347 -sTCP:LISTEN`，确认只监听 `127.0.0.1:14347`。从 Mac 向该回环地址发送带公开 Host、无 Access JWT 的请求，确认页面、配对、RPC 和 SSE 均拒绝，未返回资源。关闭 HTTPS 模式后确认监听停止。

### 真实域名和手机

- [ ] 手机使用蜂窝网或另一处 Wi-Fi，VPN 保持连接。用新的浏览器会话打开公开网址，确认先到 Access。非所有者邮箱无法进入，所有者登录后使用 6 位码配对，刷新不要求重新配对。
- [ ] 打开有可用 HTTPS 音频和字幕的单集，验证播放、字幕跳转、暂停和桌面端进度。刷新后验证续听。打开只有 HTTP 直链的单集，验证提示、字幕和问答入口。
- [ ] 提问并确认答案随生成逐段出现。检查 `/events` 返回 `Content-Type: text/event-stream`，并确认公开主机的响应未命中边缘缓存。Access JWT 到期时，现有流须在到期时结束。 [Cloudflare Tunnel 与 SSE](https://developers.cloudflare.com/tunnel/troubleshooting/#cloudflare-tunnel-is-buffering-my-streaming-response-instead-of-streaming-it-live)
- [ ] 保持已配对页面打开，等待短会话到期，或在 Access 撤销测试会话。触发一次进度回写或问答，确认页面提示刷新登录。点击按钮并完成登录，确认无需新配对码，待同步进度随后出现在桌面。若现有身份会话自动续签，可用同一浏览器先退出 Access 会话再重试，保留 SubCast 站点数据。
- [ ] Access 登录仍有效时，在 SubCast 桌面撤销该设备，确认手机下一次鉴权请求提示重新配对。关闭 HTTPS 模式后确认公开域名无法再访问 SubCast。重新开启时确认旧设备授权仍然失效。
- [ ] 切回局域网模式，在同一网络验证原 HTTP 配对和播放流程。

本地夹具验证浏览器对约定 HTTP 响应的行为。真实 DNS、Access 签名与会话行为、Tunnel 流式转发及手机 VPN 实测以集成后的验收结果为准。记录验收日期、浏览器和各项结果，保留不含凭据的证据。
