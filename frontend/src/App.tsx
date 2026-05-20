import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import mermaid from "mermaid";

type AgentKind = "interview_materials" | "programmer_interview";
type LiveStatus = "idle" | "thinking" | "waiting_for_input" | "error" | "running" | string;
type TransportStatus = "connected" | "disconnected" | "neutral";

interface UiRenderBlock {
  type: string;
  content: string;
}

interface UiMessage {
  id?: string;
  role: string;
  kind: string;
  content?: string;
  created_at?: number;
  turn_id?: string;
  tool_name?: string | null;
  tool_args?: string | null;
  tool_output?: string | null;
  render_blocks?: UiRenderBlock[];
}

interface MaterialsMeta {
  exists: boolean;
  path: string;
  updated_at?: number | null;
}

interface ReportMeta {
  path: string;
  updated_at: number;
}

interface WorkflowSnapshot {
  active_agent: AgentKind;
  materials: MaterialsMeta;
  interview_status: "not_started" | "in_progress" | "completed";
  interview_phase: string;
  materials_session_id?: string | null;
  interview_session_id?: string | null;
  report?: ReportMeta | null;
}

interface LiveSnapshot {
  agent_kind: AgentKind;
  title: string;
  session_id?: string | null;
  status: LiveStatus;
  last_error?: string | null;
  messages: UiMessage[];
}

interface LiveSubmitResponse {
  agent_kind: AgentKind;
  title: string;
  session_id?: string | null;
  status: LiveStatus;
  last_error?: string | null;
  new_messages: UiMessage[];
  total_message_count: number;
}

interface LiveEvent {
  type: string;
  agent_kind: AgentKind;
  snapshot?: LiveSnapshot | null;
  response?: LiveSubmitResponse | null;
  message?: UiMessage | null;
  workflow?: WorkflowSnapshot | null;
  error?: string | null;
}

interface SessionListItem {
  session_id: string;
  title: string;
  updated_at_unix_ms: number;
  prompt_count: number;
  is_active: boolean;
  interview_status: WorkflowSnapshot["interview_status"];
}

interface TurnGroup {
  userMessages: UiMessage[];
  systemMessages: UiMessage[];
  assistantGroups: Array<{ assistant: UiMessage; traces: UiMessage[] }>;
}

const eventTypes = [
  "snapshot",
  "turn_started",
  "message_added",
  "trace_added",
  "turn_finished",
  "turn_failed",
  "turn_cancelled",
  "workflow_updated",
];

