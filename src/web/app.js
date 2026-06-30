const state = {
  bootstrap: null,
  models: [],
  modelLoadError: null,
  projectPath: null,
  selectedModelId: "",
  selectedVariant: "",
  sidebarOpen: false,
};

function el(id) {
  return document.getElementById(id);
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      "content-type": "application/json",
      ...(options.headers || {}),
    },
  });
  if (!response.ok) {
    throw new Error(response.statusText || `request failed: ${response.status}`);
  }
  return response.json();
}

function setSidebarOpen(open) {
  state.sidebarOpen = open;
  el("app").dataset.sidebarOpen = open ? "true" : "false";
  el("sidebar-toggle").setAttribute("aria-expanded", open ? "true" : "false");
}

function toggleSidebar() {
  setSidebarOpen(!state.sidebarOpen);
}

async function bootstrap() {
  try {
    state.bootstrap = await api("/api/bootstrap");
  } catch (_) {
    state.bootstrap = null;
  }
  state.projectPath =
    state.bootstrap?.launch_project?.path ||
    state.bootstrap?.recent_projects?.[0]?.path ||
    null;
}

function syncComposerHeight() {
  const input = el("composer-input");
  if (!input) return;
  input.style.height = "0px";
  input.style.height = `${Math.min(input.scrollHeight, 180)}px`;
}

function syncComposerSubmit() {
  const input = el("composer-input");
  const submit = el("composer-submit");
  if (!input || !submit) return;
  submit.disabled = input.value.trim().length === 0;
}

function renderBootstrap() {
  const project = el("launch-project");
  if (!project) return;
  const path = state.projectPath || state.bootstrap?.launch_cwd;
  project.textContent = path ? `Launch project: ${path}` : "";
}

function normalizeModels(payload) {
  return Object.values(payload?.models || {})
    .map((model) => ({
      id: model.id,
      displayName: model.display_name || model.id,
      variants: Array.isArray(model.reasoning_effort_levels) ? model.reasoning_effort_levels : [],
    }))
    .sort((left, right) => left.displayName.localeCompare(right.displayName));
}

function resetSelectOptions(select, options) {
  select.innerHTML = "";
  for (const option of options) {
    const node = document.createElement("option");
    node.value = option.value;
    node.textContent = option.label;
    select.appendChild(node);
  }
}

function renderVariantOptions() {
  const variantSelect = el("composer-variant");
  if (!variantSelect) return;

  const model = state.models.find((item) => item.id === state.selectedModelId);
  const options = [{ value: "", label: "Default" }];
  if (model) {
    for (const variant of model.variants) {
      options.push({ value: variant, label: variant });
    }
  }

  resetSelectOptions(variantSelect, options);
  const nextVariant =
    options.some((option) => option.value === state.selectedVariant) ? state.selectedVariant : "";
  state.selectedVariant = nextVariant;
  variantSelect.value = nextVariant;
  variantSelect.disabled = options.length <= 1;
}

function renderModelOptions() {
  const modelSelect = el("composer-model");
  const variantSelect = el("composer-variant");
  if (!modelSelect || !variantSelect) return;

  if (state.modelLoadError) {
    resetSelectOptions(modelSelect, [{ value: "", label: "Models unavailable" }]);
    resetSelectOptions(variantSelect, [{ value: "", label: "Default" }]);
    modelSelect.disabled = true;
    variantSelect.disabled = true;
    return;
  }

  if (state.models.length === 0) {
    resetSelectOptions(modelSelect, [{ value: "", label: "No models found" }]);
    resetSelectOptions(variantSelect, [{ value: "", label: "Default" }]);
    modelSelect.disabled = true;
    variantSelect.disabled = true;
    return;
  }

  if (!state.models.some((model) => model.id === state.selectedModelId)) {
    state.selectedModelId = state.models[0].id;
  }

  resetSelectOptions(
    modelSelect,
    state.models.map((model) => ({
      value: model.id,
      label: model.displayName,
    })),
  );
  modelSelect.disabled = false;
  modelSelect.value = state.selectedModelId;
  renderVariantOptions();
}

async function loadModels() {
  if (!state.projectPath) {
    state.models = [];
    state.modelLoadError = null;
    renderModelOptions();
    return;
  }

  try {
    const payload = await api(`/api/models?project=${encodeURIComponent(state.projectPath)}`);
    state.models = normalizeModels(payload);
    state.modelLoadError = null;
  } catch (error) {
    state.models = [];
    state.modelLoadError = error instanceof Error ? error.message : String(error);
  }

  renderModelOptions();
}

function bindEvents() {
  el("sidebar-toggle").addEventListener("click", toggleSidebar);
  el("sidebar-hitbox").addEventListener("click", () => setSidebarOpen(false));
  el("composer-form").addEventListener("submit", (event) => {
    event.preventDefault();
  });
  el("composer-attach").addEventListener("click", () => {
    el("composer-input").focus();
  });
  el("composer-input").addEventListener("input", () => {
    syncComposerHeight();
    syncComposerSubmit();
  });
  el("composer-model").addEventListener("change", (event) => {
    state.selectedModelId = event.target.value;
    state.selectedVariant = "";
    renderVariantOptions();
  });
  el("composer-variant").addEventListener("change", (event) => {
    state.selectedVariant = event.target.value;
  });
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") setSidebarOpen(false);
  });
}

async function main() {
  bindEvents();
  setSidebarOpen(false);
  await bootstrap();
  renderBootstrap();
  renderModelOptions();
  await loadModels();
  syncComposerHeight();
  syncComposerSubmit();
}

void main();
