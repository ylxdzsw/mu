import "./components/mu-sidebar.js";
import "./components/mu-conversation-view.js";
import "./components/mu-composer.js";
import "./components/mu-project-modal.js";

import { ApiError, api } from "./lib/api.js";
import { GLOBAL_PROJECT_ID, TURN_EVENT_TYPES } from "./lib/constants.js";
import {
  buildProjects,
  resetProjectSelectionIfNeeded,
  selectedProject,
  selectedProjectQueryValue,
  sessionTitle,
} from "./lib/projects.js";
import { createStore, makeInitialState } from "./lib/store.js";

function normalizeModelGroups(payload) {
  const providers = Array.isArray(payload?.available_models?.providers)
    ? payload.available_models.providers
    : [];
  return providers
    .map((provider) => ({
      id: provider.id,
      displayName: provider.id,
      models: Array.isArray(provider.models)
        ? provider.models.map((model) => ({
          id: model.id,
          providerId: provider.id,
          modelId: model.model_id,
          displayName: model.model_id,
          variants: Array.isArray(model.supported_efforts) ? model.supported_efforts : [],
        }))
        : [],
    }))
    .filter((provider) => provider.models.length > 0);
}

function findModel(groups, modelId) {
  for (const group of groups) {
    for (const model of group.models) {
      if (model.id === modelId) {
        return model;
      }
    }
  }
  return null;
}

function firstModelId(groups) {
  return groups[0]?.models[0]?.id || "";
}

function selectedVariantForModel(groups, modelId, variant) {
  const model = findModel(groups, modelId);
  if (!model) return "";
  return model.variants.includes(variant) ? variant : "";
}

function selectionScopeKey(projectId, sessionId) {
  return `${projectId}::${sessionId || ""}`;
}

function buildCanonicalModelRef(modelId, variant) {
  if (!modelId) return null;
  return variant ? `${modelId}:${variant}` : modelId;
}

class MuWebApp {
  constructor() {
    this.store = createStore(makeInitialState());
    this.modelsRequestSerial = 0;
    this.sessionsRequestSerial = 0;
    this.transcriptRequestSerial = 0;
    this.activeTurnRequestSerial = 0;
    this.activeEventSource = null;
    this.activeReconnectTimer = null;

    this.app = document.getElementById("app");
    this.sidebar = document.getElementById("sidebar");
    this.conversation = document.getElementById("conversation-view");
    this.composer = document.getElementById("composer");
    this.projectModal = document.getElementById("project-modal");
    this.sidebarToggle = document.getElementById("sidebar-toggle");
    this.sidebarHitbox = document.getElementById("sidebar-hitbox");

    this.unsubscribe = this.store.subscribe(() => this.render());
  }

  state() {
    return this.store.getState();
  }

  update(mutator) {
    this.store.update(mutator);
  }

  routeState() {
    const params = new URLSearchParams(window.location.search);
    return {
      project: params.get("project"),
      session: params.get("session"),
    };
  }

  syncRoute() {
    const state = this.state();
    const params = new URLSearchParams();
    params.set("project", selectedProjectQueryValue(state));
    if (state.selectedSessionId) {
      params.set("session", state.selectedSessionId);
    }
    const query = params.toString();
    const next = query ? `${window.location.pathname}?${query}` : window.location.pathname;
    window.history.replaceState(null, "", next);
  }

  setSidebarOpen(open) {
    this.update((state) => {
      state.sidebarOpen = open;
    });
  }

  toggleSidebar() {
    this.setSidebarOpen(!this.state().sidebarOpen);
  }

  openProjectDialog() {
    this.update((state) => {
      state.projectDialog.open = true;
      state.projectDialog.error = null;
      state.projectDialog.confirmationPath = null;
      if (!state.projectDialog.path) {
        state.projectDialog.path =
          state.bootstrap?.launch_cwd || selectedProject(state).path || "";
      }
    });
    window.requestAnimationFrame(() => {
      this.projectModal.focusInput();
    });
  }

