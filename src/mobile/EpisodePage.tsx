import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlignJustify, ChevronLeft, Pause, Play } from "lucide-react";
import { api, Unauthorized, type Episode, type ReadingHints, type Segment, type Transcript } from "../lib/api";
import { t } from "../lib/i18n";
import { clock } from "../lib/format";
import { SPACINGS, SIZES, loadPrefs, savePrefs } from "./phonePrefs";

/** The phone's single-episode page (issue #3): play the episode's own audio address, and a bilingual subtitle
 *  panel that follows playback. Nothing here writes to the desktop: progress and settings stay untouched. */

/** The last line starting at or before `time`; -1 before the first. (Same 0.15 s grace as the desktop panel.) */
function activeIndex(segments: Segment[], time: number): number {
  let lo = 0, hi = segments.length - 1, found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (segments[mid].start <= time + 0.15) { found = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return found;
}

/** Why this episode can't be played here, if it can't: a file only the computer has, or a video. */
type Blocked = "offline" | "video" | "none" | null;

function degradeText(reason: Exclude<Blocked, null>): { title: string; hint: string } {
  switch (reason) {
    case "offline":
      return {
        title: t("离线音频在手机上不可播放"),
        hint: t("这一集已经下载到电脑上，音频文件只在电脑本地。要听可以在电脑上取消下载后再打开这一页，或直接在电脑上播放。"),
      };
    case "video":
      return { title: t("请在电脑上播放"), hint: t("这是视频单集，手机版只播放音频。") };
    default:
      return { title: t("这期没有可播放的音频"), hint: t("电脑上也没有找到这一集的音频地址。") };
  }
}

interface Props {
  episodeId: string;
  /** 401 from the gateway: forget this phone and pair again. */
  onUnauthorized: () => void;
  onBack: () => void;
}

export function EpisodePage({ episodeId, onUnauthorized, onBack }: Props) {
  const [episode, setEpisode] = useState<Episode | null | undefined>(undefined);
  const [hints, setHints] = useState<ReadingHints | null>(null);
  const [transcript, setTranscript] = useState<Transcript | null>(null);
  const [transcriptState, setTranscriptState] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [loadError, setLoadError] = useState("");
  const [audioError, setAudioError] = useState("");
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [active, setActive] = useState(-1);
  const [follow, setFollow] = useState(true);
  const [prefs, setPrefs] = useState(loadPrefs);

  const audioRef = useRef<HTMLAudioElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const rowRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const followRef = useRef(true);
  const programmaticUntil = useRef(0);
  const resumedRef = useRef(false);

  const segments = transcript?.segments ?? [];

  const setFollowing = useCallback((on: boolean) => { followRef.current = on; setFollow(on); }, []);

  // Load the episode, what the phone can't know (offline copy, translation), then the subtitles.
  useEffect(() => {
    let live = true;
    setEpisode(undefined);
    setHints(null);
    setTranscript(null);
    setTranscriptState("idle");
    setLoadError("");
    setAudioError("");
    setTime(0);
    setDuration(0);
    setActive(-1);
    resumedRef.current = false;
    (async () => {
      try {
        const [ep, readingHints] = await Promise.all([api.getEpisode(episodeId), api.readingHints(episodeId)]);
        if (!live) return;
        setEpisode(ep);
        setHints(readingHints);
        if (!ep?.hasTranscript) return;
        setTranscriptState("loading");
        try {
          const got = await api.loadTranscript(episodeId);
          if (!live) return;
          setTranscript(got);
          setTranscriptState("ready");
        } catch (e) {
          if (!live) return;
          if (e instanceof Unauthorized) { onUnauthorized(); return; }
          setTranscriptState("error");
        }
      } catch (e) {
        if (!live) return;
        if (e instanceof Unauthorized) { onUnauthorized(); return; }
        setLoadError(t("加载失败，请稍后再试"));
      }
    })();
    return () => { live = false; };
  }, [episodeId, onUnauthorized]);

  const blocked: Blocked = useMemo(() => {
    if (!episode) return null;
    // 下载过的音频是电脑上的本地文件；按 issue #3 的验收，手机端明确禁用播放并说明原因。
    if (hints?.offline) return "offline";
    if (episode.audioUrl) return null;
    if (episode.videoId) return "video";
    return "none";
  }, [episode, hints]);

  const canPlay = !!episode?.audioUrl && !blocked;

  // Lock-screen / control-center metadata (the audio element itself keeps playing in the background).
  useEffect(() => {
    if (!episode || !canPlay) return;
    if (typeof navigator === "undefined" || !("mediaSession" in navigator)) return;
    try {
      navigator.mediaSession.metadata = new MediaMetadata({
        title: episode.title,
        artist: episode.showTitle ?? "SubCast",
        album: "SubCast",
        artwork: episode.cover ? [{ src: episode.cover }] : [],
      });
    } catch { /* older browser: playback still works, just without metadata */ }
    return () => {
      try { navigator.mediaSession.metadata = null; } catch { /* ignore */ }
    };
  }, [episode, canPlay]);

  const scrollToRow = useCallback((index: number) => {
    const list = listRef.current;
    const row = rowRefs.current[index];
    if (!list || !row) return;
    programmaticUntil.current = performance.now() + 800;
    list.scrollTo({ top: Math.max(0, row.offsetTop - list.clientHeight * 0.35), behavior: "smooth" });
  }, []);

  // Follow playback: scroll only when the active line changes, so it doesn't fight the reader.
  useEffect(() => {
    if (active < 0 || !followRef.current) return;
    scrollToRow(active);
  }, [active, scrollToRow]);

  const onTimeUpdate = () => {
    const a = audioRef.current;
    if (!a) return;
    setTime(a.currentTime);
    const next = activeIndex(segments, a.currentTime);
    setActive((prev) => (prev === next ? prev : next));
  };

  const onLoadedMetadata = () => {
    const a = audioRef.current;
    if (!a) return;
    setDuration(Number.isFinite(a.duration) ? a.duration : 0);
    // 从电脑上听到一半的那期，手机上接着听（只是不写回进度）。
    if (!resumedRef.current) {
      resumedRef.current = true;
      const resumeAt = episode?.progress ?? 0;
      if (resumeAt > 5 && (!Number.isFinite(a.duration) || resumeAt < a.duration - 5)) {
        a.currentTime = resumeAt;
        setTime(resumeAt);
      }
    }
  };

  const toggle = () => {
    const a = audioRef.current;
    if (!a || !canPlay) return;
    if (a.paused) void a.play().catch(() => setAudioError(t("音频加载失败，请检查网络后刷新。")));
    else a.pause();
  };

  const seek = (seconds: number) => {
    const a = audioRef.current;
    if (!a || !canPlay) return;
    a.currentTime = seconds;
    setTime(seconds);
  };

  const seekToLine = (index: number) => {
    const seg = segments[index];
    if (!seg || !canPlay) return;
    const a = audioRef.current;
    if (a && (Number.isFinite(a.duration) ? seg.start < a.duration : true)) a.currentTime = seg.start;
    setTime(seg.start);
    setActive(index);
    setFollowing(true);
    scrollToRow(index);
  };

  const onListScroll = () => {
    if (performance.now() < programmaticUntil.current) return;
    if (followRef.current) setFollowing(false);
  };

  const bumpSize = (delta: number) => {
    const next = { ...prefs, size: Math.min(SIZES.length - 1, Math.max(0, prefs.size + delta)) };
    setPrefs(next);
    savePrefs(next);
  };
  const cycleSpacing = () => {
    const next = { ...prefs, spacing: (prefs.spacing + 1) % SPACINGS.length };
    setPrefs(next);
    savePrefs(next);
  };

  const showZh = !!hints?.zh?.some((line) => !!line);
  const degraded = blocked ? degradeText(blocked) : null;

  return (
    <div className="phone ph-ep">
      <header className="ph-ep-head">
        <button className="ph-back" onClick={onBack} aria-label={t("返回")}>
          <ChevronLeft size={20} />
        </button>
        <div className="ph-ep-head-text">
          {episode?.showTitle && <p className="ph-ep-show">{episode.showTitle}</p>}
          <h1 className="ph-ep-title">{episode ? episode.title : t("加载中…")}</h1>
        </div>
      </header>

      {loadError && <p className="ph-error ph-ep-pad">{loadError}</p>}

      {episode === null && !loadError && (
        <p className="ph-muted ph-loading ph-ep-pad">{t("这一集不存在了，可能已经在电脑上删除。")}</p>
      )}

      {episode && (
        <>
          {canPlay ? (
            <div className="ph-player">
              <audio
                ref={audioRef}
                src={episode.audioUrl ?? undefined}
                preload="metadata"
                onTimeUpdate={onTimeUpdate}
                onLoadedMetadata={onLoadedMetadata}
                onDurationChange={onLoadedMetadata}
                onPlay={() => setPlaying(true)}
                onPause={() => setPlaying(false)}
                onEnded={() => setPlaying(false)}
                onError={() => setAudioError(t("音频加载失败，请检查网络后刷新。"))}
              />
              <div className="ph-player-row">
                <button className="ph-play" onClick={toggle} aria-label={playing ? t("暂停") : t("播放")}>
                  {playing ? <Pause size={22} fill="currentColor" /> : <Play size={22} fill="currentColor" />}
                </button>
                <div className="ph-player-timeline">
                  <input
                    className="ph-range"
                    type="range"
                    min={0}
                    max={Math.max(1, duration || episode.duration || 0)}
                    step={0.5}
                    value={Math.min(time, duration || episode.duration || Math.max(1, time))}
                    aria-label={t("播放进度")}
                    onChange={(e) => seek(Number(e.target.value))}
                  />
                  <div className="ph-player-times">
                    <span>{clock(time)}</span>
                    <span>{clock(duration || episode.duration || 0)}</span>
                  </div>
                </div>
              </div>
              {audioError && <p className="ph-error">{audioError}</p>}
            </div>
          ) : degraded ? (
            <div className="ph-degrade">
              <div className="ph-player-row">
                <button className="ph-play" disabled aria-label={t("播放")}>
                  <Play size={22} fill="currentColor" />
                </button>
                <div className="ph-degrade-text">
                  <p className="ph-degrade-title">{degraded.title}</p>
                  <p className="ph-muted ph-degrade-hint">{degraded.hint}</p>
                </div>
              </div>
            </div>
          ) : null}

          <section className="ph-subs">
            <div className="ph-sub-head">
              <p className="ph-sub-title">{showZh ? t("双语字幕") : t("字幕")}</p>
              <div className="ph-sub-tools">
                <button className="ph-tool" onClick={() => bumpSize(-1)} disabled={prefs.size === 0} aria-label={t("减小字号")}>A−</button>
                <button className="ph-tool" onClick={() => bumpSize(1)} disabled={prefs.size === SIZES.length - 1} aria-label={t("增大字号")}>A+</button>
                <button className="ph-tool" onClick={cycleSpacing} aria-label={t("行距")} title={t("行距")}><AlignJustify size={16} /></button>
              </div>
            </div>

            {transcriptState === "loading" && <p className="ph-muted ph-loading">{t("加载中…")}</p>}

            {transcriptState === "error" && <p className="ph-error ph-sub-empty">{t("字幕加载失败，请刷新重试。")}</p>}

            {transcriptState !== "loading" && !segments.length && (
              <div className="ph-sub-empty">
                <p className="ph-degrade-title">{t("请先在电脑上转写")}</p>
                <p className="ph-muted ph-degrade-hint">{t("这台电脑上还没有这一集的字幕。在电脑上打开它并生成或导入字幕，手机就能跟着听了。")}</p>
              </div>
            )}

            {segments.length > 0 && (
              <div
                className="ph-sub-list"
                ref={listRef}
                onScroll={onListScroll}
                style={{
                  ["--sub-size" as string]: `${SIZES[prefs.size]}px`,
                  ["--sub-lh" as string]: String(SPACINGS[prefs.spacing]),
                }}
              >
                {segments.map((seg, i) => {
                  const zh = hints?.zh?.[i] ?? null;
                  return (
                    <button
                      key={i}
                      ref={(el) => { rowRefs.current[i] = el; }}
                      type="button"
                      className={`ph-sub-row${i === active ? " active" : ""}${canPlay ? "" : " static"}`}
                      aria-current={i === active ? "true" : undefined}
                      onClick={() => seekToLine(i)}
                    >
                      <span className="ph-sub-time">{clock(seg.start)}</span>
                      <span className="ph-sub-lines">
                        <span className="ph-sub-text">{seg.text}</span>
                        {zh && zh !== seg.text && <span className="ph-sub-zh">{zh}</span>}
                      </span>
                    </button>
                  );
                })}
              </div>
            )}

            {!follow && active >= 0 && segments.length > 0 && (
              <button className="ph-follow" onClick={() => { setFollowing(true); scrollToRow(active); }}>
                {t("回到当前句")}
              </button>
            )}
          </section>
        </>
      )}
    </div>
  );
}