export function App() {
  const [apiBaseUrl, setApiBaseUrl] = useState("");
  const [workflow, setWorkflow] = useState<WorkflowSnapshot | null>(null);
  const [activeAgent, setActiveAgent] = useState<AgentKind>("interview_materials");
  const [snapshot, setSnapshot] = useState<LiveSnapshot | null>(null);
  const [snapshots, setSnapshots] = useState<Partial<Record<AgentKind, LiveSnapshot>>>({});
  const [sessions, setSessions] = useState<SessionListItem[]>([]);
  const [messages, setMessages] = useState<UiMessage[]>([]);
  const [status, setStatus] = useState<LiveStatus>("idle");
  const [lastError, setLastError] = useState("");
  const [isRunning, setIsRunning] = useState(false);
  const [transportStatus, setTransportStatus] = useState<TransportStatus>("disconnected");
  const [transportLabel, setTransportLabel] = useState("HTTP");
  const [reportText, setReportText] = useState("");
  const [liveTrace, setLiveTrace] = useState<UiMessage[]>([]);
  const [prompt, setPrompt] = useState("");
  const [booting, setBooting] = useState(true);
  const messageStreamRef = useRef<HTMLElement | null>(null);

  const apiUrl = useCallback(
    (path: string) => {
      if (!apiBaseUrl) {
        return path;
      }
      return `${apiBaseUrl}${path}`;
    },
    [apiBaseUrl],
  );

  const requestJson = useCallback(
    async <T,>(path: string, options: RequestInit = {}): Promise<T> => {
      const response = await fetch(apiUrl(path), {
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
      return data as T;
    },
    [apiUrl],
  );

  const requestText = useCallback(
    async (path: string, options: RequestInit = {}) => {
      const response = await fetch(apiUrl(path), options);
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
    },
    [apiUrl],
  );

  const applySnapshot = useCallback((nextSnapshot: LiveSnapshot) => {
    setSnapshot(nextSnapshot);
    setActiveAgent(nextSnapshot.agent_kind || "interview_materials");
    setMessages([...(nextSnapshot.messages || [])]);
    setLiveTrace([]);
    setSnapshots((current) => ({ ...current, [nextSnapshot.agent_kind]: nextSnapshot }));
    setStatus(nextSnapshot.status || "idle");
    setLastError(nextSnapshot.last_error || "");
    setIsRunning(nextSnapshot.status === "thinking");
  }, []);

  const mergeMessagesIntoState = useCallback((incoming: UiMessage[]) => {
    if (!incoming || incoming.length === 0) {
      return;
    }
    setMessages((current) => mergeMessages(current, incoming));
  }, []);

  const mergeLiveTraceIntoState = useCallback((incoming: UiMessage[]) => {
    if (!incoming || incoming.length === 0) {
      return;
    }
    setLiveTrace((current) => mergeMessages(current, incoming));
  }, []);

  const applySubmitResponse = useCallback(
    (response: LiveSubmitResponse) => {
      const newMessages = response.new_messages || [];
      setMessages((current) => {
        const merged = mergeMessages(current, newMessages);
        const nextSnapshot: LiveSnapshot = {
          agent_kind: response.agent_kind || activeAgent,
          title: response.title || snapshot?.title || "Live conversation",
          session_id: response.session_id || null,
          status: response.status || "waiting_for_input",
          last_error: response.last_error || null,
          messages: merged,
        };
        setSnapshot(nextSnapshot);
        setActiveAgent(nextSnapshot.agent_kind);
        setSnapshots((currentSnapshots) => ({
          ...currentSnapshots,
          [nextSnapshot.agent_kind]: nextSnapshot,
        }));
        return merged;
      });
      if (response.status === "thinking") {
        setLiveTrace([]);
      }
      setStatus(response.status || "waiting_for_input");
      setLastError(response.last_error || "");
      setIsRunning(response.status === "thinking");
    },
    [activeAgent, snapshot?.title],
  );

  const refreshWorkflow = useCallback(async () => {
    const nextWorkflow = await requestJson<WorkflowSnapshot>("/api/workflow");
    setWorkflow(nextWorkflow);
    return nextWorkflow;
  }, [requestJson]);

  const loadSessions = useCallback(async () => {
    setSessions(await requestJson<SessionListItem[]>("/api/sessions"));
  }, [requestJson]);

  const refreshAfterTurn = useCallback(async () => {
    try {
      await Promise.all([loadSessions(), refreshWorkflow()]);
    } catch (error) {
      setLastError(error instanceof Error ? error.message : String(error));
    }
  }, [loadSessions, refreshWorkflow]);

  const applyLiveEvent = useCallback(
    (payload: LiveEvent) => {
      if (payload.workflow) {
        setWorkflow(payload.workflow);
      }
      if (payload.snapshot) {
        applySnapshot(payload.snapshot);
      }
      if (payload.response) {
        applySubmitResponse(payload.response);
      }
      if (payload.message && payload.type === "trace_added") {
        mergeLiveTraceIntoState([payload.message]);
        setStatus("thinking");
        setIsRunning(true);
      } else if (payload.message) {
        mergeMessagesIntoState([payload.message]);
        setStatus("thinking");
        setIsRunning(true);
      }
      if (payload.type === "turn_finished") {
        setIsRunning(false);
        setLiveTrace([]);
        setStatus(payload.response?.status || "waiting_for_input");
        setLastError(payload.response?.last_error || "");
        if (payload.workflow?.interview_status === "completed") {
          void viewReport();
        }
        void refreshAfterTurn();
      }
      if (payload.type === "turn_failed") {
        setIsRunning(false);
        setLiveTrace([]);
        setStatus("error");
        setLastError(payload.error || payload.response?.last_error || "Agent turn failed");
        void refreshAfterTurn();
      }
      if (payload.type === "turn_cancelled") {
        setIsRunning(false);
        setStatus(payload.snapshot?.status || "waiting_for_input");
        setLastError("");
        void refreshAfterTurn();
      }
    },
    [applySnapshot, applySubmitResponse, mergeLiveTraceIntoState, mergeMessagesIntoState, refreshAfterTurn],
  );

  const loadLiveSnapshot = useCallback(async () => {
    const nextWorkflow = await requestJson<WorkflowSnapshot>("/api/workflow");
    setWorkflow(nextWorkflow);
    const nextActiveAgent = nextWorkflow.active_agent || "interview_materials";
    setActiveAgent(nextActiveAgent);
    applySnapshot(await requestJson<LiveSnapshot>(`/api/agents/${nextActiveAgent}/live`));
  }, [applySnapshot, requestJson]);

  const bootstrap = useCallback(async () => {
    setBooting(true);
    try {
      const baseUrl = await resolveApiBaseUrl();
      setApiBaseUrl(baseUrl);
    } catch (error) {
      setLastError(error instanceof Error ? error.message : String(error));
      setStatus("error");
    }
  }, []);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  useEffect(() => {
    if (!apiBaseUrl && window.location.protocol === "tauri:") {
      return;
    }
    let cancelled = false;
    async function bootRuntime() {
      setBooting(true);
      try {
        await waitForHealth(apiUrl("/api/health"));
        await Promise.all([loadSessions(), loadLiveSnapshot()]);
      } catch (error) {
        if (!cancelled) {
          setLastError(error instanceof Error ? error.message : String(error));
          setStatus("error");
        }
      } finally {
        if (!cancelled) {
          setBooting(false);
        }
      }
    }
    void bootRuntime();
    return () => {
      cancelled = true;
    };
  }, [apiBaseUrl, apiUrl, loadLiveSnapshot, loadSessions]);

  useEffect(() => {
    if (booting) {
      return;
    }
    const source = new EventSource(apiUrl(`/api/agents/${activeAgent}/events`));
    setTransportStatus("disconnected");
    setTransportLabel("SSE connecting");

    source.onopen = () => {
      setTransportStatus("connected");
      setTransportLabel("SSE");
    };
    source.onerror = () => {
      setTransportStatus("disconnected");
      setTransportLabel("SSE reconnecting");
    };
    const handleEvent = (event: MessageEvent<string>) => {
      try {
        const payload = JSON.parse(event.data) as LiveEvent;
        if (payload.agent_kind && payload.agent_kind !== activeAgent) {
          return;
        }
        applyLiveEvent(payload);
      } catch (error) {
        setLastError(`Failed to parse live event: ${error instanceof Error ? error.message : String(error)}`);
      }
    };
    for (const eventType of eventTypes) {
      source.addEventListener(eventType, handleEvent);
    }
    return () => {
      source.close();
    };
  }, [activeAgent, apiUrl, applyLiveEvent, booting]);

  useEffect(() => {
    messageStreamRef.current?.scrollTo({
      top: messageStreamRef.current.scrollHeight,
      behavior: "smooth",
    });
  }, [messages, liveTrace, isRunning]);

  const viewReport = useCallback(async () => {
    try {
      setReportText(await requestText("/api/interview/report"));
    } catch (error) {
      setLastError(error instanceof Error ? error.message : String(error));
      setReportText("");
    }
  }, [requestText]);

  const onSend = async () => {
    const trimmed = prompt.trim();
    if (!trimmed || isRunning) {
      return;
    }
    setIsRunning(true);
    setLastError("");
    setStatus("running");
    let accepted = false;
    try {
      const response = await requestJson<LiveSubmitResponse>(`/api/agents/${activeAgent}/messages`, {
        method: "POST",
        body: JSON.stringify({ prompt: trimmed }),
      });
      setPrompt("");
      accepted = true;
      applySubmitResponse(response);
      await loadSessions();
      await refreshWorkflow();
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    } finally {
      if (!accepted) {
        setIsRunning(false);
      }
    }
  };

  const onCancelTurn = async () => {
    if (!isRunning && status !== "thinking") {
      return;
    }
    setLastError("");
    try {
      applySnapshot(await requestJson<LiveSnapshot>(`/api/agents/${activeAgent}/turn/cancel`, { method: "POST" }));
      await refreshAfterTurn();
    } catch (error) {
      setLastError(error instanceof Error ? error.message : String(error));
    }
  };

  const onNewSession = async () => {
    if (isRunning) {
      return;
    }
    setIsRunning(true);
    setLastError("");
    setReportText("");
    try {
      const nextSnapshot = await requestJson<LiveSnapshot>("/api/live/session/new", { method: "POST" });
      applySnapshot(nextSnapshot);
      await refreshWorkflow();
      await loadSessions();
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    } finally {
      setIsRunning(false);
    }
  };

  const onSwitchAgent = async (agentKind: AgentKind) => {
    if (isRunning || activeAgent === agentKind) {
      return;
    }
    setActiveAgent(agentKind);
    setLastError("");
    setReportText("");
    try {
      const nextSnapshot = snapshots[agentKind] || (await requestJson<LiveSnapshot>(`/api/agents/${agentKind}/live`));
      applySnapshot(nextSnapshot);
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    }
  };

  const onSelectSession = async (sessionId: string) => {
    if (isRunning) {
      return;
    }
    setIsRunning(true);
    setLastError("");
    setReportText("");
    try {
      applySnapshot(
        await requestJson<LiveSnapshot>("/api/live/session", {
          method: "POST",
          body: JSON.stringify({ session_id: sessionId }),
        }),
      );
      await refreshWorkflow();
      await loadSessions();
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    } finally {
      setIsRunning(false);
    }
  };

  const onStartInterview = async () => {
    if (isRunning) {
      return;
    }
    setIsRunning(true);
    setLastError("");
    let accepted = false;
    try {
      const nextSnapshot = await requestJson<LiveSnapshot>("/api/interview/start", { method: "POST" });
      setActiveAgent("programmer_interview");
      applySnapshot(nextSnapshot);
      accepted = true;
      await refreshWorkflow();
      await loadSessions();
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    } finally {
      if (!accepted) {
        setIsRunning(false);
      }
    }
  };

  const onFinishInterview = async () => {
    if (isRunning) {
      return;
    }
    setIsRunning(true);
    setLastError("");
    let accepted = false;
    try {
      const response = await requestJson<LiveSubmitResponse>("/api/interview/finish", { method: "POST" });
      setActiveAgent("programmer_interview");
      accepted = true;
      applySubmitResponse(response);
      await refreshWorkflow();
      await loadSessions();
    } catch (error) {
      setStatus("error");
      setLastError(error instanceof Error ? error.message : String(error));
    } finally {
      if (!accepted) {
        setIsRunning(false);
      }
    }
  };

  const isInterview = activeAgent === "programmer_interview";
  const isBusy = isRunning || status === "thinking";
  const groupedTurns = useMemo(() => groupMessagesIntoTurns(messages), [messages]);

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand-panel">
          <p className="eyebrow">Scribe Engine</p>
          <h1>Interview Agents</h1>
          <p className="subtle">Two coordinated agents: one prepares architecture interview material, one conducts the interview.</p>
        </div>

        <section className="status-panel">
          <p className="meta-label">Agent</p>
          <div className="agent-switcher">
            <button className={`agent-tab ${!isInterview ? "active" : ""}`} type="button" onClick={() => onSwitchAgent("interview_materials")}>
              Materials
            </button>
            <button className={`agent-tab ${isInterview ? "active" : ""}`} type="button" onClick={() => onSwitchAgent("programmer_interview")}>
              Interview
            </button>
          </div>
        </section>

        <section className="status-panel">
          <StatusLine label="Transport" value={<Chip status={transportStatus} label={transportLabel} />} />
          <StatusLine label="Runtime" value={<Chip status={isBusy ? "thinking" : status} label={formatStatus(isBusy ? "running" : status)} />} />
          <StatusLine label="Active Agent" value={isInterview ? "Interview" : "Materials"} />
          <StatusLine label="Live Session" value={snapshot?.session_id || "Not initialized"} />
          <StatusLine label="Turns" value={summarizeTurnCount(messages)} />
        </section>

        <section className="status-panel">
          <StatusLine label="Materials" value={workflow?.materials?.exists ? "Generated" : "Missing"} />
          <StatusLine label="Interview" value={formatInterviewStatus(workflow?.interview_status || "not_started")} />
          <StatusLine label="Phase" value={workflow?.interview_phase || "INIT"} />
          <StatusLine label="Tool Steps" value={String(messages.filter((message) => message.kind === "tool_call" || message.kind === "tool_result").length)} />
          <div className="status-line block">
            <span className="meta-label">Last Error</span>
            <p className="subtle">{lastError || "None"}</p>
          </div>
        </section>

        <section className="status-panel sessions-panel">
          <div className="status-line">
            <span className="meta-label">Sessions</span>
            <button className="inline-button" type="button" onClick={onNewSession} disabled={isBusy}>
              New
            </button>
          </div>
          <div className="session-list" aria-label="Conversation sessions">
            <button className={`session-card ${!snapshot?.session_id && messages.length === 0 ? "active" : ""}`} type="button" onClick={onNewSession} disabled={isBusy}>
              <div className="session-card-head">
                <strong>New session</strong>
                <span className="session-time">Draft</span>
              </div>
              <p className="subtle">Start a separate materials and interview workflow.</p>
            </button>
            {sessions.map((session) => (
              <button
                className={`session-card ${session.is_active ? "active" : ""}`}
                type="button"
                onClick={() => onSelectSession(session.session_id)}
                disabled={isBusy}
                key={session.session_id}
              >
                <div className="session-card-head">
                  <strong>{session.title || "Untitled session"}</strong>
                  <span className="session-time">{formatTimestamp(session.updated_at_unix_ms)}</span>
                </div>
                <p className="subtle">{session.session_id}</p>
                <p className="subtle">
                  {session.prompt_count || 0} turn{session.prompt_count === 1 ? "" : "s"} · {formatInterviewStatus(session.interview_status || "not_started")}
                </p>
              </button>
            ))}
          </div>
        </section>

        <section className="status-panel tips-panel">
          <p className="meta-label">Interview Actions</p>
          <div className="action-stack">
            <button className="secondary-button" type="button" onClick={onStartInterview} disabled={isBusy || !workflow?.materials?.exists}>
              Start Interview
            </button>
            <button className="secondary-button" type="button" onClick={onFinishInterview} disabled={isBusy || workflow?.interview_status !== "in_progress"}>
              Finish Interview
            </button>
            <button className="secondary-button" type="button" onClick={viewReport} disabled={isBusy || !workflow?.report}>
              View Report
            </button>
          </div>
          <p className="subtle">{workflow?.materials?.path || "Materials path unavailable"}</p>
          <p className="subtle">Sessions are saved independently; switching sessions restores that interview workflow.</p>
        </section>
      </aside>

      <main className="workspace">
        <header className="workspace-header">
          <div>
            <p className="eyebrow">{isInterview ? "Programmer Interview" : "Interview Materials"}</p>
            <h2>{snapshot?.title || agentTitle(activeAgent)}</h2>
          </div>
          <div className="header-meta">
            <span className="subtle">{booting ? "Starting local runtime" : describeStatus(isBusy, status, lastError)}</span>
            <span className="subtle shortcut-hint">Enter newline · Cmd/Ctrl+Enter send</span>
          </div>
        </header>

        <section className="message-stream" aria-live="polite" ref={messageStreamRef}>
          {messages.length === 0 ? (
            <EmptyState
              title={booting ? "Starting desktop runtime" : "Start the live conversation"}
              text={
                isInterview
                  ? "Start the interview after generating materials, then answer one question at a time."
                  : "Send a prompt to analyze the codebase and generate interview materials."
              }
            />
          ) : (
            <div className="conversation-stack">
              {groupedTurns.flatMap((turn, turnIndex) => [
                ...turn.userMessages.map((message, index) => <BasicMessage message={message} key={`turn-${turnIndex}-user-${message.id || index}`} />),
                ...turn.systemMessages.map((message, index) => <BasicMessage message={message} key={`turn-${turnIndex}-system-${message.id || index}`} />),
                ...turn.assistantGroups.map((group, index) => <AssistantTurn group={group} activeAgent={activeAgent} key={`turn-${turnIndex}-assistant-${group.assistant.id || index}`} />),
              ])}
              {isBusy ? <PendingCard traceMessages={liveTrace} /> : null}
            </div>
          )}
        </section>

        {reportText ? (
          <section className="report-panel">
            <div className="block-head">
              <h3>Evaluation Report</h3>
              <button className="inline-button" type="button" onClick={() => setReportText("")}>
                Hide
              </button>
            </div>
            <pre>{reportText}</pre>
          </section>
        ) : null}

        <footer className="composer">
          <div className="composer-shell">
            <label className="composer-label" htmlFor="prompt-input">
              {isInterview ? "Answer the interviewer" : "Message the materials agent"}
            </label>
            <textarea
              id="prompt-input"
              rows={4}
              placeholder={isInterview ? "Answer the current interview question..." : "Ask the materials agent to analyze this codebase and write .transcripts/interview_materials/latest_materials.md..."}
              value={prompt}
              disabled={isBusy}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                  event.preventDefault();
                  void onSend();
                }
              }}
            />
          </div>
          <div className="composer-actions">
            <p className="subtle">{isInterview ? "Reply as the programmer candidate. The interviewer uses only generated materials." : "Generate or refine interview materials before starting the interview."}</p>
            <button className="stop-button" type="button" onClick={onCancelTurn} disabled={!isBusy}>
              Stop
            </button>
            <button className="primary-button" type="button" onClick={onSend} disabled={isBusy || !prompt.trim()}>
              Send
            </button>
          </div>
        </footer>
      </main>
    </div>
  );
}

