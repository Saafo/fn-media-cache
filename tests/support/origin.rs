//! Independent HTTP fixtures. Raw sockets preserve premature EOF and held-body tests.
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub const AUTH: &str = "Bearer local-fixture-secret";
pub const TOKEN: &str = "native-test-session";
const KEY: &str = "NDzZTVxnRKP8Z0jXg1VAMonaG8akvh";
const SUFFIX: &str = "16CCEB3D-AB42-077D-36A1-F355324E4237";

pub fn data(size: usize, first: Option<u8>) -> Vec<u8> {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    let mut bytes =
        DATA.get_or_init(|| (0..8 * 1024 * 1024).map(|i| i as u8).collect())[..size].to_vec();
    if let Some(first) = first {
        bytes[0] = first;
    }
    bytes
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Generic,
    Native,
    Catalog,
    Privacy,
    Route,
    Recovery,
    Task,
}
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub range: Option<String>,
    pub if_range: Option<String>,
}
#[derive(Default)]
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    pub fn reset(&self) {
        *self.open.lock().unwrap() = false;
    }
    pub fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self, stop: &AtomicBool) {
        let mut open = self.open.lock().unwrap();
        while !*open && !stop.load(Ordering::Relaxed) {
            open = self
                .changed
                .wait_timeout(open, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}
#[derive(Clone)]
pub struct State {
    pub mode: Mode,
    pub allowed: bool,
    pub block: bool,
    pub revoke: bool,
    pub version: u64,
    pub etag: String,
    pub interrupt: HashSet<String>,
    pub slow: HashSet<String>,
    pub ignore_range: HashSet<String>,
    pub revoked_tokens: HashSet<String>,
    pub blocked: HashSet<String>,
    pub permanent: HashSet<String>,
    pub watched: HashSet<String>,
    pub delay: Duration,
    pub corrupt: bool,
    pub down: bool,
    pub diagnostic_status: Option<u16>,
    pub health_response: Option<Value>,
    pub hold_health: bool,
    pub calls: Vec<Call>,
    pub writes: Vec<(String, String, Vec<u8>)>,
    pub hold: Arc<Gate>,
    pub held: Arc<AtomicBool>,
}
impl State {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            allowed: true,
            block: false,
            revoke: false,
            version: if mode == Mode::Route { 1000 } else { 1 },
            etag: "\"v1\"".into(),
            interrupt: HashSet::new(),
            slow: HashSet::new(),
            ignore_range: HashSet::new(),
            revoked_tokens: HashSet::new(),
            blocked: HashSet::new(),
            permanent: HashSet::new(),
            watched: HashSet::new(),
            delay: Duration::ZERO,
            corrupt: false,
            down: false,
            diagnostic_status: None,
            health_response: None,
            hold_health: false,
            calls: vec![],
            writes: vec![],
            hold: Arc::new(Gate::default()),
            held: Arc::new(AtomicBool::new(false)),
        }
    }
}
pub struct Origin {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Origin {
    pub fn new(mode: Mode) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(State::new(mode)));
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_state, worker_stop) = (state.clone(), stop.clone());
        let thread = thread::spawn(move || {
            let mut connections = Vec::new();
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (state, stop) = (worker_state.clone(), worker_stop.clone());
                        connections.push(thread::spawn(move || {
                            let _ = serve(stream, state, &stop);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
                let mut i = 0;
                while i < connections.len() {
                    if connections[i].is_finished() {
                        connections.swap_remove(i).join().unwrap();
                    } else {
                        i += 1;
                    }
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            url,
            state,
            stop,
            thread: Some(thread),
        }
    }
    pub fn change(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
    }
    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }
    pub fn interrupt(&self, name: &str) {
        self.change(|s| {
            s.interrupt.insert(name.into());
        });
    }
    pub fn hold(&self) {
        let s = self.snapshot();
        s.hold.reset();
        s.held.store(false, Ordering::Relaxed);
    }
    pub fn release(&self) {
        self.snapshot().hold.release();
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.release();
        if let Some(worker) = self.thread.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

struct Request {
    method: String,
    target: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
impl Request {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map(String::as_str).unwrap_or("")
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn signed(&self, query: bool) -> bool {
        let fields: HashMap<_, _> = self
            .header("authx")
            .split('&')
            .filter_map(|p| p.split_once('='))
            .collect();
        let (Some(nonce), Some(timestamp), Some(sign)) = (
            fields.get("nonce"),
            fields.get("timestamp"),
            fields.get("sign"),
        ) else {
            return false;
        };
        let body = if self.method == "GET" {
            if query {
                let url =
                    reqwest::Url::parse(&format!("http://fixture.invalid{}", self.target)).unwrap();
                let mut pairs: Vec<_> = url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                pairs.sort();
                pairs
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("&")
            } else {
                String::new()
            }
        } else {
            String::from_utf8_lossy(&self.body).into_owned()
        };
        let expected = md5(&format!(
            "{KEY}_{}_{nonce}_{timestamp}_{}_{SUFFIX}",
            self.path,
            md5(&body)
        ));
        *sign == expected
    }
}
fn md5(text: &str) -> String {
    format!("{:x}", Md5::digest(text.as_bytes()))
}
fn headers(
    stream: &mut TcpStream,
    status: u16,
    values: &[(&str, String)],
    length: usize,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {length}\r\n"
    )?;
    for (key, value) in values {
        write!(stream, "{key}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.flush()
}
fn json_reply(stream: &mut TcpStream, status: u16, value: Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(&value).unwrap();
    headers(
        stream,
        status,
        &[("Content-Type", "application/json".into())],
        body.len(),
    )?;
    stream.write_all(&body)
}
fn empty(stream: &mut TcpStream, status: u16) -> std::io::Result<()> {
    headers(stream, status, &[], 0)
}
fn allowed(request: &Request, s: &State) -> bool {
    let token = request.header("authorization");
    if !s.allowed || s.revoked_tokens.contains(token) {
        return false;
    }
    if s.mode == Mode::Recovery {
        return ["old-session", "renewed-session", "other-account"].contains(&token);
    }
    token == TOKEN || (s.mode == Mode::Native || s.mode == Mode::Task) && token == "another-session"
}
fn size(mode: Mode, guid: &str) -> usize {
    match mode {
        Mode::Catalog | Mode::Privacy => {
            if guid == "two" || guid == "space" {
                2 * 1024 * 1024
            } else {
                512 * 1024
            }
        }
        Mode::Route => 2 * 1024 * 1024,
        Mode::Recovery => 1024 * 1024,
        _ => {
            if guid == "small" {
                512 * 1024
            } else {
                8 * 1024 * 1024
            }
        }
    }
}
pub fn native_data(mode: Mode, guid: &str, version: u64) -> Vec<u8> {
    data(
        size(mode, guid),
        match mode {
            Mode::Native | Mode::Task => Some(version as u8),
            _ => None,
        },
    )
}
const ITEMS: &[(&str, &str, &str, u64, u64)] = &[
    ("library", "MediaDB", "", 0, 0),
    ("show", "TV", "library", 0, 0),
    ("season-one", "Season", "show", 1, 0),
    ("season-two", "Season", "show", 2, 0),
    ("specials", "Season", "show", 0, 0),
    ("one", "Episode", "season-one", 1, 1),
    ("two", "Episode", "season-one", 1, 2),
    ("three", "Episode", "season-one", 1, 3),
    ("four", "Episode", "season-two", 2, 1),
    ("special", "Episode", "specials", 0, 1),
    ("alias-one", "Movie", "library", 0, 0),
    ("alias-two", "Movie", "library", 0, 0),
    ("space", "Movie", "library", 0, 0),
];
pub fn media(guid: &str) -> &str {
    if guid.starts_with("alias-") {
        "shared"
    } else {
        guid
    }
}
fn item(guid: &str, s: &State) -> Value {
    let (_, kind, parent, season, episode) = ITEMS
        .iter()
        .find(|i| i.0 == guid)
        .expect("known catalog item");
    let watched = u8::from(s.watched.contains(guid));
    json!({"guid":guid,"type":kind,"parent_guid":parent,"title":format!("剧集 {guid}"),"season_number":season,"episode_number":episode,"watched":watched,"is_watched":watched,"watched_ts":999999})
}
fn serve(
    mut stream: TcpStream,
    state: Arc<Mutex<State>>,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    if reader.read_line(&mut first)? == 0 {
        return Ok(());
    }
    let fields: Vec<_> = first.split_whitespace().collect();
    if fields.len() < 2 {
        return Ok(());
    }
    let mut request = Request {
        method: fields[0].into(),
        target: fields[1].into(),
        path: fields[1].split('?').next().unwrap().into(),
        headers: HashMap::new(),
        body: vec![],
    };
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            request
                .headers
                .insert(k.to_ascii_lowercase(), v.trim().into());
        }
    }
    let length: usize = request.header("content-length").parse().unwrap_or(0);
    assert!(length <= 65536);
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    state.lock().unwrap().calls.push(Call {
        method: request.method.clone(),
        path: request.path.clone(),
        range: request.headers.get("range").cloned(),
        if_range: request.headers.get("if-range").cloned(),
    });
    let s = state.lock().unwrap().clone();
    let generic =
        s.mode == Mode::Generic || s.mode == Mode::Task && !request.path.starts_with("/v/");
    if generic {
        return generic_reply(&mut stream, &request, &state, &s, stop);
    }
    let r = &request;
    let catalog = s.mode == Mode::Catalog || s.mode == Mode::Privacy;
    if r.method == "HEAD" {
        return empty(&mut stream, 501);
    }
    if r.method == "DELETE"
        || r.method == "POST"
            && (r.path.ends_with("/play/record")
                || r.path.ends_with("/item/watched")
                || r.path.ends_with("/playing")
                || r.path.ends_with("/play/play"))
    {
        state
            .lock()
            .unwrap()
            .writes
            .push((r.method.clone(), r.path.clone(), r.body.clone()));
        return json_reply(&mut stream, 200, json!({"code":0,"data":true}));
    }
    if r.path == "/v/api/v1/session-context" {
        return json_reply(
            &mut stream,
            if allowed(r, &s) { 200 } else { 401 },
            json!({"code":if allowed(r,&s){0}else{-1}}),
        );
    }
    if !allowed(r, &s) {
        return json_reply(&mut stream, 401, json!({"code":-1}));
    }
    if r.path.starts_with("/v/api/v1/media/range/") {
        return media_reply(&mut stream, r, &state, &s, stop);
    }
    if r.path == "/v/api/v1/play/list" {
        return json_reply(
            &mut stream,
            200,
            json!({"code":0,"data":[{"guid":"cloud-only","ts":900}]}),
        );
    }
    // Playback setup normally arrives already authenticated by the app.
    if s.mode == Mode::Recovery && r.path == "/v/api/v1/play/info" {
        let request = r.json();
        let guid = request["item_guid"].as_str().unwrap();
        let media = request["media_guid"].as_str().unwrap_or(guid);
        return json_reply(
            &mut stream,
            200,
            json!({"code":0,"data":{
                "guid":guid,"media_guid":media,"video_guid":"video-one","ts":900,
                "play_config":{"skip_opening":5},
                "item":{"guid":guid,"type":"Episode","title":"Synthetic episode","posters":"fixture-poster","watched_ts":900,"duration":1000,"private":"must-not-store"}
            }}),
        );
    }
    if !r.signed(catalog) {
        return json_reply(
            &mut stream,
            if catalog { 401 } else { 200 },
            json!({"code":-1,"data":null}),
        );
    }
    let body = r.json();
    let result = if r.path == "/v/api/v1/user/info" {
        let name = if s.mode == Mode::Recovery {
            if r.header("authorization") == "other-account" {
                "account-b"
            } else {
                "account-a"
            }
        } else if s.mode == Mode::Route {
            "route-fixture"
        } else if catalog {
            "fixture"
        } else {
            "fixture-user"
        };
        json!({"username":name})
    } else if r.path == "/v/api/v1/stream" {
        let guid = body["media_guid"].as_str().unwrap();
        let version = if catalog { 1000 } else { s.version };
        json!({"file_stream":{"guid":guid,"path":if s.mode==Mode::Route {"/fixture/movie.mp4".to_owned()}else if catalog {format!("/fixture/{guid}")}else{format!("/fixture/{guid}.mp4")},"size":size(s.mode,guid),"type":1,"can_play":1,"timestamp":if catalog||s.mode==Mode::Route{version}else{1000+version},"create_time":if s.mode==Mode::Route{1}else{1000},"update_time":if catalog||s.mode==Mode::Route{version}else{2000+version},"file_birth_time":if s.mode==Mode::Route{1}else{1000}}})
    } else if catalog && r.path == "/v/api/v1/play/info" {
        let guid = body["item_guid"].as_str().unwrap();
        json!({"guid":guid,"media_guid":media(guid),"item":item(guid,&s)})
    } else if catalog && r.path == "/v/api/v1/mediadb/list" {
        json!([item("library", &s)])
    } else if catalog && r.path == "/v/api/v1/search/list" {
        let url = reqwest::Url::parse(&format!("http://fixture.invalid{}", r.target)).unwrap();
        assert!(url.query_pairs().any(|(k, v)| k == "q" && v == "剧集"));
        json!([item("show", &s)])
    } else if catalog && r.path.starts_with("/v/api/v1/item/") {
        item(r.path.rsplit('/').next().unwrap(), &s)
    } else if catalog && r.path.starts_with("/v/api/v1/season/list/") {
        json!([
            item("specials", &s),
            item("season-two", &s),
            item("season-one", &s)
        ])
    } else if catalog && r.path.starts_with("/v/api/v1/episode/list/") {
        let parent = r.path.rsplit('/').next().unwrap();
        Value::Array(
            ITEMS
                .iter()
                .rev()
                .filter(|i| i.1 == "Episode" && i.2 == parent)
                .map(|i| item(i.0, &s))
                .collect(),
        )
    } else if catalog && r.path == "/v/api/v1/item/list" {
        let parent = body["parent_guid"]
            .as_str()
            .or(body["ancestor_guid"].as_str())
            .unwrap_or("");
        let rows: Vec<_> = ITEMS
            .iter()
            .filter(|i| i.2 == parent)
            .map(|i| item(i.0, &s))
            .collect();
        json!({"total":rows.len(),"list":rows})
    } else {
        return json_reply(&mut stream, 404, json!({"code":-2}));
    };
    json_reply(&mut stream, 200, json!({"code":0,"data":result}))
}
fn span(value: &str, total: usize) -> (usize, usize) {
    if value.is_empty() {
        return (0, total - 1);
    }
    let (start, end) = value
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    if start.is_empty() {
        return (
            total.saturating_sub(end.parse::<usize>().unwrap()),
            total - 1,
        );
    }
    (
        start.parse().unwrap(),
        if end.is_empty() {
            total - 1
        } else {
            end.parse::<usize>().unwrap().min(total - 1)
        },
    )
}
fn media_reply(
    stream: &mut TcpStream,
    r: &Request,
    state: &Arc<Mutex<State>>,
    s: &State,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    let guid = r.path.rsplit('/').next().unwrap();
    let range = r.header("range");
    if s.down
        || s.block && range != "bytes=0-0"
        || s.mode == Mode::Recovery && range.ends_with('-') && s.blocked.contains(guid)
    {
        return json_reply(stream, 503, json!({"code":-1}));
    }
    if s.mode == Mode::Recovery && range.ends_with('-') && s.permanent.contains(guid) {
        return json_reply(stream, 404, json!({"code":-1}));
    }
    let mut bytes = native_data(s.mode, guid, s.version);
    if s.corrupt {
        bytes[0] = b'x';
    }
    let (start, end) = span(range, bytes.len());
    if start > end {
        return empty(stream, 416);
    }
    headers(
        stream,
        if range.is_empty() { 200 } else { 206 },
        &[
            ("Content-Type", "video/mp4".into()),
            (
                "Content-Range",
                format!("bytes {start}-{end}/{}", bytes.len()),
            ),
        ],
        end - start + 1,
    )?;
    if (s.mode == Mode::Catalog || s.mode == Mode::Privacy) && r.target.contains('?')
        || s.mode == Mode::Task && range == "bytes=65536-1048575"
    {
        s.held.store(true, Ordering::Relaxed);
        s.hold.wait(stop);
    }
    let interrupted = range == "bytes=0-" && state.lock().unwrap().interrupt.remove(guid);
    let end = if interrupted {
        (start + if s.mode == Mode::Route { 131071 } else { 65535 }).min(end)
    } else {
        end
    };
    for chunk in bytes[start..=end].chunks(65536) {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !s.delay.is_zero() {
            thread::sleep(s.delay);
        }
        stream.write_all(chunk)?;
        stream.flush()?;
    }
    if interrupted {
        stream.shutdown(Shutdown::Both)?;
    }
    Ok(())
}
fn generic_reply(
    stream: &mut TcpStream,
    r: &Request,
    state: &Arc<Mutex<State>>,
    s: &State,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    if r.path == "/health"
        && let Some(value) = &s.health_response
    {
        if s.hold_health {
            s.hold.wait(stop);
        }
        return json_reply(stream, 200, value.clone());
    }
    if r.method == "HEAD"
        && r.path == "/"
        && let Some(status) = s.diagnostic_status
    {
        return headers(
            stream,
            status,
            &[("Location", "http://127.0.0.1:1/not-followed".into())],
            0,
        );
    }
    if r.method == "POST" && r.path == "/v/api/v1/play/record" {
        state
            .lock()
            .unwrap()
            .writes
            .push((r.method.clone(), r.path.clone(), r.body.clone()));
        return json_reply(stream, 200, json!({"code":0,"data":true}));
    }
    if ![AUTH, "Bearer another-account"].contains(&r.header("authorization"))
        || s.revoke && r.header("authorization") == AUTH
    {
        return empty(stream, 401);
    }
    let head = r.method == "HEAD";
    if head && r.path == "/nohead.mp4" {
        return empty(stream, 501);
    }
    if r.path == "/library" {
        return json_reply(stream, 200, json!({"title":"Fixture library"}));
    }
    if r.path == "/redirect" {
        return headers(
            stream,
            302,
            &[(
                "Location",
                format!(
                    "http://{}/movie.mp4?signature=fixture-secret-query",
                    r.header("host")
                ),
            )],
            0,
        );
    }
    if r.path == "/external" {
        return headers(
            stream,
            302,
            &[("Location", "https://example.invalid/movie.mp4".into())],
            0,
        );
    }
    if !r.path.ends_with(".mp4") {
        return empty(stream, 404);
    }
    if s.block && !head {
        return empty(stream, 503);
    }
    let total = if r.path == "/oversize.mp4" {
        1024 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    let range = if s.ignore_range.contains(&r.path) {
        ""
    } else {
        r.header("range")
    };
    if range.contains(',') {
        return empty(stream, 400);
    }
    let (start, end) = span(range, total);
    let mut values = vec![
        ("Content-Type", "video/mp4".into()),
        ("Accept-Ranges", "bytes".into()),
    ];
    if r.path != "/novalidator.mp4" {
        values.push(("ETag", s.etag.clone()));
    }
    if !range.is_empty() {
        values.push(("Content-Range", format!("bytes {start}-{end}/{total}")));
    }
    headers(
        stream,
        if range.is_empty() { 200 } else { 206 },
        &values,
        end - start + 1,
    )?;
    if head {
        return Ok(());
    }
    let interrupted =
        r.header("range").is_empty() && state.lock().unwrap().interrupt.remove(&r.path);
    let bytes = data(8 * 1024 * 1024, None);
    let end = if interrupted {
        (start + 256 * 1024 - 1).min(end)
    } else {
        end
    }
    .min(bytes.len() - 1);
    for chunk in bytes[start..=end].chunks(16384) {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        stream.write_all(chunk)?;
        stream.flush()?;
        if s.slow.contains(&r.path) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    if interrupted {
        stream.shutdown(Shutdown::Both)?;
    }
    Ok(())
}
