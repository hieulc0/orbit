import fs from 'node:fs';
import path from 'node:path';
const excluded=new Set(['target','.git','.agents','.codex','node_modules','dist','test-results','playwright-report','__pycache__']);
function walk(dir) { return fs.readdirSync(dir,{withFileTypes:true}).flatMap(e => {
  if(excluded.has(e.name)) return [];
  const file=path.join(dir,e.name);
  return e.isDirectory()?walk(file):e.isFile()&&file.endsWith('.md')?[file]:[];
}); }
let count=0; const errors=[];
for(const file of walk('.')) for(const match of fs.readFileSync(file,'utf8').matchAll(/\[[^\]]*\]\(([^\s)]+)\)/g)) {
  const link=match[1]; if(/^(?:[a-z]+:|#|\/)/i.test(link)) continue;
  const target=path.resolve(path.dirname(file),decodeURIComponent(link.split('#')[0]));
  count++; if(!fs.existsSync(target)) errors.push(`${file}: missing ${link}`);
}
for(const error of errors) console.error(error);
console.log(`${count} local documentation links checked; ${errors.length} missing targets.`);
if(errors.length) process.exitCode=1;
