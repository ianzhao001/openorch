use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);
static SOCKET_SEQ: AtomicU64 = AtomicU64::new(0);
static FAKE_DIRS: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

pub enum MulticaScript {
    TerminalFrame,
    TerminalFrameNoNewline,
    ZeroFrameEof,
    FramesThenEof,
    Silent,
    MalformedPayloadsText,
}

pub struct FakeMultica {
    dir: PathBuf,
    sock_path: PathBuf,
    request: Arc<Mutex<Option<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeMultica {
    pub fn spawn(tag: &str, script: MulticaScript) -> Self {
        let tag = short_tag(tag);
        let dir = test_tmp_root().join(format!("b195-{tag}-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("清理同进程遗留的 B195 假服务目录");
        }
        fs::create_dir_all(&dir).expect("创建 B195 假服务目录");

        // 试合并克隆内连 test-tmp 前缀本身都会超过 Darwin sun_path 上限，
        // 所以只有 socket 端点落系统临时目录；git cwd 仍留在上面的仓内目录。
        let sock_path = bounded_socket_path(tag.as_str());
        let sock_path_len = sock_path.as_os_str().as_bytes().len();
        assert!(
            sock_path_len < 104,
            "UNIX socket 路径超过 Darwin 上限: bytes={sock_path_len} limit=104 path={}",
            sock_path.display(),
        );
        let listener = UnixListener::bind(&sock_path).expect("绑定 B195 假 multica socket");
        listener
            .set_nonblocking(true)
            .expect("将 B195 假 multica listener 设为 nonblocking");
        fake_dirs()
            .lock()
            .expect("登记 B195 假服务目录锁")
            .insert(sock_path.clone(), dir.clone());

        let request = Arc::new(Mutex::new(None));
        let request_for_thread = Arc::clone(&request);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve_once(stream, script, &request_for_thread, &stop_for_thread);
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("B195 假 multica accept 失败: {error}"),
                }
            }
        });

        Self {
            dir,
            sock_path,
            request,
            stop,
            thread: Some(thread),
        }
    }

    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    pub fn request(&self) -> Option<serde_json::Value> {
        self.request.lock().expect("读取假服务请求锁").clone()
    }
}

