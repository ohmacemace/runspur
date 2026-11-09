import { invoke, isDesktop } from './api';
import { element, label, showNotice, type Notice, type Translate } from './dom';
import type { Message } from './i18n';
import type { ProjectRecord } from './projects';

const execution = {
  queued: 'queued',
  starting: 'startingTask',
  running: 'runningTask',
  waiting_approval: 'waitingApproval',
  succeeded: 'succeeded',
  failed: 'failed',
  canceled: 'canceled',
  timed_out: 'timedOut',
} as const satisfies Record<string, Message>;

const delivery = {
  not_ready: 'notReady',
  pending: 'pending',
  sending: 'sending',
  accepted_by_transport: 'acceptedByTransport',
  acknowledged_by_parent: 'acknowledgedByParent',
  held: 'held',
  refused: 'refused',
  retry_wait: 'retryWait',
  orphaned: 'orphaned',
  uncertain: 'uncertain',
} as const satisfies Record<string, Message>;

type Session = { origin_id: string; adapter_id: string; session_id: string; task_count: number; transport?: string };
type Dispatch = {
  task_id: string;
  project_id: string;
  worker_profile_id: string;
  execution_state: keyof typeof execution;
  delivery_state: keyof typeof delivery;
  created_at_ms: number;
};
type TaskDetail = {
  task: Dispatch & {
    delivery_id: string;
    started_at_ms: number | null;
    finished_at_ms: number | null;
    parent_turn_id: string | null;
    delivery_error: string | null;
    result: {
      summary: string;
      exit_code: number | null;
      error_class: string | null;
      verification: { description: string; outcome: string }[];
      artifact_refs: string[];
    } | null;
  };
  prompt: string;
  timeout_seconds: number;
  session_id: string | null;
  origin_id: string;
};
type Page<T> = { items: T[]; next_cursor: number | null };
type Load = 'refresh' | 'project' | 'session' | 'olderSessions' | 'olderTasks';

