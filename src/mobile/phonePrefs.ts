// 手机端字幕的显示偏好。只存在这台手机的 localStorage 里，不回写桌面端设置（issue #3 验收项）。

export interface SubtitlePrefs {
  /** Font size in px, one of SIZES. */
  size: number;
  /** Line spacing preset, index into SPACINGS. */
  spacing: number;
}

export const SIZES = [14, 16, 18, 20, 22, 24] as const;
/** Multipliers, loose enough for CJK lines to breathe. */
export const SPACINGS = [1.35, 1.6, 1.9] as const;

const KEY = "subcast.phone.subtitles";
/** Indexes into SIZES / SPACINGS, not px: 16 px, normal spacing. */
const DEFAULT_PREFS: SubtitlePrefs = { size: 1, spacing: 1 };

const inRange = (n: unknown, min: number, max: number): n is number => typeof n === "number" && Number.isInteger(n) && n >= min && n <= max;

/** Whatever was saved, with anything unrecognised falling back to the defaults. */
export function loadPrefs(): SubtitlePrefs {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return DEFAULT_PREFS;
    const parsed = JSON.parse(raw) as Partial<SubtitlePrefs>;
    return {
      size: inRange(parsed.size, 0, SIZES.length - 1) ? parsed.size : DEFAULT_PREFS.size,
      spacing: inRange(parsed.spacing, 0, SPACINGS.length - 1) ? parsed.spacing : DEFAULT_PREFS.spacing,
    };
  } catch {
    return DEFAULT_PREFS;
  }
}

export function savePrefs(prefs: SubtitlePrefs) {
  try { localStorage.setItem(KEY, JSON.stringify(prefs)); } catch { /* private mode */ }
}
