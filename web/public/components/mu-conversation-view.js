import { clear, element } from "../lib/dom.js";

const COLLAPSED_OUTPUT_LINES = 3;

function makeConversationNote(label, content, options = {}) {
  const section = element("section", "timeline-part note-part");
  if (options.error) section.classList.add("conversation-error");
  const heading = element("div", "timeline-label", label);
  const body = element("pre", "plain-transcript-text", content);
  section.appendChild(heading);
  section.appendChild(body);
  return section;
}

function appendText(parent, text) {
  parent.appendChild(document.createTextNode(text));
}

function parseInlineMarkdown(text) {
  const nodes = [];
  const pattern = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*|\[[^\]]+\]\((https?:\/\/[^)\s]+)\)|(https?:\/\/[^\s<]+))/gu;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    if (match.index > cursor) {
      nodes.push({ type: "text", text: text.slice(cursor, match.index) });
    }
    const token = match[0];
    if (token.startsWith("`")) {
      nodes.push({ type: "code", text: token.slice(1, -1) });
    } else if (token.startsWith("**")) {
      nodes.push({ type: "strong", text: token.slice(2, -2) });
    } else if (token.startsWith("*")) {
      nodes.push({ type: "em", text: token.slice(1, -1) });
    } else if (token.startsWith("[")) {
      const label = token.slice(1, token.indexOf("]("));
      nodes.push({ type: "link", text: label, href: match[2] });
    } else {
      nodes.push({ type: "link", text: token, href: match[3] });
    }
    cursor = pattern.lastIndex;
  }
  if (cursor < text.length) {
    nodes.push({ type: "text", text: text.slice(cursor) });
  }
  return nodes;
}

function appendInlineMarkdown(parent, text) {
  for (const node of parseInlineMarkdown(text)) {
    if (node.type === "text") {
      appendText(parent, node.text);
      continue;
    }
    const child = document.createElement(node.type === "strong" ? "strong" : node.type === "em" ? "em" : node.type === "link" ? "a" : "code");
    child.textContent = node.text;
    if (node.type === "link") {
      child.href = node.href;
      child.target = "_blank";
      child.rel = "noreferrer";
    }
    parent.appendChild(child);
  }
}

function flushParagraph(markdown, lines) {
  if (!lines.length) return;
  const paragraph = element("p", "markdown-paragraph");
  appendInlineMarkdown(paragraph, lines.join(" "));
  markdown.appendChild(paragraph);
  lines.length = 0;
}

function splitTableRow(line) {
  return line
    .replace(/^\s*\|/u, "")
    .replace(/\|\s*$/u, "")
    .split("|")
    .map((cell) => cell.trim());
}

function isTableSeparator(line) {
  return /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/u.test(line);
}

