const tokenKey = 'runspur.tui.token';
const validToken = /^[0-9a-f]{64}$/i;

export const isDesktop = '__TAURI_INTERNALS__' in window;

function browserToken(): string | null {
  const { hash, pathname, search } = window.location;
  const supplied = new URLSearchParams(hash.slice(1)).get('token');
  if (hash) window.history.replaceState(window.history.state, '', pathname + search);
  if (supplied !== null && !validToken.test(supplied)) return null;

  try {
    if (supplied !== null) {
      window.sessionStorage.setItem(tokenKey, supplied);
    } else {
      const saved = window.sessionStorage.getItem(tokenKey);
      return saved !== null && validToken.test(saved) ? saved : null;
    }
  } catch {
    // The current page can still connect when browser storage is disabled.
  }
  return supplied;
}

const token = isDesktop ? null : browserToken();

export async function invoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isDesktop) {
    const { invoke: invokeDesktop } = await import('@tauri-apps/api/core');
    return invokeDesktop<T>(command, args);
  }
  if (token === null) throw new Error('Open the Runspur browser panel with `runspur tui`.');

  const response = await fetch(`/api/${encodeURIComponent(command)}`, {
    method: 'POST',
    credentials: 'omit',
    redirect: 'error',
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify(args),
  });
  if (!response.ok) {
    const payload = await response.json().catch(() => null) as { error?: unknown } | null;
    throw new Error(typeof payload?.error === 'string'
      ? payload.error : `Runspur returned HTTP ${response.status}.`);
  }
  return response.json() as Promise<T>;
}
