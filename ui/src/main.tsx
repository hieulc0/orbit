import { StrictMode, useCallback, useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { stringify } from 'yaml';
import { apiFor, downloadArtifact, errorMessage, segment, terminal, type Api, type Json } from './api';
import { Editor } from './Editor';
import { Graph } from './Graph';
import './style.css';

function Badge({ state }: { state: string }) { return <span className={`badge ${state?.toLowerCase()}`}>{state?.replaceAll('_', ' ')}</span>; }
function Data({ value }: { value: unknown }) { return <pre>{JSON.stringify(value, null, 2)}</pre>; }
function RunDetail({ api, token, id, onEdit }: { api: Api; token: string; id: string; onEdit: (source: string) => void }) {
  const [run, setRun] = useState<Json>(), [events, setEvents] = useState<Json[]>([]), [error, setError] = useState('');
  const [selected, setSelected] = useState(''), [step, setStep] = useState(''), [payload, setPayload] = useState('null');
  const [comment, setComment] = useState(''), [approved, setApproved] = useState(true), [busy, setBusy] = useState(false);
  const [receipt, setReceipt] = useState<{ key: string; id: string }>();
  const load = useCallback(() => api(`/runs/${segment(id)}`).then(setRun), [api, id]);
  useEffect(() => {
    let live = true, cursor = 0; const rows = new Map<number, Json>(); const controller = new AbortController(); let timer: ReturnType<typeof setTimeout>;
    setRun(undefined); setEvents([]); setError(''); setSelected('');
    const poll = async () => {
      try {
        const [snapshot, page] = await Promise.all([api(`/runs/${segment(id)}`, undefined, controller.signal), api(`/runs/${segment(id)}/events?after=${cursor}`, undefined, controller.signal)]);
        if (!live) return; setRun(snapshot);
        for (const entry of page) { if (entry.sequence > cursor) { rows.set(entry.sequence, entry); cursor = entry.sequence; } }
        while (rows.size > 4096) rows.delete(rows.keys().next().value!);
        setEvents([...rows.values()]); setError('');
        timer = setTimeout(poll, page.length === 256 ? 50 : 1500);
      } catch (e) { if (live) { setError(errorMessage(e)); timer = setTimeout(poll, 2000); } }
    };
    void poll(); return () => { live = false; controller.abort(); clearTimeout(timer); };
  }, [api, id]);
  const act = async (action: () => Promise<unknown>) => { setBusy(true); setError(''); try { await action(); await load(); } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); } };
  const interact = () => act(async () => {
    const task = run?.tasks.find((t: Json) => t.step === step); if (!task) throw new Error('Select a waiting step');
    const human = run!.plan.definition.steps[step].uses === 'human.approval';
    const data = human ? { step, approved, comment } : { step, payload: JSON.parse(payload) };
    if (!confirm(human ? 'Record this human approval decision?' : 'Deliver this one-shot signal?')) return;
    const key = JSON.stringify(data), request_id = receipt?.key === key ? receipt.id : crypto.randomUUID(); setReceipt({ key, id: request_id });
    await api(`/runs/${segment(id)}/${human ? 'approvals' : 'signals'}`, { request_id, ...data });
  });
  if (!run) return <p role="status">{error || 'Loading run…'}</p>;
  const waits = run.tasks.filter((t: Json) => ['PENDING', 'WAITING'].includes(t.state) && !t.signal && ['engine.wait', 'human.approval'].includes(run.plan.definition.steps[t.step].uses));
  const human = run.plan.definition.steps[step]?.uses === 'human.approval';
  return <section>
    <div className="section-heading"><div><p className="eyebrow">RUN / {id}</p><h2>{run.plan.definition.metadata.name} <Badge state={run.state}/></h2></div><div className="actions"><button onClick={() => onEdit(stringify(run.plan.definition))}>Open definition</button><button className="danger" disabled={busy || terminal(run.state)} onClick={() => { if (confirm('Cancel this run and its durable children?')) void act(() => api(`/runs/${segment(id)}/cancel`, {})); }}>Cancel run</button></div></div>
    {error && <p className="error" role="alert">{error}</p>}<p>{run.plan.definition.inputs.task}</p>
    <p className="muted">Plan <code>{run.plan.digest}</code>{run.parent_run_id && <> · Parent <code>{run.parent_run_id}</code></>}</p>
    <Graph steps={run.plan.definition.steps} selected={selected} onSelect={setSelected}/>
    <h3>Tasks and attempts</h3><div className="tasks">{run.tasks.filter((t: Json) => !selected || t.step === selected).map((task: Json) => <article className="panel" key={task.id}><h4>{task.step} <Badge state={task.state}/></h4>{task.reason && <p className={task.state === 'FAILED' ? 'error' : 'muted'}>{task.reason}</p>}<p className="muted">{task.id}</p>
      {task.deadline_at && <p>Deadline {new Date(task.deadline_at).toLocaleString()}</p>}
      {task.attempts.map((attempt: Json) => <details key={attempt.id}><summary>Attempt {attempt.generation} · {attempt.worker_id} · {attempt.state}</summary><Data value={attempt}/></details>)}
      {task.signal && <details><summary>Decision / signal receipt</summary><Data value={task.signal}/></details>}{task.agent_usage && <details><summary>Agent budget ledger</summary><Data value={task.agent_usage}/></details>}
      {task.child_run_ids?.length > 0 && <details><summary>Child runs ({task.child_run_ids.length})</summary><Data value={task.child_run_ids}/></details>}
    </article>)}</div>{selected && <button onClick={() => setSelected('')}>Show all tasks</button>}
    {waits.length > 0 && !terminal(run.state) && <section className="panel interaction"><h3>Human interaction</h3><label>Waiting step<select value={step} onChange={e => { setStep(e.target.value); setReceipt(undefined); }}><option value="">Select step…</option>{waits.map((t: Json) => <option key={t.step}>{t.step}</option>)}</select></label>
      {human ? <><p>{run.plan.definition.steps[step].approval.prompt}</p><p>Assigned to: {run.plan.definition.steps[step].approval.assignees.join(', ')}</p><label>Decision<select value={String(approved)} onChange={e => setApproved(e.target.value === 'true')}><option value="true">Approve</option><option value="false">Deny</option></select></label><label>Comment<textarea value={comment} maxLength={4096} onChange={e => setComment(e.target.value)}/></label></> : <label>Signal payload (JSON)<textarea value={payload} onChange={e => setPayload(e.target.value)}/></label>}
      <button disabled={busy || !step} onClick={interact}>{human ? 'Record decision' : 'Send signal'}</button>{receipt && <small>Retry request: {receipt.id}</small>}
    </section>}
    <h3>Artifacts</h3>{run.artifacts.length === 0 ? <p className="muted">No artifacts recorded.</p> : <div className="table-scroll"><table><thead><tr><th>Kind</th><th>Size</th><th>Checksum</th><th>Publication</th><th/></tr></thead><tbody>{run.artifacts.map((a: Json) => <tr key={a.id}><td>{a.kind}</td><td>{a.size} B</td><td><code>{a.checksum}</code></td><td>{a.finalized ? 'Finalized' : 'Prepared'}</td><td><button disabled={!a.finalized || busy} onClick={() => act(() => downloadArtifact(token, id, a))}>Download {a.kind}</button></td></tr>)}</tbody></table></div>}
    <h3>Durable timeline <small>{events.length} retained events · replay cursor {events.at(-1)?.sequence || 0}</small></h3><p className="muted">Continuously replayed from the journal; this view retains the latest 4,096 entries.</p>
    <ol className="timeline">{events.slice().reverse().map(entry => <li key={entry.sequence}><span className="sequence">{entry.sequence}</span><details><summary>{entry.event.type} <small>{entry.event.step || entry.event.attempt_id} · {entry.at}</small></summary><Data value={entry.event}/></details></li>)}</ol>
  </section>;
}
function App() {
  const [token, setToken] = useState(''), [draft, setDraft] = useState(''), [page, setPage] = useState('runs');
  const [runs, setRuns] = useState<Json[]>([]), [workers, setWorkers] = useState<Json[]>([]), [queues, setQueues] = useState<Json[]>([]);
  const [runId, setRunId] = useState(''), [source, setSource] = useState<string>(), [error, setError] = useState(''), [filter, setFilter] = useState('');
  const api = useMemo(() => apiFor(token), [token]);
  useEffect(() => {
    if (!token) return; let live = true; let timer: ReturnType<typeof setTimeout>; const controller = new AbortController();
    const refresh = async () => { try { const [r, w, q] = await Promise.all([api('/runs', undefined, controller.signal), api('/workers', undefined, controller.signal).catch(() => []), api('/queues', undefined, controller.signal).catch(() => [])]); if (live) { setRuns(r); setWorkers(w); setQueues(q); setError(''); } } catch (e) { if (live) setError(errorMessage(e)); } finally { if (live) timer = setTimeout(refresh, 2500); } };
    void refresh(); return () => { live = false; controller.abort(); clearTimeout(timer); };
  }, [api, token]);
  const selectRun = (id: string) => { setRunId(id); setPage('detail'); };
  if (!token) return <main className="login"><div className="brand">◉ <span>ORBIT</span></div><h1>Execution, in view.</h1><p className="muted">Connect to this server’s operations console. Your bearer token stays in memory and is cleared on reload.</p><form onSubmit={async e => { e.preventDefault(); try { await apiFor(draft)('/runs'); setToken(draft); setDraft(''); setError(''); } catch (e) { setError(errorMessage(e)); } }}><label>Operator token<input autoComplete="off" type="password" value={draft} onChange={e => setDraft(e.target.value)} required/></label><button className="primary">Connect</button></form>{error && <p role="alert" className="error">{error}</p>}</main>;
  return <><aside><div className="brand">◉ <span>ORBIT</span></div><p className="eyebrow">CONTROL PLANE</p><nav>{[['runs', 'Runs'], ['workers', 'Workers'], ['queues', 'Queues'], ['editor', 'Definition studio']].map(([key, title]) => <button key={key} aria-current={page === key ? 'page' : undefined} onClick={() => setPage(key)}>{title}</button>)}</nav><div className="sidebar-foot"><span className="live-dot"/> Connected · same-origin API<button onClick={() => { setToken(''); setRuns([]); setWorkers([]); setQueues([]); setRunId(''); setSource(undefined); }}>Disconnect</button></div></aside>
    <main><header><span>WORKSPACE / OPERATIONS</span><span className="muted">Durable execution console</span></header>{error && <p className="error" role="alert">{error}</p>}
      {page === 'runs' && <section><div className="section-heading"><div><p className="eyebrow">EXECUTION OVERVIEW</p><h1>Runs</h1></div><button className="primary" onClick={() => { setSource(undefined); setPage('editor'); }}>New definition</button></div><div className="metrics"><div><strong>{runs.length}</strong><span>Recent runs</span></div><div><strong>{runs.filter(r => !terminal(r.state)).length}</strong><span>In progress</span></div><div><strong>{runs.filter(r => r.state === 'FAILED').length}</strong><span>Failed</span></div><div><strong>{workers.length}</strong><span>Registered workers</span></div></div>
        <label className="search">Filter runs<input placeholder="Run ID or state" value={filter} onChange={e => setFilter(e.target.value)}/></label><div className="table-scroll"><table><thead><tr><th>Run</th><th>Status</th><th>Accepted</th></tr></thead><tbody>{runs.filter(r => `${r.id} ${r.state}`.toLowerCase().includes(filter.toLowerCase())).map(r => <tr key={r.id}><td><button className="link" onClick={() => selectRun(r.id)}>{r.id}</button></td><td><Badge state={r.state}/></td><td>{r.created_at}</td></tr>)}</tbody></table></div>{runs.length === 0 && <p className="empty">No runs yet. Submit a definition from the studio or CLI.</p>}<p className="muted">Most recent 100 runs. Refreshes automatically.</p></section>}
      {page === 'workers' && <section><h1>Workers</h1><div className="tasks">{workers.map(w => <article className="panel" key={w.id}><h3>{w.id}</h3><p><Badge state={w.idle_seconds < 30 ? 'OBSERVED' : 'STALE'}/></p><p>Last observed {w.last_seen}</p><p className="muted">Recent contact is not proof of process health.</p><Data value={w.profile}/>{w.active_attempts && <details><summary>Active leases ({w.active_attempts.length})</summary><Data value={w.active_attempts}/></details>}</article>)}</div>{workers.length === 0 && <p>No workers available, or this identity lacks global worker access.</p>}</section>}
      {page === 'queues' && <section><h1>Queues</h1><table><thead><tr><th>Capability</th><th>Pool</th><th>Ready</th><th>Active</th></tr></thead><tbody>{queues.map((q, i) => <tr key={i}><td>{q.capability}</td><td>{q.pool || 'Any pool'}</td><td>{q.ready}</td><td>{q.active}</td></tr>)}</tbody></table>{queues.length === 0 && <p>No queues available, or this identity lacks global queue access.</p>}</section>}
      {page === 'detail' && <RunDetail key={runId} api={api} token={token} id={runId} onEdit={text => { setSource(text); setPage('editor'); }}/>} {page === 'editor' && <Editor key={source || 'new'} api={api} initial={source} onRun={selectRun}/>}
    </main></>;
}
createRoot(document.getElementById('root')!).render(<StrictMode><App/></StrictMode>);
