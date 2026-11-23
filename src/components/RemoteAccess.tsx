import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { api, errorText, type RemoteStatus } from "../lib/api";
import { ask } from "../lib/confirm";
import { t } from "../lib/i18n";

/** 设置 › 通用 › 手机访问：默认关闭；打开后显示地址、配对码和二维码。 */
export function RemoteAccess() {
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [qr, setQr] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  const refresh = async () => { try { setStatus(await api.remoteStatus()); } catch (e) { setError(errorText(e)); } };
  useEffect(() => { void refresh(); }, []);

  const enabled = !!status?.enabled;
  // 只在使用中才轮询：配对成功后配对码会被用掉，界面要跟着变。
  useEffect(() => {
    if (!enabled) return;
    const clock = window.setInterval(() => setNow(Date.now()), 1000);
    const poll = window.setInterval(() => { void refresh(); }, 3000);
    return () => { window.clearInterval(clock); window.clearInterval(poll); };
  }, [enabled]);

  const firstUrl = status?.urls[0]?.url ?? "";
  const code = status?.code ?? "";
  // 二维码 = 地址 + 配对码；扫码打开会给手机页自动填好。
  const qrUrl = enabled && code && firstUrl ? `${firstUrl}/?code=${code}` : "";
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
      message: t("同一个局域网里，拿到地址和配对码的人可以查看你的订阅列表；这个版本只开放订阅列表，以后开放收听时风险同样存在。关闭开关会立刻停止访问并让所有手机重新配对。"),
      ok: t("打开"),
    });
    if (!ok) return;
    setBusy(true);
    setError("");
    try { setStatus(await api.enableRemote()); }
    catch (e) { setError(errorText(e)); await refresh(); }
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
    try { setStatus(await api.regenerateRemoteCode()); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };

  const seconds = code ? Math.max(0, Math.ceil((status!.codeExpiresAt - now) / 1000)) : 0;
  const mmss = `${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`;
  const urlKind = (kind: "lan" | "tailscale" | "loopback") =>
    kind === "lan" ? t("局域网") : kind === "tailscale" ? t("Tailscale") : t("本机");

  return (
    <section className="setup-block">
      <p className="setup-title">{t("手机访问")}</p>
      <p className="muted small">{t("同一个 Wi-Fi 下，用手机浏览器打开电脑给出的地址，就能看订阅列表。默认关闭。")}</p>
      <label className="check">
        <input type="checkbox" checked={enabled} disabled={busy || !status} onChange={(e) => void (e.target.checked ? enable() : disable())} />
        {t("允许手机访问（局域网内有效）")}
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
          <p className="muted small">{t("手机连同一个网络，打开上面的地址；扫码会打开并自动填好配对码。")}</p>
          <p className="muted small">{status.devices > 0 ? t("已配对 {n} 台设备", { n: status.devices }) : t("还没有配对任何设备")}</p>
        </div>
      )}
    </section>
  );
}
