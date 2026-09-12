export type Json = Record<string, any>;
export type Api = (path: string, body?: unknown, signal?: AbortSignal) => Promise<any>;

export function apiFor(token: string): Api {
  return async (path, body, signal) => {
    if (!path.startsWith('/') || path.startsWith('//')) throw new Error('Invalid API path');
    const response = await fetch(path, {
      method: body === undefined ? 'GET' : 'POST',
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body), signal, redirect: 'error',
    });
    const value = await response.json();
    if (!response.ok) throw new Error(value.error || `HTTP ${response.status}`);
    return value;
  };
}
export const terminal = (state: string) => ['SUCCEEDED', 'FAILED', 'CANCELLED', 'SKIPPED', 'LOST'].includes(state);
export const errorMessage = (error: unknown) => error instanceof Error ? error.message : String(error);
export const segment = (value: string) => encodeURIComponent(value);

export async function downloadArtifact(token: string, run: string, artifact: Json) {
  const response = await fetch(`/runs/${segment(run)}/artifacts/${segment(artifact.id)}`, { headers: { Authorization: `Bearer ${token}` }, redirect: 'error' });
  if (!response.ok) throw new Error(`Artifact download failed (${response.status})`);
  const bytes = await response.arrayBuffer();
  const hash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))).map(n => n.toString(16).padStart(2, '0')).join('');
  if (bytes.byteLength !== artifact.size || hash !== artifact.checksum) throw new Error('Artifact checksum mismatch');
  download(new Blob([bytes], { type: 'application/octet-stream' }), `${artifact.kind}-${artifact.id}`);
}
export function download(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a'); link.href = url; link.download = name; link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
