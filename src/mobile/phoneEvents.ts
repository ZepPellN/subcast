import { deviceToken, Unauthorized } from "../lib/api";

export interface AnswerEvent { requestId: string; sequence: number; text: string; done: boolean; error: boolean }
interface AsrEvent { episodeId: string; stage: string; progress: number }

export async function watchPhoneEvents(
  episodeId: string,
  signal: AbortSignal,
  requestId?: string,
  onAnswer?: (event: AnswerEvent) => void,
  onAsr?: (event: AsrEvent) => void,
) {
  let after = 0;
  let attempts = 0;
  let lastAsr = "";
  while (!signal.aborted) {
    try {
      const query = new URLSearchParams({ episodeId, after: String(after) });
      if (requestId) query.set("requestId", requestId);
      const response = await fetch(`/events?${query}`, {
        headers: { "X-Subcast-Client": "phone", Authorization: `Bearer ${deviceToken()}` },
        referrerPolicy: "same-origin",
        signal,
      });
      if (response.status === 401) throw new Unauthorized();
      if (response.status === 404 && requestId) throw new Error("answer_lost");
      if (!response.ok || !response.body) throw new Error("connection_lost");
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let end: number;
          while ((end = buffer.indexOf("\n\n")) !== -1) {
            const frame = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            const lines = frame.split("\n");
            const event = lines.find((line) => line.startsWith("event: "))?.slice(7);
            const data = lines.find((line) => line.startsWith("data: "))?.slice(6);
            if (!data) continue;
            if (event === "llm" && requestId) {
              const answer = JSON.parse(data) as AnswerEvent;
              if (answer.requestId !== requestId) continue;
              if (answer.sequence > after) {
                after = answer.sequence;
                onAnswer?.(answer);
              }
              if (answer.done) return;
            } else if (event === "asr" && data !== lastAsr) {
              lastAsr = data;
              const update = JSON.parse(data) as AsrEvent;
              if (update.episodeId === episodeId) onAsr?.(update);
            }
          }
        }
      } finally {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
      }
      if (!requestId) attempts = 0;
    } catch (error) {
      if (signal.aborted) return;
      if (error instanceof Unauthorized || (error instanceof Error && error.message === "answer_lost")) throw error;
    }
    attempts++;
    if (requestId && attempts > 4) throw new Error("connection_lost");
    await new Promise<void>((resolve) => window.setTimeout(resolve, Math.min(20, attempts) * 1000));
  }
}