  closeProjectDialog() {
    this.update((state) => {
      state.projectDialog.open = false;
      state.projectDialog.submitting = false;
      state.projectDialog.confirmationPath = null;
      state.projectDialog.error = null;
    });
  }

  setProjectDialogPath(value) {
    this.update((state) => {
      state.projectDialog.path = value;
      state.projectDialog.error = null;
      state.projectDialog.confirmationPath = null;
    });
  }

  rememberOpenedProject(summary) {
    this.update((state) => {
      if (!state.bootstrap) {
        state.bootstrap = { launch_cwd: "", recent_projects: [] };
      }
      const recent = [
        summary,
        ...(state.bootstrap.recent_projects || []).filter((item) => item.path !== summary.path),
      ];
      state.bootstrap.recent_projects = recent.slice(0, 20);
      state.projects = buildProjects(state.bootstrap);
      resetProjectSelectionIfNeeded(state);
    });
  }

  transcriptAlreadyContainsPrompt(prompt) {
    if (!prompt) return false;
    const lastUser = [...this.state().transcript]
      .reverse()
      .find((message) => message.role === "user");
    return lastUser?.content === prompt;
  }

  cloneActiveTurnView(view) {
    return JSON.parse(JSON.stringify(view));
  }

  applySnapshotEvent(view, seq, event, payload) {
    if (!view || seq <= view.last_seq) return;
    view.last_seq = seq;
    if (!Array.isArray(view.snapshot.raw_events)) {
      view.snapshot.raw_events = [];
    }
    view.snapshot.raw_events.push({ seq, event, payload });
    if (event === "assistant_delta" && typeof payload?.text === "string") {
      view.snapshot.assistant_text += payload.text;
    }
    if (event === "stderr" && typeof payload?.text === "string") {
      view.snapshot.stderr = view.snapshot.stderr
        ? `${view.snapshot.stderr}\n${payload.text}`
        : payload.text;
    }
    if (event === "turn_finish") {
      view.completed = true;
      view.snapshot.exit_code =
        typeof payload?.exit_code === "number" ? payload.exit_code : null;
    }
  }

  closeActiveStream() {
    if (this.activeEventSource) {
      this.activeEventSource.close();
      this.activeEventSource = null;
    }
  }

  clearActiveReconnect() {
    if (this.activeReconnectTimer) {
      window.clearTimeout(this.activeReconnectTimer);
      this.activeReconnectTimer = null;
    }
  }

  scheduleActiveTurnRefresh(delayMs = 400) {
    this.clearActiveReconnect();
    this.activeReconnectTimer = window.setTimeout(() => {
      this.activeReconnectTimer = null;
      void this.loadActiveTurn({ subscribe: true });
    }, delayMs);
  }

  subscribeToTurn(turnId, after) {
    this.closeActiveStream();
    this.clearActiveReconnect();
    const source = new EventSource(
      `/api/turns/${encodeURIComponent(turnId)}/events?after=${after}`,
    );
    this.activeEventSource = source;

    for (const eventName of TURN_EVENT_TYPES) {
      source.addEventListener(eventName, (message) => {
        const state = this.state();
        if (this.activeEventSource !== source || state.activeTurn?.turn?.id !== turnId) {
          return;
        }
        if (eventName === "reset") {
          this.closeActiveStream();
          this.scheduleActiveTurnRefresh(0);
          return;
        }
        const payload = JSON.parse(message.data);
        const seq = Number(message.lastEventId || 0);
        this.update((draft) => {
          this.applySnapshotEvent(draft.activeTurn, seq, eventName, payload);
        });
        if (eventName === "turn_finish") {
          this.closeActiveStream();
          void this.finalizeCompletedTurn(turnId, this.state().selectedSessionId);
        }
      });
    }

    source.onerror = () => {
      if (this.activeEventSource !== source) return;
      this.closeActiveStream();
      this.scheduleActiveTurnRefresh();
    };
  }