export function createHistory(text: Translate) {
  const section = element('history');
  const project = element<HTMLSelectElement>('history-project');
  const session = element<HTMLSelectElement>('history-session');
  const refresh = element<HTMLButtonElement>('refresh-history');
  const moreSessions = element<HTMLButtonElement>('load-sessions');
  const resumeClaude = element<HTMLButtonElement>('resume-claude-session');
  const moreTasks = element<HTMLButtonElement>('load-history');
  const list = element('history-list');
  const detail = element('task-detail');
  const content = element('task-content');

  let ready = false;
  let loading = false;
  let resuming = false;
  let loaded = false;
  let historyVersion = 0;
  let detailVersion = 0;
  let projects: ProjectRecord[] = [];
  let sessions: Session[] = [];
  let tasks: Dispatch[] = [];
  let projectId = '';
  let originId = '';
  let sessionCursor: number | null = null;
  let taskCursor: number | null = null;
  let taskId = '';
  let taskDetail: TaskDetail | undefined;
  let notice: Notice | undefined;
  let detailNotice: Notice | undefined;

  function selectedClaudeSession() {
    if (!isDesktop || !projectId || !originId) return;
    const record = projects.find(({ config }) => config.project_id === projectId);
    if (record?.main_agent_kind !== 'claude_print') return;
    const origin = sessions.find(({ origin_id }) => origin_id === originId);
    if (origin?.adapter_id !== 'claude_code_managed' || origin.transport === 'native_inbox' || !origin.session_id) return;
    return { root: record.root, resume: origin.session_id };
  }

  function date(timestamp: number): string {
    return new Intl.DateTimeFormat(document.documentElement.lang, {
      dateStyle: 'medium', timeStyle: 'short',
    }).format(timestamp);
  }

  function renderDetail() {
    element('history-inspector').hidden = tasks.length === 0 && !taskId;
    element('task-empty').hidden = Boolean(taskId);
    detail.hidden = !taskId;
    showNotice(element('task-notice'), text, detailNotice);
    content.replaceChildren();
    if (!taskDetail) return;

    const { task, prompt, origin_id, session_id, timeout_seconds } = taskDetail;
    const facts = label('dl', '', 'task-readout');
    const fact = (message: Message, value: string | number | null) => {
      if (value == null || value === '') return;
      const item = document.createElement('div');
      item.append(label('dt', text(message)), label('dd', String(value)));
      facts.append(item);
    };
    fact('filterProject', projects.find(({ config }) => config.project_id === task.project_id)?.config.name ?? task.project_id);
    fact('sessionId', session_id);
    fact('originId', origin_id);
    fact('taskId', task.task_id);
    fact('worker', task.worker_profile_id);
    fact('execution', text(execution[task.execution_state]));
    fact('delivery', text(delivery[task.delivery_state]));
    fact('queuedAt', date(task.created_at_ms));
    fact('startedAt', task.started_at_ms == null ? null : date(task.started_at_ms));
    fact('finishedAt', task.finished_at_ms == null ? null : date(task.finished_at_ms));
    fact('timeout', new Intl.NumberFormat(document.documentElement.lang, {
      style: 'unit', unit: 'second',
    }).format(timeout_seconds));
    fact('deliveryId', task.delivery_id);
    fact('parentTurnId', task.parent_turn_id);
    fact('deliveryError', task.delivery_error);
    if (task.result) {
      fact('exitCode', task.result.exit_code);
      fact('errorClass', task.result.error_class);
    }

    content.append(facts, label('h3', text('prompt')), label('pre', prompt, 'task-text'));
    content.append(label('h3', text('result')));
    if (!task.result) {
      content.append(label('p', text('noResult')));
      return;
    }

    content.append(label('pre', task.result.summary, 'task-text'));
    for (const [message, values] of [
      ['verification', task.result.verification.map(({ description, outcome }) => `${description} · ${outcome}`)],
      ['artifacts', task.result.artifact_refs],
    ] as const) {
      if (values.length === 0) continue;
      const entries = label('ul', '', 'task-result-list');
      entries.append(...values.map((value) => label('li', value)));
      content.append(label('h3', text(message)), entries);
    }
  }

  function render() {
    project.replaceChildren(
      new Option(text('allProjects'), ''),
      ...projects.map(({ config }) => new Option(config.name, config.project_id)),
    );
    project.value = projectId;
    const sessionPlaceholder = !projectId ? 'chooseProjectForSessions'
      : sessions.length || originId ? 'allSessions' : 'noSessions';
    session.replaceChildren(
      new Option(text(sessionPlaceholder), ''),
      ...sessions.map(({ origin_id, adapter_id, session_id, task_count, transport }) =>
        new Option(`${session_id} · ${transport === 'native_inbox' ? 'Claude Code' : adapter_id} · ${task_count}`, origin_id)),
    );
    if (originId && !sessions.some(({ origin_id }) => origin_id === originId)) {
      session.add(new Option(originId, originId));
    }
    session.value = originId;
    project.disabled = refresh.disabled = !ready || loading;
    session.disabled = !ready || loading || !projectId || session.options.length < 2;
    moreSessions.hidden = sessionCursor == null;
    moreTasks.hidden = taskCursor == null;
    moreSessions.disabled = moreTasks.disabled = !ready || loading;
    resumeClaude.hidden = !selectedClaudeSession();
    resumeClaude.disabled = !ready || loading || resuming;
    showNotice(element('history-notice'), text, ready ? notice : { message: 'schedulerNeeded' });

    list.replaceChildren(...tasks.map((task) => {
      const button = label('button', '', 'entry-button dispatch-entry');
      button.type = 'button';
      button.disabled = loading;
      button.dataset.taskId = task.task_id;
      button.setAttribute('aria-pressed', String(task.task_id === taskId));
      button.append(
        label('strong', task.worker_profile_id),
        label('span', `${text(execution[task.execution_state])} · ${text(delivery[task.delivery_state])}`),
        label('span', date(task.created_at_ms)),
      );
      button.addEventListener('click', () => { void readTask(task.task_id); });
      return button;
    }));
    if (ready && !loading && !notice && tasks.length === 0) {
      list.append(label('p', text('noDispatches'), 'empty-state'));
    }
    renderDetail();
  }

  async function readTask(id: string) {
    const version = ++detailVersion;
    taskId = id;
    taskDetail = undefined;
    detailNotice = { message: 'loading' };
    for (const entry of list.querySelectorAll<HTMLButtonElement>('button')) {
      entry.setAttribute('aria-pressed', String(entry.dataset.taskId === id));
    }
    renderDetail();
    try {
      const response = await invoke<TaskDetail>('query_task_detail', { taskId: id });
      if (version !== detailVersion) return;
      taskDetail = response;
      detailNotice = undefined;
    } catch (error) {
      if (version !== detailVersion) return;
      detailNotice = { message: 'taskError', detail: String(error) };
    }
    renderDetail();
  }

  function clearTask() {
    detailVersion++;
    taskId = '';
    taskDetail = undefined;
    detailNotice = undefined;
  }

  async function load(mode: Load) {
    if (!ready || (loading && mode !== 'project')) return;
    const version = ++historyVersion;
    loading = true;
    notice = { message: 'loading' };
    if (mode === 'project' || mode === 'session') {
      tasks = [];
      taskCursor = null;
      clearTask();
    }
    if (mode === 'project') {
      sessions = [];
      sessionCursor = null;
    }
    render();

    try {
      if (mode === 'refresh') {
        const response = await invoke<ProjectRecord[]>('projects_list');
        if (version !== historyVersion) return;
        projects = response;
      }
      if (projectId && (mode === 'refresh' || mode === 'project' || mode === 'olderSessions')) {
        const older = mode === 'olderSessions';
        const page = await invoke<Page<Session>>('query_sessions', {
          projectId, before: older ? sessionCursor : null, limit: 25,
        });
        if (version !== historyVersion) return;
        if (page.items.some(({ origin_id }) => !origin_id)) throw new Error(text('originServiceUpgrade'));
        sessions = older ? sessions.concat(page.items) : page.items;
        sessionCursor = page.next_cursor;
      }
      if (mode !== 'olderSessions') {
        const older = mode === 'olderTasks';
        const page = await invoke<Page<Dispatch>>('query_task_history', {
          projectId: projectId || null, originId: originId || null, before: older ? taskCursor : null, limit: 25,
        });
        if (version !== historyVersion) return;
        tasks = older ? tasks.concat(page.items) : page.items;
        taskCursor = page.next_cursor;
      }
      notice = undefined;
      loaded = true;
    } catch (error) {
      if (version !== historyVersion) return;
      notice = { message: 'historyError', detail: String(error) };
    } finally {
      if (version === historyVersion) {
        loading = false;
        render();
      }
    }
    if (!notice && mode === 'refresh' && taskId) void readTask(taskId);
  }

  function selectProject(id: string) {
    projectId = id;
    originId = '';
    void load('project');
  }

  project.addEventListener('change', () => selectProject(project.value));
  session.addEventListener('change', () => {
    originId = session.value;
    void load('session');
  });
  refresh.addEventListener('click', () => { void load('refresh'); });
  moreSessions.addEventListener('click', () => { void load('olderSessions'); });
  moreTasks.addEventListener('click', () => { void load('olderTasks'); });
  resumeClaude.addEventListener('click', async () => {
    const target = selectedClaudeSession();
    if (!target || !ready || loading || resuming) return;
    resuming = true;
    notice = undefined;
    render();
    try {
      await invoke('launch_claude_project', target);
    } catch (error) {
      notice = { message: 'resumeClaudeError', detail: String(error) };
    } finally {
      resuming = false;
      render();
    }
  });

  function open() {
    if (!loaded) void load('refresh');
  }

  return {
    render,
    open,
    selectProject,
    setProjects(items: ProjectRecord[]) {
      projects = items;
      render();
    },
    setReady(value: boolean) {
      if (ready === value) return;
      ready = value;
      if (!ready) {
        historyVersion++;
        loading = loaded = false;
        projects = [];
        sessions = [];
        tasks = [];
        projectId = originId = '';
        sessionCursor = taskCursor = null;
        clearTask();
      }
      render();
      if (!section.hidden) open();
    },
  };
}
