import { clear, resetSelectOptions } from "../lib/dom.js";

function variantOptions(variants) {
  return [{ value: "", label: "Default" }, ...variants.map((variant) => ({
    value: variant,
    label: variant,
  }))];
}

export class MuComposer extends HTMLElement {
  constructor() {
    super();
    this.initialized = false;
    this.modelPickerOpen = false;
    this.modelSearchValue = "";
    this.handleDocumentPointerDown = (event) => {
      if (!this.modelPickerOpen) return;
      if (!this.contains(event.target)) {
        this.closeModelPicker();
      }
    };
    this.handleWindowKeyDown = (event) => {
      if (event.key === "Escape" && this.modelPickerOpen) {
        this.closeModelPicker({ restoreFocus: true });
      }
    };
  }

  set viewModel(value) {
    this._viewModel = value;
    this.render();
  }

  connectedCallback() {
    this.render();
    document.addEventListener("pointerdown", this.handleDocumentPointerDown);
    window.addEventListener("keydown", this.handleWindowKeyDown);
  }

  disconnectedCallback() {
    document.removeEventListener("pointerdown", this.handleDocumentPointerDown);
    window.removeEventListener("keydown", this.handleWindowKeyDown);
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
    this.input?.focus();
  }

  closeModelPicker({ restoreFocus = false } = {}) {
    if (!this.modelPickerOpen) return;
    this.modelPickerOpen = false;
    this.modelSearchValue = "";
    this.renderModelPicker();
    if (restoreFocus) {
      this.modelButton?.focus();
    }
  }

  openModelPicker() {
    if (this.modelButton?.disabled) return;
    this.modelPickerOpen = true;
    this.modelSearchValue = "";
    this.renderModelPicker();
    window.requestAnimationFrame(() => {
      this.modelSearch?.focus();
      this.modelSearch?.select();
    });
  }

  toggleModelPicker() {
    if (this.modelPickerOpen) {
      this.closeModelPicker({ restoreFocus: true });
    } else {
      this.openModelPicker();
    }
  }