function StatusLine({ label, value }: { label: string; value: string | ReactNode }) {
  return (
    <div className="status-line">
      <span className="meta-label">{label}</span>
      {typeof value === "string" ? <span className="meta-value">{value}</span> : value}
    </div>
  );
}

function Chip({ status, label }: { status: string; label: string }) {
  return <span className={`chip ${status || "neutral"}`}>{label}</span>;
}

function BasicMessage({ message }: { message: UiMessage }) {
  return (
    <article className={`message-row ${message.kind}`}>
      <div className="message-avatar">{avatarLabel(message.role)}</div>
      <div className="message-stack message-card">
        <div className="message-meta">
          <span className="role-badge">{roleLabel(message.role)}</span>
          <span className="message-kind">{kindLabelText(message.kind)}</span>
        </div>
        <MessageBody message={message} />
      </div>
    </article>
  );
}

function AssistantTurn({ group, activeAgent }: { group: { assistant: UiMessage; traces: UiMessage[] }; activeAgent: AgentKind }) {
  return (
    <article className="message-row assistant">
      <div className="message-avatar">A</div>
      <div className="message-stack message-card">
        <div className="message-meta">
          <span className="role-badge">{activeAgent === "programmer_interview" ? "Interviewer" : "Assistant"}</span>
          <span className="message-kind">{group.traces.length > 0 ? "reply · traced" : "reply"}</span>
        </div>
        <MessageBody message={group.assistant} />
      </div>
    </article>
  );
}