impl Drop for FakeMultica {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // listener 是 nonblocking 的；这次连接只是进一步缩短尚未 accept 时的收束。
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        fake_dirs()
            .lock()
            .expect("注销 B195 假服务目录锁")
            .remove(&self.sock_path);
        let _ = fs::remove_file(&self.sock_path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub struct ScriptOutcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run_wake_multica(
    script: &Path,
    sock: &Path,
    message: &str,
    timeout_secs: &str,
) -> ScriptOutcome {
    let (git_dir, remove_after_run) = command_git_dir(sock);
    fs::create_dir_all(&git_dir).expect("创建 B195 临时 git 仓目录");
    let init = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&git_dir)
        .status()
        .expect("执行 git init 创建脚本 cwd");
    assert!(init.success(), "git init 创建脚本 cwd 失败: {git_dir:?}");

    // 负向变异专用入口：cargo 进程启动前由外层命令设置，避免改 seed；正常门
    // 没有该变量，严格使用调用者传入的冻结脚本路径。
    let effective_script = std::env::var_os("ORCH_B195_MUTANT_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| script.to_path_buf());
    let output = Command::new("/bin/sh")
        .arg(&effective_script)
        .arg(message)
        .current_dir(&git_dir)
        .env("ORCH_MULTICA_SOCK", sock)
        .env("ORCH_MULTICA_TIMEOUT", timeout_secs)
        .output()
        .expect("运行 wake-multica.sh");

    if remove_after_run {
        let _ = fs::remove_dir_all(&git_dir);
    }

    ScriptOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub fn retry_transient_read_until_with_clock<T, N, R>(
    deadline: Instant,
    mut now: N,
    mut read_once: R,
) -> io::Result<T>
where
    N: FnMut() -> Instant,
    R: FnMut(Duration) -> io::Result<T>,
{
    loop {
        let current = now();
        if current >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "wake-multica request read exceeded the overall deadline",
            ));
        }
        let remaining = deadline.duration_since(current);
        match read_once(remaining) {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

fn serve_once(
    mut stream: UnixStream,
    script: MulticaScript,
    request: &Mutex<Option<serde_json::Value>>,
    stop: &AtomicBool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut request_line = String::new();
    let mut reader = BufReader::new(&stream);
    retry_transient_read_until_with_clock(deadline, Instant::now, |remaining| {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .expect("设置假服务请求读取上限");
        reader.read_line(&mut request_line)
    })
    .expect("读取 wake-multica 单行请求");
    if let Ok(value) = serde_json::from_str(&request_line) {
        *request.lock().expect("记录假服务请求锁") = Some(value);
    }

    match script {
        MulticaScript::TerminalFrame => {
            write_all(
                &mut stream,
                b"{\"type\":\"delta\",\"text\":\"working\"}\n{\"type\":\"status\",\"value\":\"done\"}\n{\"payloads\":[],\"ok\":true}\n",
            );
        }
        MulticaScript::TerminalFrameNoNewline => {
            write_all(
                &mut stream,
                // 首帧的 nested key 证明 stdout 透传；终帧把顶层 key 写成等价的
                // JSON unicode escape，使第 74 行 substring 快径不命中，必须由
                // EOF 后的残余缓冲清算识别。
                b"{\"type\":\"delta\",\"meta\":{\"payloads\":\"marker\"}}\n{\"payl\\u006fads\":[],\"ok\":true}",
            );
        }
        MulticaScript::ZeroFrameEof => {}
        MulticaScript::FramesThenEof => {
            write_all(&mut stream, b"{\"type\":\"delta\",\"text\":\"partial\"}\n");
        }
        MulticaScript::Silent => {
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
        }
        MulticaScript::MalformedPayloadsText => {
            write_all(&mut stream, b"not-json but contains \"payloads\"");
        }
    }
}

fn write_all(stream: &mut UnixStream, bytes: &[u8]) {
    stream.write_all(bytes).expect("假服务写响应");
    stream.flush().expect("假服务 flush 响应");
}

fn short_tag(tag: &str) -> String {
    let short: String = tag
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(5)
        .collect();
    if short.is_empty() {
        "case".to_owned()
    } else {
        short
    }
}

fn short_socket_name(tag: &str) -> String {
    let mut hasher = DefaultHasher::new();
    tag.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    SOCKET_SEQ.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    format!("orch-b195-{:06x}.sock", hasher.finish() & 0x00ff_ffff)
}

fn bounded_socket_path(tag: &str) -> PathBuf {
    let name = short_socket_name(tag);
    let preferred = std::env::temp_dir().join(&name);
    let preferred_len = preferred.as_os_str().as_bytes().len();
    if preferred_len < 104 {
        return preferred;
    }

    let fallback = Path::new("/tmp").join(&name);
    let fallback_len = fallback.as_os_str().as_bytes().len();
    assert!(
        fallback_len < 104,
        "UNIX socket 路径均超过 Darwin 上限: tmpdir_bytes={preferred_len} fallback_bytes={fallback_len} limit=104 tmpdir_path={} fallback_path={}",
        preferred.display(),
        fallback.display(),
    );
    fallback
}

fn fake_dirs() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    FAKE_DIRS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn command_git_dir(sock: &Path) -> (PathBuf, bool) {
    if let Some(dir) = fake_dirs()
        .lock()
        .expect("读取 B195 假服务目录锁")
        .get(sock)
        .cloned()
    {
        // 与 FakeMultica 同寿命，供请求契约用例在 run 返回后继续检查 cwd。
        return (dir.join("git"), false);
    }

    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    (
        test_tmp_root().join(format!("b195-run-{}-{seq}", std::process::id())),
        true,
    )
}

fn test_tmp_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("orch-host manifest 上溯两级应为 orch/")
        .join("target/test-tmp")
}
