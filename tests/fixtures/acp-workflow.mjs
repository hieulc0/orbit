// Disposable ACP and Responses peers. Never use credentials or a live repository.
import readline from 'node:readline';
import fs from 'node:fs';
import http from 'node:http';
const fixed = '#!/bin/sh\nprintf \'%s\\n\' "$(( $1 + $2 ))"\n';
const actions = [
  ['read_file', {path:'calc.sh'}],
  ['shell', {command:'sh test.sh'}],
  ['write_file', {path:'calc.sh',content:fixed}],
  ['shell', {command:'sh test.sh && test ! -e /orbit/home/.codex/auth.json && test -z "$ORBIT_TOKEN"'}],
];
if(process.argv[2] === 'responses') {
  let calls=0;
  const server=http.createServer(async(req,res)=>{
    const chunks=[]; for await (const chunk of req) chunks.push(chunk);
    if(req.method!=='POST' || !req.url.endsWith('/responses')) {res.writeHead(404);res.end();return;}
    const body=JSON.parse(Buffer.concat(chunks));
    const names=body.tools.filter(t=>t.type==='function').map(t=>t.name);
    if(!['orbit_read_file','orbit_write_file','orbit_shell'].every(n=>names.includes(n)) || names.some(n=>!n.startsWith('orbit_') && n!=='update_plan')) {
      fs.writeFileSync(process.argv[3]+'.error',JSON.stringify(names));res.writeHead(400);res.end();return;
    }
    fs.appendFileSync(process.argv[3]+'.requests',JSON.stringify({model:body.model,tool_names:names})+'\n');
    const index=calls++;
    const output=index<actions.length ? {type:'function_call',id:`fc_${index}`,call_id:`call_${index}`,name:`orbit_${actions[index][0]}`,arguments:JSON.stringify(actions[index][1]),status:'completed'}
      : {type:'message',id:'msg_final',role:'assistant',content:[{type:'output_text',text:'Fixed addition and ran the preserved tests.',annotations:[]}],status:'completed'};
    res.writeHead(200,{'content-type':'text/event-stream'});
    const emit=(type,extra)=>res.write(`event: ${type}\ndata: ${JSON.stringify({type,...extra})}\n\n`);
    const response={id:`resp_${index}`,object:'response',created_at:1,model:body.model,status:'in_progress',output:[]};
    emit('response.created',{response});
    emit('response.output_item.added',{output_index:0,item:{...output,status:'in_progress'}});
    emit('response.output_item.done',{output_index:0,item:output});
    emit('response.completed',{response:{...response,status:'completed',output:[output],usage:{input_tokens:10,output_tokens:10,total_tokens:20}}});
    res.end();
  });
  server.listen(0,'127.0.0.1',()=>fs.writeFileSync(process.argv[3],`http://127.0.0.1:${server.address().port}/v1`));
} else {
  const lines=readline.createInterface({input:process.stdin});
  const iterator=lines[Symbol.asyncIterator]();
  const read=async()=>JSON.parse((await iterator.next()).value);
  const send=v=>process.stdout.write(JSON.stringify({jsonrpc:'2.0',...v})+'\n');
  const response=(id,result)=>send({id,result});
  let next=0;
  const call=async(method,params)=>{const id=`fixture-${++next}`;send({id,method,params});const reply=await read();if(reply.id!==id || reply.error)throw Error('broker denied');return reply.result;};
  const mode=process.argv[2] || 'normal';
  let cwd;
  for await(const line of iterator) {
    const request=JSON.parse(line);
    if(request.method==='initialize') response(request.id,{protocolVersion:1,agentInfo:{name:'orbit-acp-fixture',version:'1'},agentCapabilities:{},authMethods:[]});
    else if(request.method==='session/new') {cwd=request.params.cwd;response(request.id,{sessionId:'fixture-session',models:{currentModelId:'fixture-model-v1'}});}
    else if(request.method==='session/prompt') {
      if(mode==='terminal-hang') {
        const sessionId='fixture-session';
        const {terminalId}=await call('terminal/create',{sessionId,command:'sh',args:['-c','sleep 60'],cwd});
        await call('terminal/wait_for_exit',{sessionId,terminalId});
        await new Promise(()=>{});
      }
      if(mode==='flood') { for(let i=0;i<2000;i++) send({method:'session/update',params:{sessionId:'fixture-session',update:{sessionUpdate:'agent_message_chunk',content:{type:'text',text:'x'.repeat(2048)}}}});await new Promise(()=>{}); }
      if(mode==='hang') {await new Promise(()=>{});}
      if(mode==='escape') {await call('fs/write_text_file',{sessionId:'fixture-session',path:'/etc/orbit-escape',content:'denied'});}
      if(mode==='native') {await call('session/request_permission',{sessionId:'fixture-session',toolCall:{toolCallId:'native',title:'native',status:'pending'},options:[]});}
      const sessionId='fixture-session';
      let original;
      for(const [name,args] of actions) {
        send({method:'session/update',params:{sessionId,update:{sessionUpdate:'tool_call',toolCallId:`reported-${next}`,title:name,kind:'other',status:'in_progress'}}});
        if(name==='read_file') original=(await call('fs/read_text_file',{sessionId,path:`${cwd}/${args.path}`})).content;
        else if(name==='write_file') {if(!original.includes('$1 - $2'))throw Error('wrong fixture');await call('fs/write_text_file',{sessionId,path:`${cwd}/${args.path}`,content:args.content});}
        else {
          const {terminalId}=await call('terminal/create',{sessionId,command:'sh',args:['-c',args.command],cwd,outputByteLimit:4096});
          const {exitStatus}=await call('terminal/wait_for_exit',{sessionId,terminalId});
          if(exitStatus.exitCode!==(args.command==='sh test.sh'?1:0))throw Error('incorrect tool result');
          await call('terminal/output',{sessionId,terminalId});
          await call('terminal/release',{sessionId,terminalId});
        }
      }
      response(request.id,{stopReason:'end_turn'});
    } else response(request.id,{});
  }
}
