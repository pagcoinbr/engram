import React, {useCallback, useEffect, useMemo, useRef, useState} from "react";
import {createRoot} from "react-dom/client";
import cytoscape from "cytoscape";
import fcose from "cytoscape-fcose";
import {
  Activity, AlertTriangle, BookOpen, Boxes, BrainCircuit, ChevronLeft,
  ChevronRight, CircleDot, Database, Filter, Focus, GitBranch, HeartPulse,
  Layers3, List, LocateFixed, Menu, Network, PanelLeftClose, PanelRightClose,
  RefreshCw, Search, Server, Settings2, Sparkles, X, ZoomIn, ZoomOut,
} from "lucide-react";
import "./styles.css";

cytoscape.use(fcose);

const TYPE_LABELS = {project: "Project", reference: "Reference", feedback: "Feedback", user: "User"};
const STATUS_LABELS = {indexed: "Indexed", unindexed: "Unindexed", stale: "Stale", unknown: "Coverage unknown"};
const NAV = [
  ["atlas", "Atlas", Network],
  ["memories", "Memories", BookOpen],
  ["recall", "Recall", Sparkles],
  ["activity", "Activity", Activity],
  ["system", "System", Server],
  ["models", "Models", BrainCircuit],
  ["config", "Configuration", Settings2],
];

async function api(path, options) {
  const response = await fetch(path, options);
  if (!response.ok) throw new Error((await response.text()) || `HTTP ${response.status}`);
  return response.json();
}

function useUrlState() {
  const initial = new URLSearchParams(location.search);
  const [view, setView] = useState(initial.get("view") || "atlas");
  const [project, setProject] = useState(initial.get("project") || localStorage.getItem("engram-project") || "all");
  const [node, setNode] = useState(initial.get("node") || "");
  useEffect(() => {
    const query = new URLSearchParams({view, project});
    if (node) query.set("node", node);
    history.replaceState(null, "", `?${query}`);
    localStorage.setItem("engram-project", project);
  }, [view, project, node]);
  return {view, setView, project, setProject, node, setNode};
}

function Badge({children, tone = "neutral"}) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

function TypeBadge({type}) {
  const safe = TYPE_LABELS[type] ? type : "unknown";
  return <Badge tone={safe}>{TYPE_LABELS[type] || "Other"}</Badge>;
}

function StatusBadge({status}) {
  return <Badge tone={`status-${status}`}>{STATUS_LABELS[status] || status}</Badge>;
}

function Notice({children, tone = "warning"}) {
  return <div className={`notice notice-${tone}`}><AlertTriangle size={15}/><span>{children}</span></div>;
}

function Spinner({label = "Loading memories…"}) {
  return <div className="loading"><RefreshCw size={18}/><span>{label}</span></div>;
}

function Empty({title, detail}) {
  return <div className="empty"><CircleDot size={28}/><strong>{title}</strong><span>{detail}</span></div>;
}

function Navigation({view, setView, open, setOpen}) {
  return <aside className={`navigation ${open ? "nav-open" : ""}`}>
    <div className="brand"><div className="brand-mark"><BrainCircuit size={23}/></div><div><strong>Engram</strong><span>Memory Atlas</span></div></div>
    <nav aria-label="Primary navigation">
      {NAV.map(([id, label, Icon]) => <button key={id} className={view === id ? "active" : ""} onClick={() => {setView(id); setOpen(false);}}>
        <Icon size={18}/><span>{label}</span>
      </button>)}
    </nav>
    <div className="nav-foot"><span className="pulse"/>Local memory system</div>
  </aside>;
}

function Coverage({coverage, compact = false}) {
  if (!coverage) return null;
  return <div className={`coverage ${compact ? "compact" : ""}`}>
    <span><i className="dot indexed"/>{coverage.indexed} indexed</span>
    {!!coverage.stale && <span><i className="dot stale"/>{coverage.stale} stale</span>}
    <span><i className="dot unknown"/>{coverage.unknown} unknown</span>
    <span className="coverage-total">{coverage.totalMemories} total</span>
  </div>;
}

