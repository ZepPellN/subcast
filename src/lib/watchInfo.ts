import type { Episode } from "./api";

export type WatchStatus = "unseen" | "in-progress" | "watched";
export type WatchOverride = "done" | "unseen" | null;

export interface WatchPatch {
  progress?: number;
  duration?: number;
  watchedOverride?: WatchOverride;
}

export interface WatchInfo {
  status: WatchStatus;
  pct: number;
  leftMin: number;
  progress: number;
  duration: number;
}

export function effectiveEpisode(ep: Episode, patch?: WatchPatch): Episode {
  if (!patch) return ep;
  return {
    ...ep,
    progress: patch.progress !== undefined ? patch.progress : ep.progress,
    duration: patch.duration !== undefined ? patch.duration : ep.duration,
    watchedOverride: patch.watchedOverride !== undefined ? patch.watchedOverride : ep.watchedOverride,
  };
}

export function watchInfo(ep: Episode, patch?: WatchPatch): WatchInfo {
  const e = effectiveEpisode(ep, patch);
  const progress = Math.max(0, e.progress || 0);
  const duration = Math.max(0, e.duration || 0);
  const override = e.watchedOverride ?? null;

  if (override === "done") {
    return { status: "watched", pct: 100, leftMin: 0, progress, duration };
  }
  if (override === "unseen") {
    return { status: "unseen", pct: 0, leftMin: 0, progress: 0, duration };
  }
  const autoDone = duration > 0 && (progress >= duration * 0.95 || (duration > 300 && duration - progress <= 90));
  if (autoDone) {
    return { status: "watched", pct: 100, leftMin: 0, progress, duration };
  }
  if (progress > 5) {
    const pct = duration > 0 ? Math.min(99, Math.max(1, Math.round((progress / duration) * 100))) : 0;
    const leftMin = duration > 0 ? Math.max(1, Math.round((duration - progress) / 60)) : 0;
    return { status: "in-progress", pct, leftMin, progress, duration };
  }
  return { status: "unseen", pct: 0, leftMin: 0, progress, duration };
}

export const isWatched = (ep: Episode, patch?: WatchPatch) => watchInfo(ep, patch).status === "watched";
export const isInProgress = (ep: Episode, patch?: WatchPatch) => watchInfo(ep, patch).status === "in-progress";
