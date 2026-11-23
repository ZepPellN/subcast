import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type FormEvent } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { api, deviceToken, setDeviceToken, pairDevice, PairError, Unauthorized, type Episode, type PairFailure, type Show } from "../lib/api";
import { AccessRequired, getAccessLoginRequired, subscribeToAccessLogin } from "../lib/phoneFetch";
import { t } from "../lib/i18n";
import { isWatched } from "../lib/watchInfo";
import { clock, date } from "../lib/format";
import { resetPhoneProgress, syncPhoneProgress } from "./phoneProgress";
import { EpisodePage } from "./EpisodePage";
import "./phone.css";

/** The phone page (issue #2 / T1 pairing, issue #3 / T2 listening): pair once, then read the shows, open a show's
 *  episodes and listen to one with subtitles. The desktop window renders `App` instead (see main.tsx). */

const failureText = (reason: PairFailure): string => {
  switch (reason) {
    case "invalid": return t("配对码不对，请重新输入");
    case "expired": return t("配对码已过期，请在电脑上重新生成");
    case "closed": return t("手机访问已关闭，请在电脑上打开「手机访问」开关");
    case "locked": return t("试错的次数太多，请等一分钟再试");
    case "network": return location.protocol === "https:"
      ? t("连不上电脑，请确认电脑上的 HTTPS 手机访问和 Tunnel 均已开启。")
      : t("连不上电脑，请确认手机和电脑在同一个网络，且电脑上的「手机访问」已打开");
    default: return t("配对失败，请再试一次");
  }
};

function PairPage({ onPaired, revoked }: { onPaired: () => void; revoked: boolean }) {
  const [code, setCode] = useState(() => (new URLSearchParams(location.search).get("code") ?? "").replace(/\D/g, "").slice(0, 6));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<PairFailure | null>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    input.current?.focus();
    try { if (new URLSearchParams(location.search).has("code")) history.replaceState(null, "", location.pathname); } catch { /* private mode */ }
  }, []);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (code.length !== 6 || busy) return;
    setBusy(true);
    setFailure(null);
    try {
      await pairDevice(code);
      onPaired();
    } catch (err) {
      if (!(err instanceof AccessRequired)) setFailure(err instanceof PairError ? err.reason : "unknown");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="phone">
      <div className="ph-pair">
        <p className="ph-brand">SubCast</p>
        <h1 className="ph-title">{t("输入配对码")}</h1>
        {revoked && <p className="ph-error" role="alert">{t("手机访问已失效，请重新配对。")}</p>}
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

function AccessLoginPage() {
  return (
    <div className="phone">
      <div className="ph-pair">
        <p className="ph-brand">SubCast</p>
        <h1 className="ph-title">{t("需要登录 Cloudflare Access")}</h1>
        <p className="ph-muted" role="alert">{t("刷新页面以继续。已有设备配对和待同步进度会保留。")}</p>
        <div className="ph-form">
          <button className="ph-btn primary" onClick={() => location.reload()}>{t("刷新并登录")}</button>
        </div>
      </div>
    </div>
  );
}

function ShowRow({ show, onOpen }: { show: Show; onOpen: () => void }) {
  const [broken, setBroken] = useState(false);
  return (
    <li className="ph-show">
      <button className="ph-row" onClick={onOpen}>
        {show.cover && !broken
          ? <img className="ph-cover" src={show.cover} alt="" loading="lazy" onError={() => setBroken(true)} />
          : <span className="ph-cover ph-cover-empty">{(show.title || "?").slice(0, 1)}</span>}
        <span className="ph-show-text">
          <span className="ph-show-title">{show.title}</span>
          <span className="ph-muted ph-show-meta">
            {show.author ?? ""}
            {show.unplayed > 0 && <span className="ph-badge">{t("{n} 集未听", { n: show.unplayed })}</span>}
          </span>
        </span>
        <ChevronRight className="ph-chevron" size={18} />
      </button>
    </li>
  );
}

function ShowsPage({ onUnpair, onUnauthorized, onOpenShow }: { onUnpair: () => void; onUnauthorized: () => void; onOpenShow: (show: Show) => void }) {
  const [shows, setShows] = useState<Show[] | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const load = async () => {
    setBusy(true);
    setError("");
    try {
      setShows(await api.listShows());
    } catch (e) {
      if (e instanceof Unauthorized) { onUnauthorized(); return; }
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
      <p className="ph-note">{t("可以收听、同步进度和提问。手机和电脑需在同一网络，或都已连接 Tailscale。")}</p>
      {error && <p className="ph-error">{error}</p>}
      {shows == null && !error && <p className="ph-muted ph-loading">{t("加载中…")}</p>}
      {shows != null && shows.length === 0 && <p className="ph-muted ph-loading">{t("电脑上还没有订阅。")}</p>}
      {shows != null && shows.length > 0 && (
        <ul className="ph-shows">
          {shows.map((s) => <ShowRow key={s.id} show={s} onOpen={() => onOpenShow(s)} />)}
        </ul>
      )}
    </div>
  );
}

function EpisodeRow({ episode, onOpen }: { episode: Episode; onOpen: () => void }) {
  const [broken, setBroken] = useState(false);
  const meta = [date(episode.published), episode.duration ? clock(episode.duration) : ""].filter(Boolean).join(" · ");
  return (
    <li className="ph-ep-row-wrap">
      <button className="ph-ep-row" onClick={onOpen}>
        {episode.cover && !broken && <img className="ph-cover" alt="" src={episode.cover} loading="lazy" onError={() => setBroken(true)} />}
        <span className="ph-ep-row-text">
          <span className="ph-ep-row-title">{episode.title}</span>
          <span className="ph-muted ph-ep-row-meta">
            {meta}
            {isWatched(episode) && <span className="ph-badge">{t("已看")}</span>}
            <span className={`ph-badge${episode.hasTranscript ? "" : " muted"}`}>
              {episode.hasTranscript ? t("有字幕") : t("未转写")}
            </span>
          </span>
          {episode.duration && episode.progress > 0 ? (
            <span className="ph-progress" aria-hidden="true">
              <span style={{ width: `${Math.min(100, Math.max(0, (episode.progress / episode.duration) * 100))}%` }} />
            </span>
          ) : null}
        </span>
        <ChevronRight className="ph-chevron" size={18} />
      </button>
    </li>
  );
}

function EpisodesPage({ show, onOpenEpisode, onBack, onUnauthorized }: {
  show: Show;
  onOpenEpisode: (episodeId: string) => void;
  onBack: () => void;
  onUnauthorized: () => void;
}) {
  const [episodes, setEpisodes] = useState<Episode[] | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    let live = true;
    setEpisodes(null);
    setError("");
    (async () => {
      try {
        const list = await api.listEpisodes(show.id, 300);
        if (live) setEpisodes(list);
      } catch (e) {
        if (!live) return;
        if (e instanceof Unauthorized) { onUnauthorized(); return; }
        setError(t("加载失败，请稍后再试"));
      }
    })();
    return () => { live = false; };
  }, [show.id, onUnauthorized]);

  return (
    <div className="phone ph-list-page">
      <header className="ph-head ph-show-head">
        <button className="ph-back" onClick={onBack} aria-label={t("返回")}>
          <ChevronLeft size={20} />
        </button>
        <div className="ph-show-head-text">
          <h1 className="ph-title sm">{show.title}</h1>
          <p className="ph-muted ph-show-meta">{show.author ?? ""}</p>
        </div>
        {episodes && <span className="ph-muted ph-count">{t("{n} 集", { n: episodes.length })}</span>}
      </header>
      {error && <p className="ph-error">{error}</p>}
      {episodes == null && !error && <p className="ph-muted ph-loading">{t("加载中…")}</p>}
      {episodes != null && episodes.length === 0 && <p className="ph-muted ph-loading">{t("这档节目还没有单集。")}</p>}
      {episodes != null && episodes.length > 0 && (
        <ul className="ph-ep-rows">
          {episodes.map((ep) => <EpisodeRow key={ep.id} episode={ep} onOpen={() => onOpenEpisode(ep.id)} />)}
        </ul>
      )}
    </div>
  );
}

