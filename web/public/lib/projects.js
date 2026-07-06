function basename(path) {
  const parts = String(path || "")
    .split("/")
    .filter(Boolean);
  return parts.length === 0 ? String(path || "") : parts[parts.length - 1];
}

function projectInitials(name) {
  const letters = String(name || "")
    .replace(/[^a-zA-Z0-9]+/g, " ")
    .trim()
    .split(/\s+/)
    .flatMap((part) => part.slice(0, 1).split(""));
  if (letters.length === 0) return "?";
  return letters.slice(0, 2).join("").toUpperCase();
}

export function emptyProject() {
  return {
    id: "",
    queryValue: "",
    path: "",
    marker: "",
    name: "No project",
    global: false,
    missing: true,
    initials: "?",
  };
}

export function normalizeProject(summary) {
  const path = summary?.path || "";
  const name = basename(path) || path;
  return {
    id: path,
    queryValue: path,
    path,
    marker: summary?.marker || "mu",
    name,
    global: false,
    initials: projectInitials(name),
  };
}

export function buildProjects(bootstrap) {
  const deduped = [];
  const seen = new Set();
  const candidates = [
    bootstrap?.launch_project,
    ...(bootstrap?.recent_projects || []),
  ].filter(Boolean);
  for (const summary of candidates) {
    if (!summary?.path || seen.has(summary.path)) continue;
    deduped.push(normalizeProject(summary));
    seen.add(summary.path);
  }
  return deduped;
}

export function selectedProject(state) {
  return (
    state.projects.find((project) => project.id === state.selectedProjectId) ||
    state.projects[0] ||
    emptyProject()
  );
}

export function selectedProjectQueryValue(state) {
  return selectedProject(state).queryValue;
}

export function sessionTitle(session) {
  return session?.title || "New session";
}

export function resetProjectSelectionIfNeeded(state) {
  if (!state.projects.some((project) => project.id === state.selectedProjectId)) {
    state.selectedProjectId = state.projects[0]?.id || "";
  }
}
