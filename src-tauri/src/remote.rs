//! 手机访问（issue #2 / T1、issue #3 / T2、issue #4 / T3）：局域网里的手机浏览器用配对码换一个设备令牌，再通过
//! HTTP 网关读取订阅、单集与字幕，并回写播放进度。
//!
//! 边界（刻意从严）：
//! - **复用界面那个 HTTP 服务**：不另起服务。开关打开时在同一端口族上额外绑定局域网地址，请求都走
//!   [`route`]；关闭时停掉这些监听并吊销全部令牌。
//! - 配对码 6 位、5 分钟有效、**用过一次即作废**，可随时重新生成。
//! - 设备令牌 7 天有效；SQLite 只存哈希和设备元数据，关闭开关即撤销。
//! - 网关只放行**硬编码白名单** [`ALLOWED`]：只读命令（订阅、单集、字幕、播放提示）加上**仅有的两条写入**
//!   ——播放进度与已看状态。提问按设备限流；设置、Key、订阅管理、下载、登录和文件操作一律 403。
//! - 缺令牌 / 过期 / 已吊销 / 伪造：一律 401，响应体不区分原因。
//! - 日志与错误信息不打印配对码、令牌。
//!
//! SSE 只转发当前单集的转写进度，以及当前设备自己的问答。手机端不写回设置或订阅。
//!
//! 桌面端要知道「有没有手机在看、在放什么」：每次放行的请求都把该设备记成活跃（[`VIEWER_TTL_MS`] 内算 1 个
//! 查看者），手机回写的进度记成 [`NowPlaying`]，两者都随 [`Status`] 一起返回。

use crate::remote_access::{AccessVerifier, ORIGIN_PORT};
use crate::remote_events::{Answer, Events, Stream};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 配对码有效期。
pub const PAIR_TTL_MS: i64 = 5 * 60 * 1000;
/// 设备令牌有效期。
pub const TOKEN_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 连续输错这么多次，锁一分钟再试。
pub const MAX_PAIR_FAILS: u32 = 10;
pub const LOCK_MS: i64 = 60 * 1000;
/// 一个设备最近这么长时间内有过请求，就算「正在看」。手机播放时每 15 秒回写一次进度，所以播放期间它会一直
/// 留在窗口里；停下来最多再显示这么久。
pub const VIEWER_TTL_MS: i64 = 60 * 1000;
/// 手机端放行的命令：只读的那几项 —— 订阅列表、某档节目的单集、单集详情、字幕，以及决定「这期能不能播」的
/// 提示（`reading_hints`）；再加上**仅有的两条写入** —— 回写播放进度与已看状态（issue #4）。
/// 其它写入类命令（订阅、下载、设置、登录、打开文件）一个都不放行；桌面端有的命令不在这张表里也一律 403。
pub const ALLOWED: &[&str] = &[
    "list_shows", "list_episodes", "get_episode", "load_transcript", "reading_hints",
    "set_progress", "set_watched", "phone_presence", "ask_episode",
];
/// 请求体上限，别让一个请求把内存吃掉。
const MAX_BODY: usize = 16 * 1024;
const MAX_TUNNEL_REQUESTS: usize = 8;
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

struct Device {
    /// 进程内的设备编号，只用来区分「几个设备在看」。
    id: u64,
    token_hash: String,
    label: String,
    paired_at: i64,
    rpc_window: i64,
    rpc_count: u32,
    expires_at: i64,
    /// 最近一次通过令牌的请求（毫秒时间戳）。
    last_seen: Option<i64>,
    recorded_at: i64,
    now_playing: Option<NowPlaying>,
}

/// 手机刚回写的进度：桌面端据此显示「手机正在播放」。标题桌面端自己查库，这里只存事实。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NowPlaying {
    pub episode_id: String,
    pub seconds: f64,
    pub updated_at: i64,
}

struct DeviceStore {
    path: PathBuf,
    conn: Connection,
}

#[derive(Default)]
struct Inner {
    enabled: bool,
    access_only: bool,
    code: Option<String>,
    code_expires_at: i64,
    failed: u32,
    locked_until: i64,
    devices: Vec<Device>,
    next_device_id: u64,
    storage: Option<DeviceStore>,
    stores_pending_clear: HashSet<PathBuf>,
}

/// 配对/令牌的纯状态机，不碰网络，方便测试。
pub struct Gateway {
    inner: Mutex<Inner>,
    clock: Clock,
    pub events: Events,
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
    Unavailable,
}

impl PairError {
    pub fn status(self) -> u16 {
        match self {
            PairError::Disabled => 503,
            PairError::Invalid => 401,
            PairError::Expired => 410,
            PairError::Locked => 429,
            PairError::Unavailable => 503,
        }
    }
    /// 机器可读的原因，手机页据此给出「码错 / 过期 / 已关闭」三种提示。
    pub fn code(self) -> &'static str {
        match self {
            PairError::Disabled => "disabled",
            PairError::Invalid => "invalid_code",
            PairError::Expired => "expired_code",
            PairError::Locked => "locked",
            PairError::Unavailable => "unavailable",
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
    pub paired_devices: Vec<PairedDevice>,
    /// 最近 [`VIEWER_TTL_MS`] 内有过请求的设备数（手机播放时每 15 秒回写进度，所以在放的时候一直算一个）。
    pub viewers: usize,
    /// 手机最近回写的那一期和位置；超过 [`VIEWER_TTL_MS`] 没再回写就是 None。
    pub now_playing: Option<NowPlaying>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    id: u64,
    label: String,
    paired_at: i64,
    last_seen: Option<i64>,
}

impl Gateway {
    pub fn new() -> Arc<Self> { Self::with_clock(system_clock()) }

