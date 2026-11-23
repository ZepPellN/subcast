/** Access authorization is independent of the phone's SubCast device token. */
export class AccessRequired extends Error {
  constructor() { super("access_required"); }
}

let loginRequired = false;
const listeners = new Set<() => void>();

export const getAccessLoginRequired = () => loginRequired;

export function subscribeToAccessLogin(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export async function phoneFetch(path: string, init: RequestInit): Promise<Response> {
  if (location.protocol !== "https:") return fetch(path, init);
  if (loginRequired) throw new AccessRequired();

  // Let a top-level reload handle Access login. A fetch must not follow the login redirect.
  const response = await fetch(path, { ...init, redirect: "manual" });
  if (response.type !== "opaqueredirect") {
    if (response.status !== 401) return response;
    const body = await response.clone().json().catch(() => null) as { error?: string } | null;
    if (body?.error === "unauthorized" || body?.error === "invalid_code") return response;
    // Only a recognized device rejection should cause the client to forget the pairing.
    if (body?.error !== "access_required") throw new Error("HTTP 401");
  }

  if (!loginRequired) {
    loginRequired = true;
    listeners.forEach((listener) => listener());
  }
  throw new AccessRequired();
}
