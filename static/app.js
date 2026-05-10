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
  workflow: null,
  activeAgent: "interview_materials",
  snapshot: null,
  sessions: [],
  snapshots: {},
  messages: [],
  status: "idle",
  lastError: "",
  isRunning: false,
  reportText: "",
};

const nodes = {
  connectionStatus: document.querySelector("#connection-status"),
  runStatus: document.querySelector("#run-status"),
  headerStatus: document.querySelector("#header-status"),
  activeAgentMeta: document.querySelector("#active-agent-meta"),
  sessionMeta: document.querySelector("#session-meta"),
  sessionList: document.querySelector("#session-list"),
  turnMeta: document.querySelector("#turn-meta"),
  toolStepCount: document.querySelector("#tool-step-count"),
  errorMeta: document.querySelector("#error-meta"),
  materialsStatus: document.querySelector("#materials-status"),
  materialsPath: document.querySelector("#materials-path"),
  interviewStatus: document.querySelector("#interview-status"),
  interviewPhase: document.querySelector("#interview-phase"),
  agentEyebrow: document.querySelector("#agent-eyebrow"),
  conversationTitle: document.querySelector("#conversation-title"),
  messageStream: document.querySelector("#message-stream"),
  reportPanel: document.querySelector("#report-panel"),
  promptInput: document.querySelector("#prompt-input"),
  composerLabel: document.querySelector("#composer-label"),
  composerHelp: document.querySelector("#composer-help"),
  sendButton: document.querySelector("#send-button"),
  newSessionButton: document.querySelector("#new-session-button"),
  materialsAgentButton: document.querySelector("#materials-agent-button"),
  interviewAgentButton: document.querySelector("#interview-agent-button"),
  startInterviewButton: document.querySelector("#start-interview-button"),
  finishInterviewButton: document.querySelector("#finish-interview-button"),
  viewReportButton: document.querySelector("#view-report-button"),
  messageTemplate: document.querySelector("#message-template"),
};

bootstrap();

async function bootstrap() {
  bindEvents();
  await Promise.all([loadSessions(), loadLiveSnapshot()]);
  render();
}

function bindEvents() {
  nodes.sendButton.addEventListener("click", onSend);
  nodes.newSessionButton.addEventListener("click", onNewSession);
  nodes.materialsAgentButton.addEventListener("click", () => switchAgent("interview_materials"));
  nodes.interviewAgentButton.addEventListener("click", () => switchAgent("programmer_interview"));
  nodes.startInterviewButton.addEventListener("click", onStartInterview);
  nodes.finishInterviewButton.addEventListener("click", onFinishInterview);
  nodes.viewReportButton.addEventListener("click", onViewReport);
  nodes.promptInput.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
      event.preventDefault();
      onSend();
    }
  });
}

async function loadLiveSnapshot() {
  try {
    const workflow = await requestJson("/api/workflow");
    state.workflow = workflow;
    state.activeAgent = workflow.active_agent || "interview_materials";
    const snapshot = await requestJson(`/api/agents/${state.activeAgent}/live`);
    applySnapshot(snapshot);
  } catch (error) {
    state.lastError = error instanceof Error ? error.message : String(error);
    state.status = "error";
  }
}

