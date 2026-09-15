import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
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

let server, browser, socket, success = false;
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
    {id:"fixture-task-a",source:"selfhost",summary:"Safe browser fixture task",phase:"recorded",result:"verified",alias:"smartclaw",driver:"smartclaw",purpose:"review",source_time:"2026-09-15T12:00:00Z",started_at:null,native_status:{managedScopeTerminated:true},task:{round:"r90",id:"B900",attempt:"B900-A0001",head:"abc",state:"recorded"},fusion_id:null},
    {id:"fixture-task-b",source:"selfhost",summary:"",phase:"ended",result:"none",alias:"local",driver:"local",purpose:"implement",source_time:"2026-09-15T11:59:00Z",started_at:null,native_status:{},task:{round:"r90",id:"B900",attempt:"B900-A0002",head:"def",state:"invoked"},fusion_id:null},
    {id:"fixture-fusion-a",source:"consult",summary:"Design review",phase:"ended",result:"verified",alias:"one",driver:"claude",purpose:"consult",source_time:"2026-09-15T11:58:00Z",started_at:null,native_status:{terminalSeen:true},task:null,fusion_id:"fixture-fusion"},
    {id:"fixture-fusion-b",source:"consult",summary:"",phase:"ended",result:"invalid",alias:"two",driver:"opencode",purpose:"consult",source_time:"2026-09-15T11:57:00Z",started_at:null,native_status:{},task:null,fusion_id:"fixture-fusion"},
    {id:"fixture-failed",source:"standalone",summary:"Needs attention",phase:"ended",result:"failed",alias:null,driver:null,purpose:"review",source_time:"2026-09-15T11:56:00Z",started_at:null,native_status:{},task:null,fusion_id:null},
    {id:"fixture-unknown",source:"standalone",summary:"Unknown time remains visible",phase:"published",result:"unknown",alias:"three",driver:"dsh",purpose:"consult",source_time:null,started_at:null,native_status:{},task:null,fusion_id:null},
  ];
  const fixtureGroups = [{id:"fixture-fusion",members:["fixture-fusion-a","fixture-fusion-b"],total:2,roster_complete:true,phase_counts:{ended:2},result_counts:{verified:1,invalid:1}}];
  const envelope = (generation, data) => ({serverInstanceId:"browser-fixture-instance",projectId:"browser-fixture-project",snapshotGeneration:generation,data});
  cdp.on("Fetch.requestPaused", async params => {
    try {
      const path = new URL(params.request.url).pathname;
      let body;
      if (path === "/api/v1/projects") body = envelope(0, [{id:"browser-fixture-project",name:"Browser fixture",root:"[fixture]"}]);
      else if (path.endsWith("/snapshot")) body = envelope(1, {root:"[browser fixture]",read_at:"2026-09-15T12:00:01Z",rows:fixtureRows,groups:fixtureGroups,diagnostics:[],truncated:false});
      else if (path.endsWith("/detail")) body = envelope(2, {row:fixtureRows[0],text:"# Verified fixture answer\n\n- safe\n- local\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n`code`",locator:"fixture/detail",truncated:false});
      else return await cdp.send("Fetch.continueRequest", {requestId:params.requestId});
      await cdp.send("Fetch.fulfillRequest", {requestId:params.requestId,responseCode:200,responseHeaders:[{name:"Content-Type",value:"application/json"},{name:"Cache-Control",value:"no-store"}],body:Buffer.from(JSON.stringify(body)).toString("base64")});
    } catch (error) { await cdp.send("Fetch.failRequest", {requestId:params.requestId,errorReason:"Failed"}); throw error; }
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
  const reader = await cdp.eval(`(()=>{const result=document.getElementById('result-filter');result.value='verified';result.dispatchEvent(new Event('input',{bubbles:true}));if(!document.querySelector('.card')){result.value='';result.dispatchEvent(new Event('input',{bubbles:true}));}const card=document.querySelector('.card');if(!card)return false;card.click();const member=[...document.querySelectorAll('.member')].find(x=>x.textContent.includes('verified'));if(member)member.click();document.getElementById('open-reader').click();return document.getElementById('reader').classList.contains('open');})()`);
  assert(reader, "verified reader route did not open");
  for (let i=0;i<100 && !(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')"));i++) await sleep(50);
  assert(await cdp.eval("document.getElementById('answer').textContent.includes('Verified fixture answer')&&!document.getElementById('copy-answer').disabled"), "verified detail did not reach reader/copy state");
  await cdp.eval("history.back()"); await sleep(100);
  assert(await cdp.eval("!document.getElementById('reader').classList.contains('open')&&document.getElementById('drawer').classList.contains('open')"), "browser back did not restore drawer");
  await cdp.eval("document.getElementById('open-reader').click()");
  assert(await cdp.eval("(()=>{document.getElementById('answer').textContent='stale sentinel';document.getElementById('refresh').click();return document.getElementById('answer').textContent===''})()"), "refresh retained stale answer DOM");
  const markdown = await cdp.eval(`(async()=>{const app=await import('/assets/app.mjs');const host=document.createElement('div');host.append(app.sanitizeMarkdown('# Good\\n\\n|A|B|\\n|-|-|\\n|1|2|\\n\\n<img src=https://outside.invalid/x onerror=alert(1) alt=ALT><svg><foreignObject><p>DROP</p></foreignObject></svg><math><mtext><p>DROP2</p></mtext></math><style><p>DROP3</p></style><iframe srcdoc="<p>DROP4</p>"></iframe><form><p>DROP5</p></form><script>alert(1)</script>[bad](javascript:alert(1))\\n\\n\`code\`'));return {scripts:host.querySelectorAll('script,style,svg,img,iframe,object,embed,math,form,video,audio,[onerror],[onload]').length,hrefs:[...host.querySelectorAll('a')].map(a=>a.getAttribute('href')),heading:!!host.querySelector('h1'),table:!!host.querySelector('table'),code:!!host.querySelector('code'),alt:host.textContent.includes('ALT'),drop:/DROP/.test(host.textContent)};})()`);
  assert.deepEqual(markdown, { scripts:0, hrefs:[], heading:true, table:true, code:true, alt:true, drop:false });

  await sleep(200);
  const origin = new URL(url).origin;
  const external = requests.filter(request => request.startsWith("http") && new URL(request).origin !== origin);
  assert.equal(external.length, 0, external.join("\n"));
  const after = treeDigest();
  assert.equal(after, before, "browser session changed repository bytes");
  assert.equal(providerCount(), providersBefore, "browser session changed provider process count");

  writeFileSync(resolve(scratch, "evidence.json"), JSON.stringify({schemaVersion:1,root,orchWeb,chrome,url,matrix:measurements,externalRequests:external,treeDigestBefore:before,treeDigestAfter:after,providersBefore,providersAfter:providerCount(),markdown,checks:{keyboard:true,reader:true,browserBack:true,reducedMotion:true,filter:true,readonly:true}}, null, 2) + "\n");

  console.log("matrix=light/en/wide,dark/zh/medium,system/zh/narrow");
  console.log("externalRequests=0");
  console.log("keyboard=pass");
  console.log("markdownDom=pass");
  console.log("overflow=pass");
  console.log("readonly=pass");
  console.log(`screenshots=${screenshots}`);
  success = true;
} finally {
  try { socket?.close(); } catch {}
  if (browser && browser.exitCode === null) { browser.kill("SIGTERM"); await sleep(100); if (browser.exitCode === null) browser.kill("SIGKILL"); }
  if (server && server.exitCode === null) { server.kill("SIGINT"); await sleep(200); if (server.exitCode === null) server.kill("SIGKILL"); }
  if (success && process.env.ORCH_KEEP_BROWSER_ARTIFACTS !== "1") rmSync(scratch, { recursive:true, force:true });
}