function WorkspaceHeader({view, project, setProject, projects, coverage, setNavOpen}) {
  const title = NAV.find(([id]) => id === view)?.[1] || "Atlas";
  return <header className="workspace-header">
    <button className="icon-button menu-button" onClick={() => setNavOpen(true)} aria-label="Open navigation"><Menu size={20}/></button>
    <div className="workspace-title"><span>Engram</span><h1>{title}</h1></div>
    <div className="header-actions">
      <Coverage coverage={coverage} compact/>
      <label className="scope-select"><span>Scope</span><select value={project} onChange={e => setProject(e.target.value)}>
        <option value="all">All projects</option>
        {projects.map(item => <option key={item.id} value={item.id}>{item.label} · {item.count}</option>)}
      </select></label>
    </div>
  </header>;
}

function MemoryListItem({node, selected, onSelect}) {
  return <button className={`memory-item ${selected ? "selected" : ""}`} onClick={() => onSelect(node.id)}>
    <span className={`memory-orb type-${TYPE_LABELS[node.type] ? node.type : "unknown"}`}/>
    <span className="memory-copy"><strong>{node.name}</strong><small>{node.description || node.file}</small><span className="memory-meta">{node.projectLabel}<i/> {node.file}</span></span>
    <span className={`status-pin status-${node.indexStatus}`} title={STATUS_LABELS[node.indexStatus]}/>
  </button>;
}

