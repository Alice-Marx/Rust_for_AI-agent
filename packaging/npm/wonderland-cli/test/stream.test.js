const {test}=require('node:test');
const assert=require('node:assert/strict');
const {consumeStream}=require('../bin/stream');
test('Chinese split across bytes arrives incrementally with a final response',async()=>{
  const text='data: {"frame":"event","event":{"type":"text_delta","text":"中文🦀"}}\r\n\r\ndata: {"frame":"response","response":{"output":"中文🦀"}}\n\n';
  const bytes=Buffer.from(text); const seen=[];
  const response=new Response(new ReadableStream({start(c){ for(const b of bytes)c.enqueue(Uint8Array.of(b)); c.close(); }}));
  const result=await consumeStream(response,e=>seen.push(e.text));
  assert.deepEqual(seen,['中文🦀']); assert.equal(result.output,'中文🦀');
});
test('truncation and error frames fail instead of pretending success',async()=>{
  await assert.rejects(consumeStream(new Response('data: {"frame":"event","event":{"type":"text_delta","text":"partial"}}\n\n'),()=>{}),/连接中断/);
  await assert.rejects(consumeStream(new Response('data: {"frame":"error","message":"rate limit"}\n\n'),()=>{}),/rate limit/);
});
