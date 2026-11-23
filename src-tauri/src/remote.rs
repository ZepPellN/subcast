//! 手机访问（issue #2 / T1）：局域网里的手机浏览器用配对码换一个设备令牌，再通过 HTTP 网关读取订阅列表。
//!
//! 边界（刻意从严）：
//! - **复用界面那个 HTTP 服务**：不另起服务。开关打开时在同一端口族上额外绑定局域网地址，请求都走
//!   [`route`]；关闭时停掉这些监听并吊销全部令牌。
//! - 配对码 6 位、5 分钟有效、**用过一次即作废**，可随时重新生成。
//! - 设备令牌 7 天有效，**只存 SHA-256**；进程内记录，开关关闭即清空。
//! - 网关只放行**硬编码白名单**（本票只有 `list_shows`），白名单外一律 403。
//! - 缺令牌 / 过期 / 已吊销 / 伪造：一律 401，响应体不区分原因。
//! - 日志与错误信息不打印配对码、令牌。
//!
//! T1 的 `/rpc` 只用 `list_shows`，所以命令参数目前直接忽略；手机页没有事件流，
//! 桌面端的事件在这里没有对应实现（T2 再说）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 配对码有效期。
pub const PAIR_TTL_MS: i64 = 5 * 60 * 1000;
/// 设备令牌有效期。
pub const TOKEN_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 连续输错这么多次，锁一分钟再试。
pub const MAX_PAIR_FAILS: u32 = 10;
pub const LOCK_MS: i64 = 60 * 1000;
/// 本票（T1）只放行的命令。桌面端有的命令不在表里也一律 403。
pub const ALLOWED: &[&str] = &["list_shows"];
/// 请求体上限，别让一个请求把内存吃掉。
const MAX_BODY: usize = 16 * 1024;
/// 端口族大小，与 lib.rs 的 UI_PORTS 保持一致（由调用方传入，这里只做兜底）。
const PORT_FAMILY: u16 = 20;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0))
}

fn sha256_hex(s: &str) -> String { hex::encode(Sha256::digest(s.as_bytes())) }

/// 32 字节随机数的十六进制（uuid v4 走系统 CSPRNG，不用额外引 rand）。
fn random_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    while bytes.len() < 32 { bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes()); }
    hex::encode(&bytes[..32])
}

/// 6 位十进制随机码。
fn random_code() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{n:06}")
}

#[derive(Clone, Debug)]
struct Device {
    token_hash: String,
    /// 手机自己报的名字，只用于以后在界面上区分设备。
    #[allow(dead_code)]
    label: String,
    expires_at: i64,
}

#[derive(Default)]
struct Inner {
    enabled: bool,
    code: Option<String>,
    code_expires_at: i64,
    failed: u32,
    locked_until: i64,
    devices: Vec<Device>,
}

/// 配对/令牌的纯状态机，不碰网络，方便测试。
pub struct Gateway {
    inner: Mutex<Inner>,
    clock: Clock,
}

/// 配对结果里的令牌只在这里出现一次，之后服务端只留哈希。
#[derive(Debug, Clone)]
pub struct Issued {
    pub token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// 手机访问没开（或刚关上）。
    Disabled,
    /// 码不对 / 已用过。
    Invalid,
    /// 超过 5 分钟。
    Expired,
    /// 错太多次，锁一会儿。
    Locked,
}

impl PairError {
    pub fn status(self) -> u16 {
        match self {
            PairError::Disabled => 503,
            PairError::Invalid => 401,
            PairError::Expired => 410,
            PairError::Locked => 429,
        }
    }
    /// 机器可读的原因，手机页据此给出「码错 / 过期 / 已关闭」三种提示。
    pub fn code(self) -> &'static str {
        match self {
            PairError::Disabled => "disabled",
            PairError::Invalid => "invalid_code",
            PairError::Expired => "expired_code",
            PairError::Locked => "locked",
        }
    }
}

/// 配对码与已配对设备的快照（给桌面设置页）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatus {
    pub enabled: bool,
    /// 只在有效期内给出；过期或已被使用就是 None。
    pub code: Option<String>,
    pub code_expires_at: i64,
    pub devices: usize,
}

