export class MuProjectModal extends HTMLElement {
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

  focusInput() {
    this.querySelector(".project-modal-input")?.focus();
    this.querySelector(".project-modal-input")?.select();
  }

  escapeHtml(value) {
    return String(value || "")
      .replace(/&/gu, "&amp;")
      .replace(/</gu, "&lt;")
      .replace(/>/gu, "&gt;")
      .replace(/"/gu, "&quot;")
      .replace(/'/gu, "&#39;");
  }

  render() {
    const vm = this._viewModel;
    if (!vm) return;

    const needsConfirmation = !!vm.confirmationPath;
    const errorHtml = this.escapeHtml(vm.error || "");
    this.className = "project-modal";
    this.hidden = !vm.open;
    this.innerHTML = `
      <div class="project-modal-backdrop"></div>
      <div
        class="project-modal-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="project-modal-title"
        aria-describedby="project-modal-copy"
      >
        <form class="project-modal-card">
          <div class="project-modal-header">
            <p class="project-modal-kicker">Open project</p>
            <h2 id="project-modal-title" class="project-modal-title">${needsConfirmation ? "Create .mu and open it?" : "Choose a directory"}</h2>
            <p id="project-modal-copy" class="project-modal-copy">
              ${needsConfirmation
    ? "mu found an existing directory without project state. Confirm to create `.mu` there and add it to the sidebar."
    : "Enter a path to a project root. If the folder is missing `.mu`, mu can create it after confirmation."}
            </p>
          </div>

          <label class="project-modal-field">
            <span class="project-modal-label">Path</span>
            <input
              class="project-modal-input"
              type="text"
              name="path"
              placeholder="/path/to/project"
              autocomplete="off"
              spellcheck="false"
            >
          </label>

          <p class="project-modal-error"${vm.error ? "" : " hidden"}>${errorHtml}</p>
          <div class="project-modal-confirmation"${needsConfirmation ? "" : " hidden"}>
            This directory is not a project yet. Create \`.mu\` there and open it?
          </div>

          <div class="project-modal-actions">
            <button class="project-modal-button project-modal-button-muted" type="button">
              Cancel
            </button>
            <button class="project-modal-button project-modal-button-primary" type="submit">
              ${vm.submitting ? (needsConfirmation ? "Creating..." : "Opening...") : (needsConfirmation ? "Create .mu and open" : "Open project")}
            </button>
          </div>
        </form>
      </div>
    `;

    const input = this.querySelector(".project-modal-input");
    const cancel = this.querySelector(".project-modal-button-muted");
    const submit = this.querySelector(".project-modal-button-primary");
    const form = this.querySelector("form");
    const backdrop = this.querySelector(".project-modal-backdrop");

    input.value = vm.path;
    input.disabled = vm.submitting;
    cancel.disabled = vm.submitting;
    submit.disabled = vm.submitting;

    input.addEventListener("input", () => {
      this.dispatch("mu:project-path", { value: input.value });
    });
    cancel.addEventListener("click", () => this.dispatch("mu:project-close"));
    backdrop.addEventListener("click", () => this.dispatch("mu:project-close"));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      this.dispatch("mu:project-submit");
    });
  }
}

customElements.define("mu-project-modal", MuProjectModal);
