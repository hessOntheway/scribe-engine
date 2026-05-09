import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";

mermaid.initialize({
  startOnLoad: false,
  theme: "base",
  securityLevel: "loose",
  themeVariables: {
    background: "#11182d",
    primaryColor: "#18243f",
    primaryTextColor: "#e6eefb",
    primaryBorderColor: "#6cb6ff",
    lineColor: "#91a3bd",
    secondaryColor: "#11192d",
    tertiaryColor: "#0d1425",
    fontFamily: "SF Pro Text, Segoe UI, sans-serif",
  },
});

const state = {
  snapshot: null,
  messages: [],
  status: "idle",
  lastError: "",
  isRunning: false,
};

const nodes = {
  connectionStatus: document.querySelector("#connection-status"),
  runStatus: document.querySelector("#run-status"),
  headerStatus: document.querySelector("#header-status"),
  sessionMeta: document.querySelector("#session-meta"),
  turnMeta: document.querySelector("#turn-meta"),
  toolStepCount: document.querySelector("#tool-step-count"),
  errorMeta: document.querySelector("#error-meta"),
  conversationTitle: document.querySelector("#conversation-title"),
  messageStream: document.querySelector("#message-stream"),
  promptInput: document.querySelector("#prompt-input"),
  sendButton: document.querySelector("#send-button"),
  messageTemplate: document.querySelector("#message-template"),
};

bootstrap();

async function bootstrap() {
  bindEvents();
  await loadLiveSnapshot();
  render();
}

function bindEvents() {
  nodes.sendButton.addEventListener("click", onSend);
  nodes.promptInput.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
      event.preventDefault();
      onSend();
    }
  });
}

async function loadLiveSnapshot() {
  try {
    const snapshot = await requestJson("/api/live");
    applySnapshot(snapshot);
  } catch (error) {
    state.lastError = error instanceof Error ? error.message : String(error);
    state.status = "error";
  }
}

