// Single source of truth for episode watched state across lists, cards, filters, Play All, and sidebar counts.
// Thresholds mirror `IS_WATCHED_SQL` in `src-tauri/src/db.rs` exactly.
import { create } from "zustand";
import { api, type Episode } from "./api";
import { useStore } from "../store";
import { watchInfo, isWatched, type WatchInfo, type WatchPatch, type WatchOverride } from "./watchInfo";
export * from "./watchInfo";

export const useWatched = create<{ patches: Record<string, WatchPatch> }>(() => ({ patches: {} }));

/** Subscribes only to this episode's runtime watch patch so lists with hundreds of rows stay fast. */
export function useEpisodeWatch(ep: Episode): WatchInfo {
  const patch = useWatched((s) => s.patches[ep.id]);
  return watchInfo(ep, patch);
}

/** Manually marks one episode as watched ("done") or unseen ("unseen", resetting progress to 0). */
export async function markWatched(ep: Episode, done: boolean): Promise<void> {
  const next: WatchPatch = done
    ? { watchedOverride: "done" }
    : { watchedOverride: "unseen", progress: 0 };
  useWatched.setState((s) => ({ patches: { ...s.patches, [ep.id]: { ...s.patches[ep.id], ...next } } }));

  const st = useStore.getState();
  if (st.current?.id === ep.id) {
    st.set({
      current: { ...st.current, watchedOverride: next.watchedOverride, ...(done ? {} : { progress: 0 }) },
      ...(done ? {} : { time: 0 }),
    });
  }

  await api.setWatched(ep.id, done ? "done" : "unseen");
  void useStore.getState().reloadShows();
}

/** Called when playback reaches the end (`onEnded`): guarantees episodes without RSS duration become watched too. */
export function markEnded(ep: Episode, seconds?: number): void {
  const finalSec = seconds && seconds > 0 ? seconds : (ep.duration ?? ep.progress ?? 0);
  if (finalSec > 0) void api.setProgress(ep.id, finalSec);
  void markWatched(ep, true);
}

/** Keeps in-memory watch status and sidebar counts synced as playback progress is saved. */
export function notePlaybackProgress(ep: Episode, seconds: number, duration?: number): void {
  const prevPatch = useWatched.getState().patches[ep.id];
  const wasWatched = isWatched(ep, prevPatch);
  const nextOverride: WatchOverride | undefined =
    prevPatch?.watchedOverride === "unseen" && seconds > 5
      ? null
      : prevPatch?.watchedOverride === undefined && ep.watchedOverride === "unseen" && seconds > 5
        ? null
        : prevPatch?.watchedOverride;
  const nextPatch: WatchPatch = {
    ...prevPatch,
    progress: Math.max(0, seconds),
    ...(duration && duration > 0 ? { duration } : {}),
    ...(nextOverride !== undefined ? { watchedOverride: nextOverride } : {}),
  };
  useWatched.setState((s) => ({ patches: { ...s.patches, [ep.id]: nextPatch } }));
  const nowWatched = isWatched(ep, nextPatch);
  if (wasWatched !== nowWatched) {
    void useStore.getState().reloadShows();
  }
}

/** Marks every unwatched episode in a show as watched and refreshes sidebar counts. */
export async function markShowAllWatched(showId: string, episodes: Episode[]): Promise<number> {
  const curPatches = useWatched.getState().patches;
  const additions: Record<string, WatchPatch> = {};
  for (const ep of episodes) {
    if (!isWatched(ep, curPatches[ep.id])) {
      additions[ep.id] = { ...curPatches[ep.id], watchedOverride: "done" };
    }
  }
  if (Object.keys(additions).length) {
    useWatched.setState((s) => ({ patches: { ...s.patches, ...additions } }));
  }
  const st = useStore.getState();
  if (st.current?.showId === showId) {
    st.set({ current: { ...st.current, watchedOverride: "done" } });
  }
  const n = await api.markShowWatched(showId);
  await useStore.getState().reloadShows();
  return n;
}
