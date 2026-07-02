import { clear, element } from "../lib/dom.js";

function makeConversationCard(label, content, options = {}) {
  const card = element("section", "conversation-card");
  if (options.kind) {
    card.dataset.kind = options.kind;
  }
  if (options.error) {
    card.classList.add("conversation-error");
  }

  const header = element("div", "conversation-card-header", label);
  const body = element("pre", "conversation-card-body", content);
  body.dataset.mono = options.mono ? "true" : "false";

  card.appendChild(header);
  card.appendChild(body);
  return card;
}

export class MuConversationView extends HTMLElement {
  set viewModel(value) {
    this._viewModel = value;
    this.render();
  }

  connectedCallback() {
    this.render();
  }

  render() {
    const vm = this._viewModel;
    if (!vm) return;

    clear(this);
    this.className = "conversation-body";

    const showingDraft = vm.draftProjectId === vm.selectedProject.id;
    const hasSelectedSession = !!vm.selectedSessionId;
    if (!hasSelectedSession && !showingDraft) {
      const empty = element("div", "conversation-empty");
      empty.appendChild(element("p", "conversation-kicker", "mu web"));
      empty.appendChild(element("h1", "conversation-title", "What should mu help with next?"));
      empty.appendChild(
        element(
          "p",
          "conversation-copy",
          "The browser surface now renders through native web components while keeping completed transcript from SQLite, live output from mu web memory, and raw event dumps for everything else.",
        ),
      );
      empty.appendChild(
        element(
          "p",
          "conversation-project",
          vm.selectedProject.global
            ? `Global scope: ${vm.selectedProject.path}`
            : `Project: ${vm.selectedProject.path}`,
        ),
      );
      this.appendChild(empty);
      return;
    }

    const thread = element("div", "conversation-thread");
    thread.appendChild(
      element(
        "p",
        "conversation-status",
        hasSelectedSession
          ? `Session: ${vm.selectedSessionId}`
          : "New session: your first prompt will create a web session automatically.",
      ),
    );

    if (vm.transcriptLoading) {
      thread.appendChild(
        makeConversationCard("loading", "Loading persisted transcript...", { mono: true }),
      );
    } else if (vm.transcriptError) {
      thread.appendChild(
        makeConversationCard("transcript error", vm.transcriptError, {
          mono: true,
          error: true,
        }),
      );
    } else if (vm.transcript.length === 0 && !showingDraft) {
      thread.appendChild(
        makeConversationCard("history", "No completed transcript yet.", { mono: true }),
      );
    } else {
      for (const message of vm.transcript) {
        thread.appendChild(
          makeConversationCard(`${message.role} #${message.seq}`, message.content || "", {
            mono: message.role !== "assistant",
          }),
        );
      }
    }

    if (showingDraft && !vm.activeTurn) {
      thread.appendChild(
        makeConversationCard(
          "draft session",
          "The first submitted prompt will create a web session and start streaming into this view.",
          { mono: true, kind: "live" },
        ),
      );
    }

    if (vm.activeTurn) {
      const { turn, snapshot } = vm.activeTurn;
      thread.appendChild(
        makeConversationCard(
          "live turn",
          `turn=${turn.id}\nsession=${turn.session_id}\nstarted_at=${turn.started_at}`,
          { mono: true, kind: "live" },
        ),
      );
      if (!vm.transcriptAlreadyContainsPrompt(snapshot.prompt)) {
        thread.appendChild(
          makeConversationCard("live user", snapshot.prompt, { mono: true, kind: "live" }),
        );
      }
      thread.appendChild(
        makeConversationCard(
          "live assistant",
          snapshot.assistant_text || "(waiting for assistant output)",
          { mono: false, kind: "live" },
        ),
      );
      if (snapshot.stderr) {
        thread.appendChild(
          makeConversationCard("live stderr", snapshot.stderr, {
            mono: true,
            kind: "live",
          }),
        );
      }
      thread.appendChild(
        makeConversationCard(
          "live raw events",
          JSON.stringify(snapshot.raw_events || [], null, 2),
          { mono: true, kind: "live" },
        ),
      );
    }

    this.appendChild(thread);
    this.scrollTop = this.scrollHeight;
  }
}

customElements.define("mu-conversation-view", MuConversationView);