function GraphCanvas({snapshot, nodes, layers, query, selectedId, onSelect, focusMode, onFocusMode}) {
  const holder = useRef(null);
  const cyRef = useRef(null);
  const selectRef = useRef(onSelect);
  selectRef.current = onSelect;

  const elements = useMemo(() => {
    const visibleIds = new Set(nodes.map(node => node.id));
    let allowed = visibleIds;
    if (focusMode && selectedId) {
      const neighborIds = new Set([selectedId]);
      snapshot.edges.forEach(edge => {
        if (edge.source === selectedId) neighborIds.add(edge.target);
        if (edge.target === selectedId) neighborIds.add(edge.source);
      });
      allowed = new Set([...visibleIds].filter(id => neighborIds.has(id)));
    }
    const nodeElements = snapshot.nodes.filter(node => allowed.has(node.id)).map(node => ({
      data: node,
      classes: `memory type-${TYPE_LABELS[node.type] ? node.type : "unknown"} status-${node.indexStatus}`,
    }));
    const edgeElements = snapshot.edges.filter(edge => allowed.has(edge.source) && allowed.has(edge.target) && layers[edge.kind]).map(edge => ({
      data: edge,
      classes: edge.kind,
    }));
    return [...nodeElements, ...edgeElements];
  }, [snapshot, nodes, layers, focusMode, selectedId]);

  useEffect(() => {
    if (!holder.current) return undefined;
    const cy = cytoscape({
      container: holder.current,
      elements,
      minZoom: 0.18,
      maxZoom: 2.6,
      wheelSensitivity: 0.18,
      selectionType: "single",
      style: [
        {selector: "node", style: {"background-color": "#64748b", width: 25, height: 25, label: "data(name)", color: "#dbe6f3", "font-size": 9, "text-valign": "bottom", "text-margin-y": 7, "text-max-width": 100, "text-wrap": "ellipsis", "overlay-opacity": 0}},
        {selector: "node.type-project", style: {"background-color": "#34d399"}},
        {selector: "node.type-reference", style: {"background-color": "#60a5fa"}},
        {selector: "node.type-feedback", style: {"background-color": "#fbbf24"}},
        {selector: "node.type-user", style: {"background-color": "#a78bfa"}},
        {selector: "node.status-unknown", style: {"border-width": 2, "border-color": "#64748b", "border-style": "dashed", opacity: 0.66}},
        {selector: "node.status-unindexed", style: {"border-width": 2, "border-color": "#94a3b8", "border-style": "dashed"}},
        {selector: "node.status-stale", style: {"border-width": 3, "border-color": "#fcd34d"}},
        {selector: "node:selected", style: {"border-width": 4, "border-color": "#5eead4", width: 33, height: 33, "z-index": 20}},
        {selector: "node.search-match", style: {"border-width": 4, "border-color": "#f8d66d", width: 31, height: 31, opacity: 1}},
        {selector: "node.search-dim", style: {opacity: 0.17, label: ""}},
        {selector: "edge", style: {width: 1.3, "line-color": "#4c6078", "curve-style": "bezier", opacity: 0.58}},
        {selector: "edge.wiki_link", style: {"line-color": "#5eead4", "target-arrow-color": "#5eead4", "target-arrow-shape": "triangle", "arrow-scale": 0.7, opacity: 0.9}},
        {selector: "edge.shared_entity", style: {"line-style": "dashed", "line-color": "#7c8da3", width: "mapData(count, 1, 8, 1, 4)"}},
        {selector: "edge:selected", style: {"line-color": "#f8d66d", "target-arrow-color": "#f8d66d", opacity: 1, width: 3, "z-index": 20}},
        {selector: ".neighborhood-dim", style: {opacity: 0.1, label: ""}},
      ],
      layout: {name: "fcose", quality: "default", randomize: true, animate: true, animationDuration: 450, nodeRepulsion: 5200, idealEdgeLength: 95, gravity: 0.22, padding: 45},
    });
    cy.on("tap", "node", event => selectRef.current({kind: "memory", id: event.target.id()}));
    cy.on("tap", "edge", event => selectRef.current({kind: "edge", id: event.target.id(), data: event.target.data()}));
    cy.on("tap", event => {if (event.target === cy) selectRef.current(null);});
    cyRef.current = cy;
    return () => {cy.destroy(); cyRef.current = null;};
  }, [elements]);

  useEffect(() => {
    const cy = cyRef.current;
    if (!cy) return;
    cy.elements().unselect();
    cy.elements().removeClass("neighborhood-dim");
    if (!selectedId) return;
    const selected = cy.getElementById(selectedId);
    if (!selected.length) return;
    selected.select();
    const keep = selected.closedNeighborhood();
    cy.elements().difference(keep).addClass("neighborhood-dim");
  }, [selectedId, elements]);

  useEffect(() => {
    const cy = cyRef.current;
    if (!cy) return;
    cy.nodes().removeClass("search-match search-dim");
    const normalized = query.trim().toLowerCase();
    if (!normalized) return;
    cy.nodes().forEach(node => {
      const data = node.data();
      const haystack = `${data.name} ${data.description} ${data.file}`.toLowerCase();
      node.addClass(haystack.includes(normalized) ? "search-match" : "search-dim");
    });
  }, [query, elements]);

  const command = action => {
    const cy = cyRef.current;
    if (!cy) return;
    if (action === "fit") cy.animate({fit: {eles: cy.elements(), padding: 44}, duration: 180});
    if (action === "in") cy.zoom({level: Math.min(cy.zoom() * 1.25, cy.maxZoom()), renderedPosition: {x: cy.width() / 2, y: cy.height() / 2}});
    if (action === "out") cy.zoom({level: Math.max(cy.zoom() / 1.25, cy.minZoom()), renderedPosition: {x: cy.width() / 2, y: cy.height() / 2}});
  };

  return <div className="graph-stage">
    <div ref={holder} className="graph-canvas" aria-label="Interactive memory graph"/>
    {!elements.some(item => !item.data.source) && <Empty title="No memories in this view" detail="Change the filters or project scope."/>}
    <div className="graph-controls">
      <button onClick={() => command("fit")} title="Fit graph"><LocateFixed size={17}/></button>
      <button onClick={() => command("in")} title="Zoom in"><ZoomIn size={17}/></button>
      <button onClick={() => command("out")} title="Zoom out"><ZoomOut size={17}/></button>
      {selectedId && <button className={focusMode ? "active" : ""} onClick={() => onFocusMode(!focusMode)} title="Focus neighborhood"><Focus size={17}/></button>}
    </div>
    <div className="graph-legend"><span><i className="line wiki"/>Wiki-link</span><span><i className="line shared"/>Shared entity</span><span><i className="node-key unindexed"/>Unindexed</span></div>
  </div>;
}