async function onSend() {
  const prompt = nodes.promptInput.value.trim();
  if (!prompt || state.isRunning) {
    return;
  }

  state.isRunning = true;
  state.lastError = "";
  state.status = "running";
  render();

  try {
    const response = await requestJson("/api/live/messages", {
      method: "POST",
      body: JSON.stringify({ prompt }),
    });

    nodes.promptInput.value = "";
    state.messages.push(...(response.new_messages || []));
    state.snapshot = {
      title: response.title || state.snapshot?.title || "Live conversation",
      session_id: response.session_id || null,
      status: response.status || "waiting_for_input",
      last_error: response.last_error || null,
      messages: state.messages,
    };
    state.status = response.status || "waiting_for_input";
    state.lastError = response.last_error || "";
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

function applySnapshot(snapshot) {
  state.snapshot = snapshot;
  state.messages = [...(snapshot.messages || [])];
  state.status = snapshot.status || "idle";
  state.lastError = snapshot.last_error || "";
}

function render() {
  renderHeaderState();
  renderMessages();
}

function renderHeaderState() {
  nodes.conversationTitle.textContent = state.snapshot?.title || "Live conversation";
  nodes.sessionMeta.textContent = state.snapshot?.session_id || "Not initialized";
  nodes.turnMeta.textContent = summarizeTurnCount(state.messages);
  nodes.toolStepCount.textContent = String(
    state.messages.filter((message) => message.kind === "tool_call" || message.kind === "tool_result").length,
  );
  nodes.errorMeta.textContent = state.lastError || "None";

  setChip(nodes.connectionStatus, "connected", "HTTP");
  setChip(nodes.runStatus, state.isRunning ? "thinking" : state.status, formatStatus(state.isRunning ? "running" : state.status));
  nodes.headerStatus.textContent = describeStatus();

  nodes.sendButton.disabled = state.isRunning;
  nodes.promptInput.disabled = state.isRunning;
}

function renderMessages() {
  nodes.messageStream.innerHTML = "";

  if (state.messages.length === 0) {
    nodes.messageStream.append(
      createEmptyState(
        "Start the live conversation",
        "发送第一条消息后，服务端会同步执行完整一轮，然后把新增的 user、assistant 和 tool 消息返回给这个页面。",
      ),
    );
    return;
  }

  const groupedTurns = groupMessagesIntoTurns(state.messages);
  const container = document.createElement("div");
  container.className = "conversation-stack";

  for (const turn of groupedTurns) {
    for (const userMessage of turn.userMessages) {
      container.append(renderBasicMessage(userMessage));
    }

    for (const systemMessage of turn.systemMessages) {
      container.append(renderBasicMessage(systemMessage));
    }

    for (const assistantMessage of turn.assistantGroups) {
      container.append(renderAssistantTurn(assistantMessage));
    }
  }

  if (state.isRunning) {
    container.append(renderPendingCard());
  }

  nodes.messageStream.append(container);
  scrollToBottom();
}

function groupMessagesIntoTurns(messages) {
  const turns = [];
  let currentTurn = createTurn();
  let currentTurnId = null;

  for (const message of messages) {
    if (currentTurnId !== null && message.turn_id && message.turn_id !== currentTurnId) {
      if (hasTurnContent(currentTurn)) {
        turns.push(currentTurn);
      }
      currentTurn = createTurn();
    }

    currentTurnId = message.turn_id || currentTurnId;

    if (message.kind === "user") {
      currentTurn.userMessages.push(message);
      continue;
    }

    if (message.kind === "system") {
      currentTurn.systemMessages.push(message);
      continue;
    }

    if (message.kind === "assistant") {
      currentTurn.assistantGroups.push({
        assistant: message,
        traces: [],
      });
      continue;
    }

    const currentAssistant = currentTurn.assistantGroups[currentTurn.assistantGroups.length - 1];
    if ((message.kind === "tool_call" || message.kind === "tool_result") && !currentAssistant) {
      currentTurn.assistantGroups.push({
        assistant: {
          role: "assistant",
          kind: "assistant",
          render_blocks: [],
        },
        traces: [message],
      });
      continue;
    }

    if (currentAssistant && (message.kind === "tool_call" || message.kind === "tool_result")) {
      currentAssistant.traces.push(message);
      continue;
    }

    currentTurn.systemMessages.push(message);
  }

  if (hasTurnContent(currentTurn)) {
    turns.push(currentTurn);
  }

  return turns;
}

function createTurn() {
  return {
    userMessages: [],
    systemMessages: [],
    assistantGroups: [],
  };
}

function hasTurnContent(turn) {
  return turn.userMessages.length > 0 || turn.systemMessages.length > 0 || turn.assistantGroups.length > 0;
}

function renderBasicMessage(message) {
  const fragment = nodes.messageTemplate.content.cloneNode(true);
  const row = fragment.querySelector(".message-row");
  const avatar = fragment.querySelector(".message-avatar");
  const roleBadge = fragment.querySelector(".role-badge");
  const kindLabel = fragment.querySelector(".message-kind");
  const body = fragment.querySelector(".message-body");
  const stack = fragment.querySelector(".message-stack");

  row.classList.add(message.kind);
  stack.classList.add("message-card");
  avatar.textContent = avatarLabel(message.role);
  roleBadge.textContent = roleLabel(message.role);
  kindLabel.textContent = kindLabelText(message.kind);

  for (const block of message.render_blocks || []) {
    appendBlock(body, block);
  }

  return row;
}

function renderAssistantTurn(group) {
  const fragment = nodes.messageTemplate.content.cloneNode(true);
  const row = fragment.querySelector(".message-row");
  const avatar = fragment.querySelector(".message-avatar");
  const roleBadge = fragment.querySelector(".role-badge");
  const kindLabel = fragment.querySelector(".message-kind");
  const body = fragment.querySelector(".message-body");
  const stack = fragment.querySelector(".message-stack");

  row.classList.add("assistant");
  stack.classList.add("message-card");
  avatar.textContent = "A";
  roleBadge.textContent = "Assistant";
  kindLabel.textContent = group.traces.length > 0 ? "reply · traced" : "reply";

  for (const block of group.assistant.render_blocks || []) {
    appendBlock(body, block);
  }

  if (group.traces.length > 0) {
    body.append(renderTracePanel(group.traces));
  }

  return row;
}

function appendBlock(body, block) {
  if (block.type === "text") {
    const paragraph = document.createElement("p");
    paragraph.textContent = block.content;
    body.append(paragraph);
    return;
  }

  if (block.type === "mermaid") {
    body.append(renderMermaidCard(block.content));
    return;
  }

  body.append(renderCodeCard(block.content, block.type === "code" ? "Code" : block.type));
}

function renderPendingCard() {
  const wrapper = document.createElement("section");
  wrapper.className = "pending-card";

  const text = document.createElement("div");
  const heading = document.createElement("h4");
  heading.textContent = "Agent is working";
  const detail = document.createElement("p");
  detail.className = "subtle";
  detail.textContent = "服务端正在同步执行这一轮，完成后会把新增消息一次性返回到这里。";

  text.append(heading, detail);

  const dots = document.createElement("div");
  dots.className = "pending-dots";
  dots.innerHTML = "<span></span><span></span><span></span>";

  wrapper.append(text, dots);
  return wrapper;
}

function renderTracePanel(traceMessages) {
  const wrapper = document.createElement("details");
  wrapper.className = "trace-toggle";

  const summary = document.createElement("summary");
  summary.className = "trace-head";

  const label = document.createElement("span");
  label.className = "trace-summary";
  label.textContent = "View execution steps";

  const count = document.createElement("span");
  count.className = "trace-count";
  count.textContent = `${traceMessages.length} steps`;

  summary.append(label, count);
  wrapper.append(summary);

  const panel = document.createElement("div");
  panel.className = "trace-panel";

  for (const trace of traceMessages) {
    const step = document.createElement("section");
    step.className = "trace-step";

    const head = document.createElement("div");
    head.className = "trace-step-head";

    const title = document.createElement("h4");
    title.textContent = trace.kind === "tool_call" ? `Call · ${trace.tool_name || "unknown"}` : `Result · ${trace.tool_name || "unknown"}`;

    const badge = document.createElement("span");
    badge.className = "message-kind";
    badge.textContent = trace.kind === "tool_call" ? "tool call" : "tool result";

    head.append(title, badge);
    step.append(head);

    const details = document.createElement("details");
    details.open = trace.kind === "tool_call";

    const detailsSummary = document.createElement("summary");
    detailsSummary.textContent = trace.kind === "tool_call" ? "Arguments" : "Output";

    const pre = document.createElement("pre");
    pre.textContent = trace.kind === "tool_call" ? trace.tool_args || trace.content || "(empty)" : trace.tool_output || trace.content || "(empty)";

    details.append(detailsSummary, pre);
    step.append(details);
    panel.append(step);
  }

  wrapper.append(panel);
  return wrapper;
}

function renderCodeCard(content, titleText = "Code") {
  const wrapper = document.createElement("section");
  wrapper.className = "block-card";

  const head = document.createElement("div");
  head.className = "block-head";

  const heading = document.createElement("h4");
  heading.textContent = titleText;
  head.append(heading);

  const pre = document.createElement("pre");
  pre.textContent = content;
  wrapper.append(head, pre);
  return wrapper;
}

function renderMermaidCard(source) {
  const wrapper = document.createElement("section");
  wrapper.className = "block-card";

  const head = document.createElement("div");
  head.className = "block-head";

  const heading = document.createElement("h4");
  heading.textContent = "Mermaid";

  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "inline-button";
  toggle.textContent = "Code";

  const host = document.createElement("div");
  host.className = "mermaid-host";

  const sourcePre = document.createElement("pre");
  sourcePre.textContent = source;
  sourcePre.hidden = true;

  toggle.addEventListener("click", () => {
    sourcePre.hidden = !sourcePre.hidden;
    toggle.textContent = sourcePre.hidden ? "Code" : "Hide";
  });

  head.append(heading, toggle);
  wrapper.append(head, host, sourcePre);
  renderMermaidInto(host, source);
  return wrapper;
}

async function renderMermaidInto(host, source) {
  const id = `mermaid-${crypto.randomUUID()}`;
  try {
    const { svg } = await mermaid.render(id, source);
    host.innerHTML = svg;
  } catch (error) {
    const fallback = document.createElement("p");
    fallback.className = "error-text";
    fallback.textContent = `Mermaid 渲染失败：${error instanceof Error ? error.message : String(error)}`;
    host.replaceChildren(fallback);
  }
}

function createEmptyState(title, text) {
  const div = document.createElement("div");
  div.className = "empty-state";
  div.innerHTML = `<h3>${title}</h3><p class="subtle">${text}</p>`;
  return div;
}

async function requestJson(url, options = {}) {
  const response = await fetch(url, {
    headers: {
      "Content-Type": "application/json",
      ...(options.headers || {}),
    },
    ...options,
  });

  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(data.error || `Request failed: ${response.status}`);
  }
  return data;
}

function setChip(node, status, label) {
  node.textContent = label;
  node.className = `chip ${status || "neutral"}`;
}

function formatStatus(status) {
  const labels = {
    idle: "Idle",
    running: "Running",
    thinking: "Running",
    waiting_for_input: "Waiting",
    error: "Error",
  };
  return labels[status] || status;
}

function describeStatus() {
  if (state.isRunning) {
    return "Agent is processing the current turn on the server";
  }

  const labels = {
    idle: "Live runtime is ready for the first prompt",
    waiting_for_input: "Current turn completed, waiting for your next input",
    error: "Last turn failed, inspect the error and send a follow-up",
  };

  return labels[state.status] || "Live runtime ready";
}

function summarizeTurnCount(messages) {
  const turnIds = new Set(messages.map((message) => message.turn_id).filter(Boolean));
  return turnIds.size > 0 ? `${turnIds.size} turns` : "-";
}

function avatarLabel(role) {
  const labels = {
    user: "U",
    assistant: "A",
    system: "S",
    tool: "T",
  };
  return labels[role] || "?";
}

function roleLabel(role) {
  const labels = {
    user: "User",
    assistant: "Assistant",
    tool: "Tool",
    system: "System",
  };
  return labels[role] || role;
}

function kindLabelText(kind) {
  const labels = {
    user: "prompt",
    assistant: "reply",
    tool_call: "tool call",
    tool_result: "tool result",
    system: "system",
  };
  return labels[kind] || kind;
}

function scrollToBottom() {
  requestAnimationFrame(() => {
    nodes.messageStream.scrollTop = nodes.messageStream.scrollHeight;
  });
}
