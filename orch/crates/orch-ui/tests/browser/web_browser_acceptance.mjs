import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync, existsSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import { createHash } from "node:crypto";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = resolve(scriptDir, "../../../../..");
const orchWeb = resolve(process.argv[2] ?? "");
assert(existsSync(orchWeb), `orch-web not found: ${orchWeb}`);
const chrome = process.env.ORCH_CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
assert(existsSync(chrome), `Chrome not found: ${chrome}`);

const scratch = mkdtempSync(resolve(tmpdir(), "orch-web-browser-"));
const screenshots = resolve(scratch, "screenshots");
mkdirSync(screenshots);
function checked(command, args, options = {}) { const out = spawnSync(command, args, options); assert.equal(out.status, 0, `${command} failed: ${out.stderr}`); return out; }
function treeDigest() { const out = checked("git", ["-c", "core.fsmonitor=false", "-C", root, "ls-files", "--cached", "--others", "--exclude-standard", "-z"]); const paths = out.stdout.toString().split("\0").filter(Boolean).sort(); const hash = createHash("sha256"); for (const path of paths) { hash.update(path); hash.update("\0"); hash.update(readFileSync(resolve(root, path))); } return hash.digest("hex"); }
const before = treeDigest();
const providerCount = () => checked("ps", ["-axo", "command"], { encoding: "utf8" }).stdout.split("\n").filter(line => /DewuSmartClaw|wake-(?:dsh|pi|zcode)|opencode run/.test(line)).length;
const providersBefore = providerCount();

let server, fusionServer, browser, socket, success = false;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function firstLine(stream, timeoutMs) {
  const lines = createInterface({ input: stream });
  return await Promise.race([
    new Promise(resolve => lines.once("line", line => { lines.close(); resolve(line); })),
    sleep(timeoutMs).then(() => { throw new Error("orch-web readiness timeout"); }),
  ]);
}