function MemoryInspector({node, onClose}) {
  const [detail, setDetail] = useState(null);
  const [error, setError] = useState("");
  useEffect(() => {
    setDetail(null); setError("");
    api(`/api/atlas/memory?project=${encodeURIComponent(node.project)}&file=${encodeURIComponent(node.file)}`).then(setDetail).catch(error => setError(error.message));
  }, [node.id]);
  return <div className="inspector-content">
    <div className="inspector-head"><div><span className="eyebrow">Memory</span><h2>{node.name}</h2></div><button className="icon-button" onClick={onClose} aria-label="Close inspector"><X size={19}/></button></div>
    <div className="badge-row"><TypeBadge type={node.type}/><StatusBadge status={node.indexStatus}/></div>
    <p className="description">{node.description || "No description provided."}</p>
    <dl className="metadata"><div><dt>Project</dt><dd>{node.projectLabel}</dd></div><div><dt>File</dt><dd>{node.file}</dd></div><div><dt>Entities</dt><dd>{node.entityCount || 0}</dd></div>{node.episodeCount > 1 && <div><dt>Episodes</dt><dd>{node.episodeCount} combined</dd></div>}</dl>
    <section><h3>Content</h3>{error && <Notice>{error}</Notice>}{!detail && !error ? <Spinner label="Loading content…"/> : <pre className="memory-content">{detail?.content}</pre>}</section>
  </div>;
}

function EdgeInspector({edge, snapshot, onSelectMemory, onClose}) {
  const left = snapshot.nodes.find(node => node.id === edge.source);
  const right = snapshot.nodes.find(node => node.id === edge.target);
  const wiki = edge.kind === "wiki_link";
  return <div className="inspector-content">
    <div className="inspector-head"><div><span className="eyebrow">Connection</span><h2>{wiki ? "Explicit link" : "Shared context"}</h2></div><button className="icon-button" onClick={onClose}><X size={19}/></button></div>
    <div className="connection-flow"><button onClick={() => onSelectMemory(left?.id)}>{left?.name}</button><span><GitBranch size={16}/></span><button onClick={() => onSelectMemory(right?.id)}>{right?.name}</button></div>
    <p className="description">{wiki ? `${left?.name} explicitly links to ${right?.name}.` : `These memories share ${edge.count} indexed ${edge.count === 1 ? "entity" : "entities"}. This is extracted evidence, not a similarity score.`}</p>
    {!wiki && <section><h3>Shared entities</h3><div className="entity-list">{edge.entities?.map(entity => <span key={entity}>{entity}</span>)}</div>{edge.truncated && <small>Additional entities are omitted from this compact response.</small>}</section>}
  </div>;
}

