import { invoke } from './api';
import { element, label, pathLabel, showNotice, type Notice, type Translate } from './dom';
import type { Message } from './i18n';

type AgentKind = 'codex_desktop' | 'codex_exec' | 'claude_print';

const agentFamilies: Record<AgentKind, string> = {
  codex_desktop: 'codex',
  codex_exec: 'codex',
  claude_print: 'claude',
};

export type ProjectRecord = {
  root: string;
  main_agent_kind?: AgentKind | null;
  config: {
    project_id: string;
    name: string;
    main_agent_profile_id: string;
    allowed_worker_profile_ids: string[];
  };
};

type WritePolicy = {
  allow_workspace_write: boolean;
  allow_project_write: boolean;
};

const writeChoices = {
  read_only: { label: 'workerReadOnly', detail: 'workerReadOnlyHint' },
  workspace_write: { label: 'workerScopedWrite', detail: 'workerScopedWriteHint' },
  project_write: { label: 'workerProjectWrite', detail: 'workerProjectWriteHint' },
} as const;

function writeAccess(policy: WritePolicy): keyof typeof writeChoices {
  if (policy.allow_project_write) return 'project_write';
  return policy.allow_workspace_write ? 'workspace_write' : 'read_only';
}

type Draft = WritePolicy & {
  root: string;
  registered: boolean;
  name: string;
  main_agent_profile_id: string;
  worker_profile_ids: string[];
};

type AgentProfile = { profile_id: string; kind: AgentKind; parent: boolean; worker: boolean };
type Preview = WritePolicy & {
  token: string;
  manifest_path: string;
  skill_path: string;
  skill_content: string;
  mcp_path: string;
  mcp_content: string;
  hook_path?: string | null;
  hook_content?: string | null;
  cleanup: { path: string; entry: string }[];
};

const cleanupLabels: Partial<Record<string, Message>> = {
  skill: 'projectSkill',
  runspur_scheduler: 'projectMcp',
  developer_instructions: 'projectDeveloperInstructions',
  claude_origin_hook: 'projectClaudeHook',
};

function setupFailure(message: Message, error: unknown): Notice {
  const detail = error instanceof Error ? error.message : String(error);
  const stateConflict = '[runspur_state_conflict]';
  return detail.includes(stateConflict)
    ? { message: 'projectDifferentState', detail: detail.replace(stateConflict, '').trim() }
    : { message, detail };
}

