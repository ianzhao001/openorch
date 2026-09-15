//! Loopback-only observation HTTP service. No endpoint can execute a provider,
//! mutate project files, collect results, or control a task. Browser-origin
//! protections are not an authentication boundary against same-UID processes.
use axum::{
    body::{to_bytes, Body},
    extract::{Path as RoutePath, Query, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use orch_host::observation::{safe_observation_value, ObservationReader};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};
use tokio::sync::{oneshot, Semaphore};

/// Encode every untrusted response value through key-aware credential redaction.
/// Newlines remain display text; callers must never interpret them as trusted HTML.
pub fn sanitize_payload(value: &Value) -> Value {
    safe_observation_value(value)
}

struct Project {
    root: PathBuf,
    reader: ObservationReader,
    generation: u64,
}
struct Shared {
    instance: String,
    token: String,
    host: String,
    projects: Mutex<BTreeMap<String, Arc<Mutex<Project>>>>,
    jobs: Arc<Semaphore>,
}

/// An owned local observation server. Dropping it closes the listener and joins
/// its runtime thread; it never terminates another service or provider process.
pub struct WebServer {
    address: SocketAddr,
    token: String,
    initial: String,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl WebServer {
    /// Validate a committed exact Git root and start an owned 127.0.0.1 listener.
    /// Port zero asks the OS for a free port without releasing/rebinding it.
    pub fn start(root: &Path, port: u16) -> Result<Self, String> {
        let project = validate_project(root)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|_| "local port unavailable".to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "listener unavailable".to_string())?;
        let address = listener
            .local_addr()
            .map_err(|_| "listener unavailable".to_string())?;
        let token = random_id()?;
        let instance = random_id()?;
        let initial = random_id()?;
        let shared = Arc::new(Shared {
            instance,
            token: token.clone(),
            host: address.to_string(),
            projects: Mutex::new(BTreeMap::from([(
                initial.clone(),
                Arc::new(Mutex::new(project)),
            )])),
            jobs: Arc::new(Semaphore::new(8)),
        });
        let (shutdown, stopped) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => {
                    let _ = ready_tx.send(Err("runtime unavailable".into()));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(_) => {
                        let _ = ready_tx.send(Err("listener unavailable".into()));
                        return;
                    }
                };
                let app = Router::new()
                    .route("/", get(index))
                    .route("/assets/{asset}", get(asset))
                    .route("/api/v1/projects", get(projects).post(register))
                    .route("/api/v1/projects/{project}/snapshot", get(snapshot))
                    .route("/api/v1/projects/{project}/detail", get(detail))
                    .fallback(not_found)
                    .layer(middleware::from_fn_with_state(shared.clone(), guards))
                    .with_state(shared);
                let _ = ready_tx.send(Ok::<(), String>(()));
                let (graceful, done) = oneshot::channel();
                let serving = axum::serve(listener, app).with_graceful_shutdown(async { let _ = done.await; });
                let serving = std::future::IntoFuture::into_future(serving);
                tokio::pin!(serving);
                tokio::select! {
                    _ = &mut serving => {},
                    _ = stopped => {
                        let _ = graceful.send(());
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut serving).await;
                    }
                }
            });
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                address,
                token,
                initial,
                shutdown: Some(shutdown),
                worker: Some(worker),
            }),
            result => {
                let _ = shutdown.send(());
                let _ = worker.join();
                Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "server startup failed".into()))
            }
        }
    }
    /// Actual held listener address; always IPv4 loopback, including ephemeral ports.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    /// Per-launch browser capability. Keep it local; never include it in logs or URLs.
    pub fn capability(&self) -> &str {
        &self.token
    }
    /// Opaque registered identity of the initial project, not a filesystem path.
    pub fn initial_project(&self) -> &str {
        &self.initial
    }
}
impl Drop for WebServer {
    fn drop(&mut self) {
        if let Some(s) = self.shutdown.take() {
            let _ = s.send(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "OS entropy unavailable".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn validate_project(root: &Path) -> Result<Project, String> {
    if root.as_os_str().len() > 4096 {
        return Err("invalid project".into());
    }
    let root = fs::canonicalize(root).map_err(|_| "invalid project".to_string())?;
    if !root.is_dir() {
        return Err("invalid project".into());
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-c", "core.fsmonitor=false", "--no-optional-locks", "-C"])
            .arg(&root)
            .args(args)
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
    };
    let top = git(&["rev-parse", "--show-toplevel"]).map_err(|_| "invalid project".to_string())?;
    let top_path = String::from_utf8(top.stdout).map_err(|_| "invalid project".to_string())?;
    if !top.status.success() || fs::canonicalize(top_path.trim()).ok().as_ref() != Some(&root) {
        return Err("invalid project".into());
    }
    if !git(&["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|_| "invalid project".to_string())?
        .status
        .success()
    {
        return Err("invalid project".into());
    }
    let mut reader = ObservationReader::open(&root).map_err(|_| "invalid project".to_string())?;
    reader
        .refresh()
        .map_err(|_| "project unavailable".to_string())?;
    Ok(Project {
        root,
        reader,
        generation: 0,
    })
}
fn response(
    s: &Shared,
    id: Option<&str>,
    generation: u64,
    status: StatusCode,
    data: Result<Value, &str>,
) -> Response {
    let mut value =
        json!({"serverInstanceId":s.instance,"projectId":id,"snapshotGeneration":generation});
    match data {
        Ok(v) => value["data"] = sanitize_payload(&v),
        Err(code) => value["error"] = json!({"code":code,"message":code}),
    };
    (status, Json(sanitize_payload(&value))).into_response()
}
async fn guards(State(s): State<Arc<Shared>>, req: Request<Body>, next: Next) -> Response {
    let h = req.headers();
    let header = |key: &str| h.get(key).and_then(|v| v.to_str().ok());
    let origin = format!("http://{}", s.host);
    let valid = header("host") == Some(s.host.as_str())
        && (!h.contains_key("origin") || header("origin") == Some(origin.as_str()))
        && (!h.contains_key("sec-fetch-site")
            || matches!(header("sec-fetch-site"), Some("same-origin" | "none")));
    let api = req.uri().path().starts_with("/api/");
    let mut res = if !valid || (api && header("x-orch-capability") != Some(s.token.as_str())) {
        response(&s, None, 0, StatusCode::FORBIDDEN, Err("forbidden"))
    } else if req.method() == "OPTIONS" {
        response(
            &s,
            None,
            0,
            StatusCode::METHOD_NOT_ALLOWED,
            Err("method_not_allowed"),
        )
    } else {
        match tokio::time::timeout(std::time::Duration::from_secs(10), next.run(req)).await {
            Ok(res) => res,
            Err(_) => response(
                &s,
                None,
                0,
                StatusCode::REQUEST_TIMEOUT,
                Err("request_timeout"),
            ),
        }
    };
    for (key,value) in [("cache-control","no-store"),("x-content-type-options","nosniff"),("referrer-policy","no-referrer"),("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; img-src 'none'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")] {res.headers_mut().insert(axum::http::HeaderName::from_static(key),axum::http::HeaderValue::from_static(value));}
    res
}
async fn index(State(s): State<Arc<Shared>>) -> Html<String> {
    Html(INDEX.replace("__ORCH_CAPABILITY__", &s.token))
}
async fn asset(RoutePath(asset): RoutePath<String>) -> Response {
    let found = match asset.as_str() {
        "model.mjs" => Some(("text/javascript; charset=utf-8", MODEL)),
        "app.mjs" => Some(("text/javascript; charset=utf-8", APP)),
        "style.css" => Some(("text/css; charset=utf-8", STYLE)),
        "marked.mjs" => Some(("text/javascript; charset=utf-8", MARKED)),
        _ => None,
    };
    match found {
        Some((mime, body)) => ([(header::CONTENT_TYPE, mime)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn not_found(State(s): State<Arc<Shared>>) -> Response {
    response(&s, None, 0, StatusCode::NOT_FOUND, Err("not_found"))
}
async fn blocking<F>(s: Arc<Shared>, f: F) -> Response
where
    F: FnOnce(Arc<Shared>) -> Response + Send + 'static,
{
    let permit = match s.jobs.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(&s, None, 0, StatusCode::SERVICE_UNAVAILABLE, Err("busy")),
    };
    let copy = s.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f(copy)
    })
    .await
    {
        Ok(r) => r,
        Err(_) => response(
            &s,
            None,
            0,
            StatusCode::INTERNAL_SERVER_ERROR,
            Err("internal"),
        ),
    }
}
async fn projects(State(s): State<Arc<Shared>>) -> Response {
    blocking(s,|s|{
        let registry=s.projects.lock().unwrap_or_else(|e|e.into_inner());
        let data:Vec<Value>=registry.iter().map(|(id,p)|{let p=p.lock().unwrap_or_else(|e|e.into_inner());json!({"id":id,"root":p.root.to_string_lossy(),"name":p.root.file_name().unwrap_or_default().to_string_lossy()})}).collect();
        response(&s,None,0,StatusCode::OK,Ok(json!(data)))
    }).await
}
async fn register(State(s): State<Arc<Shared>>, req: Request<Body>) -> Response {
    if req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim())
        != Some("application/json")
    {
        return response(
            &s,
            None,
            0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Err("json_required"),
        );
    }
    let body = match to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => {
            return response(
                &s,
                None,
                0,
                StatusCode::PAYLOAD_TOO_LARGE,
                Err("body_limit"),
            )
        }
    };
    let root = match serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("root").and_then(Value::as_str).map(str::to_owned))
    {
        Some(p) if p.len() <= 4096 => p,
        _ => return response(&s, None, 0, StatusCode::BAD_REQUEST, Err("bad_project")),
    };
    blocking(s, move |s| {
        let project = match validate_project(Path::new(&root)) {
            Ok(p) => p,
            Err(_) => return response(&s, None, 0, StatusCode::BAD_REQUEST, Err("bad_project")),
        };
        let mut registry = s.projects.lock().unwrap_or_else(|e| e.into_inner());
        for (id, p) in registry.iter() {
            if p.lock().unwrap_or_else(|e| e.into_inner()).root == project.root {
                return response(&s, Some(id), 0, StatusCode::OK, Ok(json!({"id":id})));
            }
        }
        if registry.len() >= 16 {
            return response(&s, None, 0, StatusCode::CONFLICT, Err("registry_full"));
        }
        let id = match random_id() {
            Ok(id) => id,
            Err(_) => {
                return response(
                    &s,
                    None,
                    0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Err("internal"),
                )
            }
        };
        registry.insert(id.clone(), Arc::new(Mutex::new(project)));
        response(&s, Some(&id), 0, StatusCode::OK, Ok(json!({"id":id})))
    })
    .await
}
async fn snapshot(State(s): State<Arc<Shared>>, RoutePath(id): RoutePath<String>) -> Response {
    read_project(s, id, None).await
}
async fn detail(
    State(s): State<Arc<Shared>>,
    RoutePath(id): RoutePath<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let call = match q.get("id") {
        Some(c) if !c.is_empty() && c.len() <= 512 => c.clone(),
        _ => {
            return response(
                &s,
                Some(&id),
                0,
                StatusCode::BAD_REQUEST,
                Err("bad_invocation"),
            )
        }
    };
    read_project(s, id, Some(call)).await
}
async fn read_project(s: Arc<Shared>, id: String, call: Option<String>) -> Response {
    blocking(s, move |s| {
        let p = {
            s.projects
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned()
        };
        let Some(p) = p else {
            return response(&s, Some(&id), 0, StatusCode::NOT_FOUND, Err("not_found"));
        };
        let mut p = p.lock().unwrap_or_else(|e| e.into_inner());
        let result = match call {
            Some(call) => p
                .reader
                .detail_multiline(&call)
                .and_then(|v| Ok(serde_json::to_value(v)?)),
            None => p
                .reader
                .refresh()
                .and_then(|v| Ok(serde_json::to_value(v)?)),
        };
        match result {
            Ok(data) => {
                p.generation += 1;
                response(&s, Some(&id), p.generation, StatusCode::OK, Ok(data))
            }
            Err(error)
                if error
                    .downcast_ref::<orch_host::observation::InvocationUnavailable>()
                    .is_some() =>
            {
                response(
                    &s,
                    Some(&id),
                    p.generation,
                    StatusCode::NOT_FOUND,
                    Err("not_found"),
                )
            }
            Err(_) => response(
                &s,
                Some(&id),
                p.generation,
                StatusCode::SERVICE_UNAVAILABLE,
                Err("read_failed"),
            ),
        }
    })
    .await
}

const INDEX: &str = include_str!("web_assets/index.html");
const STYLE: &str = include_str!("web_assets/style.css");
const MODEL: &str = include_str!("web_assets/model.mjs");
const APP: &str = include_str!("web_assets/app.mjs");
const MARKED: &str = include_str!("web_assets/marked.mjs");

static CLI_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn cli_stop(_: i32) {
    CLI_STOP.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
unsafe extern "C" {
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
}

fn install_cli_signals() -> bool {
    CLI_STOP.store(false, Ordering::SeqCst);
    #[cfg(unix)]
    unsafe {
        return signal(2, cli_stop) != usize::MAX && signal(15, cli_stop) != usize::MAX;
    }
    #[cfg(not(unix))]
    false
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result = Command::new("/usr/bin/open").arg(url).status();
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).status();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", "", url]).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser opener unavailable",
    ));
    result.map(|s| s.success()).unwrap_or(false)
}

/// Run the standalone, read-only Web observation service CLI.
///
/// The function parses only `--root`, `--port`, `--no-open`, and `--help`;
/// it never invokes a provider or mutates an observed project. The process owns
/// exactly one loopback server and keeps it alive until SIGINT/SIGTERM.
pub fn run_web_cli(args: &[String]) -> i32 {
    let mut root: Option<PathBuf> = None;
    let mut port = 0u16;
    let mut no_open = false;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!("Usage: orch-web [--root PATH] [--port PORT] [--no-open]");
                return 0;
            }
            "--no-open" => no_open = true,
            "--root" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("orch-web: --root requires a path");
                    return 2;
                };
                root = Some(PathBuf::from(value));
            }
            "--port" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("orch-web: --port requires an integer");
                    return 2;
                };
                match value.parse::<u16>() {
                    Ok(value) => port = value,
                    Err(_) => {
                        eprintln!("orch-web: invalid port");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!("orch-web: unknown argument {other}");
                return 2;
            }
        }
        index += 1;
    }
    let root = match root {
        Some(root) => root,
        None => match std::env::current_dir() {
            Ok(root) => root,
            Err(_) => {
                eprintln!("orch-web: current directory unavailable");
                return 2;
            }
        },
    };
    let server = match WebServer::start(&root, port) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("orch-web: {error}");
            return 1;
        }
    };
    if !install_cli_signals() {
        eprintln!("orch-web: signal handling unavailable");
        drop(server);
        return 1;
    }
    let url = format!("http://{}/", server.address());
    println!("OpenOrch Web: {url}");
    let _ = std::io::stdout().flush();
    if !no_open && !open_browser(&url) {
        eprintln!("orch-web: browser could not be opened; service remains available");
    }
    while !CLI_STOP.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(25));
    }
    drop(server);
    0
}