function AtlasPage({snapshot, selected, setSelected}) {
  const [query, setQuery] = useState("");
  const [type, setType] = useState("all");
  const [status, setStatus] = useState("all");
  const [layers, setLayers] = useState({wiki_link: true, shared_entity: true});
  const [listOpen, setListOpen] = useState(true);
  const [focusMode, setFocusMode] = useState(false);
  const selectedId = selected?.kind === "memory" ? selected.id : "";
  const selectedNode = snapshot.nodes.find(node => node.id === selectedId);
  const filtered = useMemo(() => snapshot.nodes.filter(node => (type === "all" || node.type === type) && (status === "all" || node.indexStatus === status)), [snapshot, type, status]);
  const matches = useMemo(() => {
    const normalized = query.trim().toLowerCase();
    if (!normalized) return filtered;
    return filtered.filter(node => `${node.name} ${node.description} ${node.file} ${node.projectLabel}`.toLowerCase().includes(normalized));
  }, [filtered, query]);
  const choose = choice => {setSelected(choice); setFocusMode(false);};
  return <div className="atlas-page">
    <div className="atlas-toolbar">
      <button className={`icon-button ${listOpen ? "active" : ""}`} onClick={() => setListOpen(!listOpen)} title="Toggle results"><PanelLeftClose size={18}/></button>
      <label className="search-field"><Search size={17}/><input value={query} onChange={event => setQuery(event.target.value)} placeholder="Find a memory…"/></label>
      <label className="filter-control"><Filter size={15}/><select value={type} onChange={event => setType(event.target.value)}><option value="all">All types</option>{Object.entries(TYPE_LABELS).map(([id, label]) => <option key={id} value={id}>{label}</option>)}</select></label>
      <label className="filter-control"><Database size={15}/><select value={status} onChange={event => setStatus(event.target.value)}><option value="all">Any coverage</option>{Object.entries(STATUS_LABELS).map(([id, label]) => <option key={id} value={id}>{label}</option>)}</select></label>
      <div className="layer-controls"><span><Layers3 size={15}/>Layers</span><button className={layers.wiki_link ? "active" : ""} onClick={() => setLayers({...layers, wiki_link: !layers.wiki_link})}>Links</button><button className={layers.shared_entity ? "active" : ""} onClick={() => setLayers({...layers, shared_entity: !layers.shared_entity})}>Entities</button></div>
      <span className="shown-count">{filtered.length} / {snapshot.nodes.length}</span>
    </div>
    {snapshot.warnings.map(warning => <Notice key={warning}>{warning}</Notice>)}
    {focusMode && <div className="focus-banner"><Focus size={15}/>Focused on {selectedNode?.name}<button onClick={() => setFocusMode(false)}>Return to overview</button></div>}
    <div className={`atlas-workspace ${listOpen ? "with-results" : ""} ${selected ? "with-inspector" : ""}`}>
      {listOpen && <aside className="results-panel"><div className="panel-title"><div><strong>Memories</strong><span>{matches.length} shown</span></div><button className="icon-button" onClick={() => setListOpen(false)}><ChevronLeft size={18}/></button></div><div className="memory-list">{matches.map(node => <MemoryListItem key={node.id} node={node} selected={selectedId === node.id} onSelect={id => choose({kind: "memory", id})}/>)}{!matches.length && <Empty title="No matches" detail="Clear or change your filters."/>}</div></aside>}
      <GraphCanvas snapshot={snapshot} nodes={filtered} layers={layers} query={query} selectedId={selectedId || selected?.id} onSelect={choose} focusMode={focusMode} onFocusMode={setFocusMode}/>
      {selected && <aside className="inspector">{selected.kind === "memory" && selectedNode ? <MemoryInspector node={selectedNode} onClose={() => choose(null)}/> : selected.kind === "edge" ? <EdgeInspector edge={selected.data} snapshot={snapshot} onSelectMemory={id => choose({kind: "memory", id})} onClose={() => choose(null)}/> : null}</aside>}
    </div>
  </div>;
}

function MemoriesPage({snapshot, selectedId, setSelectedId}) {
  const [query, setQuery] = useState("");
  const [type, setType] = useState("all");
  const selected = snapshot.nodes.find(node => node.id === selectedId) || snapshot.nodes[0];
  const shown = snapshot.nodes.filter(node => (type === "all" || node.type === type) && `${node.name} ${node.description} ${node.file}`.toLowerCase().includes(query.toLowerCase()));
  return <div className="page-content memories-page"><div className="content-toolbar"><label className="search-field"><Search size={17}/><input value={query} onChange={event => setQuery(event.target.value)} placeholder="Search all memories…"/></label><select value={type} onChange={event => setType(event.target.value)}><option value="all">All types</option>{Object.entries(TYPE_LABELS).map(([id, label]) => <option key={id} value={id}>{label}</option>)}</select></div><div className="reader-layout"><div className="reader-list">{shown.map(node => <MemoryListItem key={node.id} node={node} selected={selected?.id === node.id} onSelect={setSelectedId}/>)}</div><article className="reader">{selected ? <MemoryInspector node={selected} onClose={() => setSelectedId("")}/> : <Empty title="No memories" detail="This project scope has no memory files."/>}</article></div></div>;
}

function RecallPage({onLocate}) {
  const [query, setQuery] = useState("");
  const [result, setResult] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const run = async () => {
    if (!query.trim()) return;
    setBusy(true); setError("");
    try {setResult(await api(`/api/recall/hybrid?k=10&q=${encodeURIComponent(query)}`));} catch (error) {setError(error.message);} finally {setBusy(false);}
  };
  return <div className="page-content recall-page"><Notice tone="info">Recall currently searches the active <code>-root</code> store. All-project recall is not implied.</Notice><section className="hero-search"><span className="eyebrow">Hybrid recall</span><h2>What do you want to remember?</h2><div><Search size={19}/><input value={query} onChange={event => setQuery(event.target.value)} onKeyDown={event => event.key === "Enter" && run()} placeholder="Ask about a project, decision, person, or system…"/><button onClick={run} disabled={busy}>{busy ? "Searching…" : "Recall"}</button></div></section>{error && <Notice>{error}</Notice>}{result && <div className="recall-results"><div className="section-heading"><h3>Results</h3><span>{result.results.length} memories · {result.sources_used.join(" + ") || "no rankers"}</span></div>{result.results.map(item => <article key={item.file}><div><TypeBadge type={item.type || "reference"}/><h3>{item.name}</h3><p>{item.description}</p>{item.facts?.map((fact, index) => <blockquote key={index}>{fact}</blockquote>)}</div><button onClick={() => onLocate(`-root:${item.file}`)}><LocateFixed size={16}/>Locate</button></article>)}</div>}</div>;
}