type View =
  | { kind: "shows" }
  | { kind: "episodes"; show: Show }
  | { kind: "episode"; show: Show; episodeId: string };

export function PhoneApp() {
  const accessLoginRequired = useSyncExternalStore(subscribeToAccessLogin, getAccessLoginRequired);
  const [revoked, setRevoked] = useState(false);
  const [paired, setPaired] = useState(() => !!deviceToken());
  const [view, setView] = useState<View>(() => deviceToken() ? history.state?.view ?? { kind: "shows" } : { kind: "shows" });

  // 手机上的返回手势/浏览器返回键：每一层都记一条历史，回来时恢复那一层。
  useEffect(() => {
    const onPop = (e: PopStateEvent) => {
      const restored = (e.state as { view?: View } | null)?.view;
      setView(restored && restored.kind ? restored : { kind: "shows" });
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  const go = (next: View) => {
    setView(next);
    try { history.pushState({ view: next }, ""); } catch { /* private mode */ }
  };
  const back = () => { try { history.back(); } catch { setView({ kind: "shows" }); } };

  const pairedNow = () => {
    // 配对码别留在浏览历史里。
    try { history.replaceState({ view: { kind: "shows" } }, "", location.pathname); } catch { /* fine */ }
    setView({ kind: "shows" });
    setRevoked(false);
    setPaired(true);
  };
  const unpair = useCallback(() => {
    setDeviceToken(null);
    resetPhoneProgress();
    setView({ kind: "shows" });
    setPaired(false);
  }, []);

  const onUnauthorized = useCallback(() => {
    setRevoked(true);
    unpair();
  }, [unpair]);

  useEffect(() => {
    if (!paired || accessLoginRequired) return;
    let live = true;
    const ping = () => {
      if (document.visibilityState === "hidden") return;
      const token = deviceToken();
      void api.checkPhoneSession().catch((error) => {
        if (live && error instanceof Unauthorized && token === deviceToken()) onUnauthorized();
      });
    };
    const timer = window.setInterval(ping, 20_000);
    document.addEventListener("visibilitychange", ping);
    window.addEventListener("online", ping);
    ping();
    window.addEventListener("online", syncPhoneProgress);
    window.addEventListener("subcast:unauthorized", onUnauthorized);
    void syncPhoneProgress();
    return () => {
      live = false;
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", ping);
      window.removeEventListener("online", ping);
      window.removeEventListener("online", syncPhoneProgress);
      window.removeEventListener("subcast:unauthorized", onUnauthorized);
    };
  }, [paired, accessLoginRequired, onUnauthorized]);

  if (accessLoginRequired) return <AccessLoginPage />;
  if (!paired) return <PairPage onPaired={pairedNow} revoked={revoked} />;
  if (view.kind === "episodes") {
    return (
      <EpisodesPage
        show={view.show}
        onBack={back}
        onOpenEpisode={(episodeId) => go({ kind: "episode", show: view.show, episodeId })}
        onUnauthorized={onUnauthorized}
      />
    );
  }
  if (view.kind === "episode") {
    return <EpisodePage key={view.episodeId} episodeId={view.episodeId} onBack={back} onUnauthorized={onUnauthorized} />;
  }
  return <ShowsPage onUnpair={unpair} onUnauthorized={onUnauthorized} onOpenShow={(show) => go({ kind: "episodes", show })} />;
}