async function loadSessions() {
  try {
    state.sessions = await requestJson("/api/sessions");
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
    const response = await requestJson(`/api/agents/${state.activeAgent}/messages`, {
      method: "POST",
      body: JSON.stringify({ prompt }),
    });

    nodes.promptInput.value = "";
    state.messages.push(...(response.new_messages || []));
    state.snapshot = {
      agent_kind: response.agent_kind || state.activeAgent,
      title: response.title || state.snapshot?.title || "Live conversation",
      session_id: response.session_id || null,
      status: response.status || "waiting_for_input",
      last_error: response.last_error || null,
      messages: state.messages,
    };
    state.snapshots[state.activeAgent] = state.snapshot;
    state.status = response.status || "waiting_for_input";
    state.lastError = response.last_error || "";
    await loadSessions();
    await refreshWorkflow();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

async function onNewSession() {
  if (state.isRunning) {
    return;
  }

  state.isRunning = true;
  state.lastError = "";
  state.reportText = "";
  render();

  try {
    const snapshot = await requestJson("/api/live/session/new", { method: "POST" });
    applySnapshot(snapshot);
    state.snapshots = { [state.activeAgent]: state.snapshot };
    await refreshWorkflow();
    await loadSessions();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

async function switchAgent(agentKind) {
  if (state.isRunning || state.activeAgent === agentKind) {
    return;
  }
  state.activeAgent = agentKind;
  state.lastError = "";
  state.reportText = "";
  try {
    const snapshot = state.snapshots[agentKind] || await requestJson(`/api/agents/${agentKind}/live`);
    applySnapshot(snapshot);
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  }
  render();
}

async function onStartInterview() {
  if (state.isRunning) {
    return;
  }
  state.isRunning = true;
  state.lastError = "";
  render();
  try {
    const snapshot = await requestJson("/api/interview/start", { method: "POST" });
    state.activeAgent = "programmer_interview";
    applySnapshot(snapshot);
    await refreshWorkflow();
    await loadSessions();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

async function onSelectSession(sessionId) {
  if (state.isRunning) {
    return;
  }

  state.isRunning = true;
  state.lastError = "";
  state.reportText = "";
  render();

  try {
    const snapshot = await requestJson("/api/live/session", {
      method: "POST",
      body: JSON.stringify({ session_id: sessionId }),
    });
    applySnapshot(snapshot);
    state.snapshots = { [state.activeAgent]: state.snapshot };
    await refreshWorkflow();
    await loadSessions();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

async function onFinishInterview() {
  if (state.isRunning) {
    return;
  }
  state.isRunning = true;
  state.lastError = "";
  render();
  try {
    const response = await requestJson("/api/interview/finish", { method: "POST" });
    state.activeAgent = "programmer_interview";
    state.messages.push(...(response.new_messages || []));
    state.snapshot = {
      agent_kind: response.agent_kind || "programmer_interview",
      title: response.title || state.snapshot?.title || "Programmer interview agent",
      session_id: response.session_id || null,
      status: response.status || "waiting_for_input",
      last_error: response.last_error || null,
      messages: state.messages,
    };
    state.snapshots[state.activeAgent] = state.snapshot;
    await refreshWorkflow();
    await loadSessions();
    await onViewReport();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    render();
  }
}

async function onViewReport() {
  try {
    state.reportText = await requestText("/api/interview/report");
  } catch (error) {
    state.lastError = error instanceof Error ? error.message : String(error);
    state.reportText = "";
  }
  render();
}

async function refreshWorkflow() {
  state.workflow = await requestJson("/api/workflow");
}

function applySnapshot(snapshot) {
  state.snapshot = snapshot;
  state.activeAgent = snapshot.agent_kind || state.activeAgent;
  state.messages = [...(snapshot.messages || [])];
  state.snapshots[state.activeAgent] = state.snapshot;
  state.status = snapshot.status || "idle";
  state.lastError = snapshot.last_error || "";
}

function render() {
  renderHeaderState();
  renderSessions();
  renderMessages();
  renderReport();
}

function renderHeaderState() {
  const isInterview = state.activeAgent === "programmer_interview";
  nodes.agentEyebrow.textContent = isInterview ? "Programmer Interview" : "Interview Materials";
  nodes.conversationTitle.textContent = state.snapshot?.title || agentTitle(state.activeAgent);
  nodes.activeAgentMeta.textContent = isInterview ? "Interview" : "Materials";
  nodes.sessionMeta.textContent = state.snapshot?.session_id || "Not initialized";
  nodes.turnMeta.textContent = summarizeTurnCount(state.messages);
  nodes.toolStepCount.textContent = String(
    state.messages.filter((message) => message.kind === "tool_call" || message.kind === "tool_result").length,
  );
  nodes.errorMeta.textContent = state.lastError || "None";
  nodes.materialsStatus.textContent = state.workflow?.materials?.exists ? "Generated" : "Missing";
  nodes.materialsPath.textContent = state.workflow?.materials?.path || "Materials path unavailable";
  nodes.interviewStatus.textContent = formatInterviewStatus(state.workflow?.interview_status || "not_started");
  nodes.interviewPhase.textContent = state.workflow?.interview_phase || "INIT";

  setChip(nodes.connectionStatus, "connected", "HTTP");
  setChip(nodes.runStatus, state.isRunning ? "thinking" : state.status, formatStatus(state.isRunning ? "running" : state.status));
  nodes.headerStatus.textContent = describeStatus();

  nodes.sendButton.disabled = state.isRunning;
  nodes.promptInput.disabled = state.isRunning;
  nodes.newSessionButton.disabled = state.isRunning;
  nodes.startInterviewButton.disabled = state.isRunning || !state.workflow?.materials?.exists;
  nodes.finishInterviewButton.disabled = state.isRunning || state.workflow?.interview_status !== "in_progress";
  nodes.viewReportButton.disabled = state.isRunning || !state.workflow?.report;
  nodes.materialsAgentButton.classList.toggle("active", !isInterview);
  nodes.interviewAgentButton.classList.toggle("active", isInterview);
  nodes.composerLabel.textContent = isInterview ? "Answer the interviewer" : "Message the materials agent";
  nodes.composerHelp.textContent = isInterview
    ? "Reply as the programmer candidate. The interviewer uses only generated materials."
    : "Generate or refine interview materials before starting the interview.";
  nodes.promptInput.placeholder = isInterview
    ? "Answer the current interview question..."
    : "Ask the materials agent to analyze this codebase and write .transcripts/interview_materials/latest_materials.md...";
}

function renderSessions() {
  nodes.sessionList.innerHTML = "";

  const draftActive = !state.snapshot?.session_id && state.messages.length === 0;
  const draftCard = document.createElement("button");
  draftCard.type = "button";
  draftCard.className = `session-card ${draftActive ? "active" : ""}`;
  draftCard.disabled = state.isRunning;
  draftCard.addEventListener("click", onNewSession);
  draftCard.innerHTML = `
    <div class="session-card-head">
      <strong>New session</strong>
      <span class="session-time">Draft</span>
    </div>
    <p class="subtle">Start a separate materials and interview workflow.</p>
  `;
  nodes.sessionList.append(draftCard);

  for (const session of state.sessions || []) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `session-card ${session.is_active ? "active" : ""}`;
    button.disabled = state.isRunning;
    button.addEventListener("click", () => onSelectSession(session.session_id));

    const turnCount = session.prompt_count || 0;
    button.innerHTML = `
      <div class="session-card-head">
        <strong>${escapeHtml(session.title || "Untitled session")}</strong>
        <span class="session-time">${formatTimestamp(session.updated_at_unix_ms)}</span>
      </div>
      <p class="subtle">${escapeHtml(session.session_id)}</p>
      <p class="subtle">${turnCount} turn${turnCount === 1 ? "" : "s"} · ${formatInterviewStatus(session.interview_status || "not_started")}</p>
    `;

    nodes.sessionList.append(button);
  }
}

function renderMessages() {
  nodes.messageStream.innerHTML = "";

  if (state.messages.length === 0) {
    nodes.messageStream.append(
      createEmptyState(
        "Start the live conversation",
        state.activeAgent === "programmer_interview"
          ? "Start the interview after generating materials, then answer one question at a time."
          : "Send a prompt to analyze the codebase and generate interview materials.",
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

function renderReport() {
  if (!state.reportText) {
    nodes.reportPanel.hidden = true;
    nodes.reportPanel.innerHTML = "";
    return;
  }

  nodes.reportPanel.hidden = false;
  nodes.reportPanel.innerHTML = "";
  const head = document.createElement("div");
  head.className = "block-head";
  const title = document.createElement("h3");
  title.textContent = "Evaluation Report";
  const close = document.createElement("button");
  close.type = "button";
  close.className = "inline-button";
  close.textContent = "Hide";
  close.addEventListener("click", () => {
    state.reportText = "";
    render();
  });
  head.append(title, close);
  const pre = document.createElement("pre");
  pre.textContent = state.reportText;
  nodes.reportPanel.append(head, pre);
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
  roleBadge.textContent = state.activeAgent === "programmer_interview" ? "Interviewer" : "Assistant";
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

function formatTimestamp(timestamp) {
  if (!timestamp) {
    return "-";
  }

  try {
    return new Intl.DateTimeFormat(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    }).format(new Date(Number(timestamp)));
  } catch {
    return String(timestamp);
  }
}

function escapeHtml(text) {
  return String(text)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
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

async function requestText(url, options = {}) {
  const response = await fetch(url, options);
  const text = await response.text();
  if (!response.ok) {
    try {
      const data = JSON.parse(text);
      throw new Error(data.error || `Request failed: ${response.status}`);
    } catch (error) {
      if (error instanceof SyntaxError) {
        throw new Error(text || `Request failed: ${response.status}`);
      }
      throw error;
    }
  }
  return text;
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

function agentTitle(agentKind) {
  return agentKind === "programmer_interview" ? "Programmer interview agent" : "Interview material generator";
}

function formatInterviewStatus(status) {
  const labels = {
    not_started: "Not started",
    in_progress: "In progress",
    completed: "Completed",
  };
  return labels[status] || status;
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