function ActivityPage({snapshot}) {
  const [queues, setQueues] = useState(null);
  useEffect(() => {api("/api/queues").then(setQueues).catch(() => setQueues({staging: [], quarantine: []}));}, []);
  const cards = [{label: "Indexed", value: snapshot.coverage.indexed, tone: "indexed"}, {label: "Stale", value: snapshot.coverage.stale, tone: "stale"}, {label: "Unindexed", value: snapshot.coverage.unindexed, tone: "unindexed"}, {label: "Coverage unknown", value: snapshot.coverage.unknown, tone: "unknown"}];
  return <div className="page-content"><div className="metric-grid">{cards.map(card => <div className="metric-card" key={card.label}><i className={`dot ${card.tone}`}/><span>{card.label}</span><strong>{card.value}</strong></div>)}</div><div className="two-column"><section className="panel"><div className="section-heading"><h3>Staging</h3><Badge>{queues?.staging?.length || 0}</Badge></div>{queues?.staging?.map(item => <div className="queue-item" key={item}>{item}</div>) || <Spinner/>}{queues && !queues.staging.length && <Empty title="Staging is clear" detail="No short-term memories are waiting."/>}</section><section className="panel"><div className="section-heading"><h3>Quarantine</h3><Badge>{queues?.quarantine?.length || 0}</Badge></div>{queues?.quarantine?.map(item => <div className="queue-item" key={item}>{item}</div>)}{queues && !queues.quarantine.length && <Empty title="Quarantine is clear" detail="No suspect memories require review."/>}</section></div></div>;
}

function SystemPage() {
  const [health, setHealth] = useState(null);
  const [stats, setStats] = useState(null);
  const [vector, setVector] = useState(null);
  const [skills, setSkills] = useState(null);
  useEffect(() => {api("/api/health").then(setHealth); api("/api/graph/stats").then(setStats); api("/api/vector/stats").then(setVector); api("/api/skills").then(setSkills);}, []);
  if (!health) return <Spinner label="Checking Engram services…"/>;
  const services = [["Generation", health.generate, health.backend], ["Embeddings", health.embed, health.embed_dim ? `${health.embed_dim} dimensions` : "Unavailable"], ["Neo4j", health.neo4j, stats ? `${stats.entities} entities · ${stats.facts} facts` : "Unavailable"], ["Qdrant", vector?.reachable, vector?.reachable ? `${vector.points} points` : "Unavailable"]];
  return <div className="page-content"><div className="service-grid">{services.map(([name, ok, detail]) => <div className="service-card" key={name}><div><HeartPulse size={18}/><span>{name}</span></div><Badge tone={ok ? "ok" : "danger"}>{ok ? "Healthy" : "Down"}</Badge><p>{detail}</p></div>)}</div><section className="panel"><div className="section-heading"><h3>Installed skills</h3><span>{skills?.installed?.length || 0} available</span></div><div className="skills-grid">{skills?.installed?.map(skill => <div key={skill.name}><strong>/{skill.name}</strong><p>{skill.description}</p></div>) || <Spinner/>}</div></section></div>;
}

