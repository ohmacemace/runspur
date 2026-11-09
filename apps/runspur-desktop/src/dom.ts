import type { Message } from './i18n';

export type Translate = (message: Message) => string;

export function element<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing element: ${id}`);
  return node as T;
}

export function label<K extends keyof HTMLElementTagNameMap>(tag: K, text: string, css?: string) {
  const node = document.createElement(tag);
  node.textContent = text;
  if (css) node.className = css;
  return node;
}

export function pathLabel(path: string) {
  if (path.startsWith('\\\\?\\UNC\\')) return `\\\\${path.slice(8)}`;
  return path.startsWith('\\\\?\\') ? path.slice(4) : path;
}

export type Notice = { message: Message; detail?: string };

export function showNotice(node: HTMLElement, text: Translate, notice?: Notice) {
  node.textContent = notice ? [text(notice.message), notice.detail].filter(Boolean).join(' ') : '';
  node.dataset.error = String(Boolean(notice?.detail));
}