    pub fn with_clock(clock: Clock) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::default()), clock, events: Events::default() })
    }

    fn now(&self) -> i64 { (self.clock)() }

    /// Open the registry on explicit opt-in. Reading it never starts a listener.
    pub fn open_device_store(&self, path: &Path) -> rusqlite::Result<()> {
        let mut g = self.inner.lock().unwrap();
        if g.enabled { return Ok(()); }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(1))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS remote_devices(
                id INTEGER PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE, label TEXT NOT NULL,
                paired_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, last_seen INTEGER);",
        )?;
        if g.stores_pending_clear.contains(path) {
            conn.execute("DELETE FROM remote_devices", [])?;
            g.stores_pending_clear.remove(path);
        }
        let now = self.now();
        conn.execute("DELETE FROM remote_devices WHERE expires_at <= ?1", [now])?;
        let devices: Vec<Device> = {
            let mut stmt = conn.prepare("SELECT id, token_hash, label, paired_at, expires_at, last_seen FROM remote_devices")?;
            stmt.query_map([], |row| Ok(Device {
                id: row.get(0)?, token_hash: row.get(1)?, label: row.get(2)?,
                paired_at: row.get(3)?, expires_at: row.get(4)?, last_seen: None,
                recorded_at: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                rpc_window: now, rpc_count: 0, now_playing: None,
            }))?.collect::<rusqlite::Result<_>>()?
        };
        g.next_device_id = g.next_device_id.max(devices.iter().map(|d| d.id).max().unwrap_or(0));
        g.devices = devices;
        g.storage = Some(DeviceStore { path: path.to_owned(), conn });
        Ok(())
    }

    /// 打开：生成配对码，并允许仍在有效期内的已配对设备接入。
    pub fn enable(&self) -> GatewayStatus { self.activate(false) }

    fn activate(&self, access_only: bool) -> GatewayStatus {
        let mut g = self.inner.lock().unwrap();
        g.enabled = true;
        g.access_only = access_only;
        g.failed = 0;
        g.locked_until = 0;
        self.new_code(&mut g);
        self.status_of(&g)
    }

    /// 关闭：停用并吊销全部令牌（旧令牌立刻 401）。
    pub fn disable(&self) -> rusqlite::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.enabled = false;
        self.events.clear();
        g.code = None;
        g.code_expires_at = 0;
        g.failed = 0;
        g.locked_until = 0;
        g.devices.clear();
        let Some(store) = g.storage.take() else { return Ok(()); };
        let result = store.conn.execute("DELETE FROM remote_devices", []);
        if result.is_err() { g.stores_pending_clear.insert(store.path); }
        else { g.stores_pending_clear.remove(&store.path); }
        result.map(|_| ())
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
        // 「正在看」= 最近一分钟内有过请求；播放时手机每 15 秒回写一次，所以会一直活着。
        let alive = |at: i64| at > now - VIEWER_TTL_MS;
        let viewers = if g.enabled { g.devices.iter().filter(|d| d.expires_at > now && d.last_seen.is_some_and(alive)).count() } else { 0 };
        let now_playing = g.devices.iter()
            .filter(|d| g.enabled && d.expires_at > now)
            .filter_map(|d| d.now_playing.as_ref().filter(|p| alive(p.updated_at)))
            .max_by_key(|p| p.updated_at)
            .cloned();
        let paired_devices: Vec<_> = g.devices.iter().filter(|d| d.expires_at > now).map(|d| PairedDevice {
            id: d.id, label: d.label.clone(), paired_at: d.paired_at, last_seen: d.last_seen.or((d.recorded_at > 0).then_some(d.recorded_at)),
        }).collect();
        GatewayStatus { enabled: g.enabled, code, code_expires_at: g.code_expires_at, devices: paired_devices.len(), paired_devices, viewers, now_playing }
    }

    /// 校验配对码；成功则签发一个 7 天令牌（只把哈希留下），配对码作废。
    #[cfg(test)]
    pub fn pair(&self, code: &str, label: &str) -> Result<Issued, PairError> { self.pair_for(code, label, false) }

    fn pair_for(&self, code: &str, label: &str, access: bool) -> Result<Issued, PairError> {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        if !g.enabled || g.access_only != access { return Err(PairError::Disabled); }
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
        g.next_device_id += 1;
        let id = g.next_device_id;
        let device = Device {
            id,
            token_hash: sha256_hex(&token),
            label: label.trim().chars().filter(|c| !c.is_control()).take(80).collect(),
            paired_at: now,
            rpc_window: now,
            rpc_count: 0,
            expires_at,
            last_seen: None,
            recorded_at: 0,
            now_playing: None,
        };
        if let Some(store) = &g.storage {
            if store.conn.execute("INSERT INTO remote_devices(id,token_hash,label,paired_at,expires_at) VALUES (?1,?2,?3,?4,?5)",
                params![device.id, device.token_hash, device.label, now, expires_at]).is_err() {
                g.code = Some(expected);
                return Err(PairError::Unavailable);
            }
        }
        g.devices.push(device);
        g.failed = 0;
        g.locked_until = 0;
        Ok(Issued { token, expires_at })
    }

    fn revoke(&self, device: Option<u64>) -> rusqlite::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let result = match (device, &g.storage) {
            (Some(id), Some(store)) => store.conn.execute("DELETE FROM remote_devices WHERE id=?1", [id]),
            (None, Some(store)) => store.conn.execute("DELETE FROM remote_devices", []),
            (_, None) => Ok(0),
        };
        if let Some(id) = device {
            g.devices.retain(|d| d.id != id);
            self.events.remove(id);
        } else {
            g.devices.clear();
            self.events.clear();
        }
        if result.is_err() {
            if let Some(path) = g.storage.as_ref().map(|store| store.path.clone()) { g.stores_pending_clear.insert(path); }
        } else if device.is_none() {
            if let Some(path) = g.storage.as_ref().map(|store| store.path.clone()) { g.stores_pending_clear.remove(&path); }
        }
        result.map(|_| ())
    }

    fn admit_rpc(&self, id: u64) -> Result<(), u16> {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        let Some(device) = g.devices.iter_mut().find(|d| d.id == id && d.expires_at > now) else { return Err(401); };
        if now - device.rpc_window >= 60_000 {
            device.rpc_window = now;
            device.rpc_count = 0;
        }
        if device.rpc_count >= 180 { return Err(429); }
        device.rpc_count += 1;
        Ok(())
    }

    fn note_asr(&self, episode: &str, value: Value) {
        if self.inner.lock().unwrap().enabled { self.events.note_asr(episode, value); }
    }

    pub fn device_active(&self, id: u64) -> bool {
        let g = self.inner.lock().unwrap();
        g.enabled && g.devices.iter().any(|d| d.id == id && d.expires_at > self.now())
    }

    fn start_question(&self, device: u64, episode: &str) -> Result<(String, Arc<Answer>), u16> {
        let g = self.inner.lock().unwrap();
        if !g.enabled || !g.devices.iter().any(|d| d.id == device && d.expires_at > self.now()) { return Err(401); }
        self.events.start(device, episode).ok_or(429)
    }

    /// 令牌对应的设备编号（开关打开、未过期、没被吊销），顺手记下这次活跃时间。任何原因不通过都是 None。
    fn authorize_device(&self, token: &str, access: bool) -> Option<u64> {
        if token.is_empty() { return None; }
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        if !g.enabled || g.access_only != access { return None; }
        g.devices.retain(|d| {
            let active = d.expires_at > now;
            if !active { self.events.remove(d.id); }
            active
        });
        let hash = sha256_hex(token);
        let Inner { devices, storage, .. } = &mut *g;
        let device = devices.iter_mut().find(|d| d.token_hash == hash)?;
        device.last_seen = Some(now);
        if now - device.recorded_at >= 15_000 {
            if let Some(store) = storage {
                if store.conn.execute("UPDATE remote_devices SET last_seen=?2 WHERE id=?1", params![device.id, now]).is_ok() {
                    device.recorded_at = now;
                }
            }
        }
        Some(device.id)
    }

    /// 令牌是否可用（开关打开、未过期、没被吊销）。任何原因不通过都只是 false。
    #[cfg(test)]
    pub fn authorize(&self, token: &str) -> bool { self.authorize_device(token, false).is_some() }

    /// 手机回写进度成功：记下「在放哪一期、到哪了」。桌面设置页刷新 [`Status`] 就能看到。
    fn note_progress(&self, device_id: u64, episode_id: &str, seconds: f64, playing: bool) {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        if let Some(device) = g.devices.iter_mut().find(|d| d.id == device_id && d.expires_at > now) {
            device.now_playing = playing.then(|| NowPlaying { episode_id: episode_id.to_string(), seconds, updated_at: now });
        }
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
    fn ask(&self, _episode: &str, _question: &str, _answer: Arc<Answer>) -> Result<(), &'static str> {
        Err("chat_unavailable")
    }
}