impl Gateway {
    pub fn new() -> Arc<Self> { Self::with_clock(system_clock()) }

    pub fn with_clock(clock: Clock) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::default()), clock })
    }

    fn now(&self) -> i64 { (self.clock)() }

    /// 打开：清空旧设备，生成一个新的配对码。
    pub fn enable(&self) -> GatewayStatus {
        let mut g = self.inner.lock().unwrap();
        g.enabled = true;
        g.devices.clear();
        g.failed = 0;
        g.locked_until = 0;
        self.new_code(&mut g);
        self.status_of(&g)
    }

    /// 关闭：停用并吊销全部令牌（旧令牌立刻 401）。
    pub fn disable(&self) {
        let mut g = self.inner.lock().unwrap();
        g.enabled = false;
        g.code = None;
        g.code_expires_at = 0;
        g.failed = 0;
        g.locked_until = 0;
        g.devices.clear();
    }

    /// 重新生成配对码（手机上还没配对时用）。
    pub fn regenerate(&self) -> GatewayStatus {
        let mut g = self.inner.lock().unwrap();
        if g.enabled {
            g.failed = 0;
            g.locked_until = 0;
            self.new_code(&mut g);
        }
        self.status_of(&g)
    }

    pub fn status(&self) -> GatewayStatus { self.status_of(&self.inner.lock().unwrap()) }

    fn new_code(&self, g: &mut Inner) {
        g.code = Some(random_code());
        g.code_expires_at = self.now() + PAIR_TTL_MS;
    }

    fn status_of(&self, g: &Inner) -> GatewayStatus {
        let now = self.now();
        let code = g.code.clone().filter(|_| g.enabled && g.code_expires_at > now);
        GatewayStatus { enabled: g.enabled, code, code_expires_at: g.code_expires_at, devices: g.devices.len() }
    }

    /// 校验配对码；成功则签发一个 7 天令牌（只把哈希留下），配对码作废。
    pub fn pair(&self, code: &str, label: &str) -> Result<Issued, PairError> {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        if !g.enabled { return Err(PairError::Disabled); }
        if g.locked_until > now { return Err(PairError::Locked); }
        let Some(expected) = g.code.take() else { return Err(PairError::Invalid) };
        if g.code_expires_at <= now { return Err(PairError::Expired); }
        if expected != code.trim() {
            // 放回去：对了一次就作废，但错的时候码还在（用户可能只是打错了）。
            g.code = Some(expected);
            g.failed += 1;
            if g.failed >= MAX_PAIR_FAILS {
                g.failed = 0;
                g.locked_until = now + LOCK_MS;
                return Err(PairError::Locked);
            }
            return Err(PairError::Invalid);
        }
        let token = random_token();
        let expires_at = now + TOKEN_TTL_MS;
        g.devices.push(Device {
            token_hash: sha256_hex(&token),
            label: label.trim().chars().take(80).collect(),
            expires_at,
        });
        g.failed = 0;
        g.locked_until = 0;
        Ok(Issued { token, expires_at })
    }

    /// 令牌是否可用（开关打开、未过期、没被吊销）。任何原因不通过都只是 false。
    pub fn authorize(&self, token: &str) -> bool {
        if token.is_empty() { return false; }
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        if !g.enabled { return false; }
        g.devices.retain(|d| d.expires_at > now);
        let hash = sha256_hex(token);
        g.devices.iter().any(|d| d.token_hash == hash)
    }
}

/// 一页静态资源的内容（界面 HTML/JS/CSS 由调用方提供）。
pub struct FileBody {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub csp: Option<String>,
}

/// 由 lib.rs 提供：从内置资源或热更新包里读界面文件。
pub trait Files: Send + Sync {
    fn read(&self, path: &str) -> Option<FileBody>;
}

/// 由 lib.rs 提供：真正执行桌面端的命令。
pub trait Commands: Send + Sync {
    fn call(&self, command: &str, args: Value) -> Result<Value, String>;
}