  ensureInitialized() {
    if (this.initialized) return;
    clear(this);
    this.className = "composer-dock";
    this.innerHTML = `
      <form class="composer-card">
        <div class="composer-input-wrap">
          <textarea
            class="composer-input"
            rows="1"
            placeholder="Ask mu to inspect, edit, or run something"
            aria-label="Prompt"
          ></textarea>
        </div>
        <div class="composer-toolbar">
          <div class="composer-toolbar-main">
            <button class="composer-icon-button" type="button" aria-label="Add attachment">
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <path
                  d="M8 3v10M3 8h10"
                  fill="none"
                  stroke="currentColor"
                  stroke-linecap="round"
                  stroke-width="1.25"
                />
              </svg>
            </button>
            <div class="composer-model-shell">
              <button
                class="composer-picker-button"
                type="button"
                aria-label="Model"
                aria-haspopup="dialog"
                aria-expanded="false"
              >
                <span class="composer-picker-button-label">Select model</span>
                <span class="composer-picker-chevron" aria-hidden="true"></span>
              </button>
              <div class="composer-model-popover" hidden>
                <div class="composer-model-popover-card">
                  <div class="composer-model-search-shell">
                    <input
                      class="composer-model-search"
                      type="search"
                      placeholder="Search models"
                      aria-label="Search models"
                    >
                  </div>
                  <div class="composer-model-list" role="listbox" aria-label="Available models"></div>
                </div>
              </div>
            </div>
            <label class="composer-select-shell">
              <span class="sr-only">Variant</span>
              <select class="composer-select" aria-label="Variant"></select>
            </label>
          </div>
          <button class="composer-submit" type="submit" aria-label="Send prompt">
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path
                d="M3.5 8h7.75M8.25 4.25 12 8l-3.75 3.75"
                fill="none"
                stroke="currentColor"
                stroke-linecap="round"
                stroke-linejoin="round"
                stroke-width="1.25"
              />
            </svg>
          </button>
        </div>
      </form>
    `;

    this.form = this.querySelector("form");
    this.input = this.querySelector(".composer-input");
    this.attachButton = this.querySelector(".composer-icon-button");
    this.modelShell = this.querySelector(".composer-model-shell");
    this.modelButton = this.querySelector(".composer-picker-button");
    this.modelButtonLabel = this.querySelector(".composer-picker-button-label");
    this.modelPopover = this.querySelector(".composer-model-popover");
    this.modelSearch = this.querySelector(".composer-model-search");
    this.modelList = this.querySelector(".composer-model-list");
    this.variantShell = this.querySelector(".composer-select-shell");
    this.variantSelect = this.querySelector(".composer-select");
    this.submitButton = this.querySelector(".composer-submit");

    this.form.addEventListener("submit", (event) => {
      event.preventDefault();
      this.dispatch("mu:composer-submit");
    });
    this.attachButton.addEventListener("click", () => {
      this.dispatch("mu:composer-attach");
    });
    this.input.addEventListener("input", () => {
      this.dispatch("mu:composer-draft", { value: this.input.value });
      this.syncHeight();
    });
    this.modelButton.addEventListener("click", () => {
      this.toggleModelPicker();
    });
    this.modelSearch.addEventListener("input", () => {
      this.modelSearchValue = this.modelSearch.value;
      this.renderModelPicker();
    });
    this.modelSearch.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        this.closeModelPicker({ restoreFocus: true });
      }
    });
    this.variantSelect.addEventListener("change", () => {
      this.dispatch("mu:variant-change", { value: this.variantSelect.value });
    });

    this.initialized = true;
  }

  syncHeight() {
    if (!this.input) return;
    this.input.style.height = "0px";
    this.input.style.height = `${Math.min(this.input.scrollHeight, 180)}px`;
  }

  filteredModelGroups(groups) {
    const query = this.modelSearchValue.trim().toLowerCase();
    if (!query) return groups;
    return groups
      .map((group) => ({
        ...group,
        models: group.models.filter((model) => {
          const provider = group.id.toLowerCase();
          const modelId = model.modelId.toLowerCase();
          const fullId = model.id.toLowerCase();
          return (
            provider.includes(query) ||
            modelId.includes(query) ||
            fullId.includes(query)
          );
        }),
      }))
      .filter((group) => group.models.length > 0);
  }

  renderModelPicker() {
    if (!this.modelPopover || !this.modelList || !this.modelSearch) return;
    const vm = this._viewModel;
    const showPicker = !!vm?.modelLoadError || (vm?.modelGroups?.length || 0) > 0;
    this.modelPopover.hidden = !showPicker || !this.modelPickerOpen;
    this.modelButton?.setAttribute("aria-expanded", this.modelPickerOpen ? "true" : "false");
    if (!showPicker || !vm) return;
    if (this.modelSearch.value !== this.modelSearchValue) {
      this.modelSearch.value = this.modelSearchValue;
    }

    this.modelList.replaceChildren();
    if (vm.modelLoadError) {
      const message = document.createElement("div");
      message.className = "composer-model-empty";
      message.textContent = "Models unavailable";
      this.modelList.appendChild(message);
      return;
    }

    const groups = this.filteredModelGroups(vm.modelGroups || []);
    if (groups.length === 0) {
      const message = document.createElement("div");
      message.className = "composer-model-empty";
      message.textContent = "No matching models";
      this.modelList.appendChild(message);
      return;
    }

    for (const group of groups) {
      const section = document.createElement("section");
      section.className = "composer-model-group";

      const heading = document.createElement("div");
      heading.className = "composer-model-group-title";
      heading.textContent = group.displayName || group.id;
      section.appendChild(heading);

      for (const model of group.models) {
        const option = document.createElement("button");
        option.type = "button";
        option.className = "composer-model-option";
        option.setAttribute("role", "option");
        option.dataset.selected = model.id === vm.selectedModelId ? "true" : "false";
        option.textContent = model.displayName;
        option.addEventListener("click", () => {
          this.dispatch("mu:model-change", { value: model.id });
          this.closeModelPicker({ restoreFocus: true });
        });
        section.appendChild(option);
      }

      this.modelList.appendChild(section);
    }
  }

  render() {
    this.ensureInitialized();
    const vm = this._viewModel;
    if (!vm) return;

    if (this.input.value !== vm.draft) {
      this.input.value = vm.draft;
    }

    const hasModels = (vm.modelGroups?.length || 0) > 0;
    const showModelPicker = !!vm.modelLoadError || hasModels;
    const showVariantPicker = showModelPicker && vm.variants.length > 0;
    this.modelShell.hidden = !showModelPicker;
    this.variantShell.hidden = !showVariantPicker;

    const modelLabel = vm.selectedModelLabel
      ? `${vm.selectedProviderLabel}/${vm.selectedModelLabel}`
      : vm.modelLoadError
        ? "Models unavailable"
        : "Select model";
    this.modelButtonLabel.textContent = modelLabel;
    this.modelButton.disabled = !!vm.modelLoadError || !hasModels;

    const variants = variantOptions(vm.variants);
    resetSelectOptions(this.variantSelect, variants);
    this.variantSelect.value = variants.some((option) => option.value === vm.selectedVariant)
      ? vm.selectedVariant
      : "";
    this.variantSelect.disabled = variants.length <= 1;
    this.renderModelPicker();

    this.submitButton.disabled = vm.submitDisabled;
    this.syncHeight();
  }
}

customElements.define("mu-composer", MuComposer);