function renderMarkdownTable(lines, start) {
  if (start + 1 >= lines.length || !isTableSeparator(lines[start + 1])) {
    return null;
  }
  const rows = [splitTableRow(lines[start])];
  let cursor = start + 2;
  while (cursor < lines.length && /^\s*\|.*\|\s*$/u.test(lines[cursor])) {
    rows.push(splitTableRow(lines[cursor]));
    cursor += 1;
  }
  const table = element("table", "markdown-table");
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const cell of rows[0]) {
    const th = document.createElement("th");
    appendInlineMarkdown(th, cell);
    headRow.appendChild(th);
  }
  thead.appendChild(headRow);
  table.appendChild(thead);
  const tbody = document.createElement("tbody");
  for (const row of rows.slice(1)) {
    const tr = document.createElement("tr");
    for (const cell of row) {
      const td = document.createElement("td");
      appendInlineMarkdown(td, cell);
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  return { node: table, next: cursor };
}

function renderMarkdown(markdownText) {
  const markdown = element("div", "markdown-rendered");
  const lines = (markdownText || "").replace(/\r\n?/gu, "\n").split("\n");
  const paragraph = [];
  let list = null;
  let index = 0;

  const closeList = () => {
    if (list) {
      markdown.appendChild(list);
      list = null;
    }
  };

  while (index < lines.length) {
    const line = lines[index];

    if (/^```/u.test(line.trim())) {
      flushParagraph(markdown, paragraph);
      closeList();
      const language = line.trim().slice(3).trim();
      const codeLines = [];
      index += 1;
      while (index < lines.length && !/^```/u.test(lines[index].trim())) {
        codeLines.push(lines[index]);
        index += 1;
      }
      const pre = element("pre", "markdown-code-block");
      const code = document.createElement("code");
      if (language) code.dataset.language = language;
      code.textContent = codeLines.join("\n");
      pre.appendChild(code);
      markdown.appendChild(pre);
      index += index < lines.length ? 1 : 0;
      continue;
    }

    const table = renderMarkdownTable(lines, index);
    if (table) {
      flushParagraph(markdown, paragraph);
      closeList();
      markdown.appendChild(table.node);
      index = table.next;
      continue;
    }

    const heading = line.match(/^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$/u);
    if (heading) {
      flushParagraph(markdown, paragraph);
      closeList();
      const level = Math.min(heading[1].length, 6);
      const node = document.createElement(`h${level}`);
      node.className = `markdown-heading markdown-heading-${level}`;
      appendInlineMarkdown(node, heading[2]);
      markdown.appendChild(node);
      index += 1;
      continue;
    }

    const rule = line.match(/^\s{0,3}(-{3,}|\*{3,}|_{3,})\s*$/u);
    if (rule) {
      flushParagraph(markdown, paragraph);
      closeList();
      markdown.appendChild(document.createElement("hr"));
      index += 1;
      continue;
    }

    const bullet = line.match(/^\s{0,3}[-*+]\s+(.+)$/u);
    if (bullet) {
      flushParagraph(markdown, paragraph);
      if (!list || list.tagName !== "UL") {
        closeList();
        list = document.createElement("ul");
      }
      const item = document.createElement("li");
      appendInlineMarkdown(item, bullet[1]);
      list.appendChild(item);
      index += 1;
      continue;
    }

    const ordered = line.match(/^\s{0,3}\d+[.)]\s+(.+)$/u);
    if (ordered) {
      flushParagraph(markdown, paragraph);
      if (!list || list.tagName !== "OL") {
        closeList();
        list = document.createElement("ol");
      }
      const item = document.createElement("li");
      appendInlineMarkdown(item, ordered[1]);
      list.appendChild(item);
      index += 1;
      continue;
    }

    if (!line.trim()) {
      flushParagraph(markdown, paragraph);
      closeList();
      index += 1;
      continue;
    }

    closeList();
    paragraph.push(line.trim());
    index += 1;
  }

  flushParagraph(markdown, paragraph);
  closeList();
  return markdown;
}

