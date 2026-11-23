import { api, deviceToken, Unauthorized } from "../lib/api";

export const PROGRESS_INTERVAL = 15_000;
const KEY = "subcast.phone.pending-progress";
interface Progress { seconds: number; playing: boolean; finished: boolean; savedAt: number }
type Pending = Record<string, Progress>;

function restore(): Pending {
  try { return JSON.parse(localStorage.getItem(KEY) ?? "{}") ?? {}; } catch { return {}; }
}

let pending = restore();
let sending = false;
let retry: number | undefined;

export function pendingProgress(id: string): number | undefined { return pending[id]?.seconds; }

export function resetPhoneProgress() {
  pending = {};
  persist();
}

function persist() {
  try { localStorage.setItem(KEY, JSON.stringify(pending)); } catch { /* still retry in memory */ }
}

export function savePhoneProgress(id: string, seconds: number, playing: boolean, finished = false) {
  if (!deviceToken() || !Number.isFinite(seconds)) return;
  pending[id] = { seconds, playing, finished: finished || pending[id]?.finished === true, savedAt: Date.now() };
  persist();
  void syncPhoneProgress();
}

export async function syncPhoneProgress() {
  if (sending || !deviceToken() || !navigator.onLine) return;
  sending = true;
  window.clearTimeout(retry);
  const token = deviceToken();
  try {
    for (const [id, position] of Object.entries(pending)) {
      if (deviceToken() !== token) break;
      const age = () => Math.max(0, Date.now() - (position.savedAt ?? Date.now()));
      const applied = await api.savePhoneProgress(id, position.seconds, position.playing, age());
      if (applied && position.finished && deviceToken() === token) await api.finishPhoneEpisode(id, age());
      if (pending[id] === position) { delete pending[id]; persist(); }
    }
    if (Object.keys(pending).length) retry = window.setTimeout(syncPhoneProgress, 0);
  } catch (e) {
    if (e instanceof Unauthorized) window.dispatchEvent(new Event("subcast:unauthorized"));
    else retry = window.setTimeout(syncPhoneProgress, 5000);
  } finally {
    sending = false;
  }
}
