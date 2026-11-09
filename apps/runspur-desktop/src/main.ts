import { invoke } from '@tauri-apps/api/core';
import { element } from './dom';
import { createHistory } from './history';
import { messages, resolveLocale, type Language, type Message } from './i18n';
import { createProjects } from './projects';
import './style.css';

type Health = {
  version: string;
  protocol_version: number;
  active_workers: number;
};

type Preferences = { language: Language };
type ServiceCommand = 'service_status' | 'start_service';

type Snapshot = {
  channel: 'installed' | 'development';
} & (
  | { status: 'ready'; health: Health }
  | { status: 'offline' }
  | { status: 'error'; detail: string }
);

const status = element('status');
const detail = element('detail');
const indicator = element('indicator');
const channel = element('channel');
const refresh = element<HTMLButtonElement>('refresh');
const start = element<HTMLButtonElement>('start');
const languageSetting = element<HTMLSelectElement>('language-setting');
const settingsMessage = element('settings-message');
let preferences: Preferences | undefined;
let locale = resolveLocale('system', navigator.language);
let snapshot: Snapshot | undefined;
let pending: ServiceCommand | undefined;
let connectionError: string | undefined;
let notice: { message: Message; detail?: string } | undefined;

const text = (message: Message) => messages[locale][message];
const history = createHistory(text);
const projects = createProjects(text, history.setProjects, (projectId) => {
  history.selectProject(projectId);
  showPage('history');
});

function technicalDetails(detail: string): string {
  return `${text('technicalDetails')} — ${detail}`;
}

function renderService() {
  const health = snapshot?.status === 'ready' ? snapshot.health : undefined;
  const isDevelopment = snapshot?.channel === 'development';
  channel.hidden = !isDevelopment;
  channel.textContent = isDevelopment ? text('development') : '';
  element('version').textContent = health?.version ?? '—';
  element('protocol').textContent = health?.protocol_version.toString() ?? '—';
  element('workers').textContent = health?.active_workers.toString() ?? '—';
  const error = connectionError ?? (snapshot?.status === 'error' ? snapshot.detail : undefined);
  const state = pending ? 'pending' : error ? 'error' : snapshot?.status ?? 'pending';
  indicator.dataset.status = state;
  const railMessages = { ready: 'railReady', offline: 'railOffline', error: 'railError', pending: 'railPending' } as const;
  element('rail-status').textContent = text(railMessages[state]);
  start.hidden = state === 'ready' || state === 'pending';
  start.textContent = text(state === 'error' ? 'retry' : 'start');

  switch (state) {
    case 'pending':
      status.textContent = text(pending === 'start_service' ? 'starting' : 'checking');
      detail.textContent = '';
      break;
    case 'ready':
      status.textContent = text('running');
      detail.textContent = text('runningDetail');
      break;
    case 'offline':
      status.textContent = text('offline');
      detail.textContent = text('offlineDetail');
      break;
    case 'error':
      status.textContent = text('connectionError');
      detail.textContent = error ? technicalDetails(error) : '';
      break;
  }
}

async function update(command: ServiceCommand) {
  pending = command;
  refresh.disabled = start.disabled = true;
  renderService();
  try {
    snapshot = await invoke<Snapshot>(command);
    connectionError = undefined;
  } catch (error) {
    snapshot = undefined;
    connectionError = String(error);
  } finally {
    pending = undefined;
    refresh.disabled = start.disabled = false;
    renderService();
    const ready = snapshot?.status === 'ready';
    history.setReady(ready);
    projects.setReady(ready);
  }
}

refresh.addEventListener('click', () => { void update('service_status'); });
start.addEventListener('click', () => { void update('start_service'); });

const pages = ['projects', 'history', 'overview', 'settings'] as const;
function showPage(page: typeof pages[number]) {
  for (const item of pages) {
    element(item).hidden = item !== page;
    const nav = element(`${item}-nav`);
    if (item === page) nav.setAttribute('aria-current', 'page');
    else nav.removeAttribute('aria-current');
  }
  if (page === 'history') history.open();
  if (page === 'projects') void projects.refresh();
}

for (const page of pages) element(`${page}-nav`).addEventListener('click', () => showPage(page));

function renderNotice() {
  settingsMessage.textContent = notice
    ? `${text(notice.message)}${notice.detail ? ` ${technicalDetails(notice.detail)}` : ''}`
    : '';
}

function setLanguage(language: Language) {
  locale = resolveLocale(language, navigator.language);
  document.documentElement.lang = locale;
  for (const node of document.querySelectorAll<HTMLElement>('[data-i18n]')) {
    node.textContent = text(node.dataset.i18n as Message);
  }
  for (const node of document.querySelectorAll<HTMLElement>('[data-i18n-label]')) {
    node.setAttribute('aria-label', text(node.dataset.i18nLabel as Message));
  }
  renderService();
  renderNotice();
  history.render();
  projects.render();
}

function showPreferences(current: Preferences) {
  languageSetting.value = current.language;
  setLanguage(current.language);
}

async function savePreferences(next: Preferences) {
  if (!preferences) return;
  languageSetting.disabled = true;
  notice = { message: 'saving' };
  showPreferences(next);
  try {
    await invoke('set_preferences', { preferences: next });
    preferences = next;
    notice = { message: 'saved' };
  } catch (error) {
    notice = { message: 'preferencesWriteError', detail: String(error) };
    showPreferences(preferences);
  } finally {
    languageSetting.disabled = false;
    renderNotice();
  }
}

languageSetting.addEventListener('change', () => {
  void savePreferences({ language: languageSetting.value as Language });
});

async function initialize() {
  const startup = update('start_service');
  try {
    preferences = await invoke<Preferences>('get_preferences');
    showPreferences(preferences);
    languageSetting.disabled = false;
  } catch (error) {
    notice = { message: 'preferencesReadError', detail: String(error) };
    renderNotice();
  }
  await startup;
}

setLanguage('system');
void initialize();
