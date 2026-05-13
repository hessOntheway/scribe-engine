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
  eventSource: null,
  transportStatus: "disconnected",
  transportLabel: "HTTP",
  reportText: "",
  liveTrace: [],
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
  cancelTurnButton: document.querySelector("#cancel-turn-button"),
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
  connectEvents();
  render();
}

function bindEvents() {
  nodes.sendButton.addEventListener("click", onSend);
  nodes.cancelTurnButton.addEventListener("click", onCancelTurn);
  nodes.newSessionButton.addEventListener("click", onNewSession);
  nodes.materialsAgentButton.addEventListener("click", () => switchAgent("interview_materials"));
  nodes.interviewAgentButton.addEventListener("click", () => switchAgent("programmer_interview"));
  nodes.startInterviewButton.addEventListener("click", onStartInterview);
  nodes.finishInterviewButton.addEventListener("click", onFinishInterview);
  nodes.viewReportButton.addEventListener("click", onViewReport);
  nodes.promptInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      onSend();
    }
  });
}

function connectEvents() {
  if (state.eventSource) {
    state.eventSource.close();
    state.eventSource = null;
  }

  const source = new EventSource(`/api/agents/${state.activeAgent}/events`);
  state.eventSource = source;
  state.transportStatus = "disconnected";
  state.transportLabel = "SSE connecting";

  source.onopen = () => {
    state.transportStatus = "connected";
    state.transportLabel = "SSE";
    render();
  };

  source.onerror = () => {
    state.transportStatus = "disconnected";
    state.transportLabel = "SSE reconnecting";
    render();
  };

  for (const eventType of ["snapshot", "turn_started", "message_added", "trace_added", "turn_finished", "turn_failed", "turn_cancelled", "workflow_updated"]) {
    source.addEventListener(eventType, handleLiveEvent);
  }
}

function handleLiveEvent(event) {
  let payload;
  try {
    payload = JSON.parse(event.data);
  } catch (error) {
    state.lastError = `Failed to parse live event: ${error instanceof Error ? error.message : String(error)}`;
    render();
    return;
  }

  if (payload.agent_kind && payload.agent_kind !== state.activeAgent) {
    return;
  }

  applyLiveEvent(payload);
  render();
}

function applyLiveEvent(payload) {
  if (payload.workflow) {
    state.workflow = payload.workflow;
  }

  if (payload.snapshot) {
    applySnapshot(payload.snapshot);
  }

  if (payload.response) {
    applySubmitResponse(payload.response);
  }

  if (payload.message && payload.type === "trace_added") {
    mergeLiveTrace([payload.message]);
    state.status = "thinking";
    state.isRunning = true;
  } else if (payload.message) {
    mergeMessages([payload.message]);
    state.status = "thinking";
    state.isRunning = true;
  }

  if (payload.type === "turn_finished") {
    state.isRunning = false;
    state.liveTrace = [];
    state.status = payload.response?.status || "waiting_for_input";
    state.lastError = payload.response?.last_error || "";
    if (payload.workflow?.interview_status === "completed") {
      onViewReport();
    }
    refreshAfterTurn();
  }

  if (payload.type === "turn_failed") {
    state.isRunning = false;
    state.liveTrace = [];
    state.status = "error";
    state.lastError = payload.error || payload.response?.last_error || "Agent turn failed";
    refreshAfterTurn();
  }

  if (payload.type === "turn_cancelled") {
    state.isRunning = false;
    state.status = payload.snapshot?.status || "waiting_for_input";
    state.lastError = "";
    refreshAfterTurn();
  }
}

async function refreshAfterTurn() {
  try {
    await Promise.all([loadSessions(), refreshWorkflow()]);
  } catch (error) {
    state.lastError = error instanceof Error ? error.message : String(error);
  }
  render();
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

  let accepted = false;
  try {
    const response = await requestJson(`/api/agents/${state.activeAgent}/messages`, {
      method: "POST",
      body: JSON.stringify({ prompt }),
    });

    nodes.promptInput.value = "";
    accepted = true;
    applySubmitResponse(response);
    await loadSessions();
    await refreshWorkflow();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    if (!accepted) {
      state.isRunning = false;
    }
    render();
  }
}