/// 解析后的一次请求。测试里直接构造它，不需要真的开 socket。
#[derive(Debug, Clone)]
pub struct HttpIn {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

/// 一次响应。
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub no_store: bool,
    pub csp: Option<String>,
}

impl Reply {
    fn json(status: u16, v: Value) -> Reply {
        Reply { status, content_type: "application/json; charset=utf-8".to_string(), body: v.to_string().into_bytes(), no_store: true, csp: None }
    }
    fn file(f: FileBody) -> Reply {
        Reply { status: 200, content_type: f.mime, body: f.bytes, no_store: false, csp: f.csp }
    }
}

fn bearer(header: Option<&str>) -> Option<&str> {
    let h = header?.trim();
    let rest = h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer "))?;
    Some(rest.trim())
}

/// 只允许普通的相对路径，挡住 `..` 与编码过的变体。
fn safe_static_path(path: &str) -> bool {
    if path.starts_with('/') && !path.starts_with("//") {
        let lower = path.to_ascii_lowercase();
        return !lower.contains("..") && !lower.contains('\\') && !lower.contains("%2e") && !lower.contains("%00");
    }
    false
}

/// 命令网关：唯一入口。配对、RPC、静态资源都在这里分流。
pub fn route(gw: &Gateway, files: &dyn Files, commands: &dyn Commands, req: &HttpIn) -> Reply {
    let path = req.path.as_str();
    if req.method == "POST" && (path == "/pair" || path == "/pair/") {
        return pair(gw, req);
    }
    if let Some(name) = path.strip_prefix("/rpc/") {
        if req.method != "POST" { return Reply::json(405, json!({"error": "method_not_allowed"})); }
        // 先验令牌：缺 / 过期 / 已吊销 / 伪造一律 401，不说原因。
        let Some(token) = bearer(req.authorization.as_deref()) else { return Reply::json(401, json!({"error": "unauthorized"})); };
        if !gw.authorize(token) { return Reply::json(401, json!({"error": "unauthorized"})); }
        let name = name.trim_end_matches('/');
        if !ALLOWED.contains(&name) { return Reply::json(403, json!({"error": "forbidden"})); }
        let args = if req.body.is_empty() { json!({}) } else { match serde_json::from_slice(&req.body) { Ok(v) => v, Err(_) => return Reply::json(400, json!({"error": "bad_args"})) } };
        return match commands.call(name, args) {
            Ok(v) => Reply::json(200, json!({"result": v})),
            Err(e) => Reply::json(500, json!({"error": e})),
        };
    }
    if req.method == "GET" || req.method == "HEAD" {
        if !safe_static_path(path) { return Reply::json(404, json!({"error": "not_found"})); }
        return match files.read(path) {
            Some(f) => Reply::file(f),
            None => Reply::json(404, json!({"error": "not_found"})),
        };
    }
    Reply::json(404, json!({"error": "not_found"}))
}

fn pair(gw: &Gateway, req: &HttpIn) -> Reply {
    #[derive(Deserialize)]
    struct Body { #[serde(default)] code: String, #[serde(default)] name: String }
    if req.body.len() > MAX_BODY { return Reply::json(413, json!({"error": "too_large"})); }
    let body: Body = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(_) => return Reply::json(400, json!({"error": "bad_body"})),
    };
    match gw.pair(&body.code, &body.name) {
        Ok(issued) => Reply::json(200, json!({
            "token": issued.token,
            "expiresAt": issued.expires_at,
        })),
        Err(err) => Reply::json(err.status(), json!({"error": err.code()})),
    }
}