class Cdp {
  constructor(url) {
    this.id = 0; this.pending = new Map(); this.events = new Map(); this.listeners = new Map(); this.socket = new WebSocket(url); socket = this.socket;
  }
  async open() {
    await new Promise((resolve, reject) => { this.socket.onopen = resolve; this.socket.onerror = reject; });
    this.socket.onmessage = event => {
      const message = JSON.parse(event.data);
      if (message.id) {
        const waiter = this.pending.get(message.id); this.pending.delete(message.id);
        if (message.error) waiter.reject(new Error(message.error.message)); else waiter.resolve(message.result);
      } else {
        for (const listener of this.listeners.get(message.method) ?? []) listener(message.params);
        for (const waiter of this.events.get(message.method) ?? []) waiter(message.params);
        this.events.delete(message.method);
      }
    };
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.socket.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  once(method, timeoutMs = 10000) {
    return Promise.race([
      new Promise(resolve => this.events.set(method, [...(this.events.get(method) ?? []), resolve])),
      sleep(timeoutMs).then(() => { throw new Error(`${method} timeout`); }),
    ]);
  }
  on(method, listener) { this.listeners.set(method, [...(this.listeners.get(method) ?? []), listener]); }
  async eval(expression) {
    const result = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, userGesture: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
    return result.result.value;
  }
}

try {
  server = spawn(orchWeb, ["--no-open", "--root", root], { stdio: ["ignore", "pipe", "pipe"] });
  const line = await firstLine(server.stdout, 15000);
  const url = line.replace(/^OpenOrch Web:\s*/, "");
  assert.match(url, /^http:\/\/127\.0\.0\.1:\d+\/$/);

  const profile = resolve(scratch, "chrome");
  browser = spawn(chrome, ["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--disable-dev-shm-usage", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: "ignore" });
  const activePort = resolve(profile, "DevToolsActivePort");
  for (let i = 0; i < 200 && !existsSync(activePort); i++) await sleep(50);
  assert(existsSync(activePort), "DevToolsActivePort missing");
  const port = readFileSync(activePort, "utf8").split("\n")[0];
  const target = await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" }).then(response => response.json());
  const cdp = new Cdp(target.webSocketDebuggerUrl); await cdp.open();
  await Promise.all([cdp.send("Page.enable"), cdp.send("Runtime.enable"), cdp.send("Network.enable"), cdp.send("Fetch.enable", { patterns:[{urlPattern:"*api/v1/projects*",requestStage:"Request"}] })]);
  const fixtureRows = [
    {id:"selfhost:r90:01M2JKYB6NPJZWC7Q15EEKZBYJ",source:"selfhost",summary:"Safe browser fixture task",phase:"recorded",result:"verified",alias:"smartclaw",driver:"smartclaw",purpose:"review",source_time:"2026-09-15T12:00:00Z",started_at:null,native_status:{managedScopeTerminated:true},task:{round:"r90",id:"B900",attempt:"B900-A0001",head:"abc",state:"recorded"},fusion_id:null},
    {id:"fixture-task-b",source:"selfhost",summary:"",phase:"ended",result:"verified",alias:"local",driver:"local",purpose:"implement",source_time:"2026-09-15T11:59:00Z",started_at:null,native_status:{},task:{round:"r90",id:"B900",attempt:"B900-A0002",head:"def",state:"invoked"},fusion_id:null},
    {id:"fixture-fusion-a",source:"consult",summary:"Design review",phase:"ended",result:"verified",alias:"one",driver:"claude",purpose:"consult",source_time:"2026-09-15T11:58:00Z",started_at:null,native_status:{terminalSeen:true},task:null,fusion_id:"fixture-fusion"},
    {id:"fixture-fusion-b",source:"consult",summary:"",phase:"ended",result:"invalid",alias:"two",driver:"opencode",purpose:"consult",source_time:"2026-09-15T11:57:00Z",started_at:null,native_status:{},parameters:{channelDiagnostic:{code:"capture_evidence_missing"}},task:null,fusion_id:"fixture-fusion"},
    {id:"fixture-failed",source:"standalone",summary:"Needs attention",phase:"ended",result:"failed",alias:null,driver:null,purpose:"review",source_time:"2026-09-15T11:56:00Z",started_at:null,native_status:{},task:null,fusion_id:null},
    {id:"fixture-unknown",source:"standalone",summary:"Unknown time remains visible",phase:"published",result:"unknown",alias:"three",driver:"dsh",purpose:"consult",source_time:null,started_at:null,native_status:{},task:null,fusion_id:null},
  ];
  const fixtureGroups = [{id:"fixture-fusion",members:["fixture-fusion-a","fixture-fusion-b"],total:2,roster_complete:true,phase_counts:{ended:2},result_counts:{verified:1,invalid:1}}];
  const envelope = (generation, data, projectId="browser-fixture-project") => ({serverInstanceId:"browser-fixture-instance",projectId,snapshotGeneration:generation,data});
  let snapshotRequests = 0, detailRequests = 0, detailMode = "success", snapshotResult = "verified", cancellationPassed = false, memberReplacementPassed = false, projectCancellationPassed = false, invalidationPassed = false;
  const readerErrorStates = {};
  cdp.on("Fetch.requestPaused", async params => {
    let requestMode = null;
    try {
      const parsed = new URL(params.request.url), path = parsed.pathname;
      let body, rawBody, responseCode = 200;
      if (path === "/api/v1/projects") body = envelope(0, [{id:"browser-fixture-project",name:"Browser fixture",root:"[fixture]"},{id:"browser-fixture-project-2",name:"Second fixture",root:"[fixture 2]"}]);
      else if (path.endsWith("/snapshot")) { snapshotRequests += 1;const projectId=decodeURIComponent(path.split('/').at(-2));const second=projectId.endsWith('-2');const rows=second?[]:fixtureRows.map((row,index)=>index===0?{...row,result:snapshotResult}:row);body = envelope(snapshotRequests, {root:second?"[browser fixture 2]":"[browser fixture]",read_at:"2026-09-15T12:00:01Z",rows,groups:second?[]:fixtureGroups,diagnostics:[],truncated:false},projectId); }
      else if (path.endsWith("/detail")) {
        detailRequests += 1; requestMode = detailMode; const mode = detailMode; detailMode = "success";
        const projectId=decodeURIComponent(path.split('/').at(-2));
        if (detailRequests === 1) await sleep(2300);
        if (mode === "cancel_delay") await sleep(1000);
        if (mode === "client_timeout") await sleep(12500);
        const errors={not_found:404,read_failed:503,busy:503,request_timeout:408,forbidden:403};
        if (errors[mode]) { responseCode=errors[mode]; body=envelope(snapshotRequests + 1, null, projectId);delete body.data;body.error={code:mode}; }
        else if (mode === "invalid_response") rawBody="null";
        else if (mode === "identity_mismatch") body={...envelope(snapshotRequests + 1,{row:fixtureRows[0],text:"wrong",locator:"fixture/detail",truncated:false},projectId),serverInstanceId:"old-server"};
        else { const id=parsed.searchParams.get("id"),row=fixtureRows.find(item=>item.id===id)??fixtureRows[0];body=envelope(snapshotRequests + 1,{row,text:`# Verified fixture answer\n\n${id}\n\n- safe\n- local\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n\`code\``,locator:"fixture/detail",truncated:false},projectId); }
      }
      else return await cdp.send("Fetch.continueRequest", {requestId:params.requestId});
      await cdp.send("Fetch.fulfillRequest", {requestId:params.requestId,responseCode,responseHeaders:[{name:"Content-Type",value:"application/json"},{name:"Cache-Control",value:"no-store"}],body:Buffer.from(rawBody??JSON.stringify(body)).toString("base64")});
    } catch (error) { if (requestMode === "client_timeout" || requestMode === "cancel_delay") return; await cdp.send("Fetch.failRequest", {requestId:params.requestId,errorReason:"Failed"}); throw error; }
  });
  const requests = [];
  cdp.on("Network.requestWillBeSent", params => requests.push(params.request.url));
  const loaded = cdp.once("Page.loadEventFired"); await cdp.send("Page.navigate", { url }); await loaded;
  for (let i = 0; i < 100; i++) {
    if (await cdp.eval("document.querySelectorAll('.card').length > 0")) break;
    await sleep(100);
  }
  assert(await cdp.eval("document.querySelectorAll('.card').length > 0"), "no observation cards rendered");

  const matrix = [
    { theme:"light", lang:"en", width:1440, height:900, columns:3, name:"light-en-wide" },
    { theme:"dark", lang:"zh", width:960, height:900, columns:2, name:"dark-zh-medium" },
    { theme:"system", lang:"zh", width:390, height:844, columns:1, name:"system-zh-narrow" },
  ];
  const measurements = [];
  for (const item of matrix) {
    await cdp.send("Emulation.setDeviceMetricsOverride", { width:item.width, height:item.height, deviceScaleFactor:1, mobile:item.width < 500 });
    await cdp.eval(`(()=>{const t=document.getElementById('theme-select');t.value=${JSON.stringify(item.theme)};t.dispatchEvent(new Event('change',{bubbles:true}));const want=${JSON.stringify(item.lang)};if(!document.documentElement.lang.startsWith(want))document.getElementById('lang-toggle').click();})()`);
    await sleep(80);
    const measured = await cdp.eval(`(()=>{const cards=[...document.querySelectorAll('.card')];const columns=Math.max(0,...[...document.querySelectorAll('.card-grid')].map(grid=>{const own=[...grid.querySelectorAll(':scope > .card')];if(!own.length)return 0;const top=Math.round(own[0].getBoundingClientRect().top);return own.filter(x=>Math.round(x.getBoundingClientRect().top)===top).length}));const rgb=s=>{const m=s.match(/\\d+(?:\\.\\d+)?/g).map(Number);return m.slice(0,3)};const lum=c=>{const v=c/255;return v<=.03928?v/12.92:((v+.055)/1.055)**2.4};const ratio=(a,b)=>{const x=rgb(a).map(lum),y=rgb(b).map(lum);const l1=.2126*x[0]+.7152*x[1]+.0722*x[2],l2=.2126*y[0]+.7152*y[1]+.0722*y[2];return (Math.max(l1,l2)+.05)/(Math.min(l1,l2)+.05)};const card=cards[0];return {lang:document.documentElement.lang,theme:document.documentElement.dataset.theme,columns,overflow:document.documentElement.scrollWidth>document.documentElement.clientWidth,minTarget:Math.min(...[...document.querySelectorAll('button,input,select')].filter(x=>x.offsetParent).map(x=>Math.min(x.getBoundingClientRect().width,x.getBoundingClientRect().height))),contrast:card?ratio(getComputedStyle(card).color,getComputedStyle(card).backgroundColor):7};})()`);
    assert(measured.lang.startsWith(item.lang)); assert(["light","dark"].includes(measured.theme)); assert(!measured.overflow); assert.equal(measured.columns, item.columns); assert(measured.minTarget >= 44); assert(measured.contrast >= 4.5);
    measurements.push({ ...item, measured });
    const shot = await cdp.send("Page.captureScreenshot", { format:"png", captureBeyondViewport:false });
    writeFileSync(resolve(screenshots, `${item.name}.png`), Buffer.from(shot.data, "base64"));
  }

  assert(await cdp.eval(`(()=>{const card=[...document.querySelectorAll('.card')].find(node=>node.textContent.includes('fixture-fusion'));return card&&!card.classList.contains('failed')&&!card.textContent.includes('needs attention')})()`), "historical-only invalid incorrectly received failure priority");
  await cdp.eval(`(()=>{const filter=document.getElementById('result-filter');filter.value='invalid';filter.dispatchEvent(new Event('input',{bubbles:true}));const visible=[...document.querySelectorAll('.card')].some(card=>card.textContent.includes('fixture-fusion'));filter.value='';filter.dispatchEvent(new Event('input',{bubbles:true}));if(!visible)throw Error('historical invalid missing from invalid filter');})()`);
  let diagnosticPassed=true;
  for(const [width,lang,label] of [[1440,"en","Historical record lacks capture-closure evidence"],[900,"zh","历史记录缺少捕获闭合证据"],[390,"zh","历史记录缺少捕获闭合证据"]]){
    await cdp.send("Emulation.setDeviceMetricsOverride",{width,height:900,deviceScaleFactor:1,mobile:width<500});
    await cdp.eval(`(()=>{const want=${JSON.stringify(lang)};if(!document.documentElement.lang.startsWith(want))document.getElementById('lang-toggle').click();const card=[...document.querySelectorAll('.card')].find(node=>node.textContent.includes('fixture-fusion'));card.click();})()`);
    const view=await cdp.eval(`(()=>{const member=[...document.querySelectorAll('.member')].find(node=>node.dataset.rowId==='fixture-fusion-b');return {text:member?.textContent??'',overflow:document.documentElement.scrollWidth>document.documentElement.clientWidth,attention:member?.textContent.includes('needs attention')??true}})()`);
    assert(view.text.includes(label),JSON.stringify({width,lang,view}));assert(!view.overflow);assert(!view.attention);await cdp.eval("document.getElementById('drawer-close').click()");
  }
  await cdp.send("Emulation.setDeviceMetricsOverride", { width:1440, height:900, deviceScaleFactor:1, mobile:false });
  await cdp.eval("document.body.tabIndex=-1;document.body.focus();document.body.removeAttribute('tabindex')");

  await cdp.send("Emulation.setEmulatedMedia", { features:[{name:"prefers-reduced-motion",value:"reduce"}] });
  assert(await cdp.eval("parseFloat(getComputedStyle(document.getElementById('drawer')).transitionDuration) <= .001"), "reduced motion not honored");
  await cdp.send("Emulation.setDeviceMetricsOverride", { width:1440, height:900, deviceScaleFactor:1, mobile:false });
  await cdp.send("Page.bringToFront");
  await cdp.send("Input.dispatchKeyEvent", {type:"keyDown",key:"Tab",code:"Tab"}); await cdp.send("Input.dispatchKeyEvent", {type:"keyUp",key:"Tab",code:"Tab"});
  assert(await cdp.eval("document.activeElement.classList.contains('skip-link')"), "keyboard skip link is not first focus target");
  const filterWorks = await cdp.eval(`(()=>{const q=document.getElementById('search');q.value='__definitely_absent__';q.dispatchEvent(new Event('input',{bubbles:true}));const empty=!!document.querySelector('.empty');q.value='';q.dispatchEvent(new Event('input',{bubbles:true}));return empty&&document.querySelectorAll('.card').length>0;})()`);
  assert(filterWorks, "text filter did not update real cards");
  await cdp.eval("document.querySelector('.card').focus()");
  await cdp.send("Input.dispatchKeyEvent", {type:"keyDown",key:" ",code:"Space",text:" ",windowsVirtualKeyCode:32,nativeVirtualKeyCode:32}); await cdp.send("Input.dispatchKeyEvent", {type:"keyUp",key:" ",code:"Space",windowsVirtualKeyCode:32,nativeVirtualKeyCode:32});
  assert(await cdp.eval("document.getElementById('drawer').classList.contains('open')&&document.querySelectorAll('.member').length>0"), "drawer did not open from keyboard");
  assert.equal(await cdp.eval("Math.max(...[...document.querySelectorAll('.card-grid')].map(grid=>{const own=[...grid.querySelectorAll(':scope > .card')];if(!own.length)return 0;const top=Math.round(own[0].getBoundingClientRect().top);return own.filter(x=>Math.round(x.getBoundingClientRect().top)===top).length}))"), 2, "drawer did not reduce wide grid columns");
  await cdp.send("Input.dispatchKeyEvent", {type:"rawKeyDown",key:"Escape",code:"Escape",windowsVirtualKeyCode:27,nativeVirtualKeyCode:27}); await cdp.send("Input.dispatchKeyEvent", {type:"keyUp",key:"Escape",code:"Escape",windowsVirtualKeyCode:27,nativeVirtualKeyCode:27});
  assert(await cdp.eval("!document.getElementById('drawer').classList.contains('open')&&document.activeElement.classList.contains('card')"), "drawer keyboard/focus restoration failed");
  const readerPrepared = await cdp.eval(`(()=>{const result=document.getElementById('result-filter');result.value='verified';result.dispatchEvent(new Event('input',{bubbles:true}));if(!document.querySelector('.card')){result.value='';result.dispatchEvent(new Event('input',{bubbles:true}));}const card=document.querySelector('.card');if(!card)return false;card.click();const member=[...document.querySelectorAll('.member')].find(x=>x.textContent.includes('verified'));if(member)member.click();return !document.getElementById('open-reader').disabled&&!document.getElementById('reader').classList.contains('open');})()`);
  assert(readerPrepared, "verified member was not ready for reader open");
  assert.equal(detailRequests, 0, "member selection eagerly requested detail");
  await cdp.eval("document.getElementById('open-reader').click()");
  assert(await cdp.eval("document.getElementById('reader').classList.contains('open')"), "verified reader route did not open");
  for (let i=0;i<100 && !(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')"));i++) await sleep(50);
  assert(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')&&!document.getElementById('copy-answer').disabled"), "verified detail did not reach reader/copy state");
  assert.equal(detailRequests, 1, "reader open did not issue exactly one detail request");
  const pollsBefore = snapshotRequests; await sleep(6500);
  assert(snapshotRequests >= pollsBefore + 3, "three background refresh intervals did not run");
  assert(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')&&!document.getElementById('copy-answer').disabled&&!document.getElementById('answer-state').textContent"), "background refresh erased verified answer");
  await cdp.eval("history.back()"); await sleep(100);
  assert(await cdp.eval("!document.getElementById('reader').classList.contains('open')&&document.getElementById('drawer').classList.contains('open')"), "browser back did not restore drawer");
  await cdp.eval("document.getElementById('open-reader').click()");
  for (let i=0;i<100 && !(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')"));i++) await sleep(50);
  assert.equal(detailRequests, 2, "reader reopen did not issue one replacement request");
  await cdp.eval("(()=>{if(!document.documentElement.lang.startsWith('en'))document.getElementById('lang-toggle').click();document.getElementById('reader-back').click();})()");
  detailMode="cancel_delay";await cdp.eval("document.getElementById('open-reader').click()");await sleep(100);await cdp.eval("document.getElementById('reader-back').click()");await sleep(1200);
  cancellationPassed=await cdp.eval("!document.getElementById('reader').classList.contains('open')&&document.getElementById('answer').textContent===''&&document.getElementById('copy-answer').disabled&&document.getElementById('reader-title').textContent==='—'");
  assert(cancellationPassed,"closing a pending reader retained stale state");
  async function expectReaderError(mode,snippet,timeout=5000){detailMode=mode;await cdp.eval("document.getElementById('open-reader').click()");const deadline=Date.now()+timeout;let state="";while(Date.now()<deadline){state=await cdp.eval("document.getElementById('answer-state').textContent");if(state&&!/Loading/.test(state))break;await sleep(50);}const view=await cdp.eval("({state:document.getElementById('answer-state').textContent,body:document.getElementById('answer').textContent,copy:document.getElementById('copy-answer').disabled,title:document.getElementById('reader-title').textContent})");assert(view.state.includes(snippet),`${mode}: ${JSON.stringify(view)}`);assert.equal(view.body,"");assert(view.copy);assert.equal(view.title,fixtureRows[0].id);readerErrorStates[mode]=view.state;await cdp.eval("document.getElementById('reader-back').click()");}
  for(const [mode,snippet] of [["not_found","no longer available"],["read_failed","could not be revalidated"],["busy","busy"],["request_timeout","Server timed out"],["forbidden","authorization expired"],["invalid_response","invalid response"],["identity_mismatch","invalid response"]])await expectReaderError(mode,snippet);
  await expectReaderError("client_timeout","exceeded 12 seconds",14000);
  detailMode="cancel_delay";await cdp.eval("document.getElementById('open-reader').click()");await sleep(100);await cdp.eval("(()=>{document.getElementById('reader-back').click();const member=[...document.querySelectorAll('.member')].find(node=>node.dataset.rowId==='fixture-task-b');member.click();document.getElementById('open-reader').click();})()");for(let i=0;i<100&&!(await cdp.eval("document.getElementById('answer').textContent.includes('fixture-task-b')"));i++)await sleep(50);await sleep(1200);memberReplacementPassed=await cdp.eval("document.getElementById('reader-title').textContent==='fixture-task-b'&&document.getElementById('answer').textContent.includes('fixture-task-b')&&!document.getElementById('copy-answer').disabled");assert(memberReplacementPassed,"delayed old member completion replaced the new member answer");await cdp.eval(`(()=>{document.getElementById('reader-back').click();const member=[...document.querySelectorAll('.member')].find(node=>node.dataset.rowId===${JSON.stringify(fixtureRows[0].id)});member.click();})()`);
  detailMode="cancel_delay";await cdp.eval("document.getElementById('open-reader').click()");await sleep(100);await cdp.eval("(()=>{const p=document.getElementById('project-select');p.value='browser-fixture-project-2';p.dispatchEvent(new Event('change',{bubbles:true}));})()");await sleep(1200);
  projectCancellationPassed=await cdp.eval("document.getElementById('project-select').value==='browser-fixture-project-2'&&!document.getElementById('reader').classList.contains('open')&&!document.getElementById('drawer').classList.contains('open')&&document.getElementById('answer').textContent===''&&document.getElementById('copy-answer').disabled&&document.getElementById('reader-title').textContent==='—'");assert(projectCancellationPassed,"project switch retained pending reader state");
  await cdp.eval("(()=>{const p=document.getElementById('project-select');p.value='browser-fixture-project';p.dispatchEvent(new Event('change',{bubbles:true}));})()");for(let i=0;i<100&&!(await cdp.eval("document.querySelectorAll('.card').length>0"));i++)await sleep(50);assert(await cdp.eval("document.querySelectorAll('.card').length>0"),"original project did not restore after cancellation test");
  await cdp.eval(`(()=>{const card=[...document.querySelectorAll('.card')].find(node=>node.textContent.includes('r90 / B900'));card.click();const member=[...document.querySelectorAll('.member')].find(node=>node.dataset.rowId===${JSON.stringify(fixtureRows[0].id)});member.click();document.getElementById('open-reader').click();})()`);for(let i=0;i<100&&!(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')"));i++)await sleep(50);
  snapshotResult="invalid";await cdp.eval("document.getElementById('refresh').click()");for(let i=0;i<100&&!(await cdp.eval("document.getElementById('answer-state').textContent.includes('verification changed')"));i++)await sleep(50);invalidationPassed=await cdp.eval("document.getElementById('answer').textContent===''&&document.getElementById('copy-answer').disabled&&document.getElementById('answer-state').textContent.includes('verification changed')");assert(invalidationPassed,"snapshot verification loss retained answer text or copy state");await cdp.eval("document.getElementById('reader-back').click();document.getElementById('drawer-close').click()");snapshotResult="verified";await cdp.eval("document.getElementById('refresh').click()");
  const markdown = await cdp.eval(`(async()=>{const app=await import('/assets/app.mjs');const host=document.createElement('div');host.append(app.sanitizeMarkdown('# Good\\n\\n|A|B|\\n|-|-|\\n|1|2|\\n\\n<img src=https://outside.invalid/x onerror=alert(1) alt=ALT><svg><foreignObject><p>DROP</p></foreignObject></svg><math><mtext><p>DROP2</p></mtext></math><style><p>DROP3</p></style><iframe srcdoc="<p>DROP4</p>"></iframe><form><p>DROP5</p></form><script>alert(1)</script>[bad](javascript:alert(1))\\n\\n\`code\`'));return {scripts:host.querySelectorAll('script,style,svg,img,iframe,object,embed,math,form,video,audio,[onerror],[onload]').length,hrefs:[...host.querySelectorAll('a')].map(a=>a.getAttribute('href')),heading:!!host.querySelector('h1'),table:!!host.querySelector('table'),code:!!host.querySelector('code'),alt:host.textContent.includes('ALT'),drop:/DROP/.test(host.textContent)};})()`);
  assert.deepEqual(markdown, { scripts:0, hrefs:[], heading:true, table:true, code:true, alt:true, drop:false });

  // A second owned server uses an isolated HOME and a fake native executable.
  // Unlike the observation attack corpus above, this flow uses the actual APIs.
  await cdp.send("Fetch.disable");
  const fusionProject=resolve(scratch,"fusion project"),fusionHome=resolve(scratch,"native home");
  mkdirSync(fusionProject);mkdirSync(resolve(fusionProject,".orch"));mkdirSync(resolve(fusionHome,".claude"),{recursive:true});
  writeFileSync(resolve(fusionProject,".gitignore"),".orch/\nprovider.calls\n");
  writeFileSync(resolve(fusionHome,".claude/settings.json"),JSON.stringify({model:"browser-native",effortLevel:"off"}));
  const fake=resolve(fusionProject,"provider");
  writeFileSync(fake,"#!/bin/sh\nprintf 'called\\n' >> \"$0.calls\"\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"# Browser Fusion answer\\n\\n- complete\\n- bounded\"}'\n");chmodSync(fake,0o755);
  checked("git",["-c","core.fsmonitor=false","-C",fusionProject,"init","-q"]);
  checked("git",["-c","core.fsmonitor=false","-C",fusionProject,"add",".gitignore","provider"]);
  checked("git",["-c","core.fsmonitor=false","-c","user.name=Fixture","-c","user.email=f@example.invalid","-c","commit.gpgSign=false","-c","core.hooksPath=/dev/null","-C",fusionProject,"commit","-qm","fixture"]);
  writeFileSync(resolve(fusionProject,".orch/harnesses.yaml"),`version: 1\nharnesses:\n  fixture:\n    driver: claude\n    executable: ${fake}\n    enabled: true\n    cwdPolicy: project-root\n`);
  fusionServer=spawn(orchWeb,["--no-open","--root",fusionProject],{stdio:["ignore","pipe","pipe"],env:{HOME:fusionHome,PATH:"/usr/bin:/bin",USER:process.env.USER??"",LANG:"en_US.UTF-8",TMPDIR:process.env.TMPDIR??tmpdir()}});
  const fusionUrl=(await firstLine(fusionServer.stdout,15000)).replace(/^OpenOrch Web:\s*/,"");
  async function until(expression,message,timeout=30000){const deadline=Date.now()+timeout;while(Date.now()<deadline){if(await cdp.eval(expression))return;await sleep(80);}throw Error(message+": "+await cdp.eval("document.getElementById('fusion-note')?.textContent+' / '+document.getElementById('fusion-run-status')?.textContent"));}
  const fusionLoaded=cdp.once("Page.loadEventFired");await cdp.send("Page.navigate",{url:fusionUrl+"?view=fusion"});await fusionLoaded;
  await until("!document.getElementById('fusion-workspace').hidden&&!document.getElementById('fusion-add-role').disabled&&!document.getElementById('fusion-native-refresh').disabled&&/\\d{4}-\\d{2}-\\d{2}/.test(document.getElementById('fusion-native-status').textContent)","Fusion library did not initialize");
  await cdp.eval(`(()=>{const input=(id,value,type='input')=>{const e=document.getElementById(id);e.value=value;e.dispatchEvent(new Event(type,{bubbles:true}));};window.fusionFixtureRoles=[];for(const [name,instructions] of [['Alpha <img src=x onerror=alert(1)>','Perspective A'],['Beta','Perspective B'],['Synthesis','Synthesize']]){document.getElementById('fusion-add-role').click();input('fusion-role-name',name);input('fusion-role-harness','configured:fixture','change');input('fusion-role-instructions',instructions);window.fusionFixtureRoles.push(document.querySelector('#fusion-role-list button.active').dataset.roleId);}document.querySelectorAll('#fusion-role-list button')[1].click();input('fusion-role-model','future/model-next');input('fusion-role-effort','future-effort');document.getElementById('fusion-add-combination').click();input('fusion-combination-name','Browser pair');for(const id of window.fusionFixtureRoles.slice(0,2)){input('fusion-add-member',id,'change');document.getElementById('fusion-add-member-button').click();}input('fusion-synthesizer',window.fusionFixtureRoles[2],'change');document.getElementById('fusion-up-'+window.fusionFixtureRoles[1]).click();const check=document.getElementById('fusion-enabled-'+window.fusionFixtureRoles[1]);check.click();if(!document.getElementById('fusion-start').disabled)throw Error('one-member group enabled');check.click();document.getElementById('fusion-role-instructions').value='draft survives refresh';document.getElementById('fusion-role-instructions').dispatchEvent(new Event('input',{bubbles:true}));document.getElementById('fusion-native-refresh').click();})()`);
  await until("!document.getElementById('fusion-native-refresh').disabled","native refresh did not end");
  assert.equal(await cdp.eval("document.getElementById('fusion-role-instructions').value"),"draft survives refresh");
  assert.equal(await cdp.eval("document.querySelectorAll('#fusion-role-list img,#fusion-role-list script').length"),0);
  await cdp.eval("document.getElementById('fusion-save').click()");await until("document.getElementById('fusion-save').disabled&&!/未保存|Unsaved|读取|Loading/.test(document.getElementById('fusion-note').textContent)","role save did not finish");
  let library=JSON.parse(readFileSync(resolve(fusionProject,".orch/fusion.json"),"utf8")).config;
  assert.equal(library.roles.length,3);assert.equal(library.combinations[0].members[0],library.roles[1].id);assert.equal(library.roles[1].fixed.model,"future/model-next");assert.equal(library.roles[1].fixed.effort,"future-effort");assert.equal(library.roles[0].fixed.model,null);
  await cdp.eval(`(()=>{const set=(id,value,type='input')=>{const e=document.getElementById(id);e.value=value;e.dispatchEvent(new Event(type,{bubbles:true}));};document.getElementById('fusion-add-role').click();set('fusion-role-name','Temporary');set('fusion-role-harness','configured:fixture','change');const id=document.querySelector('#fusion-role-list button.active').dataset.roleId;set('fusion-add-member',id,'change');document.getElementById('fusion-add-member-button').click();set('fusion-synthesizer',id,'change');document.getElementById('fusion-delete-role').click();if(document.querySelectorAll('.fusion-member').length!==2||document.getElementById('fusion-synthesizer').value!=='')throw Error('dangling deleted role');set('fusion-synthesizer',window.fusionFixtureRoles[2],'change');document.getElementById('fusion-save').click();})()`);
  await until("document.getElementById('fusion-save').disabled&&!/未保存|Unsaved|读取|Loading/.test(document.getElementById('fusion-note').textContent)","deletion save did not finish");
  library=JSON.parse(readFileSync(resolve(fusionProject,".orch/fusion.json"),"utf8")).config;assert.equal(library.roles.length,3);assert.equal(library.revision,2);
  assert(!existsSync(fake+".calls"),"editing/refresh invoked a model");
  await cdp.eval("(()=>{const q=document.getElementById('fusion-question');q.value='Compare the fixture perspectives';q.dispatchEvent(new Event('input',{bubbles:true}));})()");
  await until("!document.getElementById('fusion-start').disabled","valid saved combination not runnable");
  await cdp.eval("document.getElementById('fusion-start').click()");
  await until("document.querySelectorAll('#fusion-results article.markdown').length===3&&/已完成|Completed/.test(document.getElementById('fusion-run-status').textContent)","Fusion results did not complete",40000);
  assert.equal(readFileSync(fake+".calls","utf8").trim().split("\n").length,3,"not exactly two members and one synthesis");
  assert(await cdp.eval("document.getElementById('fusion-results').textContent.includes('future/model-next')&&document.querySelector('.fusion-result.synthesis h1').textContent==='Browser Fusion answer'"));
  const fusionMeasurements=[];
  for(const [width,theme] of [[1440,"light"],[900,"dark"],[390,"dark"]]){
    await cdp.send("Emulation.setDeviceMetricsOverride",{width,height:900,deviceScaleFactor:1,mobile:width<500});
    await cdp.eval(`(()=>{const t=document.getElementById('theme-select');t.value=${JSON.stringify(theme)};t.dispatchEvent(new Event('change',{bubbles:true}));})()`);await sleep(60);
    const measurement=await cdp.eval(`(()=>{const lum=s=>{const c=s.match(/[0-9.]+/g).slice(0,3).map(Number).map(x=>{x/=255;return x<=.03928?x/12.92:((x+.055)/1.055)**2.4});return c[0]*.2126+c[1]*.7152+c[2]*.0722};const contrast=[...document.querySelectorAll('#fusion-role-list button,#fusion-history button')].map(e=>{const c=getComputedStyle(e),a=lum(c.color),b=lum(c.backgroundColor);return (Math.max(a,b)+.05)/(Math.min(a,b)+.05)});return {overflow:document.documentElement.scrollWidth>document.documentElement.clientWidth,contrast:Math.min(...contrast),buttons:[...document.querySelectorAll('#fusion-workspace button')].filter(e=>e.offsetParent).every(e=>e.getBoundingClientRect().height>=44),checkboxTargets:[...document.querySelectorAll('.fusion-member label')].every(e=>e.getBoundingClientRect().height>=44)}})()`);
    assert(!measurement.overflow);assert(measurement.buttons&&measurement.checkboxTargets);assert(measurement.contrast>=4.5,JSON.stringify({width,theme,...measurement}));fusionMeasurements.push({width,theme,...measurement});const shot=await cdp.send("Page.captureScreenshot",{format:"png",captureBeyondViewport:false});writeFileSync(resolve(screenshots,`fusion-${width}.png`),Buffer.from(shot.data,"base64"));
  }
  await cdp.send("Emulation.setDeviceMetricsOverride",{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await cdp.eval("document.querySelector('#fusion-role-list button').focus()");await cdp.send("Input.dispatchKeyEvent",{type:"keyDown",key:" ",code:"Space",text:" ",windowsVirtualKeyCode:32,nativeVirtualKeyCode:32});await cdp.send("Input.dispatchKeyEvent",{type:"keyUp",key:" ",code:"Space",windowsVirtualKeyCode:32,nativeVirtualKeyCode:32});assert(await cdp.eval("document.getElementById('fusion-role-name').value.startsWith('Alpha')"));
  // Project switches retain each project's draft and cannot show a late response in another library.
  await cdp.eval(`(()=>{const e=document.getElementById('fusion-role-instructions');e.value='unsaved project-local draft';e.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  const originalProject=await cdp.eval("document.getElementById('project-select').value");
  const otherProject=resolve(scratch,"second fusion project");mkdirSync(otherProject);checked("git",["-c","core.fsmonitor=false","-C",otherProject,"init","-q"]);checked("git",["-c","core.fsmonitor=false","-c","user.name=Fixture","-c","user.email=f@example.invalid","-c","commit.gpgSign=false","-c","core.hooksPath=/dev/null","-C",otherProject,"commit","--allow-empty","-qm","fixture"]);
  await cdp.eval(`(()=>{document.getElementById('project-path').value=${JSON.stringify(otherProject)};document.getElementById('project-form').dispatchEvent(new Event('submit',{bubbles:true,cancelable:true}));})()`);
  await until("document.getElementById('project-select').value!=="+JSON.stringify(originalProject)+"&&!document.getElementById('fusion-add-role').disabled","second project not loaded");assert.equal(await cdp.eval("document.querySelectorAll('#fusion-role-list button').length"),0);
  await cdp.eval(`(()=>{const p=document.getElementById('project-select');p.value=${JSON.stringify(originalProject)};p.dispatchEvent(new Event('change',{bubbles:true}));})()`);
  await until("document.getElementById('fusion-role-instructions').value==='unsaved project-local draft'","project draft not retained");
  const fusionEvidence={project:fusionProject,run:await cdp.eval("document.getElementById('fusion-run-status').textContent"),nativeInvocations:3,roleCrud:true,reorder:true,disable:true,fixedAndFollow:true,refreshPreservesDraft:true,projectDraftIsolation:true,keyboard:true,measurements:fusionMeasurements};

  await sleep(200);
  const origin = new URL(url).origin;
  const allowedOrigins=new Set([origin,new URL(fusionUrl).origin]);
  const external = requests.filter(request => request.startsWith("http") && !allowedOrigins.has(new URL(request).origin));
  assert.equal(external.length, 0, external.join("\n"));
  const after = treeDigest();
  assert.equal(after, before, "browser session changed repository bytes");
  assert.equal(providerCount(), providersBefore, "browser session changed provider process count");

  const readerEvidence={qualifiedId:fixtureRows[0].id,delayedFirstDetailMillis:2300,detailRequests,snapshotRequests,sustainedPolls:3,answerSurvived:true,memberSelectionPrefetch:false,cancellationPassed,memberReplacementPassed,projectCancellationPassed,invalidationPassed,diagnosticPassed,errorStates:readerErrorStates};
  writeFileSync(resolve(scratch, "evidence.json"), JSON.stringify({schemaVersion:1,root,orchWeb,chrome,url,matrix:measurements,externalRequests:external,treeDigestBefore:before,treeDigestAfter:after,providersBefore,providersAfter:providerCount(),markdown,reader:readerEvidence,fusion:fusionEvidence,checks:{keyboard:true,reader:true,readerPolling:true,browserBack:true,reducedMotion:true,filter:true,readonly:true}}, null, 2) + "\n");

  console.log("matrix=light/en/wide,dark/zh/medium,system/zh/narrow");
  console.log("externalRequests=0");
  console.log("keyboard=pass");
  console.log("markdownDom=pass");
  console.log("overflow=pass");
  console.log("readonly=pass");
  console.log("readerPolling=delayed-detail-three-refreshes-pass");
  console.log("diagnostics=historical-neutral-language-width-filter-pass");
  console.log("fusion=real-http-crud-native-config-two-members-one-synthesis-pass");
  console.log(`screenshots=${screenshots}`);
  success = true;
} finally {
  try { socket?.close(); } catch {}
  if (browser && browser.exitCode === null) { browser.kill("SIGTERM"); await sleep(100); if (browser.exitCode === null) browser.kill("SIGKILL"); }
  if (fusionServer && fusionServer.exitCode === null) { fusionServer.kill("SIGINT"); await sleep(200); if(fusionServer.exitCode===null)fusionServer.kill("SIGKILL"); }
  if (server && server.exitCode === null) { server.kill("SIGINT"); await sleep(200); if (server.exitCode === null) server.kill("SIGKILL"); }
  if (success && process.env.ORCH_KEEP_BROWSER_ARTIFACTS !== "1") rmSync(scratch, { recursive:true, force:true });
}