export function createProjects(
  text: Translate,
  onProjects: (items: ProjectRecord[]) => void,
  openHistory: (projectId: string) => void,
) {
  const section = element('projects');
  const choose = element<HTMLButtonElement>('choose-project');
  const close = element<HTMLButtonElement>('close-project-setup');
  const list = element('project-list');
  const form = element<HTMLFormElement>('project-form');
  const rootPath = element('project-root');
  const nameField = element<HTMLInputElement>('project-name');
  const parentField = element<HTMLSelectElement>('parent-agent');
  const workerList = element('worker-agents');
  const writeFields = Array.from(element('worker-file-access').querySelectorAll<HTMLInputElement>('input'));
  const previewButton = element<HTMLButtonElement>('preview-project');
  const previewPanel = element('project-preview');
  const hookPreview = element<HTMLDetailsElement>('preview-hook');
  const edit = element<HTMLButtonElement>('edit-project');
  const apply = element<HTMLButtonElement>('apply-project');
  const refreshAgents = element<HTMLButtonElement>('refresh-agents');
  const setupFeedback = element('project-setup-feedback');
  const setupMessage = element('project-setup-message');
  const setupDetails = element<HTMLDetailsElement>('project-setup-details');
  const setupError = element('project-setup-error');
  const homeFeedback = element('project-feedback');
  let projects: ProjectRecord[] = [];
  let profiles: AgentProfile[] = [];
  let draft: Draft | undefined;
  let preview: Preview | undefined;
  let ready = false;
  let busy = false;
  let loaded = false;
  let notice: Notice | undefined;
  let feedback: Notice | undefined;

  const parents = () => profiles.filter((item) => item.parent);

  function sameFamily(worker: AgentProfile, parentId: string | undefined) {
    const parent = profiles.find((item) => item.profile_id === parentId);
    return parent !== undefined && agentFamilies[worker.kind] === agentFamilies[parent.kind];
  }

  const workers = () => profiles.filter((item) => item.worker && !sameFamily(item, draft?.main_agent_profile_id));

  function profileName(profileId: string, kind?: string) {
    const labels = [
      ['codex_desktop', 'codex-desktop', 'Codex'],
      ['codex_exec', 'codex-cli', 'Codex'],
      ['claude_print', 'claude-cli', 'Claude Code'],
    ];
    const entry = labels.find(([profileKind, defaultId]) =>
      kind === undefined ? defaultId === profileId : profileKind === kind);
    if (!entry) return profileId;
    const [, defaultId, name] = entry;
    return profileId === defaultId ? name : `${name} · ${profileId}`;
  }

  function draftAgentName(profileId: string) {
    return profileName(profileId, profiles.find((profile) => profile.profile_id === profileId)?.kind);
  }

  function renderControls() {
    choose.disabled = busy || !ready;
    close.disabled = edit.disabled = busy;
    refreshAgents.disabled = busy || !ready;
    nameField.disabled = parentField.disabled = !ready || busy;
    const selectedKinds = draft?.worker_profile_ids.map((id) =>
      profiles.find((profile) => profile.profile_id === id)?.kind) ?? [];
    const readOnlyWorkers = selectedKinds.length > 0
      && selectedKinds.every((kind) => kind === 'codex_exec');
    if (draft && readOnlyWorkers) {
      draft.allow_workspace_write = false;
      draft.allow_project_write = false;
    }
    writeFields.forEach((field) => {
      field.checked = draft ? field.value === writeAccess(draft) : false;
      field.disabled = !ready || busy || (readOnlyWorkers && field.value !== 'read_only');
    });
    const normalHint = selectedKinds.includes('codex_exec') ? 'codexReadOnlyHint' : 'workerAccessHint';
    const accessHint = text(readOnlyWorkers ? 'workersReadOnlyHint' : normalHint);
    const accessHintNode = element('worker-access-hint');
    if (accessHintNode.textContent !== accessHint) accessHintNode.textContent = accessHint;
    workerList.querySelectorAll('input').forEach((input) => { input.disabled = !ready || busy; });
    const valid = draft && draft.name.trim() && draft.worker_profile_ids.length > 0
      && parents().some((item) => item.profile_id === draft?.main_agent_profile_id)
      && draft.worker_profile_ids.every((id) => workers().some((item) => item.profile_id === id));
    element('worker-hint').hidden = !draft || workers().length === 0 || draft.worker_profile_ids.length > 0;
    previewButton.disabled = busy || !ready || !valid;
    apply.disabled = previewButton.disabled || !preview;
  }

  function renderPreview() {
    form.hidden = !draft || Boolean(preview);
    previewPanel.hidden = !preview;
    hookPreview.hidden = !preview?.hook_path;
    if (hookPreview.hidden) hookPreview.open = false;
    if (preview) {
      element('configure-step').removeAttribute('aria-current');
      element('review-step').setAttribute('aria-current', 'step');
    } else {
      element('configure-step').setAttribute('aria-current', 'step');
      element('review-step').removeAttribute('aria-current');
      element<HTMLDetailsElement>('preview-skill').open = false;
      element<HTMLDetailsElement>('preview-mcp').open = false;
    }
    const choice = preview ? writeChoices[writeAccess(preview)] : undefined;
    element('preview-name').textContent = preview && draft ? draft.name.trim() : '';
    element('preview-parent').textContent = preview && draft ? draftAgentName(draft.main_agent_profile_id) : '';
    element('preview-workers').textContent = preview && draft ? draft.worker_profile_ids.map(draftAgentName).join(', ') : '';
    for (const [id, path] of [
      ['preview-manifest', preview?.manifest_path],
      ['preview-skill-path', preview?.skill_path],
      ['preview-mcp-path', preview?.mcp_path],
      ['preview-hook-path', preview?.hook_path],
    ] as const) {
      const display = path ? pathLabel(path) : '';
      const destination = element(id);
      destination.textContent = destination.title = display;
    }
    element('preview-write-policy').textContent = choice ? text(choice.label) : '';
    element('preview-write-detail').textContent = choice ? text(choice.detail) : '';
    element('preview-skill-content').textContent = preview?.skill_content ?? '';
    element('preview-mcp-content').textContent = preview?.mcp_content ?? '';
    element('preview-hook-content').textContent = preview?.hook_content ?? '';
    const cleanup = preview?.cleanup ?? [];
    element('preview-cleanup').hidden = cleanup.length === 0;
    element('preview-cleanup-items').replaceChildren(...cleanup.map(({ path, entry }) => {
      const row = label('div', '', 'preview-file');
      const message = cleanupLabels[entry];
      const destination = label('code', pathLabel(path));
      destination.title = destination.textContent;
      row.append(label('strong', message ? text(message) : entry), destination);
      return row;
    }));
  }

  function draftChanged() {
    preview = undefined;
    feedback = undefined;
    renderPreview();
    renderFeedback();
    renderControls();
  }

  function renderFeedback() {
    const active: Notice | undefined = draft ? (!ready ? { message: 'schedulerNeeded' } : feedback) : undefined;
    const detail = active?.detail ?? '';
    if (setupError.textContent !== detail) setupDetails.open = false;
    setupFeedback.hidden = !active;
    setupFeedback.dataset.error = String(Boolean(detail));
    setupMessage.textContent = active ? text(active.message) : '';
    setupDetails.hidden = !detail;
    setupError.textContent = detail;
    homeFeedback.dataset.complete = String(!draft && (feedback?.message === 'projectSaved'
      || feedback?.message === 'projectUpdated' || feedback?.message === 'projectSavedNativeClaude'));
    showNotice(homeFeedback, text, draft ? undefined : feedback);
  }

  function render() {
    list.replaceChildren(...projects.map((record) => {
      const row = label('div', '', 'project-row');
      const button = label('button', '', 'dispatch-entry');
      button.type = 'button';
      button.disabled = busy || !ready;
      button.append(label('strong', record.config.name), label('span', pathLabel(record.root)));
      button.addEventListener('click', () => openHistory(record.config.project_id));
      const configure = label('button', text('configureProject'), 'text-button');
      configure.type = 'button';
      configure.disabled = busy || !ready;
      configure.setAttribute('aria-label', `${text('configureProject')} · ${pathLabel(record.root)}`);
      configure.addEventListener('click', () => { void readProject(record.root); });
      row.append(button, configure);
      return row;
    }));
    element('project-home').hidden = Boolean(draft);
    element('project-setup').hidden = !draft;
    element('project-empty').hidden = !ready || busy || Boolean(notice) || projects.length > 0;
    if (draft) {
      element('project-setup-title').textContent = text(draft.registered ? 'projectEditTitle' : 'chooseProject');
      element('project-registered-note').hidden = !draft.registered;
      apply.textContent = text(draft.registered ? 'saveProjectChanges' : 'applyProject');
      rootPath.textContent = pathLabel(draft.root);
      nameField.value = draft.name;
      const mainProfiles = parents();
      const parentId = draft.main_agent_profile_id;
      const mainOptions = mainProfiles.map(({ profile_id, kind }) => new Option(profileName(profile_id, kind), profile_id));
      if (parentId && !mainProfiles.some((item) => item.profile_id === parentId)) {
        mainOptions.unshift(new Option(`${profileName(parentId)} · ${text('missingWorker')}`, parentId));
      }
      parentField.replaceChildren(...mainOptions);
      parentField.value = parentId;

      const available = workers();
      const unknown = draft.worker_profile_ids.filter((id) => !available.some((item) => item.profile_id === id));
      const choices = available.map(({ profile_id, kind }) => ({ id: profile_id, caption: profileName(profile_id, kind) }));
      for (const id of unknown) choices.push({ id, caption: `${profileName(id)} · ${text('missingWorker')}` });
      workerList.replaceChildren(...choices.map(({ id, caption }) => {
        const choice = label('label', '', 'worker-option');
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.value = id;
        checkbox.checked = draft?.worker_profile_ids.includes(id) ?? false;
        checkbox.addEventListener('change', () => {
          if (!draft) return;
          draft.worker_profile_ids = Array.from(workerList.querySelectorAll<HTMLInputElement>('input:checked'), (item) => item.value);
          draftChanged();
        });
        choice.append(checkbox, label('span', caption));
        return choice;
      }));
      if (available.length === 0) workerList.append(label('p', text('noWorkers'), 'empty-state'));
    }
    showNotice(element('projects-notice'), text, !ready ? { message: 'schedulerNeeded' } : notice);
    renderFeedback();
    renderPreview();
    renderControls();
  }

  async function refresh() {
    if (!ready || busy || loaded) return;
    busy = true;
    notice = { message: 'loading' };
    render();
    try {
      projects = await invoke<ProjectRecord[]>('projects_list');
      onProjects(projects);
      notice = undefined;
      loaded = true;
    } catch (error) {
      notice = { message: 'projectsLoadError', detail: String(error) };
    } finally {
      busy = false;
      render();
    }
  }

  async function loadProfiles() {
    try {
      profiles = await invoke<AgentProfile[]>('agent_profiles');
      if (draft) {
        if (!draft.main_agent_profile_id) draft.main_agent_profile_id = parents()[0]?.profile_id ?? '';
        const available = workers();
        draft.worker_profile_ids = draft.worker_profile_ids.filter((id) =>
          available.some((profile) => profile.profile_id === id));
      }
    } catch (error) {
      feedback = { message: 'agentLoadError', detail: String(error) };
    }
  }

  async function readProject(root?: string) {
    if (busy || !ready) return;
    busy = true;
    feedback = undefined;
    notice = { message: 'loading' };
    render();
    let choosing = !root;
    try {
      const path = root ?? await invoke<string | null>('choose_project_folder');
      choosing = false;
      if (path) {
        draft = await invoke<Draft>('project_read', { root: path });
        preview = undefined;
        profiles = [];
        await loadProfiles();
      }
      notice = undefined;
    } catch (error) {
      notice = { message: choosing ? 'projectBrowseError' : 'projectLoadError', detail: String(error) };
    } finally {
      busy = false;
      render();
      if (draft && !notice) nameField.focus();
    }
  }

  choose.addEventListener('click', () => { void readProject(); });
  close.addEventListener('click', () => {
    draft = undefined;
    preview = undefined;
    feedback = undefined;
    render();
    choose.focus();
  });
  edit.addEventListener('click', () => {
    draftChanged();
    nameField.focus();
  });
  nameField.addEventListener('input', () => {
    if (draft) draft.name = nameField.value;
    draftChanged();
  });
  parentField.addEventListener('change', () => {
    if (!draft) return;
    const parentId = parentField.value;
    draft.main_agent_profile_id = parentId;
    draft.worker_profile_ids = draft.worker_profile_ids.filter((id) => {
      const profile = profiles.find((item) => item.profile_id === id);
      return !profile || !sameFamily(profile, parentId);
    });
    preview = undefined;
    feedback = undefined;
    render();
  });
  for (const field of writeFields) {
    field.addEventListener('change', () => {
      if (!draft || !field.checked) return;
      draft.allow_workspace_write = field.value !== 'read_only';
      draft.allow_project_write = field.value === 'project_write';
      draftChanged();
    });
  }
  refreshAgents.addEventListener('click', async () => {
    if (busy || !ready) return;
    busy = true;
    preview = undefined;
    feedback = { message: 'loading' };
    render();
    await loadProfiles();
    if (feedback?.message === 'loading') feedback = undefined;
    busy = false;
    render();
  });
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!draft || previewButton.disabled) return;
    busy = true;
    preview = undefined;
    feedback = { message: 'loading' };
    render();
    try {
      const planned = await invoke<Preview>('project_preview', { input: { ...draft, name: draft.name.trim() } });
      if (ready) preview = planned;
      feedback = undefined;
    } catch (error) {
      feedback = setupFailure('projectPreviewError', error);
    } finally {
      busy = false;
      render();
      if (preview) element('preview-title').focus();
      else if (feedback?.detail) setupFeedback.focus();
    }
  });

  apply.addEventListener('click', async () => {
    if (!preview || !draft || apply.disabled) return;
    const token = preview.token;
    const nativeClaude = Boolean(preview.hook_path);
    const updating = draft.registered;
    preview = undefined;
    busy = true;
    feedback = { message: 'saving' };
    render();
    try {
      const saved = await invoke<ProjectRecord>('project_apply', { token });
      saved.main_agent_kind = profiles.find((profile) => profile.profile_id === saved.config.main_agent_profile_id)?.kind;
      projects = [...projects.filter((item) => item.config.project_id !== saved.config.project_id), saved];
      projects.sort((left, right) => left.config.name.localeCompare(right.config.name));
      onProjects(projects);
      draft = undefined;
      feedback = { message: nativeClaude ? 'projectSavedNativeClaude' : updating ? 'projectUpdated' : 'projectSaved' };
    } catch (error) {
      feedback = setupFailure('projectSaveError', error);
    } finally {
      busy = false;
      render();
      if (!draft) choose.focus();
      else if (feedback?.detail) setupFeedback.focus();
    }
  });

  return {
    render,
    refresh,
    setReady(value: boolean) {
      if (ready === value) return;
      ready = value;
      if (!ready) {
        loaded = false;
        preview = undefined;
      }
      render();
      if (!section.hidden) void refresh();
    },
  };
}
