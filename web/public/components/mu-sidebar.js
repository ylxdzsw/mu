import { button, clear, element, setIcon } from "../lib/dom.js";
import { sessionTitle } from "../lib/projects.js";

function makeAvatar(project) {
  const avatar = element("div", "sidebar-project-avatar");
  avatar.dataset.global = project.global ? "true" : "false";
  if (project.global) {
    setIcon(avatar, "globe");
  } else {
    avatar.textContent = project.initials;
  }
  return avatar;
}

export class MuSidebar extends HTMLElement {
  set viewModel(value) {
    this._viewModel = value;
    this.render();
  }

  connectedCallback() {
    this.render();
  }

  dispatch(name, detail = {}) {
    this.dispatchEvent(
      new CustomEvent(name, {
        bubbles: true,
        detail,
      }),
    );
  }

  render() {
    const vm = this._viewModel;
    if (!vm) return;

    clear(this);

    const shell = element("div", "sidebar-shell");
    const rail = element("div", "sidebar-rail");
    const railBody = element("div", "sidebar-rail-body");
    const railFooter = element("div", "sidebar-rail-footer");
    const panel = element("div", "sidebar-panel");
    const frame = element("div", "sidebar-panel-frame");
    const content = element("div", "sidebar-panel-content");
    const scroll = element("div", "sidebar-panel-scroll");

    for (const project of vm.projects) {
      const tile = button(
        "sidebar-project-tile",
        project.global ? "Global project" : project.path,
      );
      tile.dataset.selected = project.id === vm.selectedProjectId ? "true" : "false";
      tile.dataset.active = project.id === vm.selectedProjectId ? "true" : "false";
      tile.appendChild(makeAvatar(project));
      tile.addEventListener("click", () => {
        this.dispatch("mu:select-project", { projectId: project.id });
      });
      railBody.appendChild(tile);
    }

    const openButton = button("sidebar-rail-action", "Open project");
    setIcon(openButton, "plus");
    openButton.addEventListener("click", () => this.dispatch("mu:open-project"));
    railBody.appendChild(openButton);

    rail.appendChild(railBody);
    rail.appendChild(railFooter);

    const header = element("div", "sidebar-panel-header");
    const titleRow = element("div", "sidebar-panel-title-row");
    const title = element(
      "h2",
      "sidebar-panel-title",
      vm.selectedProject.missing ? "No project" : vm.selectedProject.name,
    );
    const badge = element("span", "sidebar-panel-badge", vm.selectedProject.marker || "none");
    const path = element(
      "p",
      "sidebar-panel-path",
      vm.selectedProject.missing ? "Open a project to start a web session." : vm.selectedProject.path,
    );
    titleRow.appendChild(title);
    titleRow.appendChild(badge);
    header.appendChild(titleRow);
    header.appendChild(path);
    scroll.appendChild(header);

    const newSessionButton = button("sidebar-new-session-button", "New session");
    newSessionButton.disabled = !!vm.selectedProject.missing;
    newSessionButton.dataset.active =
      vm.draftProjectId === vm.selectedProject.id ? "true" : "false";
    const newSessionIcon = element("span", "sidebar-new-session-button-icon");
    const newSessionTitle = element("span", "sidebar-new-session-button-title", "New session");
    setIcon(newSessionIcon, "edit");
    newSessionButton.appendChild(newSessionIcon);
    newSessionButton.appendChild(newSessionTitle);
    newSessionButton.addEventListener("click", () => {
      this.dispatch("mu:new-session", { projectId: vm.selectedProject.id });
    });
    scroll.appendChild(newSessionButton);

    scroll.appendChild(
      element(
        "p",
        "sidebar-panel-section-label sidebar-panel-section-label-spaced",
        "Recent sessions",
      ),
    );

    if (vm.sessionsLoading) {
      const skeletons = element("div", "sidebar-session-skeletons");
      for (let index = 0; index < 4; index += 1) {
        skeletons.appendChild(element("div", "sidebar-session-skeleton"));
      }
      scroll.appendChild(skeletons);
    } else if (vm.sessionsError) {
      scroll.appendChild(element("div", "sidebar-error", vm.sessionsError));
    } else if (vm.sessions.length === 0) {
      scroll.appendChild(element("div", "sidebar-empty", "No web sessions yet."));
    } else {
      const list = element("div", "sidebar-session-list");
      for (const session of vm.sessions) {
        const item = element("div", "sidebar-session-item");
        const row = button("sidebar-session-row", sessionTitle(session));
        row.dataset.selected = session.id === vm.selectedSessionId ? "true" : "false";
        const status = element("span", "sidebar-session-status");
        const text = element("span", "sidebar-session-title", sessionTitle(session));
        row.appendChild(status);
        row.appendChild(text);
        row.addEventListener("click", () => {
          this.dispatch("mu:select-session", { sessionId: session.id });
        });
        const archive = button("sidebar-session-action", "Archive session");
        setIcon(archive, "archive");
        archive.addEventListener("click", () => {
          this.dispatch("mu:archive-session", { sessionId: session.id });
        });
        item.appendChild(row);
        item.appendChild(archive);
        list.appendChild(item);
      }
      scroll.appendChild(list);
    }

    content.appendChild(scroll);
    frame.appendChild(content);
    panel.appendChild(frame);
    shell.appendChild(rail);
    shell.appendChild(panel);
    this.appendChild(shell);
  }
}

customElements.define("mu-sidebar", MuSidebar);