function ModelsPage() {
  const [status, setStatus] = useState(null);
  const [error, setError] = useState("");
  const load = () => {setStatus(null); setError(""); api("/api/atlas/models").then(setStatus).catch(error => setError(error.message));};
  useEffect(load, []);
  if (error) return <div className="page-content"><Notice>{error}</Notice><button className="action-button" onClick={load}>Retry probe</button></div>;
  if (!status) return <Spinner label="Probing configured models…"/>;
  return <div className="page-content model-page"><div className="section-heading"><div><span className="eyebrow">Model assignments</span><h3>Reasoning and retrieval health</h3></div><button className="action-button" onClick={load}><RefreshCw size={15}/>Refresh</button></div><Notice tone="info">A reachable server is not enough: embedding dimensions must match the active index. Probes are cached for 15 seconds.</Notice><div className="model-grid">{status.models.map(item => <article className="model-card" key={item.role}><div className="section-heading"><div><span className="eyebrow">{item.role}</span><h3>{item.configuredModel}</h3></div><Badge tone={item.reachable === true ? "ok" : item.reachable === false ? "danger" : "neutral"}>{item.reachable === true ? "Healthy" : item.reachable === false ? "Unavailable" : "Not probed"}</Badge></div><dl className="metadata"><div><dt>Provider</dt><dd>{item.provider}</dd></div><div><dt>Endpoint</dt><dd>{item.endpoint || "provider default"}</dd></div><div><dt>Observed</dt><dd>{item.observedModel || "—"}</dd></div>{item.role === "embedding" && <div><dt>Dimension</dt><dd>{item.observedDimension || "—"} / expected {item.expectedDimension || "—"}</dd></div>}<div><dt>Latency</dt><dd>{item.latencyMs ? `${item.latencyMs} ms` : "—"}</dd></div></dl>{item.error && <Notice>{item.error}</Notice>}</article>)}</div><RecommendedModels/></div>;
}

function RecommendedModels() {
  const models = [["BGE-M3", "1024D", "Current multilingual baseline"], ["Qwen3-Embedding-0.6B", "1024D", "Semantic-retrieval alternative"], ["EmbeddingGemma-300M", "768D", "Compact multilingual option"], ["Nomic Embed Text v1.5", "768D", "Lightweight local option"]];
  return <section className="panel recommendation-panel"><div className="section-heading"><h3>Embedding models to evaluate</h3><span>Each model needs its own index generation</span></div><div className="recommendation-grid">{models.map(([name, dimension, note]) => <div key={name}><strong>{name}</strong><Badge>{dimension}</Badge><p>{note}</p></div>)}</div></section>;
}

