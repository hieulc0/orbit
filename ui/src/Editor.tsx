import { useEffect, useMemo, useState } from 'react';
import { parse, stringify } from 'yaml';
import { type Api, type Json, download, errorMessage } from './api';
import { Graph } from './Graph';

export const starter = `apiVersion: orbit/v1
kind: Definition
metadata: {name: approval-demo}
inputs: {task: Review this durable request}
steps:
  review:
    uses: human.approval
    recovery_policy: restart_from_inputs
    max_attempts: 1
    timeout_seconds: 3600
    retry_backoff_seconds: 0
    approval:
      assignees: [operator]
      prompt: Approve this demonstration?
`;
const capabilities = ['repository.code', 'repository.test', 'container.run', 'agent.run', 'human.approval', 'engine.join', 'engine.timer', 'engine.wait', 'engine.child', 'engine.fan_out'];
function read(source: string): Json {
  if (source.length > 1024 * 1024) throw new Error('Definition exceeds 1 MiB');
  const value = parse(source, { maxAliasCount: 100 });
  if (!value || typeof value !== 'object' || !value.steps || typeof value.steps !== 'object' || Array.isArray(value.steps)) throw new Error('Definition must contain a steps object');
  if (Object.keys(value.steps).length > 256 || Object.values(value.steps).some(s => !s || typeof s !== 'object' || Array.isArray(s))) throw new Error('Expected at most 256 step objects');
  if (Object.values(value.steps).some((s: any) => s.needs != null && (!Array.isArray(s.needs) || s.needs.some((n: unknown) => typeof n !== 'string')))) throw new Error('Step needs must be an array of step IDs');
  JSON.stringify(value); // Reject cyclic YAML aliases before rendering or structured editing.
  return value;
}
function Field({ name, value, schema, onChange }: { name: string; value: any; schema: Json; onChange: (value: any) => void }) {
  const serialized = value === undefined ? '' : JSON.stringify(value);
  const [draft, setDraft] = useState(serialized), [error, setError] = useState('');
  useEffect(() => { setDraft(serialized); setError(''); }, [serialized]);
  const commit = () => { if (draft === serialized) return; try { onChange(draft.trim() ? JSON.parse(draft) : undefined); setError(''); } catch { setError('Enter valid JSON, or clear an optional field.'); } };
  return <label className="schema-field">{name}<small>{schema.description || (schema.type ? String(schema.type) : 'JSON value')}</small>
    <textarea aria-label={`Step ${name}`} rows={typeof value === 'object' ? 4 : 1} value={draft} onChange={e => setDraft(e.target.value)} onBlur={commit}/>{error && <span role="alert">{error}</span>}
  </label>;
}
export function Editor({ api, initial, onRun }: { api: Api; initial?: string; onRun: (id: string) => void }) {
  const [source, setSource] = useState(initial || starter), [baseline, setBaseline] = useState(initial || starter);
  const [selected, setSelected] = useState(''), [schema, setSchema] = useState<Json>({});
  const [message, setMessage] = useState(''), [error, setError] = useState(''), [busy, setBusy] = useState(false);
  const [validated, setValidated] = useState(''), [submission, setSubmission] = useState<{ source: string; request_id: string }>();
  const [newName, setNewName] = useState(''), [newCapability, setNewCapability] = useState('engine.wait');
  const [scope, setScope] = useState('');
  useEffect(() => { let live = true; api('/definitions/schema').then(s => { if (live) setSchema(s); }).catch(e => { if (live) setError(errorMessage(e)); }); return () => { live = false; }; }, [api]);
  const parsed = useMemo(() => { try { return { definition: read(source), error: '' }; } catch (e) { return { definition: undefined, error: errorMessage(e) }; } }, [source]);
  const definition = parsed.definition;
  const update = (next: Json) => { setSource(stringify(next)); setMessage('Structured edit applied to canonical YAML. Validate before submitting.'); };
  const step = definition?.steps[selected];
  const properties = schema.$defs?.Step?.properties || {};
  const fields = Object.keys(properties).filter(key => !['uses', 'needs'].includes(key) && (step?.[key] !== undefined || ['recovery_policy', 'max_attempts', 'timeout_seconds', 'retry_backoff_seconds'].includes(key)));
  const mutateStep = (key: string, value: any) => { if (!definition || !step) return; const next = structuredClone(definition); if (value === undefined) delete next.steps[selected][key]; else next.steps[selected][key] = value; update(next); };
  const validate = async () => { setBusy(true); setError(''); try { await api('/definitions/validate', { source }); setValidated(source); setMessage('Valid definition. Server binding permissions are checked on submission.'); } catch (e) { setValidated(''); setError(errorMessage(e)); } finally { setBusy(false); } };
  const submit = async () => {
    if (!definition || source !== validated || !confirm('Submit this definition as a new durable run?')) return;
    const identity = source + '\u0000' + scope;
    const receipt = submission?.source === identity ? submission : { source: identity, request_id: crypto.randomUUID() }; setSubmission(receipt);
    setBusy(true); setError('');
    try {
      const parts = scope.trim().split('/');
      if (scope.trim() && (parts.length !== 3 || parts.some(p => !/^[A-Za-z0-9_.-]{1,128}$/.test(p)))) throw new Error('Scope must be organization/project/environment');
      const result = await api('/runs', { request_id: receipt.request_id, definition, parent_run_id: null, ...(scope.trim() ? { scope: { organization_id: parts[0], project_id: parts[1], environment_id: parts[2] } } : {}) }); onRun(result.run_id);
    }
    catch (e) { setError(`${errorMessage(e)}. Retry preserves request ${receipt.request_id}.`); } finally { setBusy(false); }
  };
  const add = () => {
    if (!definition || !/^[A-Za-z0-9_-]{1,128}$/.test(newName) || Object.hasOwn(definition.steps, newName) || Object.keys(definition.steps).length >= 256) { setError('Choose a unique step ID (letters, digits, _ or -), within the 256-step limit.'); return; }
    const next = structuredClone(definition);
    const added: Json = { uses: newCapability, recovery_policy: 'restart_from_inputs', max_attempts: 1, timeout_seconds: 120, retry_backoff_seconds: 0 };
    if (newCapability === 'engine.timer') added.delay_seconds = 1;
    if (newCapability === 'human.approval') added.approval = { assignees: ['operator'], prompt: 'Review the preceding work.' };
    next.steps = { ...next.steps, [newName]: added }; update(next); setSelected(newName); setNewName('');
  };
  return <section aria-label="Definition IDE">
    <div className="section-heading"><div><p className="eyebrow">CANONICAL SOURCE</p><h2>Definition studio</h2></div><div className="actions"><button disabled={busy} onClick={validate}>Validate</button><button className="primary" disabled={busy || validated !== source} onClick={submit}>Submit run</button></div></div>
    <p className="muted">The graph and panels edit the same definition as the CLI. Structured edits normalize YAML formatting and comments; export before editing if those must be retained.</p>
    <label>Execution scope (blank uses the server default)<input aria-label="Execution scope" placeholder="organization/project/environment" value={scope} onChange={e => setScope(e.target.value)}/></label>
    {error && <p className="error" role="alert">{error}</p>}{message && <p role="status">{message}</p>}
    <div className="editor-layout"><div><label>Definition source<textarea className="source" aria-label="Definition source" spellCheck={false} value={source} onChange={e => { setSource(e.target.value); setMessage(''); }}/></label>
      <div className="actions"><label className="file-button">Import YAML<input aria-label="Import definition" type="file" accept=".yaml,.yml,.json" onChange={async e => { const file = e.target.files?.[0]; if (!file) return; if (file.size > 1024 * 1024) { setError('Definition exceeds 1 MiB'); return; } const text = await file.text(); setSource(text); setBaseline(text); setSelected(''); }}/></label><button onClick={() => download(new Blob([source], { type: 'text/yaml' }), 'definition.yaml')}>Export YAML</button><button onClick={() => { setBaseline(source); setMessage('Comparison baseline updated.'); }}>Set diff baseline</button></div>
      {source !== baseline && <details><summary>Source diff (baseline → current)</summary><div className="diff"><pre>{baseline}</pre><pre>{source}</pre></div></details>}
    </div><div>{parsed.error ? <p className="error" role="alert">{parsed.error}</p> : <><Graph steps={definition!.steps} selected={selected} onSelect={setSelected}/>
      <div className="actions"><input aria-label="New step ID" placeholder="New step ID" value={newName} onChange={e => setNewName(e.target.value)}/><select aria-label="New capability" value={newCapability} onChange={e => setNewCapability(e.target.value)}>{capabilities.map(c => <option key={c}>{c}</option>)}</select><button onClick={add}>Add step</button></div>
      {step && <section className="panel"><div className="section-heading"><h3>{selected}</h3><button className="danger" onClick={() => { if (!confirm(`Delete ${selected} and remove its dependency edges?`)) return; const next = structuredClone(definition!); delete next.steps[selected]; for (const item of Object.values(next.steps) as Json[]) { if (Array.isArray(item.needs)) item.needs = item.needs.filter((n: string) => n !== selected); } update(next); setSelected(''); }}>Delete step</button></div>
        <p>{step.uses}</p><fieldset><legend>Dependencies</legend>{Object.keys(definition!.steps).filter(n => n !== selected).map(n => <label className="check" key={n}><input type="checkbox" checked={(step.needs || []).includes(n)} onChange={e => mutateStep('needs', e.target.checked ? [...(step.needs || []), n] : (step.needs || []).filter((v: string) => v !== n))}/>{n}</label>)}</fieldset>
        {fields.map(name => <Field key={`${selected}-${name}`} name={name} value={step[name]} schema={properties[name]} onChange={value => mutateStep(name, value)}/>)}
        <label>Add optional schema field<select value="" onChange={e => { if (e.target.value) mutateStep(e.target.value, null); }}><option value="">Select field…</option>{Object.keys(properties).filter(k => !['uses', 'needs'].includes(k) && step[k] === undefined).map(k => <option key={k}>{k}</option>)}</select></label>
        <details><summary>Step JSON schema</summary><pre>{JSON.stringify(schema.$defs?.Step, null, 2)}</pre></details>
      </section>}</>}
    </div></div>
  </section>;
}