  async bootstrap() {
    let bootstrap = null;
    try {
      bootstrap = await api("/api/bootstrap");
    } catch (_) {
      bootstrap = null;
    }

    this.update((state) => {
      state.bootstrap = bootstrap;
      state.projects = buildProjects(bootstrap);
      const route = this.routeState();
      if (route.project && route.project !== GLOBAL_PROJECT_ID) {
        if (!state.projects.some((project) => project.id === route.project)) {
          state.projects.push({
            id: route.project,
            queryValue: route.project,
            path: route.project,
            marker: "mu",
            name: route.project.split("/").filter(Boolean).at(-1) || route.project,
            global: false,
            initials: "P",
          });
        }
        state.selectedProjectId = route.project;
      } else if (route.project === GLOBAL_PROJECT_ID) {
        state.selectedProjectId = GLOBAL_PROJECT_ID;
      } else {
        state.selectedProjectId = bootstrap?.launch_project?.path || GLOBAL_PROJECT_ID;
      }
      resetProjectSelectionIfNeeded(state);
    });
  }

  async loadModelState() {
    const requestSerial = ++this.modelsRequestSerial;
    const project = selectedProjectQueryValue(this.state());
    const sessionId = this.state().selectedSessionId;
    const params = new URLSearchParams({
      project,
      include_models: "1",
    });
    if (sessionId) {
      params.set("session", sessionId);
    }
    this.update((state) => {
      state.modelLoadError = null;
    });

    try {
      const payload = await api(`/api/status?${params.toString()}`);
      if (requestSerial !== this.modelsRequestSerial) return;
      const modelGroups = normalizeModelGroups(payload);
      const statusModelId =
        payload?.model?.provider_id && payload?.model?.model_id
          ? `${payload.model.provider_id}/${payload.model.model_id}`
          : "";
      const statusVariant =
        typeof payload?.model?.effort === "string" ? payload.model.effort : "";
      const nextScopeKey = selectionScopeKey(project, sessionId);
      this.update((state) => {
        const scopeChanged = state.modelScopeKey !== nextScopeKey;
        state.modelGroups = modelGroups;
        state.modelLoadError = null;
        state.modelScopeKey = nextScopeKey;

        const defaultModelId =
          findModel(modelGroups, statusModelId)?.id || firstModelId(modelGroups);
        if (
          scopeChanged ||
          !state.modelSelectionDirty ||
          !findModel(modelGroups, state.selectedModelId)
        ) {
          state.selectedModelId = defaultModelId;
          state.selectedVariant = selectedVariantForModel(
            modelGroups,
            defaultModelId,
            statusVariant,
          );
          state.modelSelectionDirty = false;
          return;
        }

        if (!findModel(modelGroups, state.selectedModelId)) {
          state.selectedModelId = defaultModelId;
        }
        if (
          state.selectedVariant &&
          !selectedVariantForModel(modelGroups, state.selectedModelId, state.selectedVariant)
        ) {
          state.selectedVariant = "";
        }
      });
    } catch (error) {
      if (requestSerial !== this.modelsRequestSerial) return;
      this.update((state) => {
        state.modelGroups = [];
        state.modelLoadError = error instanceof Error ? error.message : String(error);
        state.modelScopeKey = selectionScopeKey(project, sessionId);
        if (!state.modelSelectionDirty) {
          state.selectedModelId = "";
          state.selectedVariant = "";
        }
      });
    }
  }

  async loadSessions() {
    const requestSerial = ++this.sessionsRequestSerial;
    this.update((state) => {
      state.sessionsLoading = true;
      state.sessionsError = null;
    });

    try {
      const payload = await api(
        `/api/sessions?project=${encodeURIComponent(selectedProjectQueryValue(this.state()))}`,
      );
      if (requestSerial !== this.sessionsRequestSerial) return;
      const sessions = Array.isArray(payload) ? payload : [];
      this.update((state) => {
        state.sessions = sessions;
        const route = this.routeState();
        const routeSession =
          route.project === selectedProjectQueryValue(state) ? route.session : null;
        if (routeSession && state.sessions.some((session) => session.id === routeSession)) {
          state.selectedSessionId = routeSession;
        } else if (!state.sessions.some((session) => session.id === state.selectedSessionId)) {
          state.selectedSessionId = state.sessions[0]?.id || null;
        }
        state.sessionsLoading = false;
      });
    } catch (error) {
      if (requestSerial !== this.sessionsRequestSerial) return;
      this.update((state) => {
        state.sessions = [];
        state.sessionsError = error instanceof Error ? error.message : String(error);
        state.sessionsLoading = false;
      });
    }
  }