/// 把一个 tiny_http 请求走一遍 [`route`] 并把结果写回去。
pub fn respond(mut req: tiny_http::Request, gw: &Gateway, files: &dyn Files, commands: &dyn Commands) {
    let method = req.method().as_str().to_string();
    let path = req.url().split(['?', '#']).next().unwrap_or("/").to_string();
    let authorization = req.headers().iter().find(|h| h.field.equiv("authorization")).map(|h| h.value.as_str().to_string());
    let mut body = Vec::new();
    use std::io::Read;
    let _ = req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body);
    let reply = if body.len() > MAX_BODY {
        Reply::json(413, json!({"error": "too_large"}))
    } else {
        route(gw, files, commands, &HttpIn { method, path, authorization, body })
    };
    let mut resp = tiny_http::Response::from_data(reply.body).with_status_code(reply.status);
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", reply.content_type),
        ("X-Content-Type-Options", "nosniff".to_string()),
        ("Cache-Control", if reply.no_store { "no-store".to_string() } else { "no-cache".to_string() }),
        ("Referrer-Policy", "no-referrer".to_string()),
    ];
    if let Some(csp) = reply.csp { headers.push(("Content-Security-Policy", csp)); }
    for (k, v) in headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) { resp.add_header(h); }
    }
    let _ = req.respond(resp);
}

/// 服务循环：一个 listener 一条线程，`stop` 打开后立刻回 503 并退出。
pub fn serve(server: Arc<tiny_http::Server>, stop: Arc<AtomicBool>, gw: Arc<Gateway>, files: Arc<dyn Files>, commands: Arc<dyn Commands>) {
    for req in server.incoming_requests() {
        if stop.load(Ordering::Relaxed) {
            let mut resp = tiny_http::Response::from_string(r#"{"error":"disabled"}"#).with_status_code(503u16);
            if let Ok(h) = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]) { resp.add_header(h); }
            if let Ok(h) = tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]) { resp.add_header(h); }
            let _ = req.respond(resp);
            continue;
        }
        respond(req, &gw, &*files, &*commands);
    }
}

/// 一个可以给手机打开的地址。`kind` 是机器码，界面负责翻译。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Url {
    pub kind: &'static str,
    pub url: String,
}

/// 桌面设置页看到的完整状态。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub port: u16,
    pub urls: Vec<Url>,
    pub code: Option<String>,
    pub code_expires_at: i64,
    pub devices: usize,
}

struct ServerHandle {
    server: Arc<tiny_http::Server>,
    stop: Arc<AtomicBool>,
}

struct Session {
    enabled: bool,
    port: u16,
    urls: Vec<Url>,
    servers: Vec<ServerHandle>,
}

/// 整个「手机访问」服务：配对状态 + 额外绑定的局域网监听。
pub struct Remote {
    pub gw: Arc<Gateway>,
    session: Mutex<Session>,
}

impl Default for Remote {
    fn default() -> Self { Self::new() }
}

impl Remote {
    pub fn new() -> Self {
        Self { gw: Gateway::new(), session: Mutex::new(Session { enabled: false, port: 0, urls: Vec::new(), servers: Vec::new() }) }
    }

    pub fn status(&self) -> Status {
        let s = self.session.lock().unwrap();
        let g = self.gw.status();
        Status { enabled: s.enabled, port: s.port, urls: s.urls.clone(), code: g.code, code_expires_at: g.code_expires_at, devices: g.devices }
    }

