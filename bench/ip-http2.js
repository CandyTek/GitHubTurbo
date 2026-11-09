// Direct, verified HTTP/2 download probe. Input includes a temporary URL on stdin;
// output deliberately excludes paths, query strings and credentials.
const http2 = require('node:http2');
const tls = require('node:tls');
const crypto = require('node:crypto');
let input='';
process.stdin.setEncoding('utf8');
process.stdin.on('data',chunk=>input+=chunk);
process.stdin.on('end',()=>{
  const options=JSON.parse(input), url=new URL(options.url);
  if(url.protocol!=='https:' || !url.hostname.endsWith('.githubusercontent.com')) throw Error('Unsupported probe host');
  const start=process.hrtime.bigint(), elapsed=()=>Number(process.hrtime.bigint()-start)/1e9;
  let socket,session,request,done=false,bytes=0,first=null;
  const hash=crypto.createHash('sha256'), result={code:null,timing:{},headers:{}};
  const timer=setTimeout(()=>finish('download_timeout'),options.timeout*1000);
  function finish(error){
    if(done)return;done=true;clearTimeout(timer);
    const sha=hash.digest('hex'), seconds=elapsed();
    Object.assign(result,{seconds,bytes,sha256:sha,bps:bytes/Math.max(seconds,.000001),
      transfer_seconds:first===null?null:seconds-first,
      body_bps:first===null?null:bytes/Math.max(seconds-first,.000001),
      timeout:error==='download_timeout',error:error||'',errors:[],success:false});
    if(error)result.errors.push(error);
    if(result.code!==options.status)result.errors.push('http_status');
    if(bytes!==options.bytes)result.errors.push('wrong_length');
    if(sha!==options.sha256)result.errors.push('sha256_mismatch');
    if(options.range && result.headers['content-range']!==`bytes ${options.range}/${options.total}`)result.errors.push('range_mismatch');
    result.success=result.errors.length===0;
    if(request)request.close(http2.constants.NGHTTP2_CANCEL);
    if(session)session.destroy();
    if(socket)socket.destroy();
    process.stdout.write(JSON.stringify(result)+'\n');
  }
  socket=tls.connect({host:options.ip||url.hostname,port:443,servername:url.hostname,
    ALPNProtocols:['h2'],rejectUnauthorized:true});
  socket.on('lookup',()=>result.timing.time_namelookup=elapsed());
  socket.on('connect',()=>result.timing.time_connect=elapsed());
  socket.on('secureConnect',()=>{
    result.timing.time_appconnect=elapsed();
    result.remote_ip=socket.remoteAddress;result.protocol=socket.alpnProtocol;
    result.tls_verified=socket.authorized;
    if(socket.alpnProtocol!=='h2'){finish('alpn_http2_not_negotiated');return;}
    session=http2.connect(url.origin,{createConnection:()=>socket,
      settings:{initialWindowSize:1024*1024,enablePush:false}});
    session.on('error',error=>finish(error.code||'http2_session_error'));
    session.on('connect',()=>{
      session.setLocalWindowSize(4*1024*1024);
      const headers={':method':'GET',':path':url.pathname+url.search,'accept-encoding':'identity',
        'user-agent':'GitHubTurbo-IP-Probe/1'};
      if(options.range)headers.range='bytes='+options.range;
      request=session.request(headers);
      request.on('response',headers=>{
        result.code=headers[':status'];result.timing.time_starttransfer=elapsed();
        for(const key of ['content-range','content-length','etag','content-encoding'])if(headers[key])result.headers[key]=headers[key];
        if(result.code!==options.status)finish('unexpected_status');
      });
      request.on('data',chunk=>{
        if(done)return;
        if(first===null)first=elapsed();
        bytes+=chunk.length;hash.update(chunk);
        if(bytes>options.bytes)finish('response_too_large');
      });
      request.on('end',()=>finish());
      request.on('error',error=>finish(error.code||'http2_request_error'));
      request.end();
    });
  });
  socket.on('error',error=>finish(error.code||'tls_error'));
});