function formatJson(value) {
  if (value == null) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

function toolTitle(part) {
  const args = part.args && typeof part.args === "object" ? part.args : {};
  if (typeof args.title === "string" && args.title.trim()) return args.title.trim();
  return part.tool || "tool";
}

function toolCommand(part) {
  const args = part.args && typeof part.args === "object" ? part.args : {};
  if (part.tool === "bash" && typeof args.script === "string") return args.script;
  return formatJson(args);
}

function toolPreview(text, lineCount = COLLAPSED_OUTPUT_LINES) {
  if (!text) return "(no output yet)";
  const lines = text.replace(/\r\n?/gu, "\n").split("\n");
  const visible = lines.slice(0, lineCount).join("\n");
  if (lines.length <= lineCount) return visible;
  return `${visible}\n... ${lines.length - lineCount} more line${lines.length - lineCount === 1 ? "" : "s"}`;
}

function renderDisclosure(label, text, options = {}) {
  const details = element("details", "tool-disclosure");
  if (options.open) details.open = true;
  if (options.key) details.dataset.disclosureKey = options.key;
  const summary = element("summary", "tool-disclosure-summary");
  summary.appendChild(element("span", "", label));
  summary.appendChild(element("span", "tool-disclosure-preview", options.preview || toolPreview(text, 1)));
  const body = element("pre", "tool-disclosure-body", text || options.emptyText || "");
  details.appendChild(summary);
  details.appendChild(body);
  return details;
}

function renderToolPart(part) {
  const section = element("section", "timeline-part tool-part");
  section.dataset.status = part.status || "running";

  const details = element("details", "tool-part-details");
  details.dataset.toolCallId = part.id || "";
  details.dataset.disclosureKey = `${part.id || part.tool || "tool"}:tool`;
  const summary = element("summary", "tool-part-summary");
  summary.appendChild(element("span", "tool-chevron", ""));
  summary.appendChild(element("span", "tool-name", toolTitle(part)));
  summary.appendChild(element("span", "tool-kind", part.tool || "tool"));
  const status = element("span", "tool-status", part.status || "running");
  summary.appendChild(status);
  if (part.elapsedMs != null) {
    summary.appendChild(element("span", "tool-elapsed", `${part.elapsedMs}ms`));
  }
  details.appendChild(summary);

  const body = element("div", "tool-part-body");
  const command = toolCommand(part);
  body.appendChild(
    renderDisclosure(part.tool === "bash" ? "Script" : "Arguments", command, {
      key: `${part.id || part.tool || "tool"}:command`,
      preview: toolPreview(command, 1),
    }),
  );
  body.appendChild(
    renderDisclosure("Output", part.output || part.error || "", {
      key: `${part.id || part.tool || "tool"}:output`,
      preview: part.error ? part.error : toolPreview(part.output || "", COLLAPSED_OUTPUT_LINES),
      emptyText: "(no output yet)",
    }),
  );
  details.appendChild(body);
  section.appendChild(details);
  return section;
}

function renderTextPart(part) {
  const section = element("section", "timeline-part text-part");
  section.dataset.role = part.role || "assistant";
  const label = element("div", "timeline-label", part.role || "assistant");
  const body =
    part.role === "assistant"
      ? renderMarkdown(part.text || "")
      : element("pre", "plain-transcript-text", part.text || "");
  section.appendChild(label);
  section.appendChild(body);
  return section;
}

function renderPlainPart(part) {
  const section = element("section", "timeline-part plain-part");
  const label = element("div", "timeline-label", part.label || part.type || "event");
  const body = element("pre", "plain-transcript-text", part.text || "");
  section.appendChild(label);
  section.appendChild(body);
  return section;
}

function normalizeTranscriptMessages(messages) {
  const parts = [];
  const tools = new Map();
  for (const message of messages) {
    if (message.role === "assistant") {
      if (message.content?.trim()) {
        parts.push({
          id: `message-${message.seq}`,
          type: "text",
          role: "assistant",
          text: message.content || "",
        });
      }
      for (const call of message.tool_calls || []) {
        const id = call.id || `tool-${message.seq}-${parts.length}`;
        let args = call.function?.arguments || {};
        if (typeof args === "string") {
          try {
            args = JSON.parse(args);
          } catch (_) {
            args = { arguments: args };
          }
        }
        const part = {
          id,
          type: "tool",
          tool: call.function?.name || "tool",
          args,
          output: "",
          status: "done",
          elapsedMs: null,
        };
        tools.set(id, part);
        parts.push(part);
      }
      continue;
    }
    if (message.role === "tool") {
      const part = tools.get(message.tool_call_id);
      if (part) {
        part.output = message.content || "";
        continue;
      }
      parts.push({
        id: `message-${message.seq}`,
        type: "plain",
        label: "tool",
        text: message.content || "",
      });
      continue;
    }
    parts.push({
      id: `message-${message.seq}`,
      type: "text",
      role: message.role,
      text: message.content || "",
    });
  }
  return parts;
}

function normalizeLiveTurn(vm, options = {}) {
  const active = vm.activeTurn;
  if (!active) return [];
  const parts = [];
  const snapshot = active.snapshot || {};
  if (!options.hidePrompt) {
    parts.push({
      id: `live-user-${active.turn.id}`,
      type: "text",
      role: "user",
      text: snapshot.prompt || "",
    });
  }

  let assistant = null;
  const tools = new Map();
  const appendAssistant = (text) => {
    if (!text) return;
    if (!assistant) {
      assistant = {
        id: `live-assistant-${parts.length}`,
        type: "text",
        role: "assistant",
        text: "",
      };
      parts.push(assistant);
    }
    assistant.text += text;
  };

  for (const event of snapshot.raw_events || []) {
    const payload = event.payload || {};
    if (event.event === "assistant_delta" && typeof payload.text === "string") {
      appendAssistant(payload.text);
      continue;
    }
    if (event.event === "tool_start") {
      assistant = null;
      const id = payload.tool_call_id || `tool-${event.seq}`;
      const part = {
        id,
        type: "tool",
        tool: payload.tool || "tool",
        args: payload.args || {},
        output: "",
        status: "running",
        elapsedMs: null,
      };
      tools.set(id, part);
      parts.push(part);
      continue;
    }
    if (event.event === "tool_output") {
      const id = payload.tool_call_id || [...tools.keys()].at(-1);
      const part = tools.get(id);
      if (part && typeof payload.text === "string") {
        part.output += payload.text;
      }
      continue;
    }
    if (event.event === "tool_finish") {
      const id = payload.tool_call_id || [...tools.keys()].at(-1);
      const part = tools.get(id);
      if (part) {
        part.status = payload.display?.exit_code === 0 || payload.display?.kind === "none" ? "done" : "failed";
        part.elapsedMs = payload.elapsed_ms;
      }
      continue;
    }
    if (event.event === "tool_error") {
      const id = payload.tool_call_id || [...tools.keys()].at(-1);
      const part = tools.get(id);
      if (part) {
        part.status = "failed";
        part.error = payload.error || "";
        part.elapsedMs = payload.elapsed_ms;
      }
      continue;
    }
    const eventText = payload.text || payload.message;
    if ((event.event === "notice" || event.event === "error" || event.event === "stderr") && eventText) {
      assistant = null;
      parts.push({
        id: `event-${event.seq}`,
        type: "plain",
        label: event.event,
        text: String(eventText),
      });
    }
  }
  return parts;
}

function normalizeDocumentParts(vm) {
  const transcript = vm.transcript || [];
  if (vm.activeTurn?.completed && transcript.length > 0) {
    return normalizeTranscriptMessages(transcript);
  }
  const activePrompt = vm.activeTurn?.snapshot?.prompt;
  const trailingMessage = transcript.at(-1);
  const hideLivePrompt =
    vm.activeTurn &&
    !vm.activeTurn.completed &&
    activePrompt &&
    trailingMessage?.role === "user" &&
    trailingMessage?.content === activePrompt;
  const storedMessages =
    hideLivePrompt
      ? transcript.slice(0, -1)
      : transcript;
  const parts = normalizeTranscriptMessages(storedMessages);
  for (const part of normalizeLiveTurn(vm, { hidePrompt: hideLivePrompt })) parts.push(part);
  return parts;
}

function renderPart(part) {
  if (part.type === "tool") return renderToolPart(part);
  if (part.type === "plain") return renderPlainPart(part);
  return renderTextPart(part);
}

export class MuConversationView extends HTMLElement {
  constructor() {
    super();
    this.openDisclosures = new Set();
    this.addEventListener("toggle", (event) => this.trackDisclosureState(event), true);
  }

  set viewModel(value) {
    this._viewModel = value;
    this.render();
  }

  connectedCallback() {
    this.render();
  }

  rememberOpenDisclosures() {
    for (const details of this.querySelectorAll("details[data-disclosure-key][open]")) {
      this.openDisclosures.add(details.dataset.disclosureKey);
    }
    return new Set(this.openDisclosures);
  }

  restoreOpenDisclosures(open) {
    if (!open.size) return;
    for (const details of this.querySelectorAll("details[data-disclosure-key]")) {
      if (open.has(details.dataset.disclosureKey)) {
        details.open = true;
      }
    }
  }

  trackDisclosureState(event) {
    const details = event.target;
    if (!(details instanceof HTMLDetailsElement)) return;
    const key = details.dataset.disclosureKey;
    if (!key) return;
    if (details.open) {
      this.openDisclosures.add(key);
    } else {
      this.openDisclosures.delete(key);
    }
  }

  render() {
    const vm = this._viewModel;
    if (!vm) return;

    const openDisclosures = this.rememberOpenDisclosures();
    const shouldStickToBottom =
      this.scrollHeight <= this.clientHeight ||
      this.scrollHeight - this.scrollTop - this.clientHeight < 48;
    clear(this);
    this.className = "conversation-body";

    const showingDraft = vm.draftProjectId === vm.selectedProject.id;
    const hasSelectedSession = !!vm.selectedSessionId;
    if (!hasSelectedSession && !showingDraft) {
      const empty = element("div", "conversation-empty");
      empty.appendChild(element("p", "conversation-kicker", "mu web"));
      empty.appendChild(
        element(
          "h1",
          "conversation-title",
          vm.selectedProject.missing ? "Open a project to start" : "What should mu help with next?",
        ),
      );
      empty.appendChild(
        element(
          "p",
          "conversation-copy",
          vm.selectedProject.missing
            ? "Choose an existing project from the left panel or add a directory before starting a web session."
            : "Pick a session or start a new one in the selected project.",
        ),
      );
      empty.appendChild(
        element(
          "p",
          "conversation-project",
          vm.selectedProject.missing ? "No project selected" : `Project: ${vm.selectedProject.path}`,
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
        makeConversationNote("loading", "Loading persisted transcript..."),
      );
    } else if (vm.transcriptError) {
      thread.appendChild(
        makeConversationNote("transcript error", vm.transcriptError, {
          error: true,
        }),
      );
    } else if (vm.transcript.length === 0 && !showingDraft && !vm.activeTurn) {
      thread.appendChild(
        makeConversationNote("history", "No completed transcript yet."),
      );
    } else {
      for (const part of normalizeDocumentParts(vm)) {
        thread.appendChild(renderPart(part));
      }
    }

    if (showingDraft && !vm.activeTurn) {
      thread.appendChild(
        makeConversationNote(
          "draft session",
          "The first submitted prompt will create a web session and start streaming into this view.",
        ),
      );
    }

    this.appendChild(thread);
    this.restoreOpenDisclosures(openDisclosures);
    if (shouldStickToBottom) {
      this.scrollTop = this.scrollHeight;
    }
  }
}

customElements.define("mu-conversation-view", MuConversationView);