  async loadTranscript() {
    const sessionId = this.state().selectedSessionId;
    if (!sessionId) {
      this.update((state) => {
        state.transcript = [];
        state.transcriptLoading = false;
        state.transcriptError = null;
      });
      return;
    }

    const requestSerial = ++this.transcriptRequestSerial;
    this.update((state) => {
      state.transcriptLoading = true;
      state.transcriptError = null;
    });

    try {
      const payload = await api(
        `/api/sessions/${encodeURIComponent(sessionId)}/messages?project=${encodeURIComponent(selectedProjectQueryValue(this.state()))}`,
      );
      if (requestSerial !== this.transcriptRequestSerial || sessionId !== this.state().selectedSessionId) {
        return;
      }
      this.update((state) => {
        state.transcript = Array.isArray(payload) ? payload : [];
        state.transcriptError = null;
        state.transcriptLoading = false;
      });
    } catch (error) {
      if (requestSerial !== this.transcriptRequestSerial || sessionId !== this.state().selectedSessionId) {
        return;
      }
      this.update((state) => {
        state.transcript = [];
        state.transcriptError = error instanceof Error ? error.message : String(error);
        state.transcriptLoading = false;
      });
    }
  }

  async loadActiveTurn({ subscribe } = { subscribe: true }) {
    const sessionId = this.state().selectedSessionId;
    if (!sessionId) {
      this.update((state) => {
        state.activeTurn = null;
      });
      this.closeActiveStream();
      this.clearActiveReconnect();
      return;
    }

    const requestSerial = ++this.activeTurnRequestSerial;
    try {
      const payload = await api(
        `/api/turns/active?project=${encodeURIComponent(selectedProjectQueryValue(this.state()))}&session=${encodeURIComponent(sessionId)}`,
      );
      if (requestSerial !== this.activeTurnRequestSerial || sessionId !== this.state().selectedSessionId) {
        return;
      }
      this.update((state) => {
        state.activeTurn = payload ? this.cloneActiveTurnView(payload) : null;
      });
      if (subscribe && this.state().activeTurn) {
        this.subscribeToTurn(this.state().activeTurn.turn.id, this.state().activeTurn.last_seq);
      } else if (!this.state().activeTurn) {
        this.closeActiveStream();
        this.clearActiveReconnect();
      }
    } catch (_) {
      if (requestSerial !== this.activeTurnRequestSerial || sessionId !== this.state().selectedSessionId) {
        return;
      }
      this.update((state) => {
        state.activeTurn = null;
      });
      this.closeActiveStream();
      this.clearActiveReconnect();
    }
  }

  async refreshConversationState() {
    if (!this.state().selectedSessionId) {
      this.update((state) => {
        state.transcript = [];
        state.transcriptLoading = false;
        state.transcriptError = null;
        state.activeTurn = null;
      });
      this.closeActiveStream();
      this.clearActiveReconnect();
      return;
    }
    await Promise.all([this.loadTranscript(), this.loadActiveTurn({ subscribe: true })]);
  }

  async finalizeCompletedTurn(turnId, sessionId) {
    const currentProject = selectedProjectQueryValue(this.state());
    await Promise.all([this.loadSessions(), this.loadTranscript(), this.loadModelState()]);
    const state = this.state();
    if (
      state.activeTurn?.turn?.id === turnId &&
      state.selectedSessionId === sessionId &&
      selectedProjectQueryValue(state) === currentProject
    ) {
      this.update((draft) => {
        draft.activeTurn = null;
      });
    }
  }

  async selectProject(projectId) {
    if (this.state().selectedProjectId === projectId) return;
    this.closeActiveStream();
    this.clearActiveReconnect();
    this.update((state) => {
      state.selectedProjectId = projectId;
      state.selectedSessionId = null;
      state.draftProjectId = null;
      state.transcript = [];
      state.transcriptError = null;
      state.activeTurn = null;
    });
    this.syncRoute();
    await this.loadSessions();
    await this.loadModelState();
    this.syncRoute();
    await this.refreshConversationState();
  }