function ConfigPage() {
  const [data, setData] = useState(null);
  const [draft, setDraft] = useState(null);
  const [preview, setPreview] = useState(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const load = () => {setError(""); api("/api/atlas/config").then(result => {setData(result); setDraft(result.editable); setPreview(null);}).catch(error => setError(error.message));};
  useEffect(load, []);
  const update = (section, key, value) => setDraft(current => section ? {...current, [section]: {...current[section], [key]: value}} : {...current, [key]: value});
  const validate = async () => {try {setError(""); setPreview(await api("/api/atlas/config/validate", {method: "POST", headers: {"Content-Type": "application/json"}, body: JSON.stringify({config: draft})}));} catch (error) {setError(error.message);}};
  const save = async () => {setSaving(true); try {const result = await api("/api/atlas/config", {method: "PUT", headers: {"Content-Type": "application/json"}, body: JSON.stringify({revision: data.revision, config: draft})}); setPreview(result); await load();} catch (error) {setError(error.message);} finally {setSaving(false);}};
  if (error) return <div className="page-content"><Notice>{error}</Notice></div>;
  if (!data || !draft) return <Spinner label="Loading configuration…"/>;
  return <div className="page-content config-page"><span className="eyebrow">Configuration</span><h2>Models and retrieval</h2><Notice tone="info">Saved changes keep a local backup, use revision protection, and never expose secrets. Model-process restarts are required after saving.</Notice><section className="panel config-form"><div className="section-heading"><h3>{data.path}</h3><Badge>Revision {data.revision}</Badge></div><label>Generation backend<select value={draft.backend} onChange={event => update(null, "backend", event.target.value)}><option value="llama_cpp">llama.cpp</option><option value="ollama">Ollama</option><option value="claude">Claude CLI</option><option value="ccg">cc-gateway</option></select></label><h3>Reasoning model</h3><label>OpenAI-compatible endpoint<input value={draft.llama_cpp.url} onChange={event => update("llama_cpp", "url", event.target.value)} placeholder="http://host:port/v1"/></label><label>Model alias<input value={draft.llama_cpp.model} onChange={event => update("llama_cpp", "model", event.target.value)}/></label><label>Timeout seconds<input type="number" min="1" value={draft.llama_cpp.timeout_seconds} onChange={event => update("llama_cpp", "timeout_seconds", event.target.value)}/></label><h3>Embedding model</h3><label>Provider<select value={draft.embed.provider} onChange={event => update("embed", "provider", event.target.value)}><option value="llama_cpp">llama.cpp / OpenAI-compatible</option><option value="ollama">Ollama</option><option value="fastembed">FastEmbed</option></select></label><label>Embedding endpoint<input value={draft.embed.url} onChange={event => update("embed", "url", event.target.value)} placeholder="http://host:port/v1"/></label><label>Model alias<input value={draft.embed.model} onChange={event => update("embed", "model", event.target.value)}/></label><label>Vector dimension<input type="number" min="1" value={draft.embed.dim} onChange={event => update("embed", "dim", event.target.value)}/></label><h3>Graph recall</h3><label>Backend<select value={draft.graph.backend} onChange={event => update("graph", "backend", event.target.value)}><option value="graphiti_compat">Graphiti compatibility (recommended)</option><option value="native">Native Rust (shadow / evaluation)</option></select></label><div className="form-actions"><button className="action-button" onClick={validate}>Validate change</button><button className="action-button primary" onClick={save} disabled={saving}>{saving ? "Saving…" : "Save configuration"}</button></div></section>{preview && <Notice tone={preview.requiresReindex ? "warning" : "info"}>{preview.requiresReindex ? "Embedding space changed: rebuild Qdrant and Neo4j before relying on recall." : preview.ok ? `Saved. Backup: ${preview.backup}` : "Configuration is valid; no index migration is required."}</Notice>}<section className="panel"><div className="section-heading"><h3>Full configuration</h3><Badge>Secrets redacted</Badge></div><pre className="config-json">{JSON.stringify(data.config, null, 2)}</pre></section></div>;
}

function App() {
  const url = useUrlState();
  const [snapshot, setSnapshot] = useState(null);
  const [error, setError] = useState("");
  const [navOpen, setNavOpen] = useState(false);
  const [selected, setSelected] = useState(url.node ? {kind: "memory", id: url.node} : null);
  const load = useCallback(() => {setSnapshot(null); setError(""); api(`/api/atlas/snapshot?project=${encodeURIComponent(url.project)}`).then(setSnapshot).catch(error => setError(error.message));}, [url.project]);
  useEffect(load, [load]);
  useEffect(() => {url.setNode(selected?.kind === "memory" ? selected.id : "");}, [selected]);
  const locate = id => {url.setProject("all"); url.setView("atlas"); setSelected({kind: "memory", id});};
  const projects = snapshot?.projects || [];
  return <div className="app-shell">
    <Navigation view={url.view} setView={url.setView} open={navOpen} setOpen={setNavOpen}/>{navOpen && <button className="nav-scrim" onClick={() => setNavOpen(false)} aria-label="Close navigation"/>}
    <main className="workspace"><WorkspaceHeader view={url.view} project={url.project} setProject={value => {url.setProject(value); setSelected(null);}} projects={projects} coverage={snapshot?.coverage} setNavOpen={setNavOpen}/>
      {error && <div className="fatal-state"><Notice>{error}</Notice><button onClick={load}><RefreshCw size={16}/>Retry</button></div>}
      {!snapshot && !error && <Spinner/>}
      {snapshot && url.view === "atlas" && <AtlasPage snapshot={snapshot} selected={selected} setSelected={setSelected}/>} 
      {snapshot && url.view === "memories" && <MemoriesPage snapshot={snapshot} selectedId={selected?.id || ""} setSelectedId={id => setSelected(id ? {kind: "memory", id} : null)}/>} 
      {snapshot && url.view === "recall" && <RecallPage onLocate={locate}/>} 
      {snapshot && url.view === "activity" && <ActivityPage snapshot={snapshot}/>} 
      {snapshot && url.view === "system" && <SystemPage/>}
      {snapshot && url.view === "models" && <ModelsPage/>}
      {snapshot && url.view === "config" && <ConfigPage/>}
    </main>
  </div>;
}

createRoot(document.getElementById("root")).render(<App/>);