    /// 额外绑定 `targets`（局域网 + 回环）并在这些地址上服务同一套请求。
    ///
    /// `port` 是桌面界面正在用的端口（同一端口族，见 issue #2）；`0` 表示让系统挑一个（测试用）。
    /// 已经打开时是幂等的：不会再绑一遍，只返回当前状态。
    pub fn enable(
        &self,
        port: u16,
        family: u16,
        targets: Vec<Ipv4Addr>,
        files: Arc<dyn Files>,
        commands: Arc<dyn Commands>,
    ) -> Status {
        let mut s = self.session.lock().unwrap();
        if s.enabled { return self.status_locked(&s); }
        let family = if family == 0 { PORT_FAMILY } else { family };
        let mut chosen: Option<(u16, Vec<(Ipv4Addr, TcpListener)>)> = None;
        if port == 0 {
            chosen = bind_port(0, &targets);
        } else {
            for p in port..port.saturating_add(family) {
                if let Some(found) = bind_port(p, &targets) { chosen = Some(found); break; }
            }
        }
        let Some((port, listeners)) = chosen else {
            // 一个地址都绑不上：保持关闭，让界面把原因说清楚。
            return self.status_locked(&s);
        };
        let mut servers = Vec::new();
        let mut urls = Vec::new();
        for (ip, listener) in listeners {
            let Ok(server) = tiny_http::Server::from_listener(listener, None) else { continue };
            let server = Arc::new(server);
            let stop = Arc::new(AtomicBool::new(false));
            let (gw, files2, commands2, stop2, server2) = (self.gw.clone(), files.clone(), commands.clone(), stop.clone(), server.clone());
            std::thread::spawn(move || serve(server2, stop2, gw, files2, commands2));
            let kind = match ip {
                ip if ip.is_loopback() => "loopback",
                ip if is_tailscale(ip) => "tailscale",
                _ => "lan",
            };
            urls.push(Url { kind, url: format!("http://{ip}:{port}") });
            servers.push(ServerHandle { server, stop });
        }
        if servers.is_empty() { return self.status_locked(&s); }
        // 回环地址总是列出来：release 里回环被桌面界面服务占着，但它走的是同一个 route（/pair、/rpc 都能用）。
        if !urls.iter().any(|u| u.kind == "loopback") {
            urls.push(Url { kind: "loopback", url: format!("http://127.0.0.1:{port}") });
        }
        // 顺序：局域网 → Tailscale → 回环（第一项拿去做二维码）。
        urls.sort_by_key(|u| match u.kind { "lan" => 0, "tailscale" => 1, _ => 2 });
        self.gw.enable();
        s.enabled = true;
        s.port = port;
        s.urls = urls;
        s.servers = servers;
        self.status_locked(&s)
    }

    /// 关闭：停止监听（先 503 再断开）并吊销全部令牌。
    pub fn disable(&self) -> Status {
        let mut s = self.session.lock().unwrap();
        self.gw.disable();
        for h in s.servers.drain(..) {
            h.stop.store(true, Ordering::Relaxed);
            h.server.unblock();
        }
        s.enabled = false;
        s.port = 0;
        s.urls.clear();
        self.status_locked(&s)
    }

    /// 重新生成配对码；没打开时什么也不做。
    pub fn regenerate(&self) -> Status {
        let s = self.session.lock().unwrap();
        if s.enabled { self.gw.regenerate(); }
        self.status_locked(&s)
    }

    fn status_locked(&self, s: &Session) -> Status {
        let g = self.gw.status();
        Status { enabled: s.enabled, port: s.port, urls: s.urls.clone(), code: g.code, code_expires_at: g.code_expires_at, devices: g.devices }
    }
}

/// 绑定一个端口上的所有地址；至少绑上一个才算成功（回环被桌面占着时允许只有局域网）。
fn bind_port(port: u16, targets: &[Ipv4Addr]) -> Option<(u16, Vec<(Ipv4Addr, TcpListener)>)> {
    if port == 0 {
        // 测试用：让系统挑一个端口。只绑第一个地址，否则每个地址会各拿一个端口，URL 就对不上了。
        let ip = *targets.first()?;
        let listener = TcpListener::bind((ip, 0)).ok()?;
        let real = listener.local_addr().ok()?.port();
        return Some((real, vec![(ip, listener)]));
    }
    let mut bound: Vec<(Ipv4Addr, TcpListener)> = Vec::new();
    for ip in targets {
        if let Ok(l) = TcpListener::bind((*ip, port)) { bound.push((*ip, l)); }
    }
    if bound.is_empty() { None } else { Some((port, bound)) }
}