  async selectSession(sessionId) {
    const state = this.state();
    if (state.selectedSessionId === sessionId && state.draftProjectId == null) return;
    this.update((draft) => {
      draft.selectedSessionId = sessionId;
      draft.draftProjectId = null;
    });
    this.syncRoute();
    await Promise.all([this.refreshConversationState(), this.loadModelState()]);
  }

  selectDraftSession(projectId) {
    this.closeActiveStream();
    this.clearActiveReconnect();
    this.update((state) => {
      state.selectedSessionId = null;
      state.draftProjectId = projectId;
      state.transcript = [];
      state.transcriptError = null;
      state.activeTurn = null;
      state.composerDraft = "";
      state.modelSelectionDirty = false;
    });
    this.syncRoute();
    void this.loadModelState();
    this.composer.focusInput();
  }

  async submitProjectDialog() {
    const state = this.state();
    const path = (state.projectDialog.confirmationPath || state.projectDialog.path).trim();
    if (!path) {
      this.update((draft) => {
        draft.projectDialog.error = "Enter a directory path.";
      });
      return;
    }

    this.update((draft) => {
      draft.projectDialog.submitting = true;
      draft.projectDialog.error = null;
    });

    try {
      const project = await api("/api/projects/open", {
        method: "POST",
        body: JSON.stringify({
          path,
          create: !!this.state().projectDialog.confirmationPath,
        }),
      });
      this.rememberOpenedProject(project);
      this.update((draft) => {
        draft.selectedProjectId = project.path;
        draft.selectedSessionId = null;
        draft.draftProjectId = null;
        draft.modelSelectionDirty = false;
        draft.projectDialog.open = false;
        draft.projectDialog.submitting = false;
        draft.projectDialog.confirmationPath = null;
        draft.projectDialog.error = null;
      });
      this.syncRoute();
      await this.loadSessions();
      await this.loadModelState();
      this.syncRoute();
      await this.refreshConversationState();
    } catch (error) {
      this.update((draft) => {
        if (
          error instanceof ApiError &&
          error.status === 409 &&
          error.body?.needs_confirmation
        ) {
          draft.projectDialog.confirmationPath = error.body.path || path;
          draft.projectDialog.path = error.body.path || path;
          draft.projectDialog.error = null;
        } else {
          draft.projectDialog.error =
            error instanceof Error ? error.message : String(error);
        }
        draft.projectDialog.submitting = false;
      });
    }
  }

  async submitComposer() {
    const prompt = this.state().composerDraft.trim();
    if (!prompt) return;

    try {
      const launched = await api("/api/turns", {
        method: "POST",
        body: JSON.stringify({
          project: selectedProjectQueryValue(this.state()),
          session_id: this.state().selectedSessionId,
          prompt,
          model: buildCanonicalModelRef(
            this.state().selectedModelId,
            this.state().selectedVariant,
          ),
          images: [],
        }),
      });
      this.update((state) => {
        state.selectedSessionId = launched.turn.session_id;
        state.draftProjectId = null;
        state.composerDraft = "";
        state.modelSelectionDirty = false;
        state.modelScopeKey = selectionScopeKey(
          selectedProjectQueryValue(state),
          launched.turn.session_id,
        );
      });
      this.syncRoute();
      await Promise.all([
        this.loadSessions(),
        this.loadTranscript(),
        this.loadActiveTurn({ subscribe: true }),
      ]);
      this.composer.focusInput();
    } catch (error) {
      this.update((state) => {
        state.transcriptError = error instanceof Error ? error.message : String(error);
      });
    }
  }

