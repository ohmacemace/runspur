import { invoke, isDesktop } from './api';
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
type WorkerAccess = 'project' | 'full' | 'follow_main';
type SchedulerSettings = { max_concurrent_workers: number; worker_access: WorkerAccess };
type SchedulerSettingsUpdate = { maxConcurrentWorkers: number } | { workerAccess: WorkerAccess };
type ServiceCommand = 'service_status' | 'start_service' | 'stop_service';

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
const stop = element<HTMLButtonElement>('stop');
const languageSetting = element<HTMLSelectElement>('language-setting');
const settingsMessage = element('settings-message');
const schedulerSettingsForm = element<HTMLFormElement>('scheduler-settings-form');
const schedulerLimit = element<HTMLInputElement>('scheduler-limit');
const schedulerLimitSave = element<HTMLButtonElement>('scheduler-limit-save');
const workerAccess = element<HTMLSelectElement>('worker-access');
const schedulerSettingsMessage = element('scheduler-settings-message');
let preferences: Preferences | undefined;
let locale = resolveLocale('system', navigator.language);
let snapshot: Snapshot | undefined;
let pending: ServiceCommand | undefined;
let connectionError: string | undefined;
let notice: { message: Message; detail?: string } | undefined;
let schedulerSettings: SchedulerSettings | undefined;
let schedulerSettingsBusy = false;
let schedulerNotice: { message: Message; detail?: string } | undefined;

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
  stop.hidden = isDesktop || state !== 'ready';

  switch (state) {
    case 'pending':
      status.textContent = text(pending === 'start_service' ? 'starting'
        : pending === 'stop_service' ? 'stopping' : 'checking');
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
  refresh.disabled = start.disabled = stop.disabled = true;
  renderService();
  renderSchedulerSettings();
  try {
    snapshot = await invoke<Snapshot>(command);
    connectionError = undefined;
  } catch (error) {
    snapshot = undefined;
    connectionError = String(error);
  } finally {
    pending = undefined;
    refresh.disabled = start.disabled = stop.disabled = false;
    renderService();
    const ready = snapshot?.status === 'ready';
    history.setReady(ready);
    projects.setReady(ready);
    if (!ready) showSchedulerSettings(undefined);
    renderSchedulerSettings();
    if (ready && (command === 'start_service' || !element('settings').hidden)) {
      void loadSchedulerSettings();
    }
  }
}

refresh.addEventListener('click', () => { void update('service_status'); });
start.addEventListener('click', () => { void update('start_service'); });
if (!isDesktop) stop.addEventListener('click', () => { void update('stop_service'); });

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
  if (page === 'settings') void loadSchedulerSettings();
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
  renderSchedulerSettings();
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

function renderSchedulerSettings() {
  const ready = snapshot?.status === 'ready' && !pending;
  schedulerLimit.disabled = schedulerLimitSave.disabled = !ready || schedulerSettingsBusy || !schedulerSettings;
  workerAccess.disabled = schedulerLimit.disabled || !schedulerSettings?.worker_access;
  const current = pending ? { message: 'checking' as const }
    : ready ? schedulerNotice : { message: 'schedulerSettingsOffline' as const };
  schedulerSettingsMessage.textContent = current
    ? `${text(current.message)}${current.detail ? ` ${technicalDetails(current.detail)}` : ''}`
    : '';
}

function showSchedulerSettings(current: SchedulerSettings | undefined, preserveLimitDraft = false) {
  if (!preserveLimitDraft || schedulerLimit.value === schedulerSettings?.max_concurrent_workers.toString()) {
    schedulerLimit.value = current?.max_concurrent_workers.toString() ?? '';
    schedulerLimit.removeAttribute('aria-invalid');
  }
  schedulerSettings = current;
  workerAccess.value = current?.worker_access ?? 'project';
}

async function loadSchedulerSettings() {
  if (snapshot?.status !== 'ready' || pending || schedulerSettingsBusy) return;
  schedulerSettingsBusy = true;
  schedulerNotice = undefined;
  renderSchedulerSettings();
  try {
    showSchedulerSettings(await invoke<SchedulerSettings>('get_scheduler_settings'));
  } catch (error) {
    showSchedulerSettings(undefined);
    schedulerNotice = { message: 'schedulerSettingsReadError', detail: String(error) };
  } finally {
    schedulerSettingsBusy = false;
    renderSchedulerSettings();
  }
}

async function saveSchedulerLimit() {
  if (schedulerLimit.disabled) return;
  const maxConcurrentWorkers = schedulerLimit.valueAsNumber;
  if (!Number.isInteger(maxConcurrentWorkers) || maxConcurrentWorkers < 1 || maxConcurrentWorkers > 64) {
    schedulerLimit.setAttribute('aria-invalid', 'true');
    schedulerNotice = { message: 'schedulerLimitInvalid' };
    renderSchedulerSettings();
    return;
  }
  schedulerLimit.removeAttribute('aria-invalid');
  await saveSchedulerSettings({ maxConcurrentWorkers });
}

async function saveSchedulerSettings(update: SchedulerSettingsUpdate) {
  schedulerSettingsBusy = true;
  schedulerNotice = { message: 'saving' };
  renderSchedulerSettings();
  try {
    const current = await invoke<SchedulerSettings>('set_scheduler_settings', update);
    showSchedulerSettings(current, 'workerAccess' in update);
    schedulerNotice = { message: 'saved' };
  } catch (error) {
    workerAccess.value = schedulerSettings?.worker_access ?? 'project';
    schedulerNotice = { message: 'schedulerSettingsWriteError', detail: String(error) };
  } finally {
    schedulerSettingsBusy = false;
    renderSchedulerSettings();
  }
}

schedulerSettingsForm.addEventListener('submit', event => {
  event.preventDefault();
  void saveSchedulerLimit();
});

workerAccess.addEventListener('change', () => {
  if (!workerAccess.disabled) void saveSchedulerSettings({ workerAccess: workerAccess.value as WorkerAccess });
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