fn is_tailscale(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// 找出本机的局域网 / Tailscale IPv4 地址，按「局域网优先」去重。
///
/// 不引新依赖：先用 UDP `connect` 问系统「去这个地址走哪张网卡」（只查路由，不发包），
/// 再补一遍平台命令的输出，覆盖多网卡与 Tailscale 的情况。
pub fn local_targets() -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    let mut push = |ip: Ipv4Addr| {
        if ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() { return; }
        if !out.iter().any(|a| *a == ip) { out.push(ip); }
    };
    // 1) 默认路由 / Tailscale 的 MagicDNS：UDP connect 只做路由查询，不真的发包。
    for probe in ["8.8.8.8:53", "1.1.1.1:53", "100.100.100.100:53"] {
        let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else { continue };
        if sock.connect(probe).is_err() { continue; }
        if let Ok(local) = sock.local_addr() {
            if let IpAddr::V4(ip) = local.ip() { push(ip); }
        }
    }
    // 2) 平台命令的输出，补上非默认路由的网卡（以太网 + Wi-Fi 同时开、Tailscale 等）。
    for ip in platform_addresses() { push(ip); }
    out.sort_by_key(|ip| if is_tailscale(*ip) { 1 } else { 0 });
    out
}

/// 解析 `ifconfig` / `ip -o -4 addr` / `ipconfig` 里的 IPv4 地址。
fn platform_addresses() -> Vec<Ipv4Addr> {
    use std::process::Command;
    let mut out = Vec::new();
    #[cfg(unix)]
    let output = {
        let mac = cfg!(target_os = "macos");
        let cmd = if mac { Command::new("ifconfig").arg("-a").output() } else { Command::new("ip").args(["-o", "-4", "addr", "show"]).output() };
        cmd.ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
    };
    #[cfg(windows)]
    let output = { Command::new("ipconfig").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default() };
    #[cfg(not(any(unix, windows)))]
    let output: String = String::new();
    let tokens: Vec<&str> = output.split_whitespace().collect();
    // unix：只认紧跟 "inet" 的那个 token，netmask / broadcast 自然跳过；
    // windows 的 ipconfig 没有 inet 关键字，就逐个 token 试，再用私网段过滤。
    let has_inet = tokens.iter().any(|t| *t == "inet");
    for (i, t) in tokens.iter().enumerate() {
        let candidate = if *t == "inet" {
            tokens.get(i + 1).copied()
        } else if !has_inet {
            Some(*t)
        } else {
            None
        };
        let Some(token) = candidate else { continue };
        let token = token.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
        let token = token.split('/').next().unwrap_or(token);
        let Ok(ip) = token.parse::<Ipv4Addr>() else { continue };
        let o = ip.octets();
        let private = o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168) || is_tailscale(ip);
        if private { out.push(ip); }
    }
    out
}