/// 解析后的一次请求。测试里直接构造它，不需要真的开 socket。
#[derive(Debug, Clone)]
pub struct HttpIn {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub access: bool,
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

/// 手机端自己读不到的两件事：这期是否已经下载到电脑上（下载的是本地文件，手机上放不了），
/// 以及字幕的中文译文（桌面上存在 blob `zh` 里，和 segments 一一对应，允许有没有译文的行）。
/// 坏掉的 blob 当成没有，页面照常显示原文。
pub fn reading_hints(offline: bool, zh: Option<&str>) -> Value {
    let zh = zh
        .and_then(|raw| serde_json::from_str::<Vec<Option<String>>>(raw).ok())
        .map(|items| json!(items))
        .unwrap_or(Value::Null);
    json!({ "offline": offline, "zh": zh })
}

/// 只允许普通的相对路径，挡住 `..` 与编码过的变体。
fn safe_static_path(path: &str) -> bool {
    if path.starts_with('/') && !path.starts_with("//") {
        let lower = path.to_ascii_lowercase();
        return !lower.contains("..") && !lower.contains('\\') && !lower.contains("%2e") && !lower.contains("%00");
    }
    false
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskArgs { episode_id: String, question: String }

/// 命令网关：唯一入口。配对、RPC、静态资源都在这里分流。
pub fn route(gw: &Gateway, files: &dyn Files, commands: &dyn Commands, req: &HttpIn) -> Reply {
    let path = req.path.as_str();
    if req.method == "POST" && (path == "/pair" || path == "/pair/") {
        return pair(gw, req);
    }
    if let Some(name) = path.strip_prefix("/rpc/") {
        if req.method != "POST" { return Reply::json(405, json!({"error": "method_not_allowed"})); }
        // 先验令牌：缺 / 过期 / 已吊销 / 伪造一律 401，不说原因。通过时顺手记下这台设备还活跃。
        let Some(token) = bearer(req.authorization.as_deref()) else { return Reply::json(401, json!({"error": "unauthorized"})); };
        let Some(device_id) = gw.authorize_device(token, req.access) else { return Reply::json(401, json!({"error": "unauthorized"})); };
        if let Err(status) = gw.admit_rpc(device_id) {
            let code = if status == 401 { "unauthorized" } else { "rate_limited" };
            return Reply::json(status, json!({"error": code}));
        }
        let name = name.trim_end_matches('/');
        if !ALLOWED.contains(&name) { return Reply::json(403, json!({"error": "forbidden"})); }
        if name == "phone_presence" { return Reply::json(200, json!({"result": null})); }
        if name == "ask_episode" {
            let Ok(args) = serde_json::from_slice::<AskArgs>(&req.body) else {
                return Reply::json(400, json!({"error": "bad_args"}));
            };
            let question = args.question.trim();
            if args.episode_id.is_empty() || args.episode_id.len() > 256 || question.is_empty() || question.chars().count() > 1000 {
                return Reply::json(400, json!({"error": "bad_args"}));
            }
            let (request, answer) = match gw.start_question(device_id, &args.episode_id) {
                Ok(job) => job,
                Err(401) => return Reply::json(401, json!({"error": "unauthorized"})),
                Err(_) => return Reply::json(429, json!({"error": "chat_rate_limited"})),
            };
            return match commands.ask(&args.episode_id, question, answer.clone()) {
                Ok(()) => Reply::json(200, json!({"result": request})),
                Err(code) => { answer.finish(None); Reply::json(422, json!({"error": code})) }
            };
        }
        let args = if req.body.is_empty() { json!({}) } else { match serde_json::from_slice(&req.body) { Ok(v) => v, Err(_) => return Reply::json(400, json!({"error": "bad_args"})) } };
        // 回写进度成功后记下「在放什么」，桌面设置页靠它显示正在播放的单集（标题由桌面端查库补上）。
        let progress = (name == "set_progress")
            .then(|| args.get("id").and_then(Value::as_str).map(str::to_string).zip(args.get("seconds").and_then(Value::as_f64)))
            .flatten();
        let age = args.get("ageMs").and_then(Value::as_i64).unwrap_or(0);
        let playing = args.get("playing").and_then(Value::as_bool).unwrap_or(false) && age < VIEWER_TTL_MS;
        return match commands.call(name, args) {
            Ok(v) => {
                if let Some((id, seconds)) = progress.filter(|_| v.as_bool() != Some(false)) { gw.note_progress(device_id, &id, seconds, playing); }
                Reply::json(200, json!({"result": v}))
            }
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
    match gw.pair_for(&body.code, &body.name, req.access) {
        Ok(issued) => Reply::json(200, json!({
            "token": issued.token,
            "expiresAt": issued.expires_at,
        })),
        Err(err) => Reply::json(err.status(), json!({"error": err.code()})),
    }
}

fn request_header<'a>(req: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn same_origin_request(req: &tiny_http::Request, local: SocketAddr, access: Option<&AccessVerifier>) -> bool {
    if request_header(req, "x-subcast-client") != Some("phone") { return false; }
    let Some(source) = request_header(req, "origin").or_else(|| request_header(req, "referer"))
        .and_then(|raw| reqwest::Url::parse(raw).ok()) else { return false; };
    if let Some(access) = access { return access.is_same_origin(&source); }
    let Some(host) = request_header(req, "host") else { return false; };
    let Ok(expected) = reqwest::Url::parse(&format!("http://{host}")) else { return false; };
    if expected.port_or_known_default() != Some(local.port()) || expected.path() != "/" || expected.query().is_some()
        || expected.fragment().is_some() || !expected.username().is_empty() || expected.password().is_some() { return false; }
    let bound_host = match expected.host_str() {
        Some("localhost") => local.ip().is_loopback(),
        Some(ip) => ip.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip == local.ip()),
        None => false,
    };
    if !bound_host { return false; }
    source.origin() == expected.origin()
}

/// 把一个 tiny_http 请求走一遍 [`route`] 并把结果写回去。
fn respond(
    mut req: tiny_http::Request, local: SocketAddr, gw: &Arc<Gateway>,
    files: &dyn Files, commands: &dyn Commands, access: Option<&AccessVerifier>, stop: &AtomicBool,
) {
    let method = req.method().as_str().to_string();
    let path = req.url().split(['?', '#']).next().unwrap_or("/").to_string();
    let access_expiry = if let Some(access) = access {
        if !local.ip().is_loopback() || !request_header(&req, "host").is_some_and(|host| access.matches_host(host)) {
            send_reply(req, Reply::json(403, json!({"error": "forbidden"})));
            return;
        }
        let Some(expiry) = request_header(&req, "cf-access-jwt-assertion").and_then(|token| access.authenticate(token)) else {
            send_reply(req, Reply::json(401, json!({"error": "access_required"})));
            return;
        };
        Some(expiry)
    } else { None };
    if (path == "/pair" || path == "/pair/" || path.starts_with("/rpc/") || path == "/events")
        && !same_origin_request(&req, local, access) {
        send_reply(req, Reply::json(403, json!({"error": "forbidden"})));
        return;
    }
    if stop.load(Ordering::Acquire) {
        send_reply(req, Reply::json(503, json!({"error": "disabled"})));
        return;
    }
    let authorization = req.headers().iter().find(|h| h.field.equiv("authorization")).map(|h| h.value.as_str().to_string());
    if path == "/events" {
        match open_events(&req, gw, authorization.as_deref(), access_expiry) {
            Ok(stream) => {
                std::thread::spawn(move || stream.write_to(req.into_writer()));
            }
            Err(reply) => send_reply(req, reply),
        }
        return;
    }
    let mut body = Vec::new();
    use std::io::Read;
    let read = req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body);
    let mut reply = if stop.load(Ordering::Acquire) {
        Reply::json(503, json!({"error": "disabled"}))
    } else if access_expiry.is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp()) {
        Reply::json(401, json!({"error": "access_required"}))
    } else if read.is_err() {
        Reply::json(400, json!({"error": "bad_body"}))
    } else if body.len() > MAX_BODY {
        Reply::json(413, json!({"error": "too_large"}))
    } else {
        route(gw, files, commands, &HttpIn { method, path, authorization, access: access.is_some(), body })
    };
    reply.no_store |= access.is_some();
    send_reply(req, reply);
}