  bindEvents() {
    this.sidebarToggle.addEventListener("click", () => this.toggleSidebar());
    this.sidebarHitbox.addEventListener("click", () => this.setSidebarOpen(false));

    this.sidebar.addEventListener("mu:select-project", (event) => {
      this.setSidebarOpen(false);
      void this.selectProject(event.detail.projectId);
    });
    this.sidebar.addEventListener("mu:select-session", (event) => {
      this.setSidebarOpen(false);
      void this.selectSession(event.detail.sessionId);
    });
    this.sidebar.addEventListener("mu:new-session", (event) => {
      this.setSidebarOpen(false);
      this.selectDraftSession(event.detail.projectId);
    });
    this.sidebar.addEventListener("mu:open-project", () => {
      this.setSidebarOpen(false);
      this.openProjectDialog();
    });

    this.composer.addEventListener("mu:composer-draft", (event) => {
      this.update((state) => {
        state.composerDraft = event.detail.value;
      });
    });
    this.composer.addEventListener("mu:composer-submit", () => {
      void this.submitComposer();
    });
    this.composer.addEventListener("mu:model-change", (event) => {
      this.update((state) => {
        state.selectedModelId = event.detail.value;
        state.selectedVariant = "";
        state.modelSelectionDirty = true;
      });
    });
    this.composer.addEventListener("mu:variant-change", (event) => {
      this.update((state) => {
        state.selectedVariant = event.detail.value;
        state.modelSelectionDirty = true;
      });
    });
    this.composer.addEventListener("mu:composer-attach", () => {
      this.composer.focusInput();
    });

    this.projectModal.addEventListener("mu:project-path", (event) => {
      this.setProjectDialogPath(event.detail.value);
    });
    this.projectModal.addEventListener("mu:project-submit", () => {
      void this.submitProjectDialog();
    });
    this.projectModal.addEventListener("mu:project-close", () => {
      this.closeProjectDialog();
    });

    window.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      if (this.state().projectDialog.open) {
        this.closeProjectDialog();
        return;
      }
      this.setSidebarOpen(false);
    });

    window.addEventListener("popstate", () => {
      const route = this.routeState();
      if (route.project && route.project !== this.state().selectedProjectId) {
        void this.selectProject(route.project);
        return;
      }
      if (route.session && route.session !== this.state().selectedSessionId) {
        void this.selectSession(route.session);
      }
    });
  }

  render() {
    const state = this.state();
    this.app.dataset.sidebarOpen = state.sidebarOpen ? "true" : "false";
    this.sidebarToggle.setAttribute("aria-expanded", state.sidebarOpen ? "true" : "false");

    this.sidebar.viewModel = {
      projects: state.projects,
      selectedProjectId: state.selectedProjectId,
      draftProjectId: state.draftProjectId,
      sessions: state.sessions,
      sessionsLoading: state.sessionsLoading,
      sessionsError: state.sessionsError,
      selectedSessionId: state.selectedSessionId,
      selectedProject: selectedProject(state),
    };

    this.conversation.viewModel = {
      selectedProject: selectedProject(state),
      selectedSessionId: state.selectedSessionId,
      draftProjectId: state.draftProjectId,
      transcript: state.transcript,
      transcriptLoading: state.transcriptLoading,
      transcriptError: state.transcriptError,
      activeTurn: state.activeTurn,
      transcriptAlreadyContainsPrompt: (prompt) => this.transcriptAlreadyContainsPrompt(prompt),
    };

    const model = findModel(state.modelGroups, state.selectedModelId);
    this.composer.viewModel = {
      draft: state.composerDraft,
      modelGroups: state.modelGroups,
      modelLoadError: state.modelLoadError,
      selectedModelId: state.selectedModelId,
      selectedVariant: state.selectedVariant,
      selectedModelLabel: model?.displayName || "",
      selectedProviderLabel: model?.providerId || "",
      variants: model?.variants || [],
      submitDisabled: state.composerDraft.trim().length === 0,
    };

    this.projectModal.viewModel = state.projectDialog;
  }

  async start() {
    this.bindEvents();
    this.setSidebarOpen(false);
    await this.bootstrap();
    await this.loadSessions();
    await this.loadModelState();
    this.syncRoute();
    await this.refreshConversationState();
    this.render();
  }
}

const app = new MuWebApp();
void app.start();
