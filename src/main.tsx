import React from "react";
import ReactDOM from "react-dom/client";
import "./styles.css";
import "./polish.css";
import { IS_MAC, setLang } from "./lib/i18n";
import { IS_DESKTOP } from "./lib/api";

// Only macOS draws its window buttons inside the page (overlay title bar); Windows and Linux have a native title bar.
if (IS_DESKTOP) document.documentElement.classList.toggle("is-mac", IS_MAC);

// Never leave a blank window: show startup failures.
const showFatal = (msg: string) => {
  const el = document.createElement("pre");
  el.style.cssText = "position:fixed;left:0;right:0;bottom:0;margin:0;padding:12px;background:#b42318;color:#fff;font:12px monospace;white-space:pre-wrap;z-index:99";
  el.textContent = msg;
  document.body.appendChild(el);
};
window.addEventListener("error", (e) => showFatal(`${e.message}\n${String(e.error?.stack ?? "").split("\n").slice(0, 4).join("\n")}`));
// An interrupted media play() or a cancelled request is expected, not a failure.
const benign = (r: unknown) => /AbortError|aborted|interrupted by a (new load|call to pause)/i.test(String((r as Error)?.name ?? "") + String((r as Error)?.message ?? r));
window.addEventListener("unhandledrejection", (e) => { if (!benign(e.reason)) showFatal(String((e.reason as Error)?.stack ?? e.reason)); });

// The same bundle serves the desktop window (Tauri) and the phone page (a plain browser on the LAN, see
// src-tauri/src/remote.rs). Only the phone gets the mobile chunk: the desktop app stays untouched.
const root = ReactDOM.createRoot(document.getElementById("root") as HTMLElement);
async function boot() {
  if (IS_DESKTOP) {
    const { default: App } = await import("./App");
    root.render(<React.StrictMode><App /></React.StrictMode>);
  } else {
    const { PhoneApp } = await import("./mobile/PhoneApp");
    setLang("auto");
    root.render(<React.StrictMode><PhoneApp /></React.StrictMode>);
  }
}
void boot();