fn send_reply(req: tiny_http::Request, reply: Reply) {
    let mut resp = tiny_http::Response::from_data(reply.body).with_status_code(reply.status);
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", reply.content_type),
        ("X-Content-Type-Options", "nosniff".to_string()),
        ("Cache-Control", if reply.no_store { "no-store".to_string() } else { "no-cache".to_string() }),
        ("Referrer-Policy", "same-origin".to_string()),
    ];
    if let Some(csp) = reply.csp { headers.push(("Content-Security-Policy", csp)); }
    for (k, v) in headers {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) { resp.add_header(h); }
    }
    let _ = req.respond(resp);
}

fn open_events(req: &tiny_http::Request, gw: &Arc<Gateway>, authorization: Option<&str>, access_expiry: Option<i64>) -> Result<Stream, Reply> {
    let reject = |status, code| Reply::json(status, json!({"error": code}));
    if req.method() != &tiny_http::Method::Get { return Err(reject(405, "method_not_allowed")); }
    let device = bearer(authorization).and_then(|token| gw.authorize_device(token, access_expiry.is_some()))
        .ok_or_else(|| reject(401, "unauthorized"))?;
    let url = reqwest::Url::parse(&format!("http://localhost{}", req.url()))
        .map_err(|_| reject(400, "bad_args"))?;
    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
    let episode = query.get("episodeId").filter(|id| !id.is_empty() && id.len() <= 256)
        .ok_or_else(|| reject(400, "bad_args"))?.to_string();
    let question = if let Some(request) = query.get("requestId") {
        let answer = gw.events.answer(device, request, &episode).ok_or_else(|| reject(404, "not_found"))?;
        Some((request.to_string(), answer))
    } else { None };
    let after = query.get("after").and_then(|id| id.parse().ok()).unwrap_or(0);
    let slot = gw.events.connect(device).ok_or_else(|| reject(429, "too_many_connections"))?;
    Ok(Stream::new(gw.clone(), device, episode, question, after, slot, access_expiry))
}

/// 服务循环：一个 listener 一条线程，`stop` 打开后立刻回 503 并退出。
pub fn serve(
    server: Arc<tiny_http::Server>, stop: Arc<AtomicBool>, gw: Arc<Gateway>,
    files: Arc<dyn Files>, commands: Arc<dyn Commands>, access: Option<Arc<AccessVerifier>>,
) {
    let Some(local) = server.server_addr().to_ip() else { return; };
    let active_requests = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Acquire) {
        let req = match server.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(Some(req)) if !stop.load(Ordering::Acquire) => req,
            Ok(_) => continue,
            Err(_) => break,
        };
        if let Some(access) = &access {
            // A delayed request body must not hold the listener open or block other browser requests.
            if active_requests.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                |count| (count < MAX_TUNNEL_REQUESTS).then_some(count + 1)).is_err() {
                send_reply(req, Reply::json(429, json!({"error": "too_many_connections"})));
                continue;
            }
            let (gw, files, commands, access, stop, active) =
                (gw.clone(), files.clone(), commands.clone(), access.clone(), stop.clone(), active_requests.clone());
            std::thread::spawn(move || {
                respond(req, local, &gw, &*files, &*commands, Some(&access), &stop);
                active.fetch_sub(1, Ordering::Release);
            });
        } else {
            respond(req, local, &gw, &*files, &*commands, None, &stop);
        }
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
    pub paired_devices: Vec<PairedDevice>,
    /// 最近一分钟内有过请求的手机数量（「手机正在被 N 台设备查看」）。
    pub viewers: usize,
    /// 手机最近回写的进度（「正在播放哪一期」），标题由桌面端自己查库。
    pub now_playing: Option<NowPlaying>,
}

struct ServerHandle {
    ip: Ipv4Addr,
    server: Arc<tiny_http::Server>,
    stop: Arc<AtomicBool>,
}