/// 进程内唯一的一份（生产用）。
pub fn remote() -> &'static Remote {
    static R: OnceLock<Remote> = OnceLock::new();
    R.get_or_init(Remote::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;

    fn test_clock() -> (Arc<AtomicI64>, Clock) {
        let c = Arc::new(AtomicI64::new(1_000_000));
        (c.clone(), Arc::new(move || c.load(Ordering::Relaxed)))
    }
    fn advance(c: &Arc<AtomicI64>, ms: i64) { c.fetch_add(ms, Ordering::Relaxed); }

    struct NoFiles;
    impl Files for NoFiles {
        fn read(&self, path: &str) -> Option<FileBody> {
            (path == "/" || path == "/index.html").then(|| FileBody { bytes: b"<html>ok</html>".to_vec(), mime: "text/html".into(), csp: None })
        }
    }

    struct FakeCommands { allowed: bool }
    impl Commands for FakeCommands {
        fn call(&self, command: &str, _args: Value) -> Result<Value, String> {
            if command == "list_shows" { Ok(json!([{"id": "s1", "title": "节目一"}])) } else if self.allowed { Ok(json!(null)) } else { Err("forbidden".into()) }
        }
    }

    fn pair_req(code: &str) -> HttpIn {
        HttpIn { method: "POST".into(), path: "/pair".into(), authorization: None, body: serde_json::to_vec(&json!({"code": code, "name": "iPhone"})).unwrap() }
    }
    fn rpc_req(command: &str, token: Option<&str>) -> HttpIn {
        HttpIn { method: "POST".into(), path: format!("/rpc/{command}"), authorization: token.map(|t| format!("Bearer {t}")), body: b"{}".to_vec() }
    }
    fn new_gw() -> (Arc<AtomicI64>, Arc<Gateway>) {
        let (clock, c) = test_clock();
        (clock, Gateway::with_clock(c))
    }

    #[test]
    fn pairing_code_is_six_digits_and_works_once() {
        let (_clock, gw) = new_gw();
        let st = gw.enable();
        let code = st.code.expect("enabled → a code");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        let issued = gw.pair(&code, "iPhone").expect("first use works");
        assert_eq!(issued.token.len(), 64);
        // 用过一次就作废
        assert_eq!(gw.pair(&code, "iPhone").unwrap_err(), PairError::Invalid);
        assert!(gw.authorize(&issued.token));
    }

    #[test]
    fn pairing_code_expires_after_five_minutes() {
        let (clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        advance(&clock, PAIR_TTL_MS - 1);
        let issued = gw.pair(&code, "iPhone").expect("still within 5 minutes");
        assert!(gw.authorize(&issued.token));
        let code2 = gw.regenerate().code.unwrap();
        advance(&clock, PAIR_TTL_MS);
        assert_eq!(gw.pair(&code2, "iPhone").unwrap_err(), PairError::Expired);
        assert!(gw.status().code.is_none());
    }

    #[test]
    fn wrong_code_is_rejected_and_ten_mistakes_lock_it() {
        let (_clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        let wrong = if code == "000000" { "000001" } else { "000000" };
        for _ in 0..MAX_PAIR_FAILS - 1 {
            assert_eq!(gw.pair(wrong, "iPhone").unwrap_err(), PairError::Invalid);
        }
        // 第 10 次错就锁一分钟；锁住期间正确的码也不行。
        assert_eq!(gw.pair(wrong, "iPhone").unwrap_err(), PairError::Locked);
        assert_eq!(gw.pair(&code, "iPhone").unwrap_err(), PairError::Locked);
        assert!(!gw.authorize("whatever"));
    }

    #[test]
    fn disabled_gateway_rejects_pairing() {
        let (_clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        gw.disable();
        assert_eq!(gw.pair(&code, "iPhone").unwrap_err(), PairError::Disabled);
        assert!(!gw.status().enabled);
        assert!(gw.status().code.is_none());
    }

    #[test]
    fn token_expires_after_seven_days() {
        let (clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        let issued = gw.pair(&code, "iPhone").unwrap();
        advance(&clock, TOKEN_TTL_MS - 1);
        assert!(gw.authorize(&issued.token));
        advance(&clock, 1);
        assert!(!gw.authorize(&issued.token));
    }

    #[test]
    fn disable_revokes_every_token() {
        let (_clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        let issued = gw.pair(&code, "iPhone").unwrap();
        assert!(gw.authorize(&issued.token));
        gw.disable();
        assert!(!gw.authorize(&issued.token), "关闭后旧令牌立刻失效");
        gw.enable();
        assert!(!gw.authorize(&issued.token), "重新打开也不会复活旧令牌");
    }

    #[test]
    fn forged_token_is_rejected() {
        let (_clock, gw) = new_gw();
        gw.enable();
        assert!(!gw.authorize(""));
        assert!(!gw.authorize("deadbeef"));
        assert!(!gw.authorize(&"a".repeat(64)), "格式对但没签发过也不行");
    }

    #[test]
    fn rpc_needs_a_token_and_only_the_whitelist() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands { allowed: false };
        // 没令牌 → 401
        let r = route(&gw, &files, &cmds, &rpc_req("list_shows", None));
        assert_eq!(r.status, 401);
        // 伪造令牌 → 同样 401，不区分原因
        let r = route(&gw, &files, &cmds, &rpc_req("list_shows", Some("nope")));
        assert_eq!(r.status, 401);
        // 有效令牌 + 白名单内 → 200
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        let r = route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&token)));
        assert_eq!(r.status, 200);
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["result"][0]["id"], "s1");
        // 有效令牌 + 白名单外 → 403（哪怕桌面端真有这个命令）
        let r = route(&gw, &files, &cmds, &rpc_req("remove_show", Some(&token)));
        assert_eq!(r.status, 403);
        assert!(r.body.len() < 200 && !String::from_utf8_lossy(&r.body).contains("token"));
    }

    #[test]
    fn closing_the_switch_makes_old_tokens_401() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands { allowed: false };
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&token))).status, 200);
        gw.disable();
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&token))).status, 401);
        assert_eq!(route(&gw, &files, &cmds, &pair_req(&code)).status, 503);
    }

    #[test]
    fn pair_errors_are_told_apart() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands { allowed: false };
        let code = gw.enable().code.unwrap();
        // 码错
        let r = route(&gw, &files, &cmds, &pair_req("999999"));
        assert_eq!(r.status, 401);
        assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap()["error"], "invalid_code");
        // 已关闭
        gw.disable();
        let r = route(&gw, &files, &cmds, &pair_req(&code));
        assert_eq!(r.status, 503);
        assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap()["error"], "disabled");
        // 过期（重新打开后把码拨过期）
        let (clock, gw) = new_gw();
        let code = gw.enable().code.unwrap();
        advance(&clock, PAIR_TTL_MS);
        let r = route(&gw, &files, &cmds, &pair_req(&code));
        assert_eq!(r.status, 410);
        assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap()["error"], "expired_code");
    }

    #[test]
    fn static_files_are_served_and_traversal_is_refused() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands { allowed: false };
        let get = |path: &str| HttpIn { method: "GET".into(), path: path.into(), authorization: None, body: vec![] };
        let r = route(&gw, &files, &cmds, &get("/"));
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"<html>ok</html>");
        assert_eq!(route(&gw, &files, &cmds, &get("/logo.png")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &get("/../secret")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &get("/%2e%2e/secret")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &HttpIn { method: "PUT".into(), path: "/".into(), authorization: None, body: vec![] }).status, 404);
    }

    #[test]
    fn enable_and_disable_over_real_http() {
        let files: Arc<dyn Files> = Arc::new(NoFiles);
        let cmds: Arc<dyn Commands> = Arc::new(FakeCommands { allowed: false });
        let remote = Remote::new();
        let st = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files.clone(), cmds.clone());
        assert!(st.enabled, "回环也能开起来（测试环境）");
        let port = st.port;
        assert!(port > 0);
        let code = st.code.expect("a code");
        // POST /pair
        let (status, body) = http_post(port, "/pair", None, &format!(r#"{{"code":"{code}","name":"iPhone"}}"#));
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        let token = v["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 64);
        // POST /rpc/list_shows 用令牌
        let (status, body) = http_post(port, "/rpc/list_shows", Some(&token), "{}");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("s1"));
        // 关闭后：监听确实停了（旧令牌 401 由 closing_the_switch_makes_old_tokens_401 覆盖）
        remote.disable();
        assert!(wait_port_closed(port), "关闭后端口不再接受连接");
    }

    #[test]
    fn enable_is_idempotent_and_regenerate_changes_the_code() {
        let files: Arc<dyn Files> = Arc::new(NoFiles);
        let cmds: Arc<dyn Commands> = Arc::new(FakeCommands { allowed: false });
        let remote = Remote::new();
        let a = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files.clone(), cmds.clone());
        let b = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files, cmds);
        assert_eq!(a.port, b.port, "打开两次不会换端口");
        let c = remote.regenerate();
        assert_ne!(a.code, c.code, "重新生成会换个码");
        assert_eq!(c.code.as_ref().map(String::len), Some(6));
        remote.disable();
        assert!(!remote.status().enabled);
        assert_eq!(remote.status().urls.len(), 0);
    }

    /// 极简 HTTP 客户端：测试里不想为了一个 POST 再拉依赖。
    fn http_post(port: u16, path: &str, bearer: Option<&str>, body: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
        let auth = bearer.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let req = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).ok();
        let text = String::from_utf8_lossy(&raw).to_string();
        let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        (status, body)
    }

    fn wait_port_closed(port: u16) -> bool {
        for _ in 0..40 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() { return true; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn local_targets_never_include_loopback_or_link_local() {
        for ip in local_targets() {
            assert!(!ip.is_loopback());
            assert!(!ip.is_link_local());
        }
    }
}