async function onCancelTurn() {
  if (!state.isRunning && state.status !== "thinking") {
    return;
  }

  state.lastError = "";
  try {
    const snapshot = await requestJson(`/api/agents/${state.activeAgent}/turn/cancel`, {
      method: "POST",
    });
    applySnapshot(snapshot);
    await refreshAfterTurn();
  } catch (error) {
    state.lastError = error instanceof Error ? error.message : String(error);
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
  connectEvents();
  render();
}

async function onStartInterview() {
  if (state.isRunning) {
    return;
  }
  state.isRunning = true;
  state.lastError = "";
  render();
  let accepted = false;
  try {
    const snapshot = await requestJson("/api/interview/start", { method: "POST" });
    state.activeAgent = "programmer_interview";
    applySnapshot(snapshot);
    accepted = true;
    connectEvents();
    await refreshWorkflow();
    await loadSessions();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    if (!accepted) {
      state.isRunning = false;
    }
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
  let accepted = false;
  try {
    const response = await requestJson("/api/interview/finish", { method: "POST" });
    state.activeAgent = "programmer_interview";
    accepted = true;
    applySubmitResponse(response);
    await refreshWorkflow();
    await loadSessions();
  } catch (error) {
    state.status = "error";
    state.lastError = error instanceof Error ? error.message : String(error);
  } finally {
    if (!accepted) {
      state.isRunning = false;
    }
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
  state.liveTrace = [];
  state.snapshots[state.activeAgent] = state.snapshot;
  state.status = snapshot.status || "idle";
  state.lastError = snapshot.last_error || "";
  state.isRunning = state.status === "thinking";
}

function applySubmitResponse(response) {
  mergeMessages(response.new_messages || []);
  if (response.status === "thinking") {
    state.liveTrace = [];
  }
  state.snapshot = {
    agent_kind: response.agent_kind || state.activeAgent,
    title: response.title || state.snapshot?.title || "Live conversation",
    session_id: response.session_id || null,
    status: response.status || "waiting_for_input",
    last_error: response.last_error || null,
    messages: state.messages,
  };
  state.activeAgent = state.snapshot.agent_kind || state.activeAgent;
  state.snapshots[state.activeAgent] = state.snapshot;
  state.status = response.status || "waiting_for_input";
  state.lastError = response.last_error || "";
  state.isRunning = state.status === "thinking";
}

function mergeMessages(messages) {
  if (!messages || messages.length === 0) {
    return;
  }

  const byId = new Map(state.messages.map((message, index) => [message.id, index]));
  for (const message of messages) {
    if (!message?.id) {
      state.messages.push(message);
      continue;
    }
    const existingIndex = byId.get(message.id);
    if (existingIndex === undefined) {
      byId.set(message.id, state.messages.length);
      state.messages.push(message);
    } else {
      state.messages[existingIndex] = message;
    }
  }
}

function mergeLiveTrace(messages) {
  if (!messages || messages.length === 0) {
    return;
  }

  const byId = new Map(state.liveTrace.map((message, index) => [message.id, index]));
  for (const message of messages) {
    if (!message?.id) {
      state.liveTrace.push(message);
      continue;
    }
    const existingIndex = byId.get(message.id);
    if (existingIndex === undefined) {
      byId.set(message.id, state.liveTrace.length);
      state.liveTrace.push(message);
    } else {
      state.liveTrace[existingIndex] = message;
    }
  }
}

function render() {
  renderHeaderState();
  renderSessions();
  renderMessages();
  renderReport();
}

function renderHeaderState() {
  const isInterview = state.activeAgent === "programmer_interview";
  const isBusy = state.isRunning || state.status === "thinking";
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

  setChip(nodes.connectionStatus, state.transportStatus, state.transportLabel);
  setChip(nodes.runStatus, isBusy ? "thinking" : state.status, formatStatus(isBusy ? "running" : state.status));
  nodes.headerStatus.textContent = describeStatus();

  nodes.sendButton.disabled = isBusy;
  nodes.cancelTurnButton.disabled = !isBusy;
  nodes.promptInput.disabled = isBusy;
  nodes.newSessionButton.disabled = isBusy;
  nodes.startInterviewButton.disabled = isBusy || !state.workflow?.materials?.exists;
  nodes.finishInterviewButton.disabled = isBusy || state.workflow?.interview_status !== "in_progress";
  nodes.viewReportButton.disabled = isBusy || !state.workflow?.report;
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
  draftCard.disabled = state.isRunning || state.status === "thinking";
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
    button.disabled = state.isRunning || state.status === "thinking";
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

  if (state.isRunning || state.status === "thinking") {
    container.append(renderPendingCard(state.liveTrace));
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

function renderPendingCard(traceMessages = []) {
  const wrapper = document.createElement("section");
  wrapper.className = "pending-card";
  wrapper.setAttribute("aria-label", "Agent is working");

  const dots = document.createElement("div");
  dots.className = "pending-dots";
  dots.innerHTML = "<span></span><span></span><span></span>";

  wrapper.append(dots);
  wrapper.append(renderLiveProgress(traceMessages));
  return wrapper;
}

function renderLiveProgress(traceMessages = []) {
  const wrapper = document.createElement("div");
  wrapper.className = "live-progress";

  const title = document.createElement("div");
  title.className = "live-progress-title";
  title.textContent = traceMessages.length > 0 ? "Working through the task" : "Working...";
  wrapper.append(title);

  const list = document.createElement("ol");
  list.className = "live-progress-list";

  const visibleItems = traceMessages.slice(-8).map(summarizeTrace).filter(Boolean);
  if (visibleItems.length === 0) {
    const item = document.createElement("li");
    item.className = "live-progress-item current";
    item.textContent = "Preparing the next step";
    list.append(item);
  } else {
    visibleItems.forEach((summary, index) => {
      const item = document.createElement("li");
      item.className = `live-progress-item ${index === visibleItems.length - 1 ? "current" : ""}`;
      item.textContent = summary;
      list.append(item);
    });
  }

  wrapper.append(list);
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
    fallback.textContent = `Mermaid render failed: ${error instanceof Error ? error.message : String(error)}`;
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
  if (state.isRunning || state.status === "thinking") {
    return "Agent is processing the current turn and streaming updates";
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
    assistant_trace: "progress",
    context_compacted: "compact",
    tool_call: "tool call",
    tool_result: "tool result",
    system: "system",
  };
  return labels[kind] || kind;
}

function summarizeTrace(trace) {
  if (!trace) {
    return "";
  }

  if (trace.kind === "assistant_trace") {
    return truncateProgressText(firstVisibleLine(trace.content), 140);
  }

  if (trace.kind === "tool_call") {
    return summarizeToolCall(trace.tool_name, trace.tool_args || trace.content || "");
  }

  if (trace.kind === "tool_result") {
    return summarizeToolResult(trace);
  }

  if (trace.kind === "context_compacted") {
    const payload = parseJsonObject(trace.content);
    const removed = payload?.removed_messages;
    return removed ? `Context compacted · removed ${removed} messages` : "Context compacted";
  }

  return truncateProgressText(firstVisibleLine(trace.content || kindLabelText(trace.kind)), 140);
}

function summarizeToolCall(toolName, rawArgs) {
  const name = toolName || "tool";
  const args = parseJsonObject(rawArgs);
  const path = stringArg(args, "path") || stringArg(args, "file_path") || stringArg(args, "target_path");
  const pattern = stringArg(args, "pattern") || stringArg(args, "query");

  if (name === "read_file") {
    return `Reading ${path || "a file"}`;
  }
  if (name === "grep_search") {
    return `Searching ${pattern || "the codebase"}`;
  }
  if (name === "glob_search") {
    return `Scanning ${pattern || "project files"}`;
  }
  if (name === "write_file") {
    return `Writing ${path || "a file"}`;
  }
  if (name === "todo_write") {
    return "Updating task list";
  }
  if (name === "task") {
    return "Delegating subtask";
  }

  return `Running ${name}`;
}

function summarizeToolResult(trace) {
  const name = trace.tool_name || "tool";
  const output = trace.tool_output || trace.content || "";
  const lower = output.toLowerCase();
  if (lower.includes("tool_error") || lower.includes("error")) {
    return `${name} reported an issue`;
  }
  return `${name} completed`;
}

function parseJsonObject(value) {
  if (!value) {
    return null;
  }
  try {
    const parsed = JSON.parse(value);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function stringArg(args, key) {
  const value = args?.[key];
  return typeof value === "string" && value.trim() ? value.trim() : "";
}

function firstVisibleLine(text) {
  return String(text || "")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find(Boolean) || "";
}

function truncateProgressText(text, maxLength) {
  const value = String(text || "").replace(/\s+/g, " ").trim();
  if (value.length <= maxLength) {
    return value;
  }
  return `${value.slice(0, Math.max(0, maxLength - 1)).trimEnd()}…`;
}

function scrollToBottom() {
  requestAnimationFrame(() => {
    nodes.messageStream.scrollTop = nodes.messageStream.scrollHeight;
  });
}
