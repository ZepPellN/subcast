import { useEffect, useRef, useState, type FormEvent } from "react";
import { api, Unauthorized } from "../lib/api";
import { t } from "../lib/i18n";
import { watchPhoneEvents } from "./phoneEvents";

export function PhoneChat({ episodeId, ready, onUnauthorized }: {
  episodeId: string; ready: boolean; onUnauthorized: () => void;
}) {
  const [question, setQuestion] = useState("");
  const [answer, setAnswer] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const session = useRef<AbortController | null>(null);
  const response = useRef<HTMLTextAreaElement>(null);
  useEffect(() => () => session.current?.abort(), []);

  const ask = async (event: FormEvent) => {
    event.preventDefault();
    const text = question.trim();
    if (!text || session.current || !ready) return;
    const controller = new AbortController();
    session.current = controller;
    setBusy(true);
    setAnswer("");
    setError("");
    setCopied(false);
    try {
      const requestId = await api.askPhoneEpisode(episodeId, text);
      await watchPhoneEvents(episodeId, controller.signal, requestId, (update) => {
        setAnswer(update.text);
        if (update.error) setError(t("回答失败，请检查电脑上的模型设置后重试。"));
      });
    } catch (e) {
      if (controller.signal.aborted) return;
      if (e instanceof Unauthorized) { onUnauthorized(); return; }
      const code = e instanceof Error ? e.message : "";
      if (code === "chat_rate_limited") setError(t("提问太频繁，请稍等再试。"));
      else if (code === "no_transcript") setError(t("请先在电脑上转写"));
      else if (code === "chat_unavailable") setError(t("回答失败，请检查电脑上的模型设置后重试。"));
      else setError(t("问答连接中断，请确认电脑上的手机访问已打开，再重试。"));
    } finally {
      session.current = null;
      if (!controller.signal.aborted) setBusy(false);
    }
  };

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(answer);
      setCopied(true);
    } catch {
      const field = response.current;
      field?.focus();
      field?.select();
      if (document.execCommand("copy")) setCopied(true);
    }
  };

  return (
    <section className="ph-chat">
      <p className="ph-muted">{t("使用电脑上配置的模型与 Key，费用计入电脑端账号。")}</p>
      {!ready && <p className="ph-muted">{t("请先在电脑上转写")}</p>}
      <form onSubmit={ask}>
        <textarea value={question} maxLength={1000} rows={2} onChange={(e) => setQuestion(e.target.value)}
          aria-label={t("你的问题")} placeholder={t("你的问题")} />
        <button className="ph-btn primary" disabled={busy || !ready || !question.trim()}>
          {busy ? t("正在回答…") : t("提问")}
        </button>
      </form>
      {answer && <div className="ph-answer">
        <p aria-live={busy ? "off" : "polite"}>{answer}</p>
        <textarea ref={response} className="ph-copy-buffer" readOnly value={answer} tabIndex={-1} aria-hidden="true" />
        <button type="button" className="ph-btn ghost" onClick={() => void copy()}>{copied ? t("已复制") : t("复制回答")}</button>
      </div>}
      {error && <p className="ph-error" role="alert">{error}</p>}
    </section>
  );
}