function MessageBody({ message }: { message: UiMessage }) {
  const blocks = message.render_blocks || (message.content ? [{ type: "text", content: message.content }] : []);
  return (
    <div className="message-body">
      {blocks.map((block, index) => {
        if (block.type === "text") {
          return <p key={index}>{block.content}</p>;
        }
        if (block.type === "mermaid") {
          return <MermaidCard source={block.content} key={index} />;
        }
        return <CodeCard content={block.content} title={block.type === "code" ? "Code" : block.type} key={index} />;
      })}
    </div>
  );
}

function CodeCard({ content, title = "Code" }: { content: string; title?: string }) {
  return (
    <section className="block-card">
      <div className="block-head">
        <h4>{title}</h4>
      </div>
      <pre>{content}</pre>
    </section>
  );
}

function MermaidCard({ source }: { source: string }) {
  const [svg, setSvg] = useState("");
  const [error, setError] = useState("");
  const [showSource, setShowSource] = useState(false);

  useEffect(() => {
    let cancelled = false;
    async function render() {
      try {
        const id = `mermaid-${crypto.randomUUID()}`;
        const result = await mermaid.render(id, source);
        if (!cancelled) {
          setSvg(result.svg);
          setError("");
        }
      } catch (renderError) {
        if (!cancelled) {
          setSvg("");
          setError(`Mermaid render failed: ${renderError instanceof Error ? renderError.message : String(renderError)}`);
        }
      }
    }
    void render();
    return () => {
      cancelled = true;
    };
  }, [source]);

  return (
    <section className="block-card">
      <div className="block-head">
        <h4>Mermaid</h4>
        <button className="inline-button" type="button" onClick={() => setShowSource((value) => !value)}>
          {showSource ? "Hide" : "Code"}
        </button>
      </div>
      <div className="mermaid-host">{error ? <p className="error-text">{error}</p> : <div dangerouslySetInnerHTML={{ __html: svg }} />}</div>
      {showSource ? <pre>{source}</pre> : null}
    </section>
  );
}

