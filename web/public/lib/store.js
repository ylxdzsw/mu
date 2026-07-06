export function makeInitialState() {
  return {
    bootstrap: null,
    projects: [],
    selectedProjectId: "",
    selectedSessionId: null,
    draftProjectId: null,
    sessions: [],
    sessionsLoading: false,
    sessionsError: null,
    transcript: [],
    transcriptLoading: false,
    transcriptError: null,
    activeTurn: null,
    modelGroups: [],
    modelLoadError: null,
    selectedModelId: "",
    selectedVariant: "",
    modelScopeKey: null,
    modelSelectionDirty: false,
    composerDraft: "",
    composerSubmitting: false,
    sidebarOpen: false,
    projectDialog: {
      open: false,
      path: "",
      submitting: false,
      confirmationPath: null,
      error: null,
    },
  };
}

export function createStore(initialState) {
  let state = initialState;
  const listeners = new Set();

  return {
    getState() {
      return state;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    update(mutator) {
      mutator(state);
      for (const listener of listeners) {
        listener(state);
      }
    },
  };
}
