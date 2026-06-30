import { GLOBAL_PROJECT_ID } from "./constants.js";

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

export function globalProject(bootstrap) {
  return {
    id: GLOBAL_PROJECT_ID,
    queryValue: GLOBAL_PROJECT_ID,
    path: bootstrap?.global_home || "~",
    marker: "global",
    name: "Global",
    global: true,
    initials: "G",
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
  const deduped = [globalProject(bootstrap)];
  const seen = new Set([GLOBAL_PROJECT_ID]);
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
    globalProject(state.bootstrap)
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
    state.selectedProjectId = state.projects[0]?.id || GLOBAL_PROJECT_ID;
  }
}