function PendingCard({ traceMessages }: { traceMessages: UiMessage[] }) {
  return (
    <section className="pending-card" aria-label="Agent is working">
      <div className="pending-dots">
        <span />
        <span />
        <span />
      </div>
      <div className="live-progress">
        <div className="live-progress-title">{traceMessages.length > 0 ? "Working through the task" : "Working..."}</div>
        <ol className="live-progress-list">
          {traceMessages.length === 0 ? (
            <li className="live-progress-item current">Preparing the next step</li>
          ) : (
            traceMessages
              .slice(-8)
              .map(summarizeTrace)
              .filter(Boolean)
              .map((summary, index, summaries) => (
                <li className={`live-progress-item ${index === summaries.length - 1 ? "current" : ""}`} key={`${summary}-${index}`}>
                  {summary}
                </li>
              ))
          )}
        </ol>
      </div>
    </section>
  );
}

function EmptyState({ title, text }: { title: string; text: string }) {
  return (
    <div className="empty-state">
      <h3>{title}</h3>
      <p className="subtle">{text}</p>
    </div>
  );
}

function mergeMessages(current: UiMessage[], incoming: UiMessage[]) {
  const next = [...current];
  const byId = new Map(next.map((message, index) => [message.id, index]));
  for (const message of incoming) {
    if (!message?.id) {
      next.push(message);
      continue;
    }
    const existingIndex = byId.get(message.id);
    if (existingIndex === undefined) {
      byId.set(message.id, next.length);
      next.push(message);
    } else {
      next[existingIndex] = message;
    }
  }
  return next;
}

