import { useEffect, useRef, useState, type FormEvent } from "react";
import { api, deviceToken, setDeviceToken, pairDevice, PairError, Unauthorized, type PairFailure, type Show } from "../lib/api";
import { t } from "../lib/i18n";
import "./phone.css";

/** Only this page exists on a phone in T1: pair, then read the subscription list. */

const failureText = (reason: PairFailure): string => {
  switch (reason) {
    case "invalid": return t("配对码不对，请重新输入");
    case "expired": return t("配对码已过期，请在电脑上重新生成");
    case "closed": return t("手机访问已关闭，请在电脑上打开「手机访问」开关");
    case "locked": return t("试错的次数太多，请等一分钟再试");
    case "network": return t("连不上电脑，请确认手机和电脑在同一个网络，且电脑上的「手机访问」已打开");
    default: return t("配对失败，请再试一次");
  }
};

function PairPage({ onPaired }: { onPaired: () => void }) {
  const [code, setCode] = useState(() => (new URLSearchParams(location.search).get("code") ?? "").replace(/\D/g, "").slice(0, 6));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<PairFailure | null>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => { input.current?.focus(); }, []);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (code.length !== 6 || busy) return;
    setBusy(true);
    setFailure(null);
    try {
      await pairDevice(code);
      onPaired();
    } catch (err) {
      setFailure(err instanceof PairError ? err.reason : "unknown");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="phone">
      <div className="ph-pair">
        <p className="ph-brand">SubCast</p>
        <h1 className="ph-title">{t("输入配对码")}</h1>
        <p className="ph-muted">{t("在电脑的 SubCast 里打开「设置 → 通用 → 手机访问」，把那里显示的 6 位配对码填在这里。")}</p>
        <form onSubmit={submit} className="ph-form">
          <input
            ref={input}
            className="ph-code-input"
            value={code}
            inputMode="numeric"
            pattern="[0-9]*"
            autoComplete="one-time-code"
            maxLength={6}
            placeholder="000000"
            aria-label={t("配对码")}
            onChange={(e) => { setCode(e.target.value.replace(/\D/g, "").slice(0, 6)); setFailure(null); }}
          />
          <button className="ph-btn primary" type="submit" disabled={busy || code.length !== 6}>{busy ? t("正在配对…") : t("配对")}</button>
        </form>
        {failure && <p className="ph-error" role="alert">{failureText(failure)}</p>}
        <p className="ph-hint">{t("配对码 5 分钟内有效，用过一次就失效。令牌保存在这台手机里，7 天内刷新不用再配对。")}</p>
      </div>
    </div>
  );
}

function ShowRow({ show }: { show: Show }) {
  const [broken, setBroken] = useState(false);
  return (
    <li className="ph-show">
      {show.cover && !broken
        ? <img className="ph-cover" src={show.cover} alt="" loading="lazy" onError={() => setBroken(true)} />
        : <span className="ph-cover ph-cover-empty">{(show.title || "?").slice(0, 1)}</span>}
      <div className="ph-show-text">
        <p className="ph-show-title">{show.title}</p>
        <p className="ph-muted ph-show-meta">
          {show.author ?? ""}
          {show.unplayed > 0 && <span className="ph-badge">{t("{n} 集未听", { n: show.unplayed })}</span>}
        </p>
      </div>
    </li>
  );
}

function ShowsPage({ onUnpair }: { onUnpair: () => void }) {
  const [shows, setShows] = useState<Show[] | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const load = async () => {
    setBusy(true);
    setError("");
    try {
      setShows(await api.listShows());
    } catch (e) {
      if (e instanceof Unauthorized) { setDeviceToken(null); onUnpair(); return; }
      setError(t("加载失败，请稍后再试"));
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => { void load(); }, []);

  return (
    <div className="phone ph-list-page">
      <header className="ph-head">
        <div>
          <p className="ph-brand">SubCast</p>
          <h1 className="ph-title sm">{t("我的订阅")}</h1>
        </div>
        <div className="ph-head-actions">
          <button className="ph-btn ghost" onClick={() => void load()} disabled={busy}>{busy ? t("刷新中…") : t("刷新")}</button>
          <button className="ph-btn ghost" onClick={() => { setDeviceToken(null); onUnpair(); }}>{t("解除配对")}</button>
        </div>
      </header>
      <p className="ph-note">{t("只读：这个版本手机端只能看订阅列表。")}</p>
      {error && <p className="ph-error">{error}</p>}
      {shows == null && !error && <p className="ph-muted ph-loading">{t("加载中…")}</p>}
      {shows != null && shows.length === 0 && <p className="ph-muted ph-loading">{t("电脑上还没有订阅。")}</p>}
      {shows != null && shows.length > 0 && <ul className="ph-shows">{shows.map((s) => <ShowRow key={s.id} show={s} />)}</ul>}
    </div>
  );
}

export function PhoneApp() {
  const [paired, setPaired] = useState(() => !!deviceToken());
  const pairedNow = () => {
    // 配对码别留在浏览历史里。
    try { history.replaceState(null, "", location.pathname); } catch { /* fine */ }
    setPaired(true);
  };
  return paired
    ? <ShowsPage onUnpair={() => setPaired(false)} />
    : <PairPage onPaired={pairedNow} />;
}