struct Session {
    enabled: bool,
    port: u16,
    urls: Vec<Url>,
    servers: Vec<ServerHandle>,
    service: Option<(Arc<dyn Files>, Arc<dyn Commands>)>,
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
        Self { gw: Gateway::new(), session: Mutex::new(Session { enabled: false, port: 0, urls: Vec::new(), servers: Vec::new(), service: None }) }
    }

    pub fn status(&self) -> Status {
        let s = self.session.lock().unwrap();
        self.status_locked(&s)
    }

    pub fn revoke(&self, device: Option<u64>) -> rusqlite::Result<Status> {
        let s = self.session.lock().unwrap();
        self.gw.revoke(device)?;
        Ok(self.status_locked(&s))
    }

    pub fn note_asr(&self, episode: &str, value: Value) { self.gw.note_asr(episode, value); }

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
        storage: Option<&Path>,
    ) -> rusqlite::Result<Status> {
        let mut s = self.session.lock().unwrap();
        if s.enabled { return Ok(self.status_locked(&s)); }
        let family = if family == 0 { PORT_FAMILY } else { family };
        let mut chosen: Option<(u16, Vec<(Ipv4Addr, TcpListener)>)> = None;
        if port == 0 {
            chosen = bind_port(0, &targets);
        } else {
            for p in port..port.saturating_add(family) {
                if p == ORIGIN_PORT { continue; }
                if let Some(found) = bind_port(p, &targets) { chosen = Some(found); break; }
            }
        }
        let Some((port, listeners)) = chosen.filter(|(port, _)| *port != ORIGIN_PORT) else {
            // 一个地址都绑不上：保持关闭，让界面把原因说清楚。
            return Ok(self.status_locked(&s));
        };
        if let Some(path) = storage { self.gw.open_device_store(path)?; }
        let servers: Vec<_> = listeners.into_iter().filter_map(|(ip, listener)|
            self.start_listener(ip, listener, &files, &commands, None)
        ).collect();
        if servers.is_empty() { return Ok(self.status_locked(&s)); }
        self.gw.enable();
        s.enabled = true;
        s.port = port;
        s.servers = servers;
        s.service = Some((files, commands));
        Self::update_urls(&mut s);
        Ok(self.status_locked(&s))
    }

    pub fn enable_https(
        &self, port: u16, access: Arc<AccessVerifier>, files: Arc<dyn Files>,
        commands: Arc<dyn Commands>, storage: Option<&Path>,
    ) -> Result<Status, &'static str> {
        let mut session = self.session.lock().unwrap();
        if session.enabled { return Err("phone_access_already_enabled"); }
        let address = Ipv4Addr::LOCALHOST;
        let listener = TcpListener::bind((address, port)).map_err(|_| "access_port_in_use")?;
        let port = listener.local_addr().map_err(|_| "access_port_in_use")?.port();
        if let Some(path) = storage { self.gw.open_device_store(path).map_err(|_| "access_store_unavailable")?; }
        let url = access.settings().public_url.clone();
        let handle = self.start_listener(address, listener, &files, &commands, Some(access))
            .ok_or("access_port_in_use")?;
        self.gw.activate(true);
        session.enabled = true;
        session.port = port;
        session.servers = vec![handle];
        session.urls = vec![Url { kind: "https", url }];
        Ok(self.status_locked(&session))
    }

    fn start_listener(
        &self, ip: Ipv4Addr, listener: TcpListener, files: &Arc<dyn Files>, commands: &Arc<dyn Commands>,
        access: Option<Arc<AccessVerifier>>,
    ) -> Option<ServerHandle> {
        let server = Arc::new(tiny_http::Server::from_listener(listener, None).ok()?);
        let stop = Arc::new(AtomicBool::new(false));
        let (gw, files, commands, stop2, server2) = (self.gw.clone(), files.clone(), commands.clone(), stop.clone(), server.clone());
        std::thread::spawn(move || serve(server2, stop2, gw, files, commands, access));
        Some(ServerHandle { ip, server, stop })
    }

    pub fn refresh_addresses(&self, targets: &[Ipv4Addr]) {
        let mut s = self.session.lock().unwrap();
        let Some((files, commands)) = s.service.clone() else { return; };
        s.servers.retain(|server| {
            if server.ip.is_loopback() || targets.contains(&server.ip) { return true; }
            server.stop.store(true, Ordering::Release);
            server.server.unblock();
            false
        });
        for &ip in targets {
            if s.servers.iter().any(|server| server.ip == ip) { continue; }
            if let Ok(listener) = TcpListener::bind((ip, s.port)) {
                if let Some(handle) = self.start_listener(ip, listener, &files, &commands, None) { s.servers.push(handle); }
            }
        }
        Self::update_urls(&mut s);
    }

    fn update_urls(s: &mut Session) {
        let mut ips: Vec<_> = s.servers.iter().map(|server| server.ip).collect();
        if !ips.contains(&Ipv4Addr::LOCALHOST) { ips.push(Ipv4Addr::LOCALHOST); }
        ips.sort_by_key(|ip| (ip.is_loopback(), is_tailscale(*ip), *ip));
        s.urls = ips.into_iter().map(|ip| Url {
            kind: if ip.is_loopback() { "loopback" } else if is_tailscale(ip) { "tailscale" } else { "lan" },
            url: format!("http://{ip}:{}", s.port),
        }).collect();
    }

    /// 关闭：停止监听（先 503 再断开）并吊销全部令牌。
    pub fn disable(&self) -> rusqlite::Result<Status> {
        let mut s = self.session.lock().unwrap();
        let result = self.gw.disable();
        for h in s.servers.drain(..) {
            h.stop.store(true, Ordering::Relaxed);
            h.server.unblock();
        }
        s.enabled = false;
        s.service = None;
        s.port = 0;
        s.urls.clear();
        result.map(|_| self.status_locked(&s))
    }

    /// 重新生成配对码；没打开时什么也不做。
    pub fn regenerate(&self) -> Status {
        let s = self.session.lock().unwrap();
        if s.enabled { self.gw.regenerate(); }
        self.status_locked(&s)
    }

    fn status_locked(&self, s: &Session) -> Status {
        let g = self.gw.status();
        Status {
            enabled: s.enabled,
            port: s.port,
            urls: s.urls.clone(),
            code: g.code,
            code_expires_at: g.code_expires_at,
            devices: g.devices,
            paired_devices: g.paired_devices,
            viewers: g.viewers,
            now_playing: g.now_playing,
        }
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
        if !ip.is_private() && !is_tailscale(ip) { return; }
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

    /// 记录收到的命令和参数，用来验证网关把参数原样交给了命令。
    #[derive(Default)]
    struct FakeCommands { allowed: bool, seen: Mutex<Vec<(String, Value)>> }
    impl FakeCommands {
        fn new(allowed: bool) -> Self { Self { allowed, ..Default::default() } }
    }
    impl Commands for FakeCommands {
        fn call(&self, command: &str, args: Value) -> Result<Value, String> {
            self.seen.lock().unwrap().push((command.to_string(), args.clone()));
            match command {
                "list_shows" => Ok(json!([{"id": "s1", "title": "节目一"}])),
                "list_episodes" => Ok(json!([{"id": "e1", "hasTranscript": true}])),
                "get_episode" => Ok(json!({"id": "e1", "title": "第一期", "audioUrl": "https://cdn.example/e1.mp3", "hasTranscript": true})),
                "load_transcript" => Ok(json!({"episodeId": "e1", "source": "rss", "language": "en", "segments": [{"start": 0.0, "end": 2.0, "text": "hello"}]})),
                "reading_hints" => Ok(json!({"offline": false, "zh": ["你好"]})),
                _ if self.allowed => Ok(json!(null)),
                _ => Err("forbidden".into()),
            }
        }
    }

    fn pair_req(code: &str) -> HttpIn {
        HttpIn { method: "POST".into(), path: "/pair".into(), authorization: None, access: false, body: serde_json::to_vec(&json!({"code": code, "name": "iPhone"})).unwrap() }
    }
    fn rpc_req(command: &str, token: Option<&str>) -> HttpIn {
        HttpIn { method: "POST".into(), path: format!("/rpc/{command}"), authorization: token.map(|t| format!("Bearer {t}")), access: false, body: b"{}".to_vec() }
    }
    fn rpc_args_req(command: &str, token: Option<&str>, args: &Value) -> HttpIn {
        HttpIn { method: "POST".into(), path: format!("/rpc/{command}"), authorization: token.map(|t| format!("Bearer {t}")), access: false, body: serde_json::to_vec(args).unwrap() }
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
        gw.disable().unwrap();
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
        gw.disable().unwrap();
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
        let cmds = FakeCommands::new(false);
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
    fn phone_whitelist_reads_one_episode_and_writes_only_progress() {
        // 打开一期要用的全是只读命令：订阅、单集列表、单集详情、字幕、播放提示。
        for name in ["list_shows", "list_episodes", "get_episode", "load_transcript", "reading_hints"] {
            assert!(ALLOWED.contains(&name), "{name} 是手机打开一期必需的只读命令，应该放行");
        }
        for name in ["set_progress", "set_watched"] {
            assert!(ALLOWED.contains(&name), "{name} 是 issue #4 允许手机回写的命令");
        }
        // 其它写入、删除、本地文件、登录、设置、Key、订阅、下载一类命令一个都不能出现。
        // 每次扩白名单都要在这里逐条列出仍然被拒的命令。
        for name in [
            "add_subscription", "refresh_show", "refresh_all", "remove_show",
            "mark_show_watched", "set_settings", "set_api_key", "set_blob", "transcribe", "cancel_job",
            "fetch_official_transcript", "translate_texts", "download_episode", "delete_download",
            "clear_downloads", "open_data_folder", "reveal_download", "reveal_file", "open_privacy_settings",
            "youtube_sign_in", "youtube_sign_out", "save_image", "save_text", "export_anki", "share_guide",
            "share_translation", "align_audio", "save_time_map", "clear_audio_cache", "clear_temp",
            "get_settings", "has_api_key", "log_listen", "shadow_start", "shadow_stop",
        ] {
            assert!(!ALLOWED.contains(&name), "{name} 是写入 / 设置 / 登录 / 文件类命令，不能放行给手机");
        }
    }

    #[test]
    fn rpc_hands_episode_and_subtitle_arguments_through() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(false);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;

        let cases = [
            ("list_episodes", json!({"showId": "s1"})),
            ("get_episode", json!({"id": "e1"})),
            ("load_transcript", json!({"episodeId": "e1"})),
            ("reading_hints", json!({"episodeId": "e1"})),
        ];
        for (command, args) in cases {
            let r = route(&gw, &files, &cmds, &rpc_args_req(command, Some(&token), &args));
            assert_eq!(r.status, 200, "{command}: {}", String::from_utf8_lossy(&r.body));
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            assert!(body.get("result").is_some(), "{command} 的结果要原样返回");
        }
        // 参数确实原样交给了命令（手机端传的是 camelCase）。
        let seen = cmds.seen.lock().unwrap();
        let names: Vec<&str> = seen.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["list_episodes", "get_episode", "load_transcript", "reading_hints"]);
        assert_eq!(seen[0].1, json!({"showId": "s1"}));
        assert_eq!(seen[1].1, json!({"id": "e1"}));
        assert_eq!(seen[2].1, json!({"episodeId": "e1"}));
        assert_eq!(seen[3].1, json!({"episodeId": "e1"}));
    }

    #[test]
    fn other_write_commands_are_403_even_with_a_valid_token() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true); // 桌面端「有这个命令」也不影响网关的判断
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        // 进度 / 已看状态之外，写入类命令逐条列出来断言仍然是 403。
        for command in [
            "remove_show", "add_subscription", "mark_show_watched", "set_settings", "set_api_key",
            "set_blob", "delete_download", "clear_downloads", "download_episode", "transcribe",
            "youtube_sign_in", "save_image", "open_data_folder", "reveal_file",
        ] {
            let r = route(&gw, &files, &cmds, &rpc_args_req(command, Some(&token), &json!({"id": "e1"})));
            assert_eq!(r.status, 403, "{command} 必须 403");
            assert!(!String::from_utf8_lossy(&r.body).contains("token"));
        }
        // 这些请求根本没到命令层。
        assert!(cmds.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn progress_writes_pass_and_are_remembered_for_the_desktop() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        // 手机端传的是 camelCase：{ id, seconds }。
        let r = route(&gw, &files, &cmds, &rpc_args_req("set_progress", Some(&token), &json!({"id": "e1", "seconds": 42.5, "playing": true})));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        assert!(cmds.seen.lock().unwrap().iter().any(|(n, a)| n == "set_progress" && a["seconds"] == json!(42.5)));
        // 桌面端从状态里看到「哪台设备在放什么」。
        let st = gw.status();
        assert_eq!(st.viewers, 1);
        let np = st.now_playing.expect("回写成功后应该记下正在播放的单集");
        assert_eq!(np.episode_id, "e1");
        assert_eq!(np.seconds, 42.5);
        // 再回写一次，位置跟着变。
        let r = route(&gw, &files, &cmds, &rpc_args_req("set_progress", Some(&token), &json!({"id": "e1", "seconds": 57.0, "playing": true})));
        assert_eq!(r.status, 200);
        assert_eq!(gw.status().now_playing.unwrap().seconds, 57.0);
        let r = route(&gw, &files, &cmds, &rpc_args_req("set_progress", Some(&token), &json!({"id": "e1", "seconds": 58.0, "playing": false})));
        assert_eq!(r.status, 200);
        assert!(gw.status().now_playing.is_none(), "a paused phone is viewing, but no longer playing");
        assert_eq!(gw.status().viewers, 1);
    }

    #[test]
    fn watched_writes_pass_with_camel_case_arguments() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        for watched in [json!("done"), json!("unseen"), Value::Null] {
            let args = json!({"id": "e1", "watched": watched});
            let r = route(&gw, &files, &cmds, &rpc_args_req("set_watched", Some(&token), &args));
            assert_eq!(r.status, 200, "{}: {}", watched, String::from_utf8_lossy(&r.body));
        }
        let seen = cmds.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|(n, _)| n == "set_watched"));
        assert_eq!(seen[0].1, json!({"id": "e1", "watched": "done"}));
    }

    #[test]
    fn viewers_and_now_playing_expire_after_a_minute() {
        let (clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        let r = route(&gw, &files, &cmds, &rpc_args_req("set_progress", Some(&token), &json!({"id": "e1", "seconds": 10.0, "playing": true})));
        assert_eq!(r.status, 200);
        assert_eq!(gw.status().viewers, 1);
        advance(&clock, VIEWER_TTL_MS - 1);
        assert_eq!(gw.status().viewers, 1, "一分钟内算还在看");
        advance(&clock, 1);
        let st = gw.status();
        assert_eq!(st.viewers, 0, "超过一分钟没有请求就不再算在查看");
        assert!(st.now_playing.is_none(), "太久的回写不再当作正在播放");
        assert_eq!(st.devices, 1, "设备还在，只是没在看了");
    }

    #[test]
    fn two_devices_are_two_viewers_and_closing_clears_presence() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        let c1 = gw.enable().code.unwrap();
        let t1 = gw.pair(&c1, "iPhone").unwrap().token;
        let c2 = gw.regenerate().code.unwrap();
        let t2 = gw.pair(&c2, "iPad").unwrap().token;
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&t1))).status, 200);
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&t2))).status, 200);
        assert_eq!(gw.status().viewers, 2);
        gw.disable().unwrap();
        let st = gw.status();
        assert_eq!(st.viewers, 0);
        assert!(st.now_playing.is_none());
    }

    #[test]
    fn forged_token_never_becomes_a_viewer() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        gw.enable();
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some("nope"))).status, 401);
        assert_eq!(gw.status().viewers, 0);
    }

    #[test]
    fn status_serializes_presence_with_camel_case_fields() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(true);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        let _ = route(&gw, &files, &cmds, &rpc_args_req("set_progress", Some(&token), &json!({"id": "e1", "seconds": 12.0, "playing": true})));
        // 桌面端读的是 camelCase。
        let st = serde_json::to_value(gw.status()).unwrap();
        assert_eq!(st["viewers"], 1);
        assert_eq!(st["nowPlaying"]["episodeId"], "e1");
        assert_eq!(st["nowPlaying"]["seconds"], 12.0);
        assert!(st["nowPlaying"].get("updatedAt").is_some());
        let np = serde_json::to_value(gw.status().now_playing.unwrap()).unwrap();
        assert_eq!(np["episodeId"], "e1");
        assert_eq!(np["seconds"], 12.0);
        assert!(np.get("updatedAt").is_some());
    }

    #[test]
    fn reading_hints_flag_offline_downloads_and_the_translation() {
        // 下载过的音频在手机上放不了；中文译文和原文分段一一对应（可能有没有译文的行）。
        let h = reading_hints(true, Some(r#"[null,"你好"]"#));
        assert_eq!(h["offline"], true);
        assert_eq!(h["zh"], json!([null, "你好"]));
        // 没下载、没译文：页面只显示原文。
        let h = reading_hints(false, None);
        assert_eq!(h["offline"], false);
        assert!(h["zh"].is_null());
        // 坏掉的 blob 当没有，别让整页打不开。
        let h = reading_hints(false, Some("not json"));
        assert!(h["zh"].is_null());
    }

    #[test]
    fn closing_the_switch_makes_old_tokens_401() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(false);
        let code = gw.enable().code.unwrap();
        let token = gw.pair(&code, "iPhone").unwrap().token;
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&token))).status, 200);
        gw.disable().unwrap();
        assert_eq!(route(&gw, &files, &cmds, &rpc_req("list_shows", Some(&token))).status, 401);
        assert_eq!(route(&gw, &files, &cmds, &pair_req(&code)).status, 503);
    }

    #[test]
    fn pair_errors_are_told_apart() {
        let (_clock, gw) = new_gw();
        let files = NoFiles;
        let cmds = FakeCommands::new(false);
        let code = gw.enable().code.unwrap();
        // 码错
        let r = route(&gw, &files, &cmds, &pair_req("999999"));
        assert_eq!(r.status, 401);
        assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap()["error"], "invalid_code");
        // 已关闭
        gw.disable().unwrap();
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
        let cmds = FakeCommands::new(false);
        let get = |path: &str| HttpIn { method: "GET".into(), path: path.into(), authorization: None, access: false, body: vec![] };
        let r = route(&gw, &files, &cmds, &get("/"));
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"<html>ok</html>");
        assert_eq!(route(&gw, &files, &cmds, &get("/logo.png")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &get("/../secret")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &get("/%2e%2e/secret")).status, 404);
        assert_eq!(route(&gw, &files, &cmds, &HttpIn { method: "PUT".into(), path: "/".into(), authorization: None, access: false, body: vec![] }).status, 404);
    }

    #[test]
    fn device_registry_retains_active_pairing_until_explicitly_disabled() {
        let path = std::env::temp_dir().join(format!("subcast-devices-{}.db", uuid::Uuid::new_v4()));
        let token = {
            let (_, gateway) = new_gw();
            gateway.open_device_store(&path).unwrap();
            let pair = gateway.pair(&gateway.enable().code.unwrap(), "iPhone").unwrap();
            let store = Connection::open(&path).unwrap();
            let stored: String = store.query_row("SELECT token_hash FROM remote_devices", [], |row| row.get(0)).unwrap();
            assert_ne!(stored, pair.token);
            assert_eq!(stored, sha256_hex(&pair.token));
            pair.token
        };
        let (_, gateway) = new_gw();
        gateway.open_device_store(&path).unwrap();
        assert!(!gateway.authorize(&token));
        gateway.enable();
        assert!(gateway.authorize(&token));
        gateway.disable().unwrap();
        gateway.open_device_store(&path).unwrap();
        gateway.enable();
        assert!(!gateway.authorize(&token));
        drop(gateway);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn revoking_a_device_keeps_the_other_phone_connected() {
        let (_clock, gateway) = new_gw();
        let one = gateway.pair(&gateway.enable().code.unwrap(), "iPhone").unwrap().token;
        let two = gateway.pair(&gateway.regenerate().code.unwrap(), "Android").unwrap().token;
        let first = gateway.status().paired_devices[0].id;
        gateway.revoke(Some(first)).unwrap();
        let commands = FakeCommands::new(false);
        assert_eq!(route(&gateway, &NoFiles, &commands, &rpc_req("phone_presence", Some(&one))).status, 401);
        assert_eq!(route(&gateway, &NoFiles, &commands, &rpc_req("phone_presence", Some(&two))).status, 200);
        let remaining = gateway.status().paired_devices;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].label, "Android");
        assert!(remaining[0].last_seen.is_some());
        gateway.revoke(None).unwrap();
        assert_eq!(route(&gateway, &NoFiles, &commands, &rpc_req("phone_presence", Some(&two))).status, 401);
    }

    struct ChatCommands(Mutex<Option<Arc<Answer>>>);

    impl Commands for ChatCommands {
        fn call(&self, _: &str, _: Value) -> Result<Value, String> { Err("forbidden".to_owned()) }

        fn ask(&self, _: &str, _: &str, answer: Arc<Answer>) -> Result<(), &'static str> {
            answer.push("第一段");
            *self.0.lock().unwrap() = Some(answer);
            Ok(())
        }
    }

    #[test]
    fn answer_streams_before_completion_and_reconnects_only_for_its_owner() {
        use std::io::{BufRead, BufReader, Write};
        let remote = Remote::new();
        let commands = Arc::new(ChatCommands(Mutex::new(None)));
        let status = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], Arc::new(NoFiles), commands.clone(), None).unwrap();
        let port = status.port;
        let owner = remote.gw.pair(&status.code.unwrap(), "owner").unwrap().token;
        let other = remote.gw.pair(&remote.regenerate().code.unwrap(), "other").unwrap().token;
        let args = r#"{"episodeId":"e1","question":"What was the main point?"}"#;
        let (status, body) = http_post(port, "/rpc/ask_episode", Some(&owner), args);
        assert_eq!(status, 200, "{body}");
        let response: Value = serde_json::from_str(&body).unwrap();
        let request = response["result"].as_str().unwrap();
        assert_eq!(http_post(port, "/rpc/ask_episode", Some(&owner), args).0, 429);

        let open = |token: &str, after: u64, client_header: &str| {
            let mut socket = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            write!(socket, "GET /events?episodeId=e1&requestId={request}&after={after} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nReferer: http://127.0.0.1:{port}/\r\n{client_header}Authorization: Bearer {token}\r\nConnection: close\r\n\r\n").unwrap();
            BufReader::new(socket)
        };
        let header = "X-Subcast-Client: phone\r\n";
        let mut wrong_phone = open(&other, 0, header);
        let mut status_line = String::new();
        wrong_phone.read_line(&mut status_line).unwrap();
        assert!(status_line.contains("404"), "{status_line}");
        status_line.clear();
        open(&owner, 0, "").read_line(&mut status_line).unwrap();
        assert!(status_line.contains("403"), "{status_line}");

        let mut owned = open(&owner, 0, header);
        let mut received = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(owned.read_line(&mut line).unwrap(), 0);
            received.push_str(&line);
            if received.contains("第一段") { break; }
        }
        assert!(received.contains("event: llm") && received.contains(r#""done":false"#));
        drop(owned);
        commands.0.lock().unwrap().as_ref().unwrap().finish(Some("第一段，结束。".into()));
        let mut replay = String::new();
        let mut reconnected = open(&owner, 1, header);
        let mut line = String::new();
        loop {
            line.clear();
            if reconnected.read_line(&mut line).unwrap_or(0) == 0 { break; }
            replay.push_str(&line);
            if replay.ends_with("0\r\n\r\n") { break; }
        }
        assert!(replay.contains("第一段，结束。") && replay.contains(r#""done":true"#));
        assert!(!replay.contains("id: 1"));
        remote.disable().unwrap();
    }

    #[test]
    fn failed_https_revocation_is_retried_only_for_its_own_device_store() {
        let dir = std::env::temp_dir().join(format!("subcast-access-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let lan = dir.join("lan.db");
        let https = dir.join("https.db");

        let (_, other) = new_gw();
        other.open_device_store(&lan).unwrap();
        let lan_code = other.enable().code.unwrap();
        let lan_token = other.pair(&lan_code, "Existing LAN device").unwrap().token;
        drop(other);

        let (_, gateway) = new_gw();
        gateway.open_device_store(&https).unwrap();
        let https_code = gateway.activate(true).code.unwrap();
        let https_token = gateway.pair_for(&https_code, "HTTPS device", true).unwrap().token;
        let db = Connection::open(&https).unwrap();
        db.execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON remote_devices
            BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;").unwrap();
        assert!(gateway.disable().is_err());

        gateway.open_device_store(&lan).unwrap();
        gateway.enable();
        assert!(gateway.authorize(&lan_token));
        assert!(!gateway.authorize(&https_token));
        gateway.disable().unwrap();

        db.execute_batch("DROP TRIGGER reject_delete;").unwrap();
        gateway.open_device_store(&https).unwrap();
        gateway.activate(true);
        assert!(gateway.authorize_device(&https_token, true).is_none());
        gateway.disable().unwrap();
        drop(db);
        std::fs::remove_file(lan).unwrap();
        std::fs::remove_file(https).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn lan_never_binds_the_reserved_access_origin_port() {
        let remote = Remote::new();
        let status = remote.enable(ORIGIN_PORT, 2, vec![Ipv4Addr::LOCALHOST],
            Arc::new(NoFiles), Arc::new(FakeCommands::new(false)), None).unwrap();
        assert_ne!(status.port, ORIGIN_PORT);
        remote.disable().unwrap();
    }

    #[test]
    fn tunnel_origin_checks_access_before_assets_pairing_rpc_and_events() {
        use crate::remote_access::tests::{fixture, settings};

        let data = fixture();
        let access = AccessVerifier::with_test_keys(settings(), data["jwks"].clone());
        let remote = Remote::new();
        let commands = Arc::new(FakeCommands::new(false));
        let status = remote.enable_https(0, access, Arc::new(NoFiles), commands.clone(), None).unwrap();
        let port = status.port;
        assert_eq!(status.urls, vec![Url { kind: "https", url: "https://subcast.example.com".into() }]);
        let valid = data["valid"].as_str().unwrap();
        let expired = data["expired"].as_str().unwrap();
        let request = |method, path, extras: &str, body: &str| {
            exchange_http(port, &format!("{method} {path} HTTP/1.1\r\nHost: subcast.example.com\r\n{extras}Connection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
        };

        for (method, path) in [("GET", "/"), ("POST", "/pair"), ("POST", "/rpc/list_shows"), ("GET", "/events?episodeId=e1")] {
            let (code, body) = request(method, path, "", "");
            assert_eq!(code, 401, "{method} {path}");
            assert!(body.contains("access_required"));
        }
        assert_eq!(request("GET", "/", &format!("Cf-Access-Jwt-Assertion: {expired}\r\n"), "").0, 401);
        let assertion = format!("Cf-Access-Jwt-Assertion: {valid}\r\n");
        let (code, page) = request("GET", "/", &assertion, "");
        assert_eq!(code, 200);
        assert_eq!(page, "<html>ok</html>");
        let wrong_host = format!("GET / HTTP/1.1\r\nHost: other.example.com\r\n{assertion}Connection: close\r\n\r\n");
        assert_eq!(exchange_http(port, &wrong_host).0, 403);
        let bad_origin = format!("{assertion}Origin: https://other.example.com\r\nX-Subcast-Client: phone\r\n");
        assert_eq!(request("POST", "/pair", &bad_origin, "{}").0, 403);

        let browser = format!("{assertion}Origin: https://subcast.example.com\r\nX-Subcast-Client: phone\r\n");
        let body = json!({"code": status.code.unwrap(), "name": "Test browser"}).to_string();
        let (code, paired) = request("POST", "/pair", &browser, &body);
        assert_eq!(code, 200);
        let paired: Value = serde_json::from_str(&paired).unwrap();
        let device = paired["token"].as_str().unwrap();
        assert_eq!(request("POST", "/rpc/list_shows", &browser, "{}").0, 401);
        let headers = format!("{browser}Authorization: Bearer {device}\r\n");
        assert_eq!(request("POST", "/rpc/list_shows", &headers, "{}").0, 200);
        assert_eq!(request("POST", "/rpc/get_settings", &headers, "{}").0, 403);

        // The authenticated event connection uses the same identity and device boundaries as RPC.
        use std::io::{BufRead, BufReader, Write};
        remote.note_asr("e1", json!({"episodeId": "e1", "stage": "transcribe", "progress": 0.5}));
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        write!(stream, "GET /events?episodeId=e1 HTTP/1.1\r\nHost: subcast.example.com\r\n{headers}Connection: close\r\n\r\n").unwrap();
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        while !response.contains(r#""progress":0.5"#) {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            response.push_str(&line);
        }
        assert!(response.contains("Content-Type: text/event-stream") && response.contains("event: asr"));
        drop(reader);

        // A device token from the HTTPS origin does not authorize the regular LAN/desktop HTTP endpoint.
        assert_eq!(route(&remote.gw, &NoFiles, &*commands, &rpc_req("list_shows", Some(device))).status, 401);
        remote.refresh_addresses(&["192.0.2.1".parse().unwrap()]);
        assert_eq!(remote.status().urls[0].kind, "https");
        remote.disable().unwrap();
        assert!(wait_port_closed(port));
    }

    #[test]
    fn enable_and_disable_over_real_http() {
        let files: Arc<dyn Files> = Arc::new(NoFiles);
        let cmds: Arc<dyn Commands> = Arc::new(FakeCommands::new(false));
        let remote = Remote::new();
        let st = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files.clone(), cmds.clone(), None).unwrap();
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
        remote.disable().unwrap();
        assert!(wait_port_closed(port), "关闭后端口不再接受连接");
    }

    #[test]
    fn enable_is_idempotent_and_regenerate_changes_the_code() {
        let files: Arc<dyn Files> = Arc::new(NoFiles);
        let cmds: Arc<dyn Commands> = Arc::new(FakeCommands::new(false));
        let remote = Remote::new();
        let a = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files.clone(), cmds.clone(), None).unwrap();
        let b = remote.enable(0, PORT_FAMILY, vec![Ipv4Addr::LOCALHOST], files, cmds, None).unwrap();
        assert_eq!(a.port, b.port, "打开两次不会换端口");
        let c = remote.regenerate();
        assert_ne!(a.code, c.code, "重新生成会换个码");
        assert_eq!(c.code.as_ref().map(String::len), Some(6));
        remote.disable().unwrap();
        assert!(!remote.status().enabled);
        assert_eq!(remote.status().urls.len(), 0);
    }

    /// 极简 HTTP 客户端：测试里不想为了一个 POST 再拉依赖。
    fn http_post(port: u16, path: &str, bearer: Option<&str>, body: &str) -> (u16, String) {
        let auth = bearer.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let req = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Subcast-Client: phone\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        exchange_http(port, &req)
    }

    fn exchange_http(port: u16, req: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
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