function groupMessagesIntoTurns(messages: UiMessage[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
  let currentTurn = createTurn();
  let currentTurnId: string | null = null;

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
      currentTurn.assistantGroups.push({ assistant: message, traces: [] });
      continue;
    }
    const currentAssistant = currentTurn.assistantGroups[currentTurn.assistantGroups.length - 1];
    if ((message.kind === "tool_call" || message.kind === "tool_result") && !currentAssistant) {
      currentTurn.assistantGroups.push({
        assistant: { role: "assistant", kind: "assistant", render_blocks: [] },
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

function createTurn(): TurnGroup {
  return { userMessages: [], systemMessages: [], assistantGroups: [] };
}

function hasTurnContent(turn: TurnGroup) {
  return turn.userMessages.length > 0 || turn.systemMessages.length > 0 || turn.assistantGroups.length > 0;
}

async function resolveApiBaseUrl() {
  const envBase = import.meta.env.VITE_API_BASE_URL?.trim();
  if (envBase) {
    return stripTrailingSlash(envBase);
  }

  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const apiBaseUrl = await invoke<string>("get_api_base_url");
    if (apiBaseUrl) {
      return stripTrailingSlash(apiBaseUrl);
    }
  } catch {
    // Browser-only dev mode can still use relative API paths.
  }

  return "";
}

async function waitForHealth(url: string) {
  const deadline = Date.now() + 10_000;
  let lastError = "";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url);
      const data = await response.json().catch(() => null);
      if (response.ok && data?.ok === true) {
        return;
      }
      lastError = `Health check failed: ${response.status}`;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(lastError || "Timed out waiting for local runtime");
}

function stripTrailingSlash(value: string) {
  return value.endsWith("/") ? value.slice(0, -1) : value;
}

function summarizeTrace(message: UiMessage) {
  if (message.kind === "tool_call") {
    return `Calling ${message.tool_name || "tool"}`;
  }
  if (message.kind === "tool_result") {
    return `Read result from ${message.tool_name || "tool"}`;
  }
  if (message.kind === "context_compacted") {
    return "Compacted conversation context";
  }
  if (message.content) {
    return message.content.length > 120 ? `${message.content.slice(0, 117)}...` : message.content;
  }
  return "";
}

function formatTimestamp(timestamp?: number) {
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

function summarizeTurnCount(messages: UiMessage[]) {
  const turns = new Set(messages.map((message) => message.turn_id).filter(Boolean));
  return turns.size === 0 ? "-" : String(turns.size);
}

function formatInterviewStatus(status: WorkflowSnapshot["interview_status"] | string) {
  const labels: Record<string, string> = {
    not_started: "Not started",
    in_progress: "In progress",
    completed: "Completed",
  };
  return labels[status] || status;
}

function formatStatus(value: LiveStatus) {
  const labels: Record<string, string> = {
    idle: "Idle",
    running: "Running",
    thinking: "Running",
    waiting_for_input: "Waiting",
    error: "Error",
  };
  return labels[value] || value;
}

function describeStatus(isBusy: boolean, value: LiveStatus, lastError: string) {
  if (lastError) {
    return lastError;
  }
  if (isBusy) {
    return "Agent is working";
  }
  if (value === "waiting_for_input") {
    return "Waiting for input";
  }
  if (value === "idle") {
    return "Waiting for runtime";
  }
  return formatStatus(value);
}

function avatarLabel(role: string) {
  const labels: Record<string, string> = {
    user: "U",
    assistant: "A",
    system: "S",
    tool: "T",
  };
  return labels[role] || role.slice(0, 1).toUpperCase();
}

function roleLabel(role: string) {
  const labels: Record<string, string> = {
    user: "You",
    assistant: "Assistant",
    system: "System",
    tool: "Tool",
  };
  return labels[role] || role;
}

function kindLabelText(kind: string) {
  const labels: Record<string, string> = {
    user: "prompt",
    assistant: "reply",
    system: "system",
    tool_call: "tool call",
    tool_result: "tool result",
    context_compacted: "compacted",
  };
  return labels[kind] || kind;
}

function agentTitle(agentKind: AgentKind) {
  return agentKind === "programmer_interview" ? "Programmer interview agent" : "Interview material generator";
}
