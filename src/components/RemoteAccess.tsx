import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { api, errorText, type RemoteHttpsConfig, type RemoteStatus, type RemoteUrl } from "../lib/api";
import { ask } from "../lib/confirm";
import { clock as formatClock } from "../lib/format";
import { t } from "../lib/i18n";

const HTTPS_ORIGIN = "http://127.0.0.1:14347";
const EMPTY_HTTPS: RemoteHttpsConfig = { publicUrl: "", teamName: "", audience: "", email: "" };

function accessError(err: unknown): string {
  switch (String(err)) {
    case "invalid_access_config": return t("请检查 HTTPS 地址、团队名称、应用 AUD 和允许的邮箱。");
    case "access_keys_unavailable": return t("无法读取 Access 公钥，请检查团队名称和 Mac 的网络。");
    case "access_port_in_use": return t("HTTPS 回环端口 14347 暂不可用。");
    case "access_store_unavailable": return t("无法打开 HTTPS 设备记录。");
    case "phone_access_already_enabled": return t("请先关闭当前的手机访问。");
    default: return errorText(err);
  }
}

/** 设置 › 通用 › 手机访问：默认关闭；打开后显示地址、配对码和二维码。 */
export function RemoteAccess() {
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [useHttps, setUseHttps] = useState(false);
  const [config, setConfig] = useState(EMPTY_HTTPS);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [episodeTitle, setEpisodeTitle] = useState("");
  const [qr, setQr] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  const refresh = async () => { try { setStatus(await api.remoteStatus()); } catch (e) { setError(errorText(e)); } };
  useEffect(() => {
    let live = true;
    void refresh();
    void api.remoteHttpsConfig().then((saved) => {
      if (live && saved) setConfig(saved);
    }).catch(() => {});
    return () => { live = false; };
  }, []);

  const enabled = !!status?.enabled;
  const isHttps = enabled ? status?.urls.some((entry) => entry.kind === "https") : useHttps;
  // 只在使用中才轮询：配对成功后配对码会被用掉，界面要跟着变。
  useEffect(() => {
    if (!enabled) return;
    const clock = window.setInterval(() => setNow(Date.now()), 1000);
    const poll = window.setInterval(() => { void refresh(); }, 3000);
    return () => { window.clearInterval(clock); window.clearInterval(poll); };
  }, [enabled]);

  const playingId = status?.nowPlaying?.episodeId;
  useEffect(() => {
    let live = true;
    setEpisodeTitle("");
    if (playingId) void api.getEpisode(playingId).then((episode) => {
      if (live) setEpisodeTitle(episode?.title ?? "");
    }).catch(() => {});
    return () => { live = false; };
  }, [playingId]);

  const firstUrl = status?.urls[0]?.url ?? "";
  const code = status?.code ?? "";
  // 二维码 = 地址 + 配对码；扫码打开会给手机页自动填好。
  const qrUrl = !enabled || !firstUrl ? ""
    : isHttps ? firstUrl
    : code ? `${firstUrl}/?code=${code}` : "";
  useEffect(() => {
    let live = true;
    if (!qrUrl) { setQr(null); return; }
    void (async () => {
      try {
        const { default: qrcode } = await import("qrcode-generator");
        const q = qrcode(0, "M");
        q.addData(qrUrl);
        q.make();
        if (live) setQr(q.createDataURL(6, 4));
      } catch { if (live) setQr(null); }
    })();
    return () => { live = false; };
  }, [qrUrl]);

  const enable = async () => {
    const ok = await ask({
      title: t("打开手机访问？"),
      message: isHttps
        ? t("允许的邮箱登录 Access 并完成配对后，可以跨网收听和提问。提问会使用这台电脑配置的模型与 Key。")
        : t("同一网络内，拿到地址和配对码的人可以查看订阅、收听和提问。提问会使用电脑配置的模型与 Key。关闭开关会让所有设备立即失去访问权限。"),
      ok: t("打开"),
    });
    if (!ok) return;
    setBusy(true);
    setError("");
    try {
      const opened = await (isHttps ? api.enableRemoteHttps(config) : api.enableRemote());
      setNow(Date.now());
      setStatus(opened);
    }
    catch (e) { setError(accessError(e)); await refresh(); }
    finally { setBusy(false); }
  };

  const disable = async () => {
    setBusy(true);
    setError("");
    try { setStatus(await api.disableRemote()); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };

  const regenerate = async () => {
    setBusy(true);
    setError("");
    try { const next = await api.regenerateRemoteCode(); setNow(Date.now()); setStatus(next); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };

  const revoke = async (id: number | null) => {
    setBusy(true);
    setError("");
    try { setStatus(await api.revokePhone(id)); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };

  const dateTime = (timestamp: number) => new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "short" }).format(timestamp);
  const seconds = code ? Math.max(0, Math.ceil((status!.codeExpiresAt - now) / 1000)) : 0;
  const mmss = `${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`;
  const urlKind = (kind: RemoteUrl["kind"]) =>
    kind === "lan" ? t("局域网") : kind === "tailscale" ? t("Tailscale") : kind === "https" ? "HTTPS" : t("本机");
  const httpsFields = [
    { key: "publicUrl", label: t("公开 HTTPS 地址"), placeholder: "https://subcast.example.com", type: "url" },
    { key: "teamName", label: t("Access 团队名称"), placeholder: "your-team", type: "text" },
    { key: "audience", label: t("Access 应用 AUD"), placeholder: "", type: "text" },
    { key: "email", label: t("允许登录的邮箱"), placeholder: "you@example.com", type: "email" },
  ] as const;

  return (
    <section className="setup-block">
      <p className="setup-title">{t("手机访问")}</p>
      <p className="muted small">{t("在手机浏览器里收听电脑上的单集、同步进度和提问。默认关闭。")}</p>
      {!enabled && (
        <>
          <label className="remote-field">
            <span>{t("手机访问方式")}</span>
            <select value={useHttps ? "https" : "lan"} disabled={busy} onChange={(event) => { setUseHttps(event.target.value === "https"); setError(""); }}>
              <option value="lan">{t("局域网或 Tailscale")}</option>
              <option value="https">{t("跨网络 HTTPS")}</option>
            </select>
          </label>
          {isHttps && (
            <div className="remote-https-fields">
              <p className="muted small">{t("使用 Cloudflare Tunnel 和 Access。将 Tunnel 的来源设为 {origin}。", { origin: HTTPS_ORIGIN })}</p>
              {httpsFields.map(({ key, label, placeholder, type }) => (
                <label className="remote-field" key={key}>
                  <span>{label}</span>
                  <input type={type} value={config[key]} placeholder={placeholder} disabled={busy}
                    onChange={(event) => setConfig({ ...config, [key]: event.target.value })} />
                </label>
              ))}
            </div>
          )}
        </>
      )}
      <label className="check">
        <input type="checkbox" checked={enabled} disabled={busy || !status || (!enabled && isHttps && Object.values(config).some((value) => !value.trim()))}
          onChange={(e) => void (e.target.checked ? enable() : disable())} />
        {isHttps ? t("允许手机通过 HTTPS 访问") : t("允许手机访问（局域网内有效）")}
      </label>
      {error && <p className="error-text small">{error}</p>}
      {enabled && status && (
        <div className="remote-panel">
          <div className="remote-code">
            {code ? (
              <>
                <p className="remote-code-digits">{code.slice(0, 3)} {code.slice(3)}</p>
                <p className="muted small">{t("配对码 {time} 后失效", { time: mmss })}</p>
              </>
            ) : (
              <p className="muted small">{t("配对码已被使用或已过期。再配一台手机请重新生成。")}</p>
            )}
            <button className="btn sm" onClick={() => void regenerate()} disabled={busy}>
              <RefreshCw size={13} /> {t("重新生成配对码")}
            </button>
          </div>
          {qr && <img className="remote-qr" src={qr} alt={t("手机访问二维码")} />}
          <ul className="remote-urls">
            {status.urls.map((u) => (
              <li key={u.url}>
                <span className="remote-url-kind">{urlKind(u.kind)}</span>
                <code>{u.url}</code>
              </li>
            ))}
          </ul>
          <p className="muted small">{isHttps
            ? t("在手机浏览器打开 HTTPS 地址，登录 Access 后输入配对码。")
            : t("手机连同一个网络，打开上面的地址；扫码会打开并自动填好配对码。")}</p>
          <p className="muted small">{t("正在被 {n} 台设备查看", { n: status.viewers })}</p>
          {status.nowPlaying && episodeTitle && <p className="muted small">{t("手机正在播放《{title}》 · {time}", { title: episodeTitle, time: formatClock(status.nowPlaying.seconds) })}</p>}
          {!isHttps && <p className="muted small">{t("离开家时，可让手机和电脑都连上 Tailscale，再打开这里的 Tailscale 地址。请勿将端口直接转发到公网。")}</p>}
          <div className="remote-devices">
            <p className="muted small">{status.devices > 0 ? t("已配对 {n} 台设备", { n: status.devices }) : t("还没有配对任何设备")}</p>
            {status.pairedDevices?.length > 0 && <>
              <ul>
                {status.pairedDevices.map((device) => <li key={device.id}>
                  <div>
                    <strong>{device.label || t("未命名设备")}</strong>
                    <p className="muted small">{t("配对于 {time}", { time: dateTime(device.pairedAt) })}
                      {device.lastSeen !== null && <> · {t("最近活跃于 {time}", { time: dateTime(device.lastSeen) })}</>}
                    </p>
                  </div>
                  <button className="btn sm" disabled={busy} aria-label={t("断开 {name}", { name: device.label || t("未命名设备") })} onClick={() => void revoke(device.id)}>{t("断开")}</button>
                </li>)}
              </ul>
              <button className="btn sm" disabled={busy} onClick={() => void revoke(null)}>{t("全部断开")}</button>
            </>}
          </div>
        </div>
      )}
    </section>
  );
}
