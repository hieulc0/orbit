import type { Json } from './api';

export function Graph({ steps, selected, onSelect }: { steps: Record<string, Json>; selected?: string; onSelect: (id: string) => void }) {
  const names = Object.keys(steps).slice(0, 256);
  const levels = new Map<string, number>();
  for (let pass = 0; pass < names.length; pass++) {
    for (const name of names) {
      const needs: string[] = Array.isArray(steps[name]?.needs) ? steps[name].needs : [];
      if (!levels.has(name) && needs.every(n => levels.has(n))) levels.set(name, needs.length ? Math.max(...needs.map(n => levels.get(n)!)) + 1 : 0);
    }
  }
  const fallback = levels.size ? Math.max(...levels.values()) + 1 : 0;
  const counts = new Map<number, number>();
  const positions = new Map(names.map(name => {
    const level = levels.get(name) ?? fallback;
    const row = counts.get(level) ?? 0; counts.set(level, row + 1);
    return [name, { x: 24 + level * 235, y: 24 + row * 105 }];
  }));
  const width = Math.max(500, ...Array.from(positions.values()).map(p => p.x + 230));
  const height = Math.max(155, ...Array.from(positions.values()).map(p => p.y + 100));
  return <div className="graph-scroll" aria-label="Definition graph"><svg width={width} height={height} role="group" aria-label="Steps and dependencies">
    <defs><marker id="arrow" markerWidth="7" markerHeight="7" refX="6" refY="3" orient="auto"><path d="M0,0 L6,3 L0,6" fill="var(--muted)"/></marker></defs>
    {names.flatMap(name => (Array.isArray(steps[name]?.needs) ? steps[name].needs : []).map((dependency: string) => {
      const a = positions.get(dependency), b = positions.get(name); if (!a || !b) return null;
      return <path key={`${name}-${dependency}`} d={`M${a.x + 195},${a.y + 35} C${a.x + 218},${a.y + 35} ${b.x - 20},${b.y + 35} ${b.x},${b.y + 35}`} fill="none" stroke="var(--muted)" markerEnd="url(#arrow)"/>;
    }))}
    {names.map(name => { const p = positions.get(name)!; return <g key={name} transform={`translate(${p.x},${p.y})`} role="button" tabIndex={0} aria-label={`Select step ${name}`} onClick={() => onSelect(name)} onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect(name); } }}>
      <rect width="195" height="72" rx="9" fill="var(--panel)" stroke={selected === name ? 'var(--accent)' : 'var(--border)'} strokeWidth={selected === name ? 2 : 1}/>
      <text x="14" y="28" fill="var(--text)" fontSize="14">{name.length > 23 ? name.slice(0, 21) + '…' : name}</text>
      <text x="14" y="51" fill="var(--muted)" fontSize="12">{String(steps[name]?.uses || 'unconfigured')}</text>
    </g>; })}
  </svg></div>;
}
