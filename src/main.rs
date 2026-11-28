use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UI_HTML: &str = include_str!("ui.html");
const FALLBACK_PRELOAD: &str = r#"(function(){const p=new URLSearchParams(location.search);const t=p.get('token');window.sidecar={conversationId:p.get('conversationId')||'',fetch:(u,o={})=>{const h=new Headers(o.headers||{});if(o.method&&o.method!=='GET')h.set('Content-Type','application/json');if(t)h.set('X-Sidecar-Token',t);return fetch(u,{...o,headers:h});},ui:{toggleAuxPane:r=>window.parent&&window.parent!==window&&window.parent.postMessage({type:'toggle-aux-pane',payload:r},'*'),toggleConversation:(c,o)=>window.parent&&window.parent!==window&&window.parent.postMessage({type:'toggle-conversation',payload:o&&typeof o==='object'?{conversationId:c,...o}:{conversationId:c}},'*')}};})();"#;

const MAX_AUTO_COMPACT_TOKENS: u64 = 500_000;
const MIN_AUTO_COMPACT_TOKENS: u64 = 10_000;
const MAX_STORED_CHATS: usize = 500;
const MAX_REVERT_CUTOFFS: usize = 200;
const MAX_ACTIVE_CONNECTIONS: usize = 32;
const MAX_NOTE_BYTES: usize = 256 * 1024;
const MAX_NOTE_CARDS: usize = 200;
const MAX_SEARCH_RESULTS: usize = 60;
const MAX_ALL_SEARCH_CHATS: usize = 500;
const MAX_SEARCH_TEXT_CHARS: usize = 12_000;
const SEARCH_CLOSE_SPAN_CHARS: usize = 160;
const MAX_BODY_BYTES: usize = 512 * 1024;
const DRAFT_CONV_ID: &str = "__draft__";

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct NoteCard {
    id: u64,
    text: String,
    pinned: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct NotesData {
    draft: String,
    cards: Vec<NoteCard>,
    next_id: u64,
}

impl NotesData {
    fn is_empty(&self) -> bool {
        self.draft.is_empty() && self.cards.is_empty()
    }

    fn to_json(&self) -> String {
        let mut out = String::from(r#"{"draft":""#);
        out.push_str(&json_escape(&self.draft));
        out.push_str(r#"","cards":["#);
        for (idx, card) in self.cards.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                r#"{{"id":{},"text":"{}","pinned":{}}}"#,
                card.id,
                json_escape(&card.text),
                card.pinned
            ));
        }
        out.push_str(&format!(r#"],"next_id":{}}}"#, self.next_id.max(1)));
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Settings {
    auto_compact_enabled: bool,
    auto_compact_threshold: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_compact_enabled: false,
            auto_compact_threshold: 250_000,
        }
    }
}

impl Settings {
    fn to_json(self) -> String {
        format!(
            r#"{{"auto_compact_enabled":{},"auto_compact_threshold":{}}}"#,
            self.auto_compact_enabled, self.auto_compact_threshold
        )
    }

    fn validate(self) -> Result<(), &'static str> {
        if self.auto_compact_threshold < MIN_AUTO_COMPACT_TOKENS
            || self.auto_compact_threshold > MAX_AUTO_COMPACT_TOKENS
        {
            return Err("Auto-compact threshold must be between 10K and 500K.");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RevertCutoff {
    bytes_len: u64,
    max_step: u64,
}

struct SettingsStore {
    chats: HashMap<String, Settings>,
    chat_order: Vec<String>,
    draft: Settings,
    draft_updated_at: u64,
    has_pending_draft: bool,
    cutoffs: HashMap<String, RevertCutoff>,
    cutoff_order: Vec<String>,
    update_checked_at: u64,
    latest_version: String,
    rev: u64,
    path: PathBuf,
}

impl SettingsStore {
    fn get_for_chat(&self, conv: &str) -> Settings {
        if is_draft_conv(conv) {
            return self.draft;
        }
        self.chats.get(conv).copied().unwrap_or_default()
    }

    fn insert_chat(&mut self, conv: String, settings: Settings) {
        if let Some(pos) = self.chat_order.iter().position(|k| k == &conv) {
            self.chat_order.remove(pos);
        } else if !self.chats.contains_key(&conv) && self.chats.len() >= MAX_STORED_CHATS {
            while self.chats.len() >= MAX_STORED_CHATS {
                if !self.chat_order.is_empty() {
                    let old = self.chat_order.remove(0);
                    self.chats.remove(&old);
                } else if let Some(any_key) = self.chats.keys().next().cloned() {
                    self.chats.remove(&any_key);
                } else {
                    break;
                }
            }
        }
        self.chat_order.push(conv.clone());
        self.chats.insert(conv, settings);
    }

    fn insert_cutoff(&mut self, conv: String, cutoff: RevertCutoff) {
        if let Some(pos) = self.cutoff_order.iter().position(|k| k == &conv) {
            self.cutoff_order.remove(pos);
        } else if !self.cutoffs.contains_key(&conv) && self.cutoffs.len() >= MAX_REVERT_CUTOFFS {
            while self.cutoffs.len() >= MAX_REVERT_CUTOFFS {
                if !self.cutoff_order.is_empty() {
                    let old = self.cutoff_order.remove(0);
                    self.cutoffs.remove(&old);
                } else if let Some(any_key) = self.cutoffs.keys().next().cloned() {
                    self.cutoffs.remove(&any_key);
                } else {
                    break;
                }
            }
        }
        self.cutoff_order.push(conv.clone());
        self.cutoffs.insert(conv, cutoff);
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct ContextBreakdown {
    system: u64,
    summary: u64,
    tools: u64,
    replies: u64,
    user: u64,
}

impl ContextBreakdown {
    fn total(self) -> u64 {
        self.system
            .saturating_add(self.summary)
            .saturating_add(self.tools)
            .saturating_add(self.replies)
            .saturating_add(self.user)
    }

    fn scale_to(&mut self, target: u64) {
        let cur = self.total();
        if cur == 0 || target == 0 {
            *self = Self::default();
            return;
        }
        if cur == target {
            return;
        }
        let scale = |v: u64| ((v as u128).saturating_mul(target as u128) / (cur as u128)) as u64;
        self.system = scale(self.system);
        self.summary = scale(self.summary);
        self.tools = scale(self.tools);
        self.replies = scale(self.replies);
        self.user = scale(self.user);
        let scaled = self.total();
        if target > scaled {
            let diff = target - scaled;
            let max_val = self
                .tools
                .max(self.system)
                .max(self.replies)
                .max(self.summary)
                .max(self.user);
            if self.tools == max_val {
                self.tools = self.tools.saturating_add(diff);
            } else if self.system == max_val {
                self.system = self.system.saturating_add(diff);
            } else if self.replies == max_val {
                self.replies = self.replies.saturating_add(diff);
            } else if self.summary == max_val {
                self.summary = self.summary.saturating_add(diff);
            } else {
                self.user = self.user.saturating_add(diff);
            }
        }
    }
}

#[derive(Default, Clone)]
struct ChatScanSummary {
    user_messages: u64,
    model_messages: u64,
    checkpoint_count: u64,
    last_checkpoint_step: u64,
    undo_step_index: Option<u64>,
    compact_in_flight: bool,
    first_message_at: String,
    last_message_at: String,
    has_context_data: bool,
    context_tokens: u64,
    latest_step_index: u64,
    has_delta: bool,
    delta_tokens: i64,
    just_compressed: bool,
    is_busy: bool,
    has_bg_work: bool,
    turn_finished: bool,
    active_quota_group: ModelGroup,
    breakdown: ContextBreakdown,
}

#[derive(Default, Clone)]
struct ChatScanState {
    offset: u64,
    cutoff_applied: Option<RevertCutoff>,
    steps: Vec<StepMetrics>,
    active_bg_tasks: Vec<String>,
    active_subagents: Vec<String>,
    user_messages: u64,
    model_messages: u64,
    checkpoint_count: u64,
    last_checkpoint_step: u64,
    last_non_compact_step: Option<u64>,
    undo_step_index: Option<u64>,
    compact_in_flight: bool,
    first_message_at: String,
    last_message_at: String,
    has_context_data: bool,
    context_tokens: u64,
    latest_step_index: u64,
    session_baseline_tokens: Option<u64>,
    has_delta: bool,
    delta_tokens: i64,
    just_compressed: bool,
    is_busy: bool,
    turn_finished: bool,
    active_quota_group: ModelGroup,
    breakdown: ContextBreakdown,
    breakdown_initialized: bool,
    prev_planner_output: u64,
    pending_user_bytes: u64,
    pending_tool_bytes: u64,
}

impl ChatScanState {
    fn to_summary(&self) -> ChatScanSummary {
        let has_bg_work = !self.active_bg_tasks.is_empty() || !self.active_subagents.is_empty();
        ChatScanSummary {
            user_messages: self.user_messages,
            model_messages: self.model_messages,
            checkpoint_count: self.checkpoint_count,
            last_checkpoint_step: self.last_checkpoint_step,
            undo_step_index: if has_bg_work { None } else { self.undo_step_index },
            compact_in_flight: self.compact_in_flight,
            first_message_at: self.first_message_at.clone(),
            last_message_at: self.last_message_at.clone(),
            has_context_data: self.has_context_data,
            context_tokens: self.context_tokens,
            latest_step_index: self.latest_step_index,
            has_delta: self.has_delta,
            delta_tokens: self.delta_tokens,
            just_compressed: self.just_compressed,
            is_busy: self.is_busy || has_bg_work,
            has_bg_work,
            turn_finished: self.turn_finished && !has_bg_work,
            active_quota_group: self.active_quota_group,
            breakdown: self.breakdown,
        }
    }
}

struct ActiveConnGuard(Arc<AtomicUsize>);

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum StepKind {
    #[default]
    Other,
    UserInput,
    PlannerResponse,
    Checkpoint,
    SystemMessage,
    Generic,
    ErrorMessage,
}

impl StepKind {
    fn from_str(s: &str) -> Self {
        match s {
            "USER_INPUT" => Self::UserInput,
            "PLANNER_RESPONSE" => Self::PlannerResponse,
            "CHECKPOINT" => Self::Checkpoint,
            "SYSTEM_MESSAGE" => Self::SystemMessage,
            "GENERIC" => Self::Generic,
            "ERROR_MESSAGE" => Self::ErrorMessage,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum ModelGroup {
    #[default]
    None,
    Gemini,
    Other,
}

impl ModelGroup {
    fn from_model_name(s: &str) -> Self {
        let lower = s.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Self::None;
        }
        if lower.contains("gemini")
            || lower == "flash"
            || lower.starts_with("flash-")
            || lower.starts_with("flash_")
        {
            Self::Gemini
        } else {
            Self::Other
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Gemini => "gemini",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum StepStatus {
    #[default]
    Other,
    Done,
    Running,
    FailedOrCancelled,
}

impl StepStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "DONE" => Self::Done,
            "RUNNING" | "IN_PROGRESS" | "PENDING" => Self::Running,
            "ERROR" | "CANCELLED" | "CANCELED" | "FAILED" | "ABORTED" => Self::FailedOrCancelled,
            _ => Self::Other,
        }
    }
}

#[derive(Default, Clone)]
struct LsLiveCache {
    fetched_at: Option<Instant>,
    running_convs: HashMap<String, bool>,
    bg_convs: Vec<String>,
}

#[derive(Default, Clone)]
struct QuotaCache {
    fetched_at: Option<Instant>,
    last_force_at: Option<Instant>,
    ok: bool,
    json: String,
}

#[derive(Default, Clone, Copy)]
struct LsLiveConvStatus {
    known: bool,
    is_running: bool,
    has_bg_task: bool,
}

#[derive(Clone)]
struct AppState {
    app_data: PathBuf,
    notes_dir: PathBuf,
    token: Option<String>,
    default_conv: Option<String>,
    ls_address: Option<String>,
    csrf_token: Option<String>,
    preload_js: String,
    settings: Arc<Mutex<SettingsStore>>,
    notes_lock: Arc<Mutex<()>>,
    cache: Arc<Mutex<HashMap<String, (String, String)>>>,
    scans: Arc<Mutex<HashMap<String, ChatScanState>>>,
    chat_titles: Arc<Mutex<(u64, HashMap<String, String>)>>,
    ls_live: Arc<Mutex<LsLiveCache>>,
    quota_cache: Arc<Mutex<QuotaCache>>,
    last_prune_at: Arc<Mutex<Option<Instant>>>,
    search_gen: Arc<AtomicUsize>,
    done_sound_mp3: Arc<OnceLock<Vec<u8>>>,
    active_conns: Arc<AtomicUsize>,
}

#[derive(Default, Clone)]
struct StepMetrics {
    step_index: u64,
    kind: StepKind,
    status: StepStatus,
    model_group: ModelGroup,
    created_at: String,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    content_bytes: u64,
    has_tokens: bool,
    has_content: bool,
    has_tool_calls: bool,
    has_error: bool,
    is_compact_cmd: bool,
    bg_task_started: Option<String>,
    bg_task_ended: Option<String>,
    subagent_started: Vec<String>,
    subagent_ended: Option<String>,
    clears_all_subagents: bool,
}

impl StepMetrics {
    fn is_failed(&self) -> bool {
        self.has_error
            || self.status == StepStatus::FailedOrCancelled
            || self.kind == StepKind::ErrorMessage
    }
}

fn main() {
    let port: u16 = env::var("ANTIGRAVITY_SIDECAR_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18765);

    let token = env::var("ANTIGRAVITY_SIDECAR_UI_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());

    let default_conv = env::var("ANTIGRAVITY_CONVERSATION_ID")
        .ok()
        .filter(|s| !s.is_empty());

    let ls_address = env::var("ANTIGRAVITY_LS_ADDRESS")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let csrf_token = env::var("ANTIGRAVITY_CSRF_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let app_data = resolve_app_data();
    let data_dir = resolve_data_dir(&app_data);
    let settings_path = data_dir.join("settings.json");
    let notes_dir = data_dir.join("notes");
    let loaded = load_settings_store_data(&settings_path);
    let draft_note_exists = note_path_for(&notes_dir, DRAFT_CONV_ID).is_some_and(|p| p.exists());
    let has_pending_draft = loaded.draft != Settings::default() || draft_note_exists;
    let preload_js = load_preload_js();

    let state = Arc::new(AppState {
        app_data,
        notes_dir,
        token,
        default_conv,
        ls_address,
        csrf_token,
        preload_js,
        settings: Arc::new(Mutex::new(SettingsStore {
            chats: loaded.chats,
            chat_order: loaded.chat_order,
            draft: loaded.draft,
            draft_updated_at: loaded.draft_updated_at,
            has_pending_draft,
            cutoffs: loaded.cutoffs,
            cutoff_order: loaded.cutoff_order,
            update_checked_at: loaded.update_checked_at,
            latest_version: loaded.latest_version,
            rev: 1,
            path: settings_path,
        })),
        notes_lock: Arc::new(Mutex::new(())),
        cache: Arc::new(Mutex::new(HashMap::new())),
        scans: Arc::new(Mutex::new(HashMap::new())),
        chat_titles: Arc::new(Mutex::new((0, HashMap::new()))),
        ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
        quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
        last_prune_at: Arc::new(Mutex::new(None)),
        search_gen: Arc::new(AtomicUsize::new(0)),
        done_sound_mp3: Arc::new(OnceLock::new()),
        active_conns: Arc::new(AtomicUsize::new(0)),
    });

    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("Failed to bind 127.0.0.1:{port}: {e}"));

    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    println!("[sidecar] listening on 127.0.0.1:{bound_port}");
    let _ = std::io::stdout().flush();

    for stream in listener.incoming().flatten() {
        let mut cur = state.active_conns.load(Ordering::Acquire);
        let acquired = loop {
            if cur >= MAX_ACTIVE_CONNECTIONS {
                break false;
            }
            match state.active_conns.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break true,
                Err(next) => cur = next,
            }
        };
        if !acquired {
            continue;
        }
        let st = Arc::clone(&state);
        thread::spawn(move || {
            let _guard = ActiveConnGuard(Arc::clone(&st.active_conns));
            handle_client(stream, &st);
        });
    }
}

fn home_dir() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_app_data() -> PathBuf {
    if let Ok(exec_data) = env::var("ANTIGRAVITY_EXECUTABLE_DATA_DIR") {
        let mut p = PathBuf::from(exec_data);
        for _ in 0..8 {
            if p.file_name().and_then(|n| n.to_str()) == Some("sidecar_data") {
                if let Some(parent) = p.parent() {
                    return parent.to_path_buf();
                }
            }
            if !p.pop() {
                break;
            }
        }
    }
    home_dir().join(".gemini").join("antigravity")
}

fn resolve_data_dir(app_data: &Path) -> PathBuf {
    if let Ok(exec_data) = env::var("ANTIGRAVITY_EXECUTABLE_DATA_DIR") {
        if !exec_data.trim().is_empty() {
            return PathBuf::from(exec_data);
        }
    }
    app_data.join("sidecar_data").join("antimetro")
}

#[cfg(unix)]
fn set_unix_private_dir_perms(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_unix_private_dir_perms(_dir: &Path) {}

#[cfg(unix)]
fn set_unix_private_file_perms(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(file, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_unix_private_file_perms(_file: &Path) {}

fn atomic_write_file(path: &Path, bytes: &[u8], tmp_ext: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_unix_private_dir_perms(parent);
    }
    let tmp_path = path.with_extension(tmp_ext);
    fs::write(&tmp_path, bytes)?;
    set_unix_private_file_perms(&tmp_path);

    for attempt in 0..3 {
        if fs::rename(&tmp_path, path).is_ok() {
            set_unix_private_file_perms(path);
            return Ok(());
        }
        if attempt < 2 {
            thread::sleep(Duration::from_millis(15));
        }
    }

    let res = fs::write(path, bytes);
    if res.is_ok() {
        set_unix_private_file_perms(path);
    }
    let _ = fs::remove_file(&tmp_path);
    res
}

fn backup_corrupt_file(path: &Path) {
    let bak = path.with_extension("json.bak");
    let _ = fs::copy(path, &bak);
    set_unix_private_file_perms(&bak);
}

#[derive(Default)]
struct LoadedStoreData {
    chats: HashMap<String, Settings>,
    chat_order: Vec<String>,
    draft: Settings,
    draft_updated_at: u64,
    cutoffs: HashMap<String, RevertCutoff>,
    cutoff_order: Vec<String>,
    update_checked_at: u64,
    latest_version: String,
}

fn is_draft_conv(conv: &str) -> bool {
    conv.is_empty() || conv == DRAFT_CONV_ID
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn transcript_path(app_data: &Path, conv: &str) -> PathBuf {
    app_data
        .join("brain")
        .join(conv)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl")
}

fn load_settings_store_data(path: &Path) -> LoadedStoreData {
    let Ok(raw) = fs::read(path) else {
        return LoadedStoreData::default();
    };
    if trim_ascii(&raw).is_empty() {
        return LoadedStoreData::default();
    }
    match parse_settings_store_data(&raw) {
        Some(d) => d,
        None => {
            backup_corrupt_file(path);
            LoadedStoreData::default()
        }
    }
}

fn save_settings_store(store: &SettingsStore) -> std::io::Result<()> {
    let mut out = String::from("{\"chats\":{");
    let mut first_chat = true;
    for conv in &store.chat_order {
        if let Some(s) = store.chats.get(conv) {
            if !first_chat {
                out.push(',');
            }
            first_chat = false;
            out.push('"');
            out.push_str(&json_escape(conv));
            out.push_str("\":");
            out.push_str(&s.to_json());
        }
    }
    for (conv, s) in &store.chats {
        if !store.chat_order.iter().any(|k| k == conv) {
            if !first_chat {
                out.push(',');
            }
            first_chat = false;
            out.push('"');
            out.push_str(&json_escape(conv));
            out.push_str("\":");
            out.push_str(&s.to_json());
        }
    }
    out.push_str("},\"draft\":{\"auto_compact_enabled\":");
    out.push_str(if store.draft.auto_compact_enabled { "true" } else { "false" });
    out.push_str(",\"auto_compact_threshold\":");
    out.push_str(&store.draft.auto_compact_threshold.to_string());
    out.push_str(",\"updated_at\":");
    out.push_str(&store.draft_updated_at.to_string());
    out.push_str("},\"cutoffs\":{");
    let mut first_cutoff = true;
    for conv in &store.cutoff_order {
        if let Some(c) = store.cutoffs.get(conv) {
            if !first_cutoff {
                out.push(',');
            }
            first_cutoff = false;
            out.push('"');
            out.push_str(&json_escape(conv));
            out.push_str(&format!(
                "\":{{\"bytes_len\":{},\"max_step\":{}}}",
                c.bytes_len, c.max_step
            ));
        }
    }
    for (conv, c) in &store.cutoffs {
        if !store.cutoff_order.iter().any(|k| k == conv) {
            if !first_cutoff {
                out.push(',');
            }
            first_cutoff = false;
            out.push('"');
            out.push_str(&json_escape(conv));
            out.push_str(&format!(
                "\":{{\"bytes_len\":{},\"max_step\":{}}}",
                c.bytes_len, c.max_step
            ));
        }
    }
    out.push_str("},\"update\":{\"checked_at\":");
    out.push_str(&store.update_checked_at.to_string());
    out.push_str(",\"latest_version\":\"");
    out.push_str(&json_escape(&store.latest_version));
    out.push_str("\"}}");

    atomic_write_file(&store.path, out.as_bytes(), "json.tmp")
}

fn load_preload_js() -> String {
    let sdk_path = home_dir()
        .join(".gemini")
        .join("config")
        .join("node_modules")
        .join("sidecar_sdk")
        .join("preload.js");
    fs::read_to_string(sdk_path).unwrap_or_else(|_| FALLBACK_PRELOAD.to_string())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_loopback_host_authority(host_val: &str) -> bool {
    let h = host_val.trim();
    if h.is_empty() || h.bytes().any(|b| b == b'\r' || b == b'\n' || b == b' ') {
        return false;
    }
    if let Some(rest) = h.strip_prefix("[::1]") {
        return rest.is_empty()
            || rest
                .strip_prefix(':')
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    }
    let host_only = match h.split_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            host
        }
        None => h,
    };
    host_only == "127.0.0.1" || host_only.eq_ignore_ascii_case("localhost")
}

fn handle_client(mut stream: TcpStream, state: &AppState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut req_buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];

    for _ in 0..120 {
        let header_end = loop {
            if let Some(pos) = find_subslice(&req_buf, b"\r\n\r\n") {
                break pos;
            }
            if req_buf.len() > 32768 {
                send_response(&mut stream, 413, "text/plain", b"Header Too Large", false);
                return;
            }
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => req_buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        };

        let head = String::from_utf8_lossy(&req_buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let first_line = match lines.next() {
            Some(l) if !l.is_empty() => l,
            _ => return,
        };

        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("GET");
        let target = parts.next().unwrap_or("/");

        let mut header_token: Option<String> = None;
        let mut host_header: Option<String> = None;
        let mut content_length: usize = 0;
        let mut has_content_length = false;
        let mut wants_close = false;

        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let val = v.trim();
                if key.eq_ignore_ascii_case("x-sidecar-token") {
                    header_token = Some(val.to_string());
                } else if key.eq_ignore_ascii_case("host") {
                    host_header = Some(val.to_string());
                } else if key.eq_ignore_ascii_case("content-length") {
                    let Ok(parsed_len) = val.parse::<usize>() else {
                        send_response(
                            &mut stream,
                            400,
                            "text/plain; charset=utf-8",
                            b"Invalid Content-Length",
                            false,
                        );
                        return;
                    };
                    if has_content_length && content_length != parsed_len {
                        send_response(
                            &mut stream,
                            400,
                            "text/plain; charset=utf-8",
                            b"Conflicting Content-Length",
                            false,
                        );
                        return;
                    }
                    content_length = parsed_len;
                    has_content_length = true;
                } else if key.eq_ignore_ascii_case("connection")
                    && val.eq_ignore_ascii_case("close")
                {
                    wants_close = true;
                }
            }
        }

        if let Some(ref h) = host_header {
            if !is_loopback_host_authority(h) {
                send_response(
                    &mut stream,
                    403,
                    "text/plain; charset=utf-8",
                    b"Forbidden Host",
                    false,
                );
                return;
            }
        }

        if content_length > MAX_BODY_BYTES {
            send_response(&mut stream, 413, "text/plain", b"Body Too Large", false);
            return;
        }

        let body_start = header_end + 4;
        let total_needed = body_start + content_length;
        while req_buf.len() < total_needed {
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => req_buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }

        let req_body = req_buf[body_start..total_needed].to_vec();
        req_buf.drain(..total_needed);

        let (path, query_str) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        let query = parse_query(query_str);

        let is_public_asset = method == "OPTIONS"
            || (method == "GET" && matches!(path, "/" | "/index.html" | "/preload.js"));

        if !is_public_asset {
            if let Some(expected) = &state.token {
                let provided = header_token
                    .as_deref()
                    .or_else(|| query.get("token").map(String::as_str));
                let authorized = provided
                    .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()));
                if !authorized {
                    send_response(
                        &mut stream,
                        401,
                        "application/json; charset=utf-8",
                        br#"{"error":"unauthorized"}"#,
                        false,
                    );
                    return;
                }
            }
        }

        let keep_alive = !wants_close;

        match (method, path) {
            ("GET", "/") | ("GET", "/index.html") => {
                send_response(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    UI_HTML.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/preload.js") => {
                send_response(
                    &mut stream,
                    200,
                    "application/javascript; charset=utf-8",
                    state.preload_js.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/settings") => {
                let conv = query
                    .get("conversationId")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
                    .or(state.default_conv.as_deref())
                    .unwrap_or("");
                maybe_promote_draft(state, conv);
                let body = match state.settings.lock() {
                    Ok(store) => format!(r#"{{"settings":{}}}"#, store.get_for_chat(conv).to_json()),
                    Err(_) => r#"{"error":"lock failed"}"#.to_string(),
                };
                send_response(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/api/settings") => {
                let (status, body) = handle_post_settings(state, &req_body);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/update-state") => {
                let body = handle_get_update_state(state);
                send_response(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/api/update-state") => {
                let (status, body) = handle_post_update_state(state, &req_body);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/notes") => {
                let conv = query
                    .get("conversationId")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
                    .or(state.default_conv.as_deref())
                    .unwrap_or("");
                let (status, body) = handle_get_notes(state, conv);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/api/notes") => {
                let (status, body) = handle_post_notes(state, &req_body);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/search") => {
                let conv = query
                    .get("conversationId")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
                    .or(state.default_conv.as_deref())
                    .unwrap_or("");
                let q_text = query.get("q").map(String::as_str).unwrap_or("");
                let scope = query.get("scope").map(String::as_str).unwrap_or("this");
                let (status, body) = handle_get_search(state, conv, q_text, scope);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/api/compact") => {
                let (status, body) = handle_post_compact(state, &req_body);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/api/undo-compact") => {
                let (status, body) = handle_post_undo_compact(state, &req_body);
                send_response(
                    &mut stream,
                    status,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/cascade-done.mp3") => {
                match get_native_cascade_done_mp3(state) {
                    Some(bytes) => {
                        send_response(&mut stream, 200, "audio/mpeg", bytes, keep_alive);
                    }
                    None => {
                        send_response(
                            &mut stream,
                            404,
                            "text/plain; charset=utf-8",
                            b"Not Found",
                            keep_alive,
                        );
                    }
                }
            }
            ("GET", "/api/quota") => {
                let force = query
                    .get("force")
                    .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
                let body = handle_get_quota(state, force);
                send_response(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("GET", "/api/context") => {
                let conv = query
                    .get("conversationId")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
                    .or(state.default_conv.as_deref())
                    .unwrap_or("");
                let req_etag = query.get("etag").map(String::as_str).unwrap_or("");
                let body = build_context_json(state, conv, req_etag);
                send_response(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    keep_alive,
                );
            }
            ("POST", "/_sidecar/get-conversation-metadata") => {
                send_response(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    b"{}",
                    keep_alive,
                );
            }
            ("OPTIONS", _) => {
                send_response(&mut stream, 204, "text/plain", b"", keep_alive);
            }
            _ => {
                send_response(
                    &mut stream,
                    404,
                    "text/plain; charset=utf-8",
                    b"Not found",
                    keep_alive,
                );
            }
        }

        if !keep_alive {
            return;
        }
    }
}

fn count_user_messages_up_to(path: &Path, limit: u64) -> u64 {
    let Ok(f) = File::open(path) else {
        return 0;
    };
    let mut reader = BufReader::with_capacity(16 * 1024, f);
    let mut buf = Vec::with_capacity(2048);
    let mut count = 0u64;
    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let trimmed = trim_ascii(&buf);
        if trimmed.is_empty() {
            continue;
        }
        if let Some(m) = parse_line(trimmed) {
            if m.kind == StepKind::UserInput && !m.is_compact_cmd {
                count = count.saturating_add(1);
                if count >= limit {
                    return count;
                }
            }
        }
    }
    count
}

fn maybe_promote_draft(state: &AppState, conv: &str) {
    if is_draft_conv(conv) || !is_valid_conv_id(conv) {
        return;
    }

    let should_try = match state.settings.lock() {
        Ok(store) => store.has_pending_draft && !store.chats.contains_key(conv),
        Err(_) => return,
    };
    if !should_try {
        return;
    }

    let Ok(_notes_guard) = state.notes_lock.lock() else {
        return;
    };
    let Ok(mut store) = state.settings.lock() else {
        return;
    };
    if !store.has_pending_draft || store.chats.contains_key(conv) {
        return;
    }

    let Some(target_note_path) = note_path_for(&state.notes_dir, conv) else {
        return;
    };
    if target_note_path.exists() {
        return;
    }

    let t_path = transcript_path(&state.app_data, conv);
    if let Ok(meta) = fs::metadata(&t_path) {
        let file_secs = meta
            .created()
            .ok()
            .or_else(|| meta.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if store.draft_updated_at > 0 && file_secs < store.draft_updated_at.saturating_sub(5) {
            return;
        }
        if meta.len() > 0 && count_user_messages_up_to(&t_path, 2) > 1 {
            return;
        }
    }

    let mut promoted = false;
    if store.draft != Settings::default() {
        let draft_settings = store.draft;
        store.insert_chat(conv.to_string(), draft_settings);
        store.draft = Settings::default();
        promoted = true;
    }

    let mut notes_promoted_cleanly = true;
    if let Some(draft_note_path) = note_path_for(&state.notes_dir, DRAFT_CONV_ID) {
        if draft_note_path.exists() {
            match read_notes_data(&state.notes_dir, DRAFT_CONV_ID) {
                Ok(notes) if !notes.is_empty() => {
                    if write_notes_data(&state.notes_dir, conv, &notes).is_ok() {
                        promoted = true;
                        let _ = fs::remove_file(&draft_note_path);
                    } else {
                        notes_promoted_cleanly = false;
                    }
                }
                Ok(_) => {
                    let _ = fs::remove_file(&draft_note_path);
                }
                Err(_) => {
                    notes_promoted_cleanly = false;
                }
            }
        }
    }

    if notes_promoted_cleanly {
        store.draft_updated_at = 0;
        store.has_pending_draft = false;
    }
    if promoted {
        store.rev = store.rev.saturating_add(1);
    }
    let _ = save_settings_store(&store);
}

fn handle_post_settings(state: &AppState, body: &[u8]) -> (u16, String) {
    let conv = parse_json_string_field(body, "conversationId")
        .or_else(|| state.default_conv.clone())
        .unwrap_or_default();

    let is_draft = is_draft_conv(&conv);
    if !is_draft && !is_valid_conv_id(&conv) {
        return (
            400,
            r#"{"error":"Invalid conversation ID for settings"}"#.to_string(),
        );
    }

    let _notes_guard = if is_draft {
        match state.notes_lock.lock() {
            Ok(g) => Some(g),
            Err(_) => return (500, r#"{"error":"Internal lock error"}"#.to_string()),
        }
    } else {
        None
    };

    let Ok(mut store) = state.settings.lock() else {
        return (500, r#"{"error":"Internal lock error"}"#.to_string());
    };

    let mut next = store.get_for_chat(&conv);
    if !parse_settings_patch(body, &mut next) {
        return (400, r#"{"error":"Invalid settings payload"}"#.to_string());
    }
    if let Err(msg) = next.validate() {
        return (400, format!(r#"{{"error":"{}"}}"#, json_escape(msg)));
    }

    if is_draft {
        store.draft = next;
        let draft_note_exists =
            note_path_for(&state.notes_dir, DRAFT_CONV_ID).is_some_and(|p| p.exists());
        let has_draft = next != Settings::default() || draft_note_exists;
        store.has_pending_draft = has_draft;
        store.draft_updated_at = if has_draft { unix_now_secs() } else { 0 };
    } else {
        store.insert_chat(conv, next);
    }

    if let Err(e) = save_settings_store(&store) {
        return (
            500,
            format!(
                r#"{{"error":"Failed to save settings: {}"}}"#,
                json_escape(&e.to_string())
            ),
        );
    }

    store.rev = store.rev.saturating_add(1);
    let json = format!(r#"{{"ok":true,"settings":{}}}"#, next.to_json());
    (200, json)
}

fn is_clean_semver(s: &str) -> bool {
    let clean = s.trim().trim_start_matches(['v', 'V']);
    if clean.len() < 5 || clean.len() > 24 {
        return false;
    }
    let Some((core, pre)) = clean
        .split_once('-')
        .map(|(a, b)| (a, Some(b)))
        .or(Some((clean, None)))
    else {
        return false;
    };
    let mut parts = core.split('.');
    let (Some(maj), Some(min), Some(pat), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let core_ok = [maj, min, pat]
        .iter()
        .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()));
    if !core_ok {
        return false;
    }
    if let Some(suffix) = pre {
        if suffix.is_empty()
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return false;
        }
    }
    true
}

fn normalize_semver(s: &str) -> Option<String> {
    let clean = s.trim().trim_start_matches(['v', 'V']);
    if is_clean_semver(clean) {
        Some(clean.to_string())
    } else {
        None
    }
}

fn handle_get_update_state(state: &AppState) -> String {
    match state.settings.lock() {
        Ok(store) => format!(
            r#"{{"checked_at":{},"latest_version":"{}"}}"#,
            store.update_checked_at,
            json_escape(&store.latest_version)
        ),
        Err(_) => r#"{"checked_at":0,"latest_version":""}"#.to_string(),
    }
}

fn handle_post_update_state(state: &AppState, body: &[u8]) -> (u16, String) {
    let mut checked_at: Option<u64> = None;
    let mut latest_ver: Option<String> = None;
    let mut ok = true;

    let parsed = parse_json_object_entries(body, |k, v| {
        let mut idx = 0;
        skip_ws(v, &mut idx);
        match k.as_str() {
            "checked_at" => {
                if let Some(ts) = parse_u64(v, &mut idx) {
                    checked_at = Some(ts);
                } else {
                    ok = false;
                }
            }
            "latest_version" if v.first() == Some(&b'"') => {
                if let Some(s) = parse_string(v, &mut idx) {
                    if s.trim().is_empty() {
                        latest_ver = Some(String::new());
                    } else if let Some(norm) = normalize_semver(&s) {
                        latest_ver = Some(norm);
                    } else {
                        ok = false;
                    }
                } else {
                    ok = false;
                }
            }
            _ => {}
        }
    });

    if parsed.is_none() || !ok {
        return (400, r#"{"error":"Invalid update state payload"}"#.to_string());
    }

    let Ok(mut store) = state.settings.lock() else {
        return (500, r#"{"error":"Internal lock error"}"#.to_string());
    };

    let max_now = unix_now_secs().saturating_add(60);
    if let Some(ts) = checked_at {
        store.update_checked_at = ts.min(max_now);
    } else {
        store.update_checked_at = unix_now_secs();
    }
    if let Some(ver) = latest_ver {
        store.latest_version = ver;
    }

    if let Err(e) = save_settings_store(&store) {
        return (
            500,
            format!(
                r#"{{"error":"Failed to save update state: {}"}}"#,
                json_escape(&e.to_string())
            ),
        );
    }

    (
        200,
        format!(
            r#"{{"ok":true,"checked_at":{},"latest_version":"{}"}}"#,
            store.update_checked_at,
            json_escape(&store.latest_version)
        ),
    )
}

fn note_path_for(notes_dir: &Path, conv: &str) -> Option<PathBuf> {
    if conv == DRAFT_CONV_ID || is_valid_conv_id(conv) {
        Some(notes_dir.join(format!("{conv}.json")))
    } else {
        None
    }
}

fn read_notes_data(notes_dir: &Path, conv: &str) -> Result<NotesData, std::io::Error> {
    let Some(path) = note_path_for(notes_dir, conv) else {
        return Ok(NotesData {
            next_id: 1,
            ..Default::default()
        });
    };
    match fs::read(&path) {
        Ok(bytes) => {
            if trim_ascii(&bytes).is_empty() {
                return Ok(NotesData {
                    next_id: 1,
                    ..Default::default()
                });
            }
            match parse_notes_data(&bytes) {
                Some(data) => Ok(data),
                None => {
                    backup_corrupt_file(&path);
                    Ok(NotesData {
                        next_id: 1,
                        ..Default::default()
                    })
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NotesData {
            next_id: 1,
            ..Default::default()
        }),
        Err(e) => Err(e),
    }
}

fn write_notes_data(notes_dir: &Path, conv: &str, data: &NotesData) -> Result<(), String> {
    let path = note_path_for(notes_dir, conv)
        .ok_or_else(|| "No active conversation ID for notes".to_string())?;
    if data.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Failed to clear notes: {e}")),
        }
    }
    if data.cards.len() > MAX_NOTE_CARDS {
        return Err(format!("Maximum {MAX_NOTE_CARDS} cards per chat reached."));
    }
    let json = data.to_json();
    if json.len() > MAX_NOTE_BYTES {
        return Err("Notes are too large (max 256 KB).".to_string());
    }
    atomic_write_file(&path, json.as_bytes(), "json.tmp")
        .map_err(|e| format!("Failed to save notes: {e}"))
}

fn parse_notes_data(b: &[u8]) -> Option<NotesData> {
    let mut data = NotesData {
        next_id: 1,
        ..Default::default()
    };
    let mut seen_known = false;
    parse_json_object_entries(b, |k, v| match k.as_str() {
        "draft" if v.first() == Some(&b'"') => {
            let mut idx = 0;
            if let Some(txt) = parse_string(v, &mut idx) {
                data.draft = txt;
                seen_known = true;
            }
        }
        "cards" if v.first() == Some(&b'[') => {
            parse_note_cards_array(v, &mut data.cards);
            seen_known = true;
        }
        "next_id" => {
            let mut idx = 0;
            skip_ws(v, &mut idx);
            if let Some(n) = parse_u64(v, &mut idx) {
                data.next_id = n.max(1);
                seen_known = true;
            }
        }
        _ => {}
    })?;
    if !seen_known {
        return None;
    }
    let max_card_id = data.cards.iter().map(|c| c.id).max().unwrap_or(0);
    if data.next_id <= max_card_id {
        data.next_id = max_card_id.saturating_add(1);
    }
    Some(data)
}

fn parse_note_cards_array(b: &[u8], cards: &mut Vec<NoteCard>) {
    let mut i = 0;
    skip_ws(b, &mut i);
    if b.get(i) != Some(&b'[') {
        return;
    }
    i += 1;
    let mut auto_id: u64 = 1;
    loop {
        skip_ws(b, &mut i);
        if b.get(i) == Some(&b']') {
            return;
        }
        let start = i;
        if skip_value(b, &mut i).is_none() {
            return;
        }
        // Allow one extra card so POST can detect overflow and return HTTP 413
        if cards.len() <= MAX_NOTE_CARDS {
            let slice = &b[start..i];
            if slice.first() == Some(&b'{') {
                let mut id = 0u64;
                let mut text = String::new();
                let mut pinned = false;
                let mut has_text = false;
                let _ = parse_json_object_entries(slice, |k, v| {
                    if k == "id" {
                        let mut k_idx = 0;
                        skip_ws(v, &mut k_idx);
                        if let Some(n) = parse_u64(v, &mut k_idx) {
                            id = n;
                        }
                    } else if k == "text" && v.first() == Some(&b'"') {
                        let mut k_idx = 0;
                        if let Some(t) = parse_string(v, &mut k_idx) {
                            text = t;
                            has_text = true;
                        }
                    } else if k == "pinned" {
                        let mut k_idx = 0;
                        skip_ws(v, &mut k_idx);
                        if let Some(p) = parse_bool(v, &mut k_idx) {
                            pinned = p;
                        }
                    }
                });
                if has_text {
                    if id == 0 {
                        id = auto_id;
                    }
                    auto_id = auto_id.max(id).saturating_add(1);
                    cards.push(NoteCard { id, text, pinned });
                }
            }
        }
        skip_ws(b, &mut i);
        match b.get(i) {
            Some(&b',') => i += 1,
            Some(&b']') => return,
            _ => return,
        }
    }
}

fn handle_get_notes(state: &AppState, conv: &str) -> (u16, String) {
    let target = if is_draft_conv(conv) {
        DRAFT_CONV_ID
    } else if is_valid_conv_id(conv) {
        maybe_promote_draft(state, conv);
        conv
    } else {
        return (400, r#"{"error":"Invalid conversation ID for notes"}"#.to_string());
    };

    let Ok(_guard) = state.notes_lock.lock() else {
        return (500, r#"{"error":"Internal lock error"}"#.to_string());
    };
    match read_notes_data(&state.notes_dir, target) {
        Ok(mut data) => {
            if data.cards.len() > MAX_NOTE_CARDS {
                data.cards.truncate(MAX_NOTE_CARDS);
            }
            (200, data.to_json())
        }
        Err(e) => (
            500,
            format!(r#"{{"error":"Failed to read notes: {}"}}"#, json_escape(&e.to_string())),
        ),
    }
}

fn handle_post_notes(state: &AppState, body: &[u8]) -> (u16, String) {
    let conv = parse_json_string_field(body, "conversationId")
        .or_else(|| state.default_conv.clone())
        .unwrap_or_default();

    let is_draft = is_draft_conv(&conv);
    if !is_draft && !is_valid_conv_id(&conv) {
        return (400, r#"{"error":"Invalid conversation ID for notes"}"#.to_string());
    }
    let target = if is_draft { DRAFT_CONV_ID } else { conv.as_str() };

    let Some(data) = parse_notes_data(body) else {
        return (400, r#"{"error":"Invalid notes payload"}"#.to_string());
    };

    if data.cards.len() > MAX_NOTE_CARDS {
        return (
            413,
            format!(r#"{{"error":"Maximum {MAX_NOTE_CARDS} cards per chat reached."}}"#),
        );
    }

    if data.to_json().len() > MAX_NOTE_BYTES {
        return (
            413,
            r#"{"error":"Notes are too large (max 256 KB)."}"#.to_string(),
        );
    }

    let Ok(_guard) = state.notes_lock.lock() else {
        return (500, r#"{"error":"Internal lock error"}"#.to_string());
    };

    if let Err(msg) = write_notes_data(&state.notes_dir, target, &data) {
        return (500, format!(r#"{{"error":"{}"}}"#, json_escape(&msg)));
    }

    if is_draft {
        if let Ok(mut store) = state.settings.lock() {
            let has_draft = !data.is_empty() || store.draft != Settings::default();
            store.has_pending_draft = has_draft;
            store.draft_updated_at = if has_draft { unix_now_secs() } else { 0 };
            let _ = save_settings_store(&store);
        }
    }

    (200, r#"{"ok":true}"#.to_string())
}

fn handle_post_compact(state: &AppState, body: &[u8]) -> (u16, String) {
    let conv = parse_json_string_field(body, "conversationId")
        .or_else(|| state.default_conv.clone())
        .unwrap_or_default();

    if is_draft_conv(&conv) || !is_valid_conv_id(&conv) {
        return (400, r#"{"ok":false,"error":"No active conversation ID"}"#.to_string());
    }

    let path = transcript_path(&state.app_data, &conv);
    let Ok(meta) = fs::metadata(&path) else {
        return (200, r#"{"ok":false,"error":"Nothing to compact yet"}"#.to_string());
    };
    let cutoff = state
        .settings
        .lock()
        .ok()
        .and_then(|s| s.cutoffs.get(&conv).copied());
    let mut scan = update_chat_scan(state, &conv, &path, meta.len(), cutoff);
    apply_ls_live_to_summary(&mut scan, resolve_ls_live_status(state, &conv));
    if !scan.has_context_data {
        return (200, r#"{"ok":false,"error":"Nothing to compact yet"}"#.to_string());
    }
    if scan.is_busy || scan.compact_in_flight {
        return (
            200,
            r#"{"ok":false,"error":"Wait for the current turn to finish"}"#.to_string(),
        );
    }
    if scan.just_compressed {
        return (
            200,
            r#"{"ok":false,"error":"Already compacted; send a prompt first"}"#.to_string(),
        );
    }

    match trigger_compact_via_ls(state, &conv) {
        Ok(()) => (200, r#"{"ok":true}"#.to_string()),
        Err(err) => (
            200,
            format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(&err)),
        ),
    }
}

fn resolve_ls_creds(state: &AppState) -> Result<(&str, &str), String> {
    let raw_addr = state
        .ls_address
        .as_deref()
        .ok_or_else(|| "Missing ANTIGRAVITY_LS_ADDRESS".to_string())?;
    let csrf = state
        .csrf_token
        .as_deref()
        .ok_or_else(|| "Missing ANTIGRAVITY_CSRF_TOKEN".to_string())?;

    if csrf.bytes().any(|b| b == b'\r' || b == b'\n') {
        return Err("Invalid ANTIGRAVITY_CSRF_TOKEN".to_string());
    }

    let trimmed = raw_addr.trim().trim_end_matches('/');
    if trimmed.starts_with("https://") {
        return Err("HTTPS language server is not supported yet".to_string());
    }
    let host_port = trimmed.strip_prefix("http://").unwrap_or(trimmed);
    if !is_loopback_host_authority(host_port) {
        return Err("Language server address must be on local loopback (127.0.0.1 / localhost)".to_string());
    }
    Ok((host_port, csrf))
}

fn handle_post_undo_compact(state: &AppState, body: &[u8]) -> (u16, String) {
    let conv = parse_json_string_field(body, "conversationId")
        .or_else(|| state.default_conv.clone())
        .unwrap_or_default();

    if is_draft_conv(&conv) || !is_valid_conv_id(&conv) {
        return (400, r#"{"ok":false,"error":"No active conversation ID"}"#.to_string());
    }

    let path = transcript_path(&state.app_data, &conv);
    let Ok(meta) = fs::metadata(&path) else {
        return (200, r#"{"ok":false,"error":"Transcript not found"}"#.to_string());
    };
    let cutoff = state
        .settings
        .lock()
        .ok()
        .and_then(|s| s.cutoffs.get(&conv).copied());
    let mut scan = update_chat_scan(state, &conv, &path, meta.len(), cutoff);
    apply_ls_live_to_summary(&mut scan, resolve_ls_live_status(state, &conv));
    if scan.is_busy {
        return (
            200,
            r#"{"ok":false,"error":"Wait for the current turn to finish"}"#.to_string(),
        );
    }
    let Some(step_index) = scan.undo_step_index else {
        return (
            200,
            r#"{"ok":false,"error":"Nothing to undo"}"#.to_string(),
        );
    };

    match trigger_undo_compact_via_ls(state, &conv, step_index) {
        Ok(()) => {
            if let Ok(mut store) = state.settings.lock() {
                store.insert_cutoff(
                    conv.clone(),
                    RevertCutoff {
                        bytes_len: meta.len(),
                        max_step: step_index,
                    },
                );
                store.rev = store.rev.saturating_add(1);
                let _ = save_settings_store(&store);
            }
            if let Ok(mut guard) = state.scans.lock() {
                guard.remove(&conv);
            }
            if let Ok(mut guard) = state.cache.lock() {
                guard.remove(&conv);
            }
            let (play_sound, sound_volume) = read_native_sound_settings(&state.app_data);
            (
                200,
                format!(
                    r#"{{"ok":true,"play_sound":{play_sound},"sound_volume":{sound_volume}}}"#
                ),
            )
        }
        Err(err) => (
            200,
            format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(&err)),
        ),
    }
}

fn trigger_undo_compact_via_ls(
    state: &AppState,
    conv: &str,
    step_index: u64,
) -> Result<(), String> {
    let (host_port, csrf) = resolve_ls_creds(state)?;
    let endpoint = "/exa.language_server_pb.LanguageServerService/RevertToCascadeStep";
    let body = format!(
        r#"{{"cascadeId":"{}","stepIndex":{},"conversationOnly":true}}"#,
        json_escape(conv),
        step_index
    );
    let (status, resp_body) = post_ls_json(host_port, csrf, endpoint, &body)?;
    if (200..=299).contains(&status) {
        return Ok(());
    }
    Err(format!(
        "LS rejected undo (HTTP {status}): {}",
        truncate_chars(&resp_body, 160)
    ))
}

fn trigger_compact_via_ls(state: &AppState, conv: &str) -> Result<(), String> {
    let (host_port, csrf) = resolve_ls_creds(state)?;
    let escaped_conv = json_escape(conv);

    let mut info_json = resolve_compact_command_info(host_port, csrf, &escaped_conv);
    if info_json.is_none() {
        info_json = Some(r#"{"name":"compact"}"#.to_string());
    }
    let info_payload = info_json.unwrap();

    let send_endpoint = "/exa.language_server_pb.LanguageServerService/SendUserCascadeMessage";
    let body_structured = format!(
        r#"{{"cascadeId":"{}","items":[{{"item":{{"slashCommand":{{"info":{}}}}}}}]}}"#,
        escaped_conv, info_payload
    );
    let (status, resp_body) = post_ls_json(host_port, csrf, send_endpoint, &body_structured)?;
    if (200..=299).contains(&status) {
        return Ok(());
    }

    if info_payload != r#"{"name":"compact"}"# {
        let body_min = format!(
            r#"{{"cascadeId":"{}","items":[{{"item":{{"slashCommand":{{"info":{{"name":"compact"}}}}}}}}]}}"#,
            escaped_conv
        );
        let (st2, rb2) = post_ls_json(host_port, csrf, send_endpoint, &body_min)?;
        if (200..=299).contains(&st2) {
            return Ok(());
        }
        return Err(format!(
            "LS rejected compact (HTTP {status}/{st2}): {}",
            truncate_chars(if !rb2.is_empty() { &rb2 } else { &resp_body }, 160)
        ));
    }

    Err(format!(
        "LS rejected compact (HTTP {status}): {}",
        truncate_chars(&resp_body, 160)
    ))
}

fn resolve_compact_command_info(host_port: &str, csrf: &str, escaped_conv: &str) -> Option<String> {
    let endpoint = "/exa.language_server_pb.LanguageServerService/GetSlashCommands";
    let req_body = format!(r#"{{"cascadeId":"{}"}}"#, escaped_conv);
    let (status, body) = post_ls_json(host_port, csrf, endpoint, &req_body).ok()?;
    if !(200..=299).contains(&status) || body.is_empty() {
        return None;
    }
    extract_compact_info_from_response(body.as_bytes())
}

fn extract_compact_info_from_response(b: &[u8]) -> Option<String> {
    let mut found = None;
    parse_json_object_entries(b, |key, val_slice| {
        if found.is_none() && key == "commands" && val_slice.first() == Some(&b'[') {
            let mut i = 0;
            found = scan_commands_array(val_slice, &mut i);
        }
    })?;
    found
}

fn scan_commands_array(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) != Some(&b'[') {
        return None;
    }
    *i += 1;
    loop {
        skip_ws(b, i);
        if b.get(*i) == Some(&b']') {
            *i += 1;
            return None;
        }
        let start = *i;
        skip_value(b, i)?;
        let elem = &b[start..*i];
        if let Some(info_json) = extract_info_if_compact(elem) {
            return Some(info_json);
        }
        skip_ws(b, i);
        match b.get(*i) {
            Some(&b',') => *i += 1,
            Some(&b']') => {
                *i += 1;
                return None;
            }
            _ => return None,
        }
    }
}

fn extract_info_if_compact(obj: &[u8]) -> Option<String> {
    let mut nested_info: Option<Vec<u8>> = None;
    let mut direct_name: Option<String> = None;
    let mut allowed_fields: Vec<(String, String)> = Vec::new();

    parse_json_object_entries(obj, |key, val_slice| {
        if key == "info" && val_slice.first() == Some(&b'{') {
            nested_info = Some(val_slice.to_vec());
        } else if key == "name" && val_slice.first() == Some(&b'"') {
            let mut k = 0;
            direct_name = parse_string(val_slice, &mut k);
        }

        if matches!(
            key.as_str(),
            "name"
                | "modelFacingText"
                | "type"
                | "icon"
                | "definitionPath"
                | "displayName"
                | "logo"
                | "plugin"
        ) {
            allowed_fields.push((key, String::from_utf8_lossy(val_slice).into_owned()));
        }
    })?;

    if let Some(info_bytes) = nested_info {
        if let Some(nm) = parse_json_string_field(&info_bytes, "name") {
            let clean = nm.trim_start_matches('/');
            if clean.eq_ignore_ascii_case("compact") {
                return Some(String::from_utf8_lossy(&info_bytes).into_owned());
            }
        }
    }

    if let Some(nm) = direct_name {
        let clean = nm.trim_start_matches('/');
        if clean.eq_ignore_ascii_case("compact") && !allowed_fields.is_empty() {
            let mut out = String::from("{");
            for (idx, (k, v)) in allowed_fields.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&json_escape(k));
                out.push_str("\":");
                out.push_str(v);
            }
            out.push('}');
            return Some(out);
        }
    }

    None
}

fn decode_chunked_body(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let Some(rel_end) = find_subslice(&raw[i..], b"\r\n") else {
            break;
        };
        let line = String::from_utf8_lossy(&raw[i..i + rel_end]);
        let hex_part = line.split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(hex_part, 16) else {
            break;
        };
        i += rel_end + 2;
        if chunk_len == 0 {
            break;
        }
        let end = (i + chunk_len).min(raw.len());
        out.extend_from_slice(&raw[i..end]);
        i = (end + 2).min(raw.len());
    }
    if out.is_empty() && !raw.is_empty() {
        raw.to_vec()
    } else {
        out
    }
}

fn ls_connect_addr(host_port: &str) -> String {
    if let Some(port) = host_port.strip_prefix("localhost:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = host_port.strip_prefix("LOCALHOST:") {
        format!("127.0.0.1:{port}")
    } else {
        host_port.to_string()
    }
}

fn ls_http_request_with_cap(
    host_port: &str,
    req: &str,
    timeout: Duration,
    init_cap: usize,
    max_cap: usize,
) -> Result<(u16, Vec<u8>), String> {
    let connect_addr = ls_connect_addr(host_port);
    let mut stream = TcpStream::connect(&connect_addr)
        .map_err(|e| format!("Failed to connect to LS ({host_port}): {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Failed to write to LS: {e}"))?;
    let _ = stream.flush();

    let mut resp = Vec::with_capacity(init_cap);
    let mut buf = [0u8; 8192];
    while resp.len() < max_cap {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if resp.is_empty() {
                    return Err(format!("LS read failed: {e}"));
                }
                break;
            }
        }
    }

    if resp.is_empty() {
        return Err("LS closed connection without response".to_string());
    }

    let split = find_subslice(&resp, b"\r\n\r\n");
    let (head_bytes, body_bytes) = match split {
        Some(pos) => (&resp[..pos], &resp[pos + 4..]),
        None => (&resp[..], &[][..]),
    };

    let head = String::from_utf8_lossy(head_bytes);
    let first_line = head.lines().next().unwrap_or("");
    let status = first_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("Unexpected LS response: {}", truncate_chars(first_line, 80)))?;

    let is_chunked = head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });

    let body = if is_chunked {
        decode_chunked_body(body_bytes)
    } else {
        body_bytes.to_vec()
    };

    Ok((status, body))
}

fn ls_http_request(
    host_port: &str,
    req: &str,
    timeout: Duration,
    init_cap: usize,
) -> Result<(u16, Vec<u8>), String> {
    ls_http_request_with_cap(host_port, req, timeout, init_cap, 1_048_576)
}

fn post_ls_json_bytes(
    host_port: &str,
    csrf: &str,
    endpoint: &str,
    json_body: &str,
    timeout: Duration,
    max_cap: usize,
) -> Result<(u16, Vec<u8>), String> {
    let req = format!(
        "POST {endpoint} HTTP/1.0\r\nHost: {host_port}\r\nContent-Type: application/json\r\nx-codeium-csrf-token: {csrf}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json_body}",
        json_body.len()
    );
    ls_http_request_with_cap(host_port, &req, timeout, 4096, max_cap)
}

fn post_ls_json(
    host_port: &str,
    csrf: &str,
    endpoint: &str,
    json_body: &str,
) -> Result<(u16, String), String> {
    let (status, body_bytes) = post_ls_json_bytes(
        host_port,
        csrf,
        endpoint,
        json_body,
        Duration::from_secs(10),
        1_048_576,
    )?;
    let body_text = String::from_utf8_lossy(&body_bytes).trim().to_string();
    Ok((status, body_text))
}

fn parse_trajectories_live_json(b: &[u8]) -> (HashMap<String, String>, HashMap<String, bool>) {
    let mut titles = HashMap::new();
    let mut running = HashMap::new();
    let _ = parse_json_object_entries(b, |key, val_slice| {
        if key == "trajectorySummaries" && val_slice.first() == Some(&b'{') {
            let _ = parse_json_object_entries(val_slice, |conv_id, obj_slice| {
                if let Some(summary) = parse_json_string_field(obj_slice, "summary") {
                    let clean = summary.trim().to_string();
                    if !clean.is_empty() {
                        titles.insert(conv_id.clone(), clean);
                    }
                }
                if let Some(status) = parse_json_string_field(obj_slice, "status") {
                    let is_run = status.contains("RUNNING") || status.contains("CANCELING");
                    running.insert(conv_id, is_run);
                }
            });
        }
    });
    (titles, running)
}

fn parse_bg_tasks_convs_json(b: &[u8]) -> Vec<String> {
    let mut convs = Vec::new();
    let _ = parse_json_object_entries(b, |key, val_slice| {
        if key == "tasks" && val_slice.first() == Some(&b'[') {
            let mut i = 1usize;
            loop {
                skip_ws(val_slice, &mut i);
                if val_slice.get(i) == Some(&b']') || i >= val_slice.len() {
                    break;
                }
                let start = i;
                if skip_value(val_slice, &mut i).is_none() {
                    break;
                }
                let elem = &val_slice[start..i];
                if elem.first() == Some(&b'{') {
                    if let Some(cid) = parse_json_string_field(elem, "conversationId") {
                        let task_finished = find_subslice(elem, b"CORTEX_STEP_STATUS_DONE").is_some()
                            || find_subslice(elem, b"CORTEX_STEP_STATUS_CANCEL").is_some()
                            || find_subslice(elem, b"CORTEX_STEP_STATUS_ERROR").is_some();
                        if !task_finished && is_valid_conv_id(&cid) && !convs.contains(&cid) {
                            convs.push(cid);
                        }
                    }
                }
                skip_ws(val_slice, &mut i);
                if val_slice.get(i) == Some(&b',') {
                    i += 1;
                } else {
                    break;
                }
            }
        }
    });
    convs
}

fn resolve_ls_live_status(state: &AppState, conv: &str) -> LsLiveConvStatus {
    if is_draft_conv(conv) || !is_valid_conv_id(conv) {
        return LsLiveConvStatus::default();
    }
    let Ok((host_port, csrf)) = resolve_ls_creds(state) else {
        return LsLiveConvStatus::default();
    };

    if let Ok(guard) = state.ls_live.lock() {
        if let Some(ts) = guard.fetched_at {
            if ts.elapsed() < Duration::from_millis(350) {
                let known = guard.running_convs.contains_key(conv);
                let is_running = guard.running_convs.get(conv).copied().unwrap_or(false);
                let has_bg_task = guard.bg_convs.iter().any(|c| c == conv);
                return LsLiveConvStatus {
                    known,
                    is_running,
                    has_bg_task,
                };
            }
        }
    }

    let Ok((st_all, all_bytes)) = post_ls_json_bytes(
        host_port,
        csrf,
        "/exa.language_server_pb.LanguageServerService/GetAllCascadeTrajectories",
        "{}",
        Duration::from_millis(700),
        8 * 1024 * 1024,
    ) else {
        return LsLiveConvStatus::default();
    };
    if !(200..=299).contains(&st_all) || all_bytes.is_empty() {
        return LsLiveConvStatus::default();
    }
    let (titles, running_convs) = parse_trajectories_live_json(&all_bytes);
    if !titles.is_empty() {
        if let Ok(mut guard) = state.chat_titles.lock() {
            guard.0 = unix_now_secs();
            guard.1 = titles;
        }
    }

    let bg_convs = match post_ls_json_bytes(
        host_port,
        csrf,
        "/exa.language_server_pb.LanguageServerService/ListBackgroundTasks",
        "{}",
        Duration::from_millis(500),
        2 * 1024 * 1024,
    ) {
        Ok((st_bg, bg_bytes)) if (200..=299).contains(&st_bg) && !bg_bytes.is_empty() => {
            parse_bg_tasks_convs_json(&bg_bytes)
        }
        _ => Vec::new(),
    };

    let known = running_convs.contains_key(conv);
    let is_running = running_convs.get(conv).copied().unwrap_or(false);
    let has_bg_task = bg_convs.iter().any(|c| c == conv);

    if !running_convs.is_empty() {
        maybe_prune_deleted_chats(state, conv, &running_convs);
    }

    if let Ok(mut guard) = state.ls_live.lock() {
        guard.fetched_at = Some(Instant::now());
        guard.running_convs = running_convs;
        guard.bg_convs = bg_convs;
    }

    LsLiveConvStatus {
        known,
        is_running,
        has_bg_task,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct QuotaBucketInfo {
    label: String,
    pct: u8,
    reset_time: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct QuotaGroupInfo {
    label: String,
    weekly: Option<QuotaBucketInfo>,
    five_hour: Option<QuotaBucketInfo>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct QuotaSummaryParsed {
    gemini: Option<QuotaGroupInfo>,
    other: Option<QuotaGroupInfo>,
}

impl QuotaBucketInfo {
    fn to_json(&self) -> String {
        format!(
            r#"{{"label":"{}","pct":{},"reset_time":"{}"}}"#,
            json_escape(&self.label),
            self.pct,
            json_escape(&self.reset_time)
        )
    }
}

impl QuotaGroupInfo {
    fn has_any(&self) -> bool {
        self.weekly.is_some() || self.five_hour.is_some()
    }

    fn to_json(&self) -> String {
        let w = match &self.weekly {
            Some(b) => b.to_json(),
            None => "null".to_string(),
        };
        let h = match &self.five_hour {
            Some(b) => b.to_json(),
            None => "null".to_string(),
        };
        format!(
            r#"{{"label":"{}","weekly":{},"five_hour":{}}}"#,
            json_escape(&self.label),
            w,
            h
        )
    }
}

impl QuotaSummaryParsed {
    fn has_any(&self) -> bool {
        self.gemini.as_ref().is_some_and(QuotaGroupInfo::has_any)
            || self.other.as_ref().is_some_and(QuotaGroupInfo::has_any)
    }

    fn to_json(&self) -> String {
        if !self.has_any() {
            return r#"{"available":false}"#.to_string();
        }
        let g = match &self.gemini {
            Some(gr) if gr.has_any() => gr.to_json(),
            _ => "null".to_string(),
        };
        let o = match &self.other {
            Some(gr) if gr.has_any() => gr.to_json(),
            _ => "null".to_string(),
        };
        format!(r#"{{"available":true,"gemini":{g},"other":{o}}}"#)
    }
}

fn parse_f64_slice(b: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(trim_ascii(b)).ok()?;
    let val = s.parse::<f64>().ok()?;
    if val.is_finite() { Some(val) } else { None }
}

fn parse_quota_bucket(obj: &[u8]) -> Option<(QuotaBucketInfo, String)> {
    let mut label = String::new();
    let mut window = String::new();
    let mut reset_time = String::new();
    let mut frac: Option<f64> = None;
    let mut disabled = false;

    parse_json_object_entries(obj, |k, v| match k.as_str() {
        "displayName" | "display_name" if v.first() == Some(&b'"') => {
            let mut idx = 0;
            if let Some(s) = parse_string(v, &mut idx) {
                label = s;
            }
        }
        "window" if v.first() == Some(&b'"') => {
            let mut idx = 0;
            if let Some(s) = parse_string(v, &mut idx) {
                window = s;
            }
        }
        "resetTime" | "reset_time" if v.first() == Some(&b'"') => {
            let mut idx = 0;
            if let Some(s) = parse_string(v, &mut idx) {
                reset_time = s;
            }
        }
        "remainingFraction" | "remaining_fraction" => {
            if let Some(f) = parse_f64_slice(v) {
                frac = Some(f);
            }
        }
        "disabled" => {
            let mut idx = 0;
            skip_ws(v, &mut idx);
            if let Some(d) = parse_bool(v, &mut idx) {
                disabled = d;
            }
        }
        _ => {}
    })?;

    if disabled {
        return None;
    }
    let f = frac.unwrap_or(0.0).clamp(0.0, 1.0);
    let raw_pct = (f * 100.0).round().clamp(0.0, 100.0) as u8;
    let pct = if f > 0.0 && raw_pct == 0 { 1 } else { raw_pct };
    Some((
        QuotaBucketInfo {
            label,
            pct,
            reset_time,
        },
        window,
    ))
}

fn parse_quota_group(obj: &[u8]) -> Option<QuotaGroupInfo> {
    let mut label = String::new();
    let mut buckets: Vec<(QuotaBucketInfo, String)> = Vec::new();

    parse_json_object_entries(obj, |k, v| match k.as_str() {
        "displayName" | "display_name" if v.first() == Some(&b'"') => {
            let mut idx = 0;
            if let Some(s) = parse_string(v, &mut idx) {
                label = s;
            }
        }
        "buckets" if v.first() == Some(&b'[') => {
            let mut i = 1usize;
            loop {
                skip_ws(v, &mut i);
                if v.get(i) == Some(&b']') || i >= v.len() {
                    break;
                }
                let start = i;
                if skip_value(v, &mut i).is_none() {
                    break;
                }
                let elem = &v[start..i];
                if elem.first() == Some(&b'{') {
                    if let Some(parsed) = parse_quota_bucket(elem) {
                        buckets.push(parsed);
                    }
                }
                skip_ws(v, &mut i);
                if v.get(i) == Some(&b',') {
                    i += 1;
                } else {
                    break;
                }
            }
        }
        _ => {}
    })?;

    if buckets.is_empty() {
        return None;
    }

    let mut group = QuotaGroupInfo {
        label,
        weekly: None,
        five_hour: None,
    };
    let mut unmatched: Vec<QuotaBucketInfo> = Vec::new();

    for (b, win) in buckets {
        let hint = format!("{} {}", b.label, win).to_ascii_lowercase();
        if (hint.contains("week") || hint.contains("7d") || hint.contains("168h"))
            && group.weekly.is_none()
        {
            group.weekly = Some(b);
        } else if (hint.contains("5h") || hint.contains("5 h") || hint.contains("hour"))
            && group.five_hour.is_none()
        {
            group.five_hour = Some(b);
        } else {
            unmatched.push(b);
        }
    }

    for b in unmatched {
        if group.weekly.is_none() {
            group.weekly = Some(b);
        } else if group.five_hour.is_none() {
            group.five_hour = Some(b);
        }
    }

    if group.has_any() { Some(group) } else { None }
}

fn parse_quota_summary_json(b: &[u8]) -> QuotaSummaryParsed {
    let mut inner: Option<Vec<u8>> = None;
    let _ = parse_json_object_entries(b, |k, v| {
        if k == "response" && v.first() == Some(&b'{') {
            inner = Some(v.to_vec());
        }
    });
    let root = inner.as_deref().unwrap_or(b);

    let mut groups: Vec<QuotaGroupInfo> = Vec::new();
    let _ = parse_json_object_entries(root, |k, v| {
        if k == "groups" && v.first() == Some(&b'[') {
            let mut i = 1usize;
            loop {
                skip_ws(v, &mut i);
                if v.get(i) == Some(&b']') || i >= v.len() {
                    break;
                }
                let start = i;
                if skip_value(v, &mut i).is_none() {
                    break;
                }
                let elem = &v[start..i];
                if elem.first() == Some(&b'{') {
                    if let Some(g) = parse_quota_group(elem) {
                        groups.push(g);
                    }
                }
                skip_ws(v, &mut i);
                if v.get(i) == Some(&b',') {
                    i += 1;
                } else {
                    break;
                }
            }
        }
    });

    let mut out = QuotaSummaryParsed::default();
    for g in groups {
        let lower = g.label.to_ascii_lowercase();
        if lower.contains("gemini") {
            if out.gemini.is_none() {
                out.gemini = Some(g);
            }
        } else if out.other.is_none() {
            out.other = Some(g);
        }
    }
    out
}

fn handle_get_quota(state: &AppState, force: bool) -> String {
    if let Ok(guard) = state.quota_cache.lock() {
        let fresh = if force {
            guard
                .last_force_at
                .is_some_and(|ts| ts.elapsed() < Duration::from_millis(800))
        } else if let Some(ts) = guard.fetched_at {
            let max_age = if guard.ok {
                Duration::from_secs(8)
            } else {
                Duration::from_secs(15)
            };
            ts.elapsed() < max_age
        } else {
            false
        };
        if fresh && !guard.json.is_empty() {
            return guard.json.clone();
        }
    }

    let req_body = if force {
        r#"{"forceRefresh":true}"#
    } else {
        "{}"
    };

    let (ok, json) = match resolve_ls_creds(state) {
        Ok((host_port, csrf)) => match post_ls_json_bytes(
            host_port,
            csrf,
            "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary",
            req_body,
            Duration::from_secs(5),
            512 * 1024,
        ) {
            Ok((status, body_bytes)) if (200..=299).contains(&status) && !body_bytes.is_empty() => {
                let parsed = parse_quota_summary_json(&body_bytes);
                let has_data = parsed.has_any();
                (has_data, parsed.to_json())
            }
            _ => (false, r#"{"available":false}"#.to_string()),
        },
        Err(_) => (false, r#"{"available":false}"#.to_string()),
    };

    if let Ok(mut guard) = state.quota_cache.lock() {
        let now = Instant::now();
        guard.fetched_at = Some(now);
        if force {
            guard.last_force_at = Some(now);
        }
        guard.ok = ok;
        guard.json = json.clone();
    }
    json
}

fn maybe_prune_deleted_chats(
    state: &AppState,
    active_conv: &str,
    live_convs: &HashMap<String, bool>,
) {
    if live_convs.is_empty() {
        return;
    }
    let should_run = match state.last_prune_at.lock() {
        Ok(mut guard) => match *guard {
            Some(ts) if ts.elapsed() < Duration::from_secs(600) => false,
            _ => {
                *guard = Some(Instant::now());
                true
            }
        },
        Err(_) => false,
    };
    if !should_run {
        return;
    }
    let _ = prune_deleted_chats(state, active_conv, live_convs, Duration::from_secs(60));
}

fn prune_deleted_chats(
    state: &AppState,
    active_conv: &str,
    live_convs: &HashMap<String, bool>,
    min_note_age: Duration,
) -> usize {
    if live_convs.is_empty() {
        return 0;
    }
    let brain_dir = state.app_data.join("brain");
    if !brain_dir.is_dir() {
        return 0;
    }

    let mut removed = 0usize;
    let mut removed_ids: Vec<String> = Vec::new();

    if let Ok(_notes_guard) = state.notes_lock.lock() {
        if let Ok(entries) = fs::read_dir(&state.notes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let conv_id = if let Some(stem) = file_name.strip_suffix(".json") {
                    stem
                } else if let Some(stem) = file_name.strip_suffix(".json.bak") {
                    stem
                } else {
                    continue;
                };
                if conv_id == DRAFT_CONV_ID
                    || (!active_conv.is_empty() && conv_id == active_conv)
                    || !is_valid_conv_id(conv_id)
                {
                    continue;
                }
                if live_convs.contains_key(conv_id) || brain_dir.join(conv_id).exists() {
                    continue;
                }
                if !min_note_age.is_zero() {
                    let is_recent = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age < min_note_age);
                    if is_recent {
                        continue;
                    }
                }
                if fs::remove_file(&path).is_ok() {
                    removed = removed.saturating_add(1);
                    if !removed_ids.iter().any(|id| id == conv_id) {
                        removed_ids.push(conv_id.to_string());
                    }
                }
            }
        }
    }

    if let Ok(mut store) = state.settings.lock() {
        let mut settings_changed = false;
        let orphan_chats: Vec<String> = store
            .chats
            .keys()
            .filter(|id| {
                id.as_str() != DRAFT_CONV_ID
                    && (active_conv.is_empty() || id.as_str() != active_conv)
                    && is_valid_conv_id(id)
                    && !live_convs.contains_key(id.as_str())
                    && !brain_dir.join(id).exists()
            })
            .cloned()
            .collect();
        for id in orphan_chats {
            store.chats.remove(&id);
            store.chat_order.retain(|k| k != &id);
            settings_changed = true;
            removed = removed.saturating_add(1);
            if !removed_ids.contains(&id) {
                removed_ids.push(id);
            }
        }

        let orphan_cutoffs: Vec<String> = store
            .cutoffs
            .keys()
            .filter(|id| {
                id.as_str() != DRAFT_CONV_ID
                    && (active_conv.is_empty() || id.as_str() != active_conv)
                    && is_valid_conv_id(id)
                    && !live_convs.contains_key(id.as_str())
                    && !brain_dir.join(id).exists()
            })
            .cloned()
            .collect();
        for id in orphan_cutoffs {
            store.cutoffs.remove(&id);
            store.cutoff_order.retain(|k| k != &id);
            settings_changed = true;
            removed = removed.saturating_add(1);
            if !removed_ids.contains(&id) {
                removed_ids.push(id);
            }
        }

        if settings_changed {
            store.rev = store.rev.saturating_add(1);
            let _ = save_settings_store(&store);
        }
    }

    if !removed_ids.is_empty() {
        if let Ok(mut scans) = state.scans.lock() {
            for id in &removed_ids {
                scans.remove(id);
            }
        }
        if let Ok(mut cache) = state.cache.lock() {
            for id in &removed_ids {
                cache.remove(id);
            }
        }
    }

    removed
}

fn apply_ls_live_to_summary(scan: &mut ChatScanSummary, ls: LsLiveConvStatus) {
    if !ls.known {
        return;
    }
    if ls.is_running || ls.has_bg_task {
        scan.is_busy = true;
        scan.has_bg_work = ls.has_bg_task;
        scan.turn_finished = false;
        scan.undo_step_index = None;
    } else {
        scan.is_busy = false;
        scan.has_bg_work = false;
        scan.compact_in_flight = false;
    }
}

fn resolve_chat_titles(state: &AppState) -> HashMap<String, String> {
    let now = unix_now_secs();
    if let Ok(guard) = state.chat_titles.lock() {
        if !guard.1.is_empty() && now.saturating_sub(guard.0) < 5 {
            return guard.1.clone();
        }
    }

    if let Ok((host_port, csrf)) = resolve_ls_creds(state) {
        if let Ok((status, body_bytes)) = post_ls_json_bytes(
            host_port,
            csrf,
            "/exa.language_server_pb.LanguageServerService/GetAllCascadeTrajectories",
            "{}",
            Duration::from_millis(1800),
            8 * 1024 * 1024,
        ) {
            if (200..=299).contains(&status) && !body_bytes.is_empty() {
                let (parsed, _) = parse_trajectories_live_json(&body_bytes);
                if !parsed.is_empty() {
                    if let Ok(mut guard) = state.chat_titles.lock() {
                        guard.0 = now;
                        guard.1 = parsed.clone();
                    }
                    return parsed;
                }
            }
        }
    }

    state
        .chat_titles
        .lock()
        .map(|g| g.1.clone())
        .unwrap_or_default()
}

fn get_native_cascade_done_mp3(state: &AppState) -> Option<&[u8]> {
    if let Some(cached) = state.done_sound_mp3.get() {
        return Some(cached.as_slice());
    }
    let bytes = fetch_ls_static_asset(state, "/antigravityCascadeDone.mp3").ok()?;
    let _ = state.done_sound_mp3.set(bytes);
    state.done_sound_mp3.get().map(Vec::as_slice)
}

fn fetch_ls_static_asset(state: &AppState, asset_path: &str) -> Result<Vec<u8>, String> {
    let (host_port, csrf) = resolve_ls_creds(state)?;
    let req = format!(
        "GET {asset_path} HTTP/1.0\r\nHost: {host_port}\r\nx-codeium-csrf-token: {csrf}\r\nConnection: close\r\n\r\n"
    );
    let (status, body) = ls_http_request(host_port, &req, Duration::from_secs(5), 65_536)?;
    if status != 200 || body.is_empty() {
        return Err(format!("LS asset returned HTTP {status}"));
    }
    Ok(body)
}

fn read_native_sound_settings(app_data: &Path) -> (bool, u8) {
    let gemini_dir = app_data
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home_dir().join(".gemini"));
    let cfg_path = gemini_dir.join("config").join("config.json");
    let Ok(bytes) = fs::read(cfg_path) else {
        return (false, 100);
    };
    parse_native_sound_settings(&bytes)
}

fn parse_native_sound_settings(b: &[u8]) -> (bool, u8) {
    let mut enabled = false;
    let mut volume: u8 = 100;
    let _ = parse_json_object_entries(b, |k, v| {
        if k == "userSettings" {
            let _ = parse_json_object_entries(v, |uk, uv| {
                let mut idx = 0;
                skip_ws(uv, &mut idx);
                if uk == "enableSoundsForSpecialEvents" {
                    if let Some(val) = parse_bool(uv, &mut idx) {
                        enabled = val;
                    }
                } else if uk == "agentSoundVolume" {
                    if let Some(val) = parse_u64(uv, &mut idx) {
                        volume = val.min(100) as u8;
                    }
                }
            });
        }
    });
    (enabled, volume)
}

struct SearchMatchedRecord {
    step_index: u64,
    role: &'static str,
    created_at: String,
    epoch: u64,
    rank: u8,
    snippet: String,
    text: String,
}

struct SearchMatchResult {
    conv_id: String,
    chat_title: String,
    is_current: bool,
    chat_archived: bool,
    step_index: u64,
    role: &'static str,
    created_at: String,
    archived: bool,
    rank: u8,
    snippet: String,
    text: String,
}

struct SearchMatchWindow {
    snippet_start: usize,
    snippet_end: usize,
    text_start: usize,
    text_end: usize,
    rank: u8,
}

fn parse_search_query_terms(raw_query: &str) -> Vec<String> {
    let trimmed = raw_query.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('«') && trimmed.ends_with('»')))
    {
        let inner = trimmed
            .trim_start_matches(['"', '«'])
            .trim_end_matches(['"', '»'])
            .trim()
            .to_lowercase();
        return if inner.is_empty() {
            Vec::new()
        } else {
            vec![inner]
        };
    }
    trimmed
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

fn find_occurrences_in_lower(lower: &str, needle: &str, limit: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() || limit == 0 {
        return out;
    }
    let mut pos = 0usize;
    while pos <= lower.len() && out.len() < limit {
        let Some(rel) = lower[pos..].find(needle) else {
            break;
        };
        let start = pos + rel;
        let end = start + needle.len();
        out.push((start, end));
        pos = end.max(start + 1);
    }
    out
}

fn match_query_best_window(text: &str, terms: &[String]) -> Option<SearchMatchWindow> {
    if terms.is_empty() {
        return None;
    }

    if terms.len() == 1 {
        let (start, end) = find_case_insensitive_byte_range(text, &terms[0])?;
        return Some(SearchMatchWindow {
            snippet_start: start,
            snippet_end: end,
            text_start: start,
            text_end: end,
            rank: 0,
        });
    }

    let lower = text.to_lowercase();
    for term in terms {
        if !lower.contains(term.as_str()) {
            return None;
        }
    }

    let exact_phrase = terms.join(" ");

    // Fallback when lowercase changes UTF-8 byte lengths (e.g. dotted I)
    if text.len() != lower.len() {
        if let Some((start, end)) = find_case_insensitive_byte_range(text, &exact_phrase) {
            return Some(SearchMatchWindow {
                snippet_start: start,
                snippet_end: end,
                text_start: start,
                text_end: end,
                rank: 0,
            });
        }
        let mut first_range: Option<(usize, usize)> = None;
        for term in terms {
            let range = find_case_insensitive_byte_range(text, term)?;
            if first_range.is_none() {
                first_range = Some(range);
            }
        }
        let (start, end) = first_range?;
        return Some(SearchMatchWindow {
            snippet_start: start,
            snippet_end: end,
            text_start: start,
            text_end: end,
            rank: 2,
        });
    }

    // Tier 0: exact phrase match in order
    if let Some(start) = lower.find(&exact_phrase) {
        let end = start + exact_phrase.len();
        if text.is_char_boundary(start) && text.is_char_boundary(end) {
            return Some(SearchMatchWindow {
                snippet_start: start,
                snippet_end: end,
                text_start: start,
                text_end: end,
                rank: 0,
            });
        }
    }

    // Deduplicate terms while preserving order for minimum covering window search
    let mut unique_terms: Vec<&str> = Vec::with_capacity(terms.len());
    for t in terms {
        if !unique_terms.contains(&t.as_str()) {
            unique_terms.push(t.as_str());
        }
    }
    if unique_terms.len() == 1 {
        let (start, end) = find_case_insensitive_byte_range(text, unique_terms[0])?;
        return Some(SearchMatchWindow {
            snippet_start: start,
            snippet_end: end,
            text_start: start,
            text_end: end,
            rank: 0,
        });
    }

    let mut occs: Vec<Vec<(usize, usize)>> = Vec::with_capacity(unique_terms.len());
    for term in &unique_terms {
        let list = find_occurrences_in_lower(&lower, term, 64);
        if list.is_empty() {
            return None;
        }
        occs.push(list);
    }

    let mut ptrs = vec![0usize; occs.len()];
    let mut best: Option<(usize, usize, usize, usize)> = None;
    loop {
        let mut min_ti = 0usize;
        let mut min_start = usize::MAX;
        let mut min_first_end = 0usize;
        let mut max_end = 0usize;

        for (ti, list) in occs.iter().enumerate() {
            let (s, e) = list[ptrs[ti]];
            if s < min_start {
                min_start = s;
                min_first_end = e;
                min_ti = ti;
            }
            if e > max_end {
                max_end = e;
            }
        }

        let span = max_end.saturating_sub(min_start);
        if best.is_none_or(|(_, _, _, best_span)| span < best_span) {
            best = Some((min_start, max_end, min_first_end, span));
        }

        ptrs[min_ti] += 1;
        if ptrs[min_ti] >= occs[min_ti].len() {
            break;
        }
    }

    let (win_start, win_end, first_end, _) = best?;
    if !text.is_char_boundary(win_start)
        || !text.is_char_boundary(win_end)
        || !text.is_char_boundary(first_end)
    {
        let (start, end) = find_case_insensitive_byte_range(text, &terms[0])?;
        return Some(SearchMatchWindow {
            snippet_start: start,
            snippet_end: end,
            text_start: start,
            text_end: end,
            rank: 2,
        });
    }

    let span_chars = text[win_start..win_end].chars().count();
    if span_chars <= SEARCH_CLOSE_SPAN_CHARS {
        Some(SearchMatchWindow {
            snippet_start: win_start,
            snippet_end: win_end,
            text_start: win_start,
            text_end: win_end,
            rank: 1,
        })
    } else {
        Some(SearchMatchWindow {
            snippet_start: win_start,
            snippet_end: first_end,
            text_start: win_start,
            text_end: win_end,
            rank: 2,
        })
    }
}

fn handle_get_search(
    state: &AppState,
    conv: &str,
    raw_query: &str,
    scope: &str,
) -> (u16, String) {
    // Advance generation for every search request so any in-flight All-chats scan stops immediately
    let my_gen = state.search_gen.fetch_add(1, Ordering::AcqRel) + 1;
    let q_trimmed = raw_query.trim();
    if q_trimmed.is_empty() {
        return (200, r#"{"results":[],"total":0}"#.to_string());
    }

    let search_all = scope.eq_ignore_ascii_case("all");
    let (results, total) = if search_all {
        let cutoffs = state
            .settings
            .lock()
            .map(|s| s.cutoffs.clone())
            .unwrap_or_default();
        let titles = resolve_chat_titles(state);
        let cancel = Some((state.search_gen.as_ref(), my_gen));
        let skip_notes_conv = if conv.is_empty() { "__draft__" } else { conv };
        let (mut note_results, note_total) = search_all_notes(
            &state.notes_dir,
            &state.app_data,
            skip_notes_conv,
            q_trimmed,
            &titles,
            cancel,
        );
        let (msg_results, msg_total) =
            search_all_transcripts(&state.app_data, conv, q_trimmed, &cutoffs, &titles, cancel);
        note_results.extend(msg_results);
        note_results.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then_with(|| (a.role != "note").cmp(&(b.role != "note")))
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.step_index.cmp(&a.step_index))
        });
        if note_results.len() > MAX_SEARCH_RESULTS {
            note_results.truncate(MAX_SEARCH_RESULTS);
        }
        (note_results, note_total.saturating_add(msg_total))
    } else {
        if is_draft_conv(conv) || !is_valid_conv_id(conv) {
            return (200, r#"{"results":[],"total":0}"#.to_string());
        }
        let path = transcript_path(&state.app_data, conv);
        if !path.exists() {
            return (200, r#"{"results":[],"total":0}"#.to_string());
        }
        let cutoff = state
            .settings
            .lock()
            .ok()
            .and_then(|s| s.cutoffs.get(conv).copied());
        search_transcript(&path, q_trimmed, cutoff)
    };

    let mut out = String::from(r#"{"results":["#);
    for (idx, r) in results.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"conv_id":"{}","chat_title":"{}","is_current":{},"chat_archived":{},"step_index":{},"role":"{}","created_at":"{}","archived":{},"rank":{},"snippet":"{}","text":"{}"}}"#,
            json_escape(&r.conv_id),
            json_escape(&r.chat_title),
            r.is_current,
            r.chat_archived,
            r.step_index,
            r.role,
            json_escape(&r.created_at),
            r.archived,
            r.rank,
            json_escape(&r.snippet),
            json_escape(&r.text),
        ));
    }
    out.push_str(&format!(r#"],"total":{total}}}"#));
    (200, out)
}

fn search_transcript_with_fallback_title(
    path: &Path,
    query: &str,
    cutoff: Option<RevertCutoff>,
) -> (Vec<SearchMatchResult>, usize, Option<String>) {
    let terms = parse_search_query_terms(query);
    if terms.is_empty() {
        return (Vec::new(), 0, None);
    }

    let Ok(f) = File::open(path) else {
        return (Vec::new(), 0, None);
    };
    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut buf = Vec::with_capacity(4096);
    let mut offset: u64 = 0;
    let mut step_history: Vec<(u64, bool)> = Vec::new();
    let mut matched_records: Vec<SearchMatchedRecord> = Vec::new();
    let mut epoch: u64 = 0;
    let mut first_user_title: Option<String> = None;

    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let line_end = offset.saturating_add(n as u64);
        offset = line_end;
        if !buf.ends_with(b"\n") {
            break;
        }
        let trimmed = trim_ascii(&buf);
        if trimmed.is_empty() {
            continue;
        }
        let Some((step_index, kind, created_at, content_pos)) = parse_search_line(trimmed) else {
            continue;
        };
        if cutoff.is_some_and(|c| line_end <= c.bytes_len && step_index > c.max_step) {
            continue;
        }

        // Handle historical branch rewind consistently with ChatScanState
        if step_history
            .last()
            .is_some_and(|&(last_idx, _)| step_index <= last_idx)
        {
            step_history.retain(|&(idx, _)| idx < step_index);
            matched_records.retain(|r| r.step_index < step_index);
            epoch = step_history.iter().filter(|&&(_, is_sum)| is_sum).count() as u64;
        }

        let role = match kind {
            StepKind::UserInput => "user",
            StepKind::PlannerResponse => "agent",
            StepKind::Checkpoint => "summary",
            _ => continue,
        };

        let is_summary = role == "summary";
        step_history.push((step_index, is_summary));

        let Some(pos) = content_pos else {
            if is_summary {
                epoch = epoch.saturating_add(1);
            }
            continue;
        };
        let mut c_idx = pos;
        let Some(raw_content) = parse_string(trimmed, &mut c_idx) else {
            if is_summary {
                epoch = epoch.saturating_add(1);
            }
            continue;
        };

        if role == "user" && is_compact_request_text(&raw_content) {
            continue;
        }

        let clean_text = if role == "user" {
            extract_clean_user_text(&raw_content)
        } else {
            raw_content.trim().to_string()
        };

        if role == "user" && first_user_title.is_none() && !clean_text.is_empty() {
            let one_line: String = clean_text
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect();
            first_user_title = Some(truncate_chars(one_line.trim(), 60));
        }

        let rec_epoch = epoch;
        if is_summary {
            epoch = epoch.saturating_add(1);
        }

        if clean_text.is_empty() {
            continue;
        }

        if let Some(win) = match_query_best_window(&clean_text, &terms) {
            matched_records.push(SearchMatchedRecord {
                step_index,
                role,
                created_at,
                epoch: rec_epoch,
                rank: win.rank,
                snippet: build_search_snippet(&clean_text, win.snippet_start, win.snippet_end),
                text: truncate_around_match(
                    &clean_text,
                    win.text_start,
                    win.text_end,
                    MAX_SEARCH_TEXT_CHARS,
                ),
            });
        }
    }

    let total = matched_records.len();
    matched_records.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.step_index.cmp(&a.step_index))
    });

    let mut matches = Vec::with_capacity(total.min(MAX_SEARCH_RESULTS));
    for rec in matched_records.into_iter().take(MAX_SEARCH_RESULTS) {
        let archived = if rec.role == "summary" {
            rec.epoch.saturating_add(1) < epoch
        } else {
            rec.epoch < epoch
        };
        matches.push(SearchMatchResult {
            conv_id: String::new(),
            chat_title: String::new(),
            is_current: false,
            chat_archived: false,
            step_index: rec.step_index,
            role: rec.role,
            created_at: rec.created_at,
            archived,
            rank: rec.rank,
            snippet: rec.snippet,
            text: rec.text,
        });
    }

    (matches, total, first_user_title)
}

fn search_transcript(
    path: &Path,
    query: &str,
    cutoff: Option<RevertCutoff>,
) -> (Vec<SearchMatchResult>, usize) {
    let (matches, total, _) = search_transcript_with_fallback_title(path, query, cutoff);
    (matches, total)
}

fn search_all_transcripts(
    app_data: &Path,
    current_conv: &str,
    query: &str,
    cutoffs: &HashMap<String, RevertCutoff>,
    titles: &HashMap<String, String>,
    cancel_check: Option<(&AtomicUsize, usize)>,
) -> (Vec<SearchMatchResult>, usize) {
    let brain_dir = app_data.join("brain");
    let Ok(entries) = fs::read_dir(&brain_dir) else {
        return (Vec::new(), 0);
    };

    let mut candidates: Vec<(u128, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_valid_conv_id(&name) {
            continue;
        }
        let t_path = entry
            .path()
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl");
        let Ok(meta) = fs::metadata(&t_path) else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        candidates.push((mtime_ms, name, t_path));
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    if candidates.len() > MAX_ALL_SEARCH_CHATS {
        candidates.truncate(MAX_ALL_SEARCH_CHATS);
    }

    let mut all_matches: Vec<SearchMatchResult> = Vec::new();
    let mut grand_total: usize = 0;

    for (_, conv_id, t_path) in candidates {
        if let Some((gen_ref, expected)) = cancel_check {
            if gen_ref.load(Ordering::Acquire) != expected {
                return (Vec::new(), 0);
            }
        }
        let cutoff = cutoffs.get(&conv_id).copied();
        let (mut chat_matches, chat_total, fallback_title) =
            search_transcript_with_fallback_title(&t_path, query, cutoff);
        if chat_total == 0 {
            continue;
        }
        grand_total = grand_total.saturating_add(chat_total);
        let title = titles
            .get(&conv_id)
            .cloned()
            .or(fallback_title)
            .unwrap_or_else(|| truncate_chars(&conv_id, 12));
        let is_cur = !current_conv.is_empty() && conv_id == current_conv;
        let is_chat_archived = !titles.is_empty() && !is_cur && !titles.contains_key(&conv_id);
        for m in &mut chat_matches {
            m.conv_id = conv_id.clone();
            m.chat_title = title.clone();
            m.is_current = is_cur;
            m.chat_archived = is_chat_archived;
        }
        all_matches.extend(chat_matches);
    }

    all_matches.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.step_index.cmp(&a.step_index))
    });
    if all_matches.len() > MAX_SEARCH_RESULTS {
        all_matches.truncate(MAX_SEARCH_RESULTS);
    }
    (all_matches, grand_total)
}

fn read_transcript_fallback_title(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(16 * 1024, f);
    let mut buf = Vec::with_capacity(2048);
    for _ in 0..32 {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 || !buf.ends_with(b"\n") {
            break;
        }
        let trimmed = trim_ascii(&buf);
        let Some((_, kind, _, content_pos)) = parse_search_line(trimmed) else {
            continue;
        };
        if kind != StepKind::UserInput {
            continue;
        }
        let Some(pos) = content_pos else {
            continue;
        };
        let mut c_idx = pos;
        let Some(raw) = parse_string(trimmed, &mut c_idx) else {
            continue;
        };
        if is_compact_request_text(&raw) {
            continue;
        }
        let clean = extract_clean_user_text(&raw);
        let trimmed_clean = clean.trim();
        if trimmed_clean.is_empty() {
            continue;
        }
        let one_line: String = trimmed_clean
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        return Some(truncate_chars(one_line.trim(), 60));
    }
    None
}

fn search_all_notes(
    notes_dir: &Path,
    app_data: &Path,
    skip_conv: &str,
    query: &str,
    titles: &HashMap<String, String>,
    cancel_check: Option<(&AtomicUsize, usize)>,
) -> (Vec<SearchMatchResult>, usize) {
    let terms = parse_search_query_terms(query);
    if terms.is_empty() {
        return (Vec::new(), 0);
    }
    let Ok(entries) = fs::read_dir(notes_dir) else {
        return (Vec::new(), 0);
    };

    let mut candidates: Vec<(u128, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !skip_conv.is_empty() && stem == skip_conv {
            continue;
        }
        if !is_valid_conv_id(stem) && !is_draft_conv(stem) {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        candidates.push((mtime_ms, stem.to_string(), path));
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    if candidates.len() > MAX_ALL_SEARCH_CHATS {
        candidates.truncate(MAX_ALL_SEARCH_CHATS);
    }

    let mut note_matches: Vec<SearchMatchResult> = Vec::new();
    for (_, conv_id, path) in candidates {
        if let Some((gen_ref, expected)) = cancel_check {
            if gen_ref.load(Ordering::Acquire) != expected {
                return (Vec::new(), 0);
            }
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Some(data) = parse_notes_data(&bytes) else {
            continue;
        };
        let matched_cards: Vec<(u64, &str, SearchMatchWindow)> = data
            .cards
            .iter()
            .filter_map(|c| {
                let txt = c.text.trim();
                if txt.is_empty() {
                    return None;
                }
                match_query_best_window(txt, &terms).map(|w| (c.id, txt, w))
            })
            .collect();
        if matched_cards.is_empty() {
            continue;
        }

        let title = if is_draft_conv(&conv_id) {
            "New Chat Draft".to_string()
        } else {
            titles
                .get(&conv_id)
                .cloned()
                .or_else(|| read_transcript_fallback_title(&transcript_path(app_data, &conv_id)))
                .unwrap_or_else(|| truncate_chars(&conv_id, 12))
        };
        let is_chat_archived =
            !titles.is_empty() && !is_draft_conv(&conv_id) && !titles.contains_key(&conv_id);

        for (card_id, txt, win) in matched_cards {
            note_matches.push(SearchMatchResult {
                conv_id: conv_id.clone(),
                chat_title: title.clone(),
                is_current: false,
                chat_archived: is_chat_archived,
                step_index: card_id,
                role: "note",
                created_at: String::new(),
                archived: false,
                rank: win.rank,
                snippet: build_search_snippet(txt, win.snippet_start, win.snippet_end),
                text: truncate_around_match(txt, win.text_start, win.text_end, MAX_SEARCH_TEXT_CHARS),
            });
        }
    }

    let total = note_matches.len();
    note_matches.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| b.step_index.cmp(&a.step_index))
    });
    if note_matches.len() > MAX_SEARCH_RESULTS {
        note_matches.truncate(MAX_SEARCH_RESULTS);
    }
    (note_matches, total)
}

fn parse_search_line(b: &[u8]) -> Option<(u64, StepKind, String, Option<usize>)> {
    let mut step_index = 0u64;
    let mut kind = StepKind::Other;
    let mut created_at = String::new();
    let mut content_pos: Option<usize> = None;

    parse_json_object_entries(b, |key, val_slice| {
        let mut idx = 0;
        match key.as_str() {
            "step_index" => {
                if let Some(n) = parse_u64(val_slice, &mut idx) {
                    step_index = n;
                }
            }
            "type" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    kind = StepKind::from_str(&s);
                }
            }
            "created_at" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    created_at = s;
                }
            }
            "content" if val_slice.first() == Some(&b'"') => {
                let offset = val_slice.as_ptr() as usize - b.as_ptr() as usize;
                content_pos = Some(offset);
            }
            _ => {}
        }
    })?;

    Some((step_index, kind, created_at, content_pos))
}

fn extract_clean_user_text(raw: &str) -> String {
    if let Some(open_pos) = raw.rfind("<USER_REQUEST>") {
        let after_open = &raw[open_pos + "<USER_REQUEST>".len()..];
        if let Some((inner, _)) = after_open.split_once("</USER_REQUEST>") {
            return inner.trim().to_string();
        }
    }
    let without_summary = if let Some(cs_end) = raw.rfind("</CONTEXT_SUMMARY>") {
        &raw[cs_end + "</CONTEXT_SUMMARY>".len()..]
    } else {
        raw
    };
    let clean = match without_summary.split_once("<ADDITIONAL_METADATA>") {
        Some((before, _)) => before,
        None => without_summary,
    };
    clean.trim().to_string()
}

fn find_case_insensitive_byte_range(text: &str, q_lower: &str) -> Option<(usize, usize)> {
    let lower = text.to_lowercase();
    let lower_start = lower.find(q_lower)?;
    if text.len() == lower.len() && text.is_char_boundary(lower_start) {
        let end = lower_start + q_lower.len();
        if text.is_char_boundary(end) {
            return Some((lower_start, end));
        }
    }

    // Fallback for characters whose lowercase representation changes byte length (e.g. İ)
    let q_chars: Vec<char> = q_lower.chars().collect();
    if q_chars.is_empty() {
        return None;
    }
    let orig_chars: Vec<(usize, char)> = text.char_indices().collect();
    for start_idx in 0..orig_chars.len() {
        let mut qi = 0usize;
        let mut end_idx = start_idx;
        while end_idx < orig_chars.len() && qi < q_chars.len() {
            let mut matched_all = true;
            for lc in orig_chars[end_idx].1.to_lowercase() {
                if qi < q_chars.len() && lc == q_chars[qi] {
                    qi += 1;
                } else {
                    matched_all = false;
                    break;
                }
            }
            if !matched_all {
                break;
            }
            end_idx += 1;
        }
        if qi == q_chars.len() {
            let byte_start = orig_chars[start_idx].0;
            let byte_end = orig_chars
                .get(end_idx)
                .map(|&(p, _)| p)
                .unwrap_or(text.len());
            return Some((byte_start, byte_end));
        }
    }
    None
}

fn build_search_snippet(text: &str, match_start: usize, match_end: usize) -> String {
    let before = &text[..match_start];
    let after = &text[match_end..];
    let left_chars: Vec<(usize, char)> = before.char_indices().collect();
    let slice_start = if left_chars.len() > 60 {
        left_chars[left_chars.len() - 60].0
    } else {
        0
    };
    let right_end = after
        .char_indices()
        .nth(90)
        .map(|(p, _)| match_end + p)
        .unwrap_or(text.len());

    let mut out = String::new();
    if slice_start > 0 {
        out.push('…');
    }
    let raw_slice = &text[slice_start..right_end];
    let mut prev_ws = false;
    for ch in raw_slice.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    if right_end < text.len() {
        out.push('…');
    }
    out
}

fn truncate_around_match(
    text: &str,
    match_start: usize,
    match_end: usize,
    max_chars: usize,
) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    let before_chars = text[..match_start].chars().count();
    let match_chars = text[match_start..match_end].chars().count();
    let half = max_chars.saturating_sub(match_chars) / 2;
    let start_char = before_chars.saturating_sub(half);
    let end_char = (start_char + max_chars).min(total_chars);
    let byte_start = text
        .char_indices()
        .nth(start_char)
        .map(|(p, _)| p)
        .unwrap_or(0);
    let byte_end = text
        .char_indices()
        .nth(end_char)
        .map(|(p, _)| p)
        .unwrap_or(text.len());
    let mut out = String::new();
    if start_char > 0 {
        out.push_str("…\n");
    }
    out.push_str(&text[byte_start..byte_end]);
    if end_char < total_chars {
        out.push_str("\n…");
    }
    out
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn send_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let conn_hdr = if keep_alive { "keep-alive" } else { "close" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: {conn_hdr}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn is_valid_conv_id(s: &str) -> bool {
    s.len() >= 8
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn update_chat_scan(
    state: &AppState,
    conv: &str,
    path: &Path,
    file_size: u64,
    cutoff: Option<RevertCutoff>,
) -> ChatScanSummary {
    let Ok(mut guard) = state.scans.lock() else {
        return ChatScanSummary::default();
    };
    if guard.len() > 150 && !guard.contains_key(conv) {
        guard.clear();
    }
    let st = guard.entry(conv.to_string()).or_default();

    if file_size < st.offset || st.cutoff_applied != cutoff {
        *st = ChatScanState {
            cutoff_applied: cutoff,
            ..Default::default()
        };
    }
    if file_size == st.offset {
        return st.to_summary();
    }

    let Ok(mut f) = File::open(path) else {
        return st.to_summary();
    };
    if st.offset > 0 && f.seek(SeekFrom::Start(st.offset)).is_err() {
        *st = ChatScanState {
            cutoff_applied: cutoff,
            ..Default::default()
        };
        let _ = f.seek(SeekFrom::Start(0));
    }

    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut buf = Vec::with_capacity(4096);
    let mut busy_partial = false;

    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let ended_with_nl = buf.ends_with(b"\n");
        if !ended_with_nl {
            busy_partial = true;
            break;
        }

        let line_end = st.offset.saturating_add(n as u64);
        st.offset = line_end;
        let trimmed = trim_ascii(&buf);
        if trimmed.is_empty() {
            continue;
        }
        let Some(m) = parse_line(trimmed) else {
            continue;
        };
        if cutoff.is_some_and(|c| line_end <= c.bytes_len && m.step_index > c.max_step) {
            continue;
        }
        push_step_to_scan(st, m);
    }

    let mut summary = st.to_summary();
    if busy_partial {
        summary.is_busy = true;
        summary.turn_finished = false;
    }
    summary
}

fn push_step_to_scan(scan: &mut ChatScanState, m: StepMetrics) {
    let needs_rewind = scan
        .steps
        .last()
        .is_some_and(|last| m.step_index <= last.step_index);
    if needs_rewind {
        scan.steps.retain(|s| s.step_index < m.step_index);
        scan.steps.push(m);
        recompute_scan_summary(scan);
    } else {
        apply_step_to_scan(scan, &m);
        scan.steps.push(m);
    }
}

fn recompute_scan_summary(scan: &mut ChatScanState) {
    let offset = scan.offset;
    let cutoff_applied = scan.cutoff_applied;
    let steps = std::mem::take(&mut scan.steps);
    *scan = ChatScanState {
        offset,
        cutoff_applied,
        ..Default::default()
    };
    for m in &steps {
        apply_step_to_scan(scan, m);
    }
    scan.steps = steps;
}

fn apply_step_to_scan(pending: &mut ChatScanState, m: &StepMetrics) {
    let is_user = m.kind == StepKind::UserInput && !m.is_compact_cmd;
    let is_model_reply =
        m.kind == StepKind::PlannerResponse && m.has_content && !m.has_tool_calls;
    let failed = m.is_failed();

    if m.step_index > pending.latest_step_index {
        pending.latest_step_index = m.step_index;
    }

    if let Some(ref tid) = m.bg_task_started {
        if !pending.active_bg_tasks.contains(tid) {
            pending.active_bg_tasks.push(tid.clone());
        }
    }
    if let Some(ref tid) = m.bg_task_ended {
        pending.active_bg_tasks.retain(|t| t != tid);
    }
    for sid in &m.subagent_started {
        if !pending.active_subagents.contains(sid) {
            pending.active_subagents.push(sid.clone());
        }
    }
    if let Some(ref sid) = m.subagent_ended {
        pending.active_subagents.retain(|s| s != sid);
    }
    if m.clears_all_subagents {
        pending.active_subagents.clear();
    }

    if m.is_compact_cmd {
        pending.compact_in_flight = !failed;
    } else if is_user {
        pending.compact_in_flight = false;
        pending.undo_step_index = None;
        pending.last_non_compact_step = Some(m.step_index);
        pending.user_messages = pending.user_messages.saturating_add(1);
        pending.session_baseline_tokens = Some(if pending.has_context_data {
            pending.context_tokens
        } else {
            0
        });
        pending.delta_tokens = 0;
        pending.has_delta = true;
        pending.just_compressed = false;
    } else {
        if failed || m.kind == StepKind::ErrorMessage {
            pending.compact_in_flight = false;
        }
        if is_model_reply {
            pending.model_messages = pending.model_messages.saturating_add(1);
        }
        if m.kind != StepKind::Checkpoint && !pending.compact_in_flight {
            pending.last_non_compact_step = Some(m.step_index);
            if matches!(
                m.kind,
                StepKind::PlannerResponse | StepKind::Generic | StepKind::ErrorMessage
            ) {
                pending.undo_step_index = None;
                pending.just_compressed = false;
            }
        }
    }

    if is_user {
        pending.pending_user_bytes = pending
            .pending_user_bytes
            .saturating_add(m.content_bytes.max(1));
    } else if matches!(m.kind, StepKind::Generic | StepKind::SystemMessage) {
        pending.pending_tool_bytes = pending
            .pending_tool_bytes
            .saturating_add(m.content_bytes.max(1));
    }

    if m.kind == StepKind::Checkpoint {
        let prev_tokens = if pending.has_context_data {
            pending.context_tokens
        } else {
            0
        };
        pending.checkpoint_count = pending.checkpoint_count.saturating_add(1);
        if m.step_index >= pending.last_checkpoint_step {
            pending.last_checkpoint_step = m.step_index;
        }
        pending.undo_step_index = pending.last_non_compact_step;
        pending.compact_in_flight = false;
        pending.delta_tokens = (m.output_tokens as i64) - (prev_tokens as i64);
        pending.has_delta = true;
        pending.context_tokens = m.output_tokens;
        pending.has_context_data = true;
        pending.just_compressed = true;
        pending.session_baseline_tokens = Some(m.output_tokens);
        pending.breakdown = ContextBreakdown {
            summary: m.output_tokens,
            ..Default::default()
        };
        pending.breakdown_initialized = false;
        pending.prev_planner_output = 0;
        pending.pending_user_bytes = 0;
        pending.pending_tool_bytes = 0;
    } else if m.kind == StepKind::PlannerResponse && m.has_tokens {
        let total = m.cache_read_tokens.saturating_add(m.input_tokens);
        if total > 0 {
            let base = *pending.session_baseline_tokens.get_or_insert(
                if pending.has_context_data {
                    pending.context_tokens
                } else {
                    0
                },
            );
            pending.delta_tokens = (total as i64) - (base as i64);
            pending.has_delta = true;
            pending.context_tokens = total;
            pending.has_context_data = true;

            if !pending.breakdown_initialized {
                let summary_tok = pending.breakdown.summary.min(total);
                let after_summary = total.saturating_sub(summary_tok);
                let est_user = if pending.pending_user_bytes > 0 {
                    (pending.pending_user_bytes / 4).min(after_summary / 2)
                } else {
                    0
                };
                pending.breakdown = ContextBreakdown {
                    system: after_summary.saturating_sub(est_user),
                    summary: summary_tok,
                    tools: 0,
                    replies: 0,
                    user: est_user,
                };
                pending.breakdown_initialized = true;
            } else {
                let prev_total = pending.breakdown.total();
                if total > prev_total {
                    let delta = total - prev_total;
                    let reply_add = pending.prev_planner_output.min(delta);
                    pending.breakdown.replies = pending.breakdown.replies.saturating_add(reply_add);
                    let rem = delta - reply_add;
                    if rem > 0 {
                        let ub = pending.pending_user_bytes;
                        let tb = pending.pending_tool_bytes;
                        if tb > 0 && ub == 0 {
                            pending.breakdown.tools = pending.breakdown.tools.saturating_add(rem);
                        } else if ub > 0 && tb == 0 {
                            pending.breakdown.user = pending.breakdown.user.saturating_add(rem);
                        } else if ub > 0 && tb > 0 {
                            let sum_b = ub.saturating_add(tb);
                            let user_part = ((rem as u128)
                                .saturating_mul(ub as u128)
                                / (sum_b as u128)) as u64;
                            pending.breakdown.user =
                                pending.breakdown.user.saturating_add(user_part);
                            pending.breakdown.tools = pending
                                .breakdown
                                .tools
                                .saturating_add(rem.saturating_sub(user_part));
                        } else {
                            pending.breakdown.system = pending.breakdown.system.saturating_add(rem);
                        }
                    }
                } else if total < prev_total {
                    pending.breakdown.scale_to(total);
                }
            }
            pending.prev_planner_output = m.output_tokens;
            pending.pending_user_bytes = 0;
            pending.pending_tool_bytes = 0;
        }
    }

    if m.model_group != ModelGroup::None {
        pending.active_quota_group = m.model_group;
    }

    if !m.created_at.is_empty() {
        if pending.first_message_at.is_empty() && (is_user || is_model_reply) {
            pending.first_message_at = m.created_at.clone();
        }
        if is_user || is_model_reply || pending.last_message_at.is_empty() {
            pending.last_message_at = m.created_at.clone();
        }
    }

    pending.turn_finished = m.kind == StepKind::PlannerResponse
        && m.status == StepStatus::Done
        && !m.has_tool_calls
        && !failed;

    pending.is_busy = !failed
        && (m.status == StepStatus::Running
            || m.kind == StepKind::UserInput
            || m.kind == StepKind::SystemMessage
            || (m.kind == StepKind::PlannerResponse && m.has_tool_calls)
            || m.kind == StepKind::Generic);
}

fn build_context_json(state: &AppState, conv: &str, req_etag: &str) -> String {
    maybe_promote_draft(state, conv);

    let (chat_settings, cutoff, srev) = match state.settings.lock() {
        Ok(store) => (
            store.get_for_chat(conv),
            store.cutoffs.get(conv).copied(),
            store.rev,
        ),
        Err(_) => (Settings::default(), None, 0),
    };
    let settings_json = chat_settings.to_json();

    if is_draft_conv(conv) || !is_valid_conv_id(conv) {
        let etag = format!("none:s{srev}");
        if req_etag == etag {
            return format!(r#"{{"unchanged":true,"etag":"{}"}}"#, json_escape(&etag));
        }
        return format!(
            r#"{{"status":"no_conversation","etag":"{}","settings":{}}}"#,
            json_escape(&etag),
            settings_json
        );
    }

    let path = transcript_path(&state.app_data, conv);

    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => {
            let etag = format!("{conv}:missing:s{srev}");
            if req_etag == etag {
                return format!(r#"{{"unchanged":true,"etag":"{}"}}"#, json_escape(&etag));
            }
            return format!(
                r#"{{"status":"no_transcript","etag":"{}","settings":{}}}"#,
                json_escape(&etag),
                settings_json
            );
        }
    };

    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let ls_live = resolve_ls_live_status(state, conv);
    let ls_tag = if ls_live.known {
        format!(
            ":l{}{}",
            if ls_live.is_running { 1 } else { 0 },
            if ls_live.has_bg_task { 1 } else { 0 }
        )
    } else {
        String::new()
    };

    let etag = format!("{conv}:{size}:{mtime_ms}:s{srev}{ls_tag}");
    if !req_etag.is_empty() && req_etag == etag {
        return format!(r#"{{"unchanged":true,"etag":"{}"}}"#, json_escape(&etag));
    }

    if let Ok(guard) = state.cache.lock() {
        if let Some((cached_etag, cached_json)) = guard.get(conv) {
            if cached_etag == &etag {
                return cached_json.clone();
            }
        }
    }

    if size == 0 {
        return format!(
            r#"{{"status":"empty","etag":"{}","settings":{}}}"#,
            json_escape(&etag),
            settings_json
        );
    }

    let mut scan = update_chat_scan(state, conv, &path, size, cutoff);
    apply_ls_live_to_summary(&mut scan, ls_live);
    let json = format_scan_json(&etag, &settings_json, &scan);

    if let Ok(mut guard) = state.cache.lock() {
        if guard.len() > 150 {
            guard.clear();
        }
        guard.insert(conv.to_string(), (etag, json.clone()));
    }

    json
}

fn format_scan_json(etag: &str, settings_json: &str, scan: &ChatScanSummary) -> String {
    if !scan.has_context_data {
        return format!(
            r#"{{"status":"empty","etag":"{}","settings":{}}}"#,
            json_escape(etag),
            settings_json
        );
    }

    let msg_total = scan.user_messages.saturating_add(scan.model_messages);
    let undo_json = match scan.undo_step_index {
        Some(step) => step.to_string(),
        None => "null".to_string(),
    };

    format!(
        r#"{{"status":"ok","etag":"{}","context_tokens":{},"step_index":{},"message_count":{},"user_messages":{},"model_messages":{},"checkpoint_count":{},"last_checkpoint_step":{},"undo_step_index":{},"compact_in_flight":{},"first_message_at":"{}","last_message_at":"{}","has_delta":{},"delta_tokens":{},"just_compressed":{},"is_busy":{},"has_bg_work":{},"turn_finished":{},"active_quota_group":"{}","breakdown":{{"tools":{},"replies":{},"system":{},"user":{},"summary":{}}},"settings":{}}}"#,
        json_escape(etag),
        scan.context_tokens,
        scan.latest_step_index,
        msg_total,
        scan.user_messages,
        scan.model_messages,
        scan.checkpoint_count,
        scan.last_checkpoint_step,
        undo_json,
        scan.compact_in_flight,
        json_escape(&scan.first_message_at),
        json_escape(&scan.last_message_at),
        scan.has_delta,
        scan.delta_tokens,
        scan.just_compressed,
        scan.is_busy,
        scan.has_bg_work,
        scan.turn_finished,
        scan.active_quota_group.as_str(),
        scan.breakdown.tools,
        scan.breakdown.replies,
        scan.breakdown.system,
        scan.breakdown.user,
        scan.breakdown.summary,
        settings_json,
    )
}

fn is_valid_bg_task_id(id: &str) -> bool {
    let Some((conv_part, task_num)) = id.split_once("/task-") else {
        return false;
    };
    is_valid_conv_id(conv_part)
        && !task_num.is_empty()
        && task_num.bytes().all(|b| b.is_ascii_digit())
}

fn inspect_step_content_signals(txt: &str, m: &mut StepMetrics) {
    if m.kind == StepKind::Generic {
        const BG_START_MARKER: &str = "Tool is running as a background task with task id: ";
        if let Some(pos) = txt.find(BG_START_MARKER) {
            let rest = &txt[pos + BG_START_MARKER.len()..];
            let id = rest.lines().next().unwrap_or("").trim();
            if is_valid_bg_task_id(id) {
                m.bg_task_started = Some(id.to_string());
            }
        }

        if txt.contains("Created the following subagent") {
            let mut search_from = 0usize;
            const CONV_KEY: &str = "\"conversationId\":";
            while let Some(rel) = txt[search_from..].find(CONV_KEY) {
                let after_key = &txt[search_from + rel + CONV_KEY.len()..];
                let trimmed = after_key.trim_start();
                if let Some(after_quote) = trimmed.strip_prefix('"') {
                    if let Some((id, _)) = after_quote.split_once('"') {
                        let id_trim = id.trim();
                        if is_valid_conv_id(id_trim)
                            && !m.subagent_started.iter().any(|s| s == id_trim)
                        {
                            m.subagent_started.push(id_trim.to_string());
                        }
                    }
                }
                search_from += rel + CONV_KEY.len();
            }
        }
        return;
    }

    if m.kind != StepKind::SystemMessage {
        return;
    }

    for marker in ["Task id \"", "Task \""] {
        if let Some(pos) = txt.find(marker) {
            let rest = &txt[pos + marker.len()..];
            if let Some((id, after_id)) = rest.split_once('"') {
                let id_trim = id.trim();
                let tail = after_id.trim_start();
                if is_valid_bg_task_id(id_trim)
                    && (tail.starts_with("finished")
                        || tail.starts_with("was canceled")
                        || tail.starts_with("was cancelled")
                        || tail.starts_with("cancelled.")
                        || tail.starts_with("canceled."))
                {
                    m.bg_task_ended = Some(id_trim.to_string());
                    break;
                }
            }
        }
    }

    if let Some(pos) = txt.find("sender=") {
        let rest = &txt[pos + "sender=".len()..];
        let sender = rest.split_whitespace().next().unwrap_or("").trim();
        if is_valid_conv_id(sender) {
            m.subagent_ended = Some(sender.to_string());
        }
    }
}

fn inspect_tool_calls_signals(raw_tc: &[u8], m: &mut StepMetrics) {
    if find_subslice(raw_tc, b"manage_subagents").is_none() {
        return;
    }
    if find_subslice(raw_tc, b"kill").is_some() {
        m.clears_all_subagents = true;
    }
}

fn parse_line(b: &[u8]) -> Option<StepMetrics> {
    let mut m = StepMetrics::default();
    let mut raw_content: Option<Vec<u8>> = None;

    parse_json_object_entries(b, |key, val_slice| {
        let mut idx = 0;
        match key.as_str() {
            "type" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    m.kind = StepKind::from_str(&s);
                }
            }
            "status" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    m.status = StepStatus::from_str(&s);
                }
            }
            "model" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    m.model_group = ModelGroup::from_model_name(&s);
                }
            }
            "created_at" if val_slice.first() == Some(&b'"') => {
                if let Some(s) = parse_string(val_slice, &mut idx) {
                    m.created_at = s;
                }
            }
            "step_index" => {
                if let Some(n) = parse_u64(val_slice, &mut idx) {
                    m.step_index = n;
                }
            }
            "input_tokens" => {
                if let Some(n) = parse_u64(val_slice, &mut idx) {
                    m.input_tokens = n;
                    m.has_tokens = true;
                }
            }
            "cache_read_tokens" => {
                if let Some(n) = parse_u64(val_slice, &mut idx) {
                    m.cache_read_tokens = n;
                    m.has_tokens = true;
                }
            }
            "output_tokens" => {
                if let Some(n) = parse_u64(val_slice, &mut idx) {
                    m.output_tokens = n;
                }
            }
            "content" => {
                if val_slice.first() == Some(&b'"') {
                    m.content_bytes = val_slice.len().saturating_sub(2) as u64;
                    let mut look = 1;
                    skip_ws(val_slice, &mut look);
                    if val_slice.get(look) != Some(&b'"') {
                        m.has_content = true;
                    }
                    let need_compact = find_subslice(val_slice, b"/compact").is_some()
                        || find_subslice(val_slice, b"\\u002fcompact").is_some()
                        || find_subslice(val_slice, b"\\u002Fcompact").is_some();
                    let need_bg = find_subslice(val_slice, b"background task with task id:").is_some()
                        || find_subslice(val_slice, b"Task id ").is_some()
                        || find_subslice(val_slice, b"Task \\\"").is_some()
                        || find_subslice(val_slice, b"Created the following subagent").is_some()
                        || find_subslice(val_slice, b"sender=").is_some();
                    if need_compact || need_bg {
                        raw_content = Some(val_slice.to_vec());
                    }
                } else if !val_slice.starts_with(b"null") {
                    m.has_content = true;
                }
            }
            "tool_calls" if val_slice.first() == Some(&b'[') => {
                let mut look = 1;
                skip_ws(val_slice, &mut look);
                if val_slice.get(look) != Some(&b']') {
                    m.has_tool_calls = true;
                    inspect_tool_calls_signals(val_slice, &mut m);
                }
            }
            "error"
                if !val_slice.starts_with(b"null") && !val_slice.starts_with(b"\"\"") =>
            {
                m.has_error = true;
            }
            _ => {}
        }
    })?;

    if let Some(content_bytes) = raw_content {
        let mut k = 0;
        if let Some(txt) = parse_string(&content_bytes, &mut k) {
            if m.kind == StepKind::UserInput {
                m.is_compact_cmd = is_compact_request_text(&txt);
            }
            if matches!(m.kind, StepKind::Generic | StepKind::SystemMessage) {
                inspect_step_content_signals(&txt, &mut m);
            }
        }
    }

    Some(m)
}

fn parse_json_u64_field(b: &[u8], target: &str) -> Option<u64> {
    let mut found = None;
    parse_json_object_entries(b, |key, val_slice| {
        if found.is_none() && key == target {
            let mut idx = 0;
            found = parse_u64(val_slice, &mut idx);
        }
    })?;
    found
}

fn parse_settings_store_data(b: &[u8]) -> Option<LoadedStoreData> {
    let mut data = LoadedStoreData::default();
    let mut seen_known = false;

    parse_json_object_entries(b, |key, val_slice| {
        if key == "chats" && val_slice.first() == Some(&b'{') {
            seen_known = true;
            let _ = parse_json_object_entries(val_slice, |conv_id, obj| {
                let mut s = Settings::default();
                if parse_settings_patch(obj, &mut s)
                    && conv_id != DRAFT_CONV_ID
                    && is_valid_conv_id(&conv_id)
                {
                    s.auto_compact_threshold = s
                        .auto_compact_threshold
                        .clamp(MIN_AUTO_COMPACT_TOKENS, MAX_AUTO_COMPACT_TOKENS);
                    if !data.chats.contains_key(&conv_id) && data.chats.len() < MAX_STORED_CHATS {
                        data.chat_order.push(conv_id.clone());
                        data.chats.insert(conv_id, s);
                    } else if data.chats.contains_key(&conv_id) {
                        data.chats.insert(conv_id, s);
                    }
                }
            });
        } else if key == "draft" && val_slice.first() == Some(&b'{') {
            seen_known = true;
            data.draft_updated_at = parse_json_u64_field(val_slice, "updated_at").unwrap_or(0);
            if data.draft_updated_at > 0 {
                let mut s = Settings::default();
                if parse_settings_patch(val_slice, &mut s) {
                    s.auto_compact_threshold = s
                        .auto_compact_threshold
                        .clamp(MIN_AUTO_COMPACT_TOKENS, MAX_AUTO_COMPACT_TOKENS);
                    data.draft = s;
                }
            }
        } else if key == "cutoffs" && val_slice.first() == Some(&b'{') {
            seen_known = true;
            let _ = parse_json_object_entries(val_slice, |conv_id, obj| {
                if conv_id != DRAFT_CONV_ID && is_valid_conv_id(&conv_id) {
                    if let (Some(bytes_len), Some(max_step)) = (
                        parse_json_u64_field(obj, "bytes_len"),
                        parse_json_u64_field(obj, "max_step"),
                    ) {
                        if !data.cutoffs.contains_key(&conv_id)
                            && data.cutoffs.len() < MAX_REVERT_CUTOFFS
                        {
                            data.cutoff_order.push(conv_id.clone());
                            data.cutoffs.insert(
                                conv_id,
                                RevertCutoff {
                                    bytes_len,
                                    max_step,
                                },
                            );
                        } else if data.cutoffs.contains_key(&conv_id) {
                            data.cutoffs.insert(
                                conv_id,
                                RevertCutoff {
                                    bytes_len,
                                    max_step,
                                },
                            );
                        }
                    }
                }
            });
        } else if key == "update" && val_slice.first() == Some(&b'{') {
            seen_known = true;
            let max_now = unix_now_secs().saturating_add(60);
            data.update_checked_at = parse_json_u64_field(val_slice, "checked_at")
                .unwrap_or(0)
                .min(max_now);
            if let Some(ver) = parse_json_string_field(val_slice, "latest_version") {
                if let Some(norm) = normalize_semver(&ver) {
                    data.latest_version = norm;
                }
            }
        }
    })?;

    let _ = seen_known;
    Some(data)
}

fn parse_json_object_entries<F: FnMut(String, &[u8])>(b: &[u8], mut on_entry: F) -> Option<()> {
    let mut i = 0;
    skip_ws(b, &mut i);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        skip_ws(b, &mut i);
        if b.get(i) == Some(&b'}') {
            return Some(());
        }
        let key = parse_string(b, &mut i)?;
        skip_ws(b, &mut i);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        skip_ws(b, &mut i);
        let start = i;
        skip_value(b, &mut i)?;
        on_entry(key, &b[start..i]);
        skip_ws(b, &mut i);
        match b.get(i) {
            Some(&b',') => i += 1,
            Some(&b'}') => return Some(()),
            _ => return None,
        }
    }
}

fn parse_settings_patch(b: &[u8], s: &mut Settings) -> bool {
    let mut ok = true;
    let res = parse_json_object_entries(b, |key, val_slice| {
        let mut idx = 0;
        match key.as_str() {
            "auto_compact_threshold" => {
                if let Some(v) = parse_u64(val_slice, &mut idx) {
                    s.auto_compact_threshold = v;
                } else {
                    ok = false;
                }
            }
            "auto_compact_enabled" => {
                if let Some(v) = parse_bool(val_slice, &mut idx) {
                    s.auto_compact_enabled = v;
                } else {
                    ok = false;
                }
            }
            _ => {}
        }
    });
    res.is_some() && ok
}

fn parse_json_string_field(b: &[u8], target: &str) -> Option<String> {
    let mut found = None;
    parse_json_object_entries(b, |key, val_slice| {
        if found.is_none() && key == target && val_slice.first() == Some(&b'"') {
            let mut idx = 0;
            found = parse_string(val_slice, &mut idx);
        }
    })?;
    found
}

fn parse_bool(b: &[u8], i: &mut usize) -> Option<bool> {
    if b[*i..].starts_with(b"true") {
        *i += 4;
        Some(true)
    } else if b[*i..].starts_with(b"false") {
        *i += 5;
        Some(false)
    } else {
        None
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\r' | b'\n') {
        *i += 1;
    }
}

fn skip_string(b: &[u8], i: &mut usize) -> Option<()> {
    if b.get(*i) != Some(&b'"') {
        return None;
    }
    *i += 1;
    while *i < b.len() {
        let c = b[*i];
        *i += 1;
        if c == b'"' {
            return Some(());
        }
        if c == b'\\' {
            if *i >= b.len() {
                return None;
            }
            *i += 1;
        }
    }
    None
}

fn parse_hex4(b: &[u8], i: &mut usize) -> Option<u16> {
    if *i + 4 > b.len() {
        return None;
    }
    let mut val: u16 = 0;
    for _ in 0..4 {
        let d = hex_val(b[*i])? as u16;
        val = (val << 4) | d;
        *i += 1;
    }
    Some(val)
}

fn parse_string(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) != Some(&b'"') {
        return None;
    }
    *i += 1;
    let mut out = Vec::with_capacity(24);
    while *i < b.len() {
        let c = b[*i];
        *i += 1;
        if c == b'"' {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        if c == b'\\' {
            if *i >= b.len() {
                return None;
            }
            let esc = b[*i];
            *i += 1;
            match esc {
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'/' => out.push(b'/'),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0c),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'u' => {
                    let u1 = parse_hex4(b, i)?;
                    let codepoint = if (0xD800..=0xDBFF).contains(&u1) {
                        if *i + 2 <= b.len() && b[*i] == b'\\' && b[*i + 1] == b'u' {
                            *i += 2;
                            let u2 = parse_hex4(b, i)?;
                            if (0xDC00..=0xDFFF).contains(&u2) {
                                0x10000 + (((u1 - 0xD800) as u32) << 10) + ((u2 - 0xDC00) as u32)
                            } else {
                                u1 as u32
                            }
                        } else {
                            u1 as u32
                        }
                    } else {
                        u1 as u32
                    };
                    if let Some(ch) = char::from_u32(codepoint) {
                        let mut utf8_buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut utf8_buf).as_bytes());
                    }
                }
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn parse_u64(b: &[u8], i: &mut usize) -> Option<u64> {
    let start = *i;
    let mut val: u64 = 0;
    while *i < b.len() && b[*i].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add((b[*i] - b'0') as u64);
        *i += 1;
    }
    if *i > start { Some(val) } else { None }
}

fn skip_value(b: &[u8], i: &mut usize) -> Option<()> {
    skip_ws(b, i);
    let c = *b.get(*i)?;
    if c == b'"' {
        return skip_string(b, i);
    }
    if c == b'{' || c == b'[' {
        *i += 1;
        let mut depth: usize = 1;
        while *i < b.len() && depth > 0 {
            match b[*i] {
                b'"' => skip_string(b, i)?,
                b'{' | b'[' => {
                    depth += 1;
                    *i += 1;
                }
                b'}' | b']' => {
                    depth -= 1;
                    *i += 1;
                }
                _ => *i += 1,
            }
        }
        return if depth == 0 { Some(()) } else { None };
    }
    let start = *i;
    while *i < b.len() && !matches!(b[*i], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
        *i += 1;
    }
    if *i > start { Some(()) } else { None }
}

fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let Some(&first) = s.first() {
        if first.is_ascii_whitespace() {
            s = &s[1..];
        } else {
            break;
        }
    }
    while let Some(&last) = s.last() {
        if last.is_ascii_whitespace() {
            s = &s[..s.len() - 1];
        } else {
            break;
        }
    }
    s
}

fn is_compact_request_text(txt: &str) -> bool {
    extract_clean_user_text(txt).eq_ignore_ascii_case("/compact")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((a, b)) => (a, b),
            None => (pair, ""),
        };
        map.insert(url_decode(k), url_decode(v));
    }
    map
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_request_detection_handles_context_summary_and_large_metadata() {
        assert!(is_compact_request_text("/compact"));
        assert!(is_compact_request_text("  /compact \n"));
        assert!(is_compact_request_text(
            "<USER_REQUEST>\n/compact\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\n</ADDITIONAL_METADATA>"
        ));
        assert!(is_compact_request_text(
            "<CONTEXT_SUMMARY>\nOld summary\n</CONTEXT_SUMMARY>\n<USER_REQUEST>\n/compact\n</USER_REQUEST>"
        ));
        assert!(!is_compact_request_text(
            "<USER_REQUEST>\nPlease check /compact handling\n</USER_REQUEST>"
        ));

        // Large >2KB metadata in JSON line must still detect /compact
        let huge_meta = "x".repeat(2400);
        let raw_line = format!(
            r#"{{"step_index":9,"type":"USER_INPUT","status":"DONE","content":"<CONTEXT_SUMMARY>sum</CONTEXT_SUMMARY>\n<USER_REQUEST>\n/compact\n</USER_REQUEST>\n<ADDITIONAL_METADATA>{huge_meta}</ADDITIONAL_METADATA>"}}"#
        );
        let parsed = parse_line(raw_line.as_bytes()).unwrap();
        assert_eq!(parsed.kind, StepKind::UserInput);
        assert!(parsed.is_compact_cmd);

        assert_eq!(
            parse_native_sound_settings(br#"{"userSettings":{"enableSoundsForSpecialEvents":true,"agentSoundVolume":75}}"#),
            (true, 75)
        );
        assert_eq!(
            parse_native_sound_settings(br#"{"userSettings":{}}"#),
            (false, 100)
        );
    }

    #[test]
    fn notes_json_roundtrip_and_disk_lifecycle() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_notes_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let conv = "f7a802c6-bf8d-4297-9f13-0b9b1f90c5f0";
        let empty = read_notes_data(&dir, conv).unwrap();
        assert!(empty.is_empty());

        let payload = br#"{"conversationId":"f7a802c6-bf8d-4297-9f13-0b9b1f90c5f0","draft":"Line 1\nCaf\u00e9 \"world\" \uD83D\uDE80","cards":[{"id":1,"text":"Card one","pinned":true},{"id":2,"text":"Second \u00fcber card"}],"next_id":3}"#;
        let parsed = parse_notes_data(payload).unwrap();
        assert_eq!(parsed.draft, "Line 1\nCafé \"world\" 🚀");
        assert_eq!(parsed.cards.len(), 2);
        assert_eq!(
            parsed.cards[0],
            NoteCard {
                id: 1,
                text: "Card one".to_string(),
                pinned: true,
            }
        );
        assert_eq!(
            parsed.cards[1],
            NoteCard {
                id: 2,
                text: "Second über card".to_string(),
                pinned: false,
            }
        );
        assert_eq!(parsed.next_id, 3);

        write_notes_data(&dir, conv, &parsed).unwrap();
        assert_eq!(read_notes_data(&dir, conv).unwrap(), parsed);

        // Corrupt file is backed up to .json.bak before returning clean default
        let note_file = note_path_for(&dir, conv).unwrap();
        fs::write(&note_file, b"{broken json").unwrap();
        assert!(read_notes_data(&dir, conv).unwrap().is_empty());
        assert!(note_file.with_extension("json.bak").exists());

        // Clearing draft and cards removes the file cleanly
        write_notes_data(&dir, conv, &NotesData { next_id: 3, ..Default::default() }).unwrap();
        assert!(!note_path_for(&dir, conv).unwrap().exists());
        assert!(read_notes_data(&dir, conv).unwrap().is_empty());

        // Traversal attempt rejected
        assert!(write_notes_data(&dir, "../evil", &parsed).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_transcript_handles_unicode_multiword_rewind_and_archived_flag() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_search_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let conv = "f7a802c6-bf8d-4297-9f13-0b9b1f90c5f0";
        let t_path = transcript_path(&dir, conv);
        fs::create_dir_all(t_path.parent().unwrap()).unwrap();

        let lines = concat!(
            r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:00:00Z","content":"<USER_REQUEST>\nDiscuss Façade and palette\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nsecret_meta\n</ADDITIONAL_METADATA>"}"#, "\n",
            r#"{"step_index":1,"type":"GENERIC","status":"DONE","created_at":"2026-11-20T10:00:02Z","content":"façade raw tool output"}"#, "\n",
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:00:05Z","content":"Old reply about façade before retry"}"#, "\n",
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:00:06Z","content":"New reply about FAÇADE after retry"}"#, "\n",
            r#"{"step_index":3,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:01:00Z","content":"/compact"}"#, "\n",
            r#"{"step_index":4,"type":"CHECKPOINT","status":"DONE","created_at":"2026-11-20T10:01:05Z","content":"Summary: approved façade and palette"}"#, "\n",
            r#"{"step_index":5,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:02:00Z","content":"<USER_REQUEST>\nCheck the façade on dark theme again\n</USER_REQUEST>"}"#, "\n"
        );
        fs::write(&t_path, lines).unwrap();

        // Must ignore metadata wrapper
        let (meta_res, meta_total) = search_transcript(&t_path, "secret_meta", None);
        assert_eq!(meta_total, 0);
        assert!(meta_res.is_empty());

        // Case-insensitive Unicode across user, planner, and checkpoint, ignoring GENERIC and overwritten step 2
        let (res, total) = search_transcript(&t_path, "façade", None);
        assert_eq!(total, 4);
        assert_eq!(res.len(), 4);
        assert_eq!(res[0].step_index, 5);
        assert_eq!(res[0].role, "user");
        assert!(!res[0].archived);
        assert_eq!(res[1].step_index, 4);
        assert_eq!(res[1].role, "summary");
        assert!(!res[1].archived);
        assert_eq!(res[2].step_index, 2);
        assert_eq!(res[2].role, "agent");
        assert!(res[2].archived);
        assert!(res[2].text.contains("after retry"));
        assert_eq!(res[3].step_index, 0);
        assert_eq!(res[3].role, "user");
        assert!(res[3].archived);

        // Multi-word out-of-order search ("theme façade" matches "façade on dark theme")
        let (mw_res, mw_total) = search_transcript(&t_path, "theme façade", None);
        assert_eq!(mw_total, 1);
        assert_eq!(mw_res[0].step_index, 5);

        // Quoted exact phrase search ("theme façade" in quotes does NOT match)
        let (_, exact_total) = search_transcript(&t_path, "\"theme façade\"", None);
        assert_eq!(exact_total, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_step_tracks_pre_compact_and_clears_on_next_user_turn() {
        let mut st = ChatScanState::default();

        push_step_to_scan(&mut st, StepMetrics {
            step_index: 10,
            kind: StepKind::UserInput,
            status: StepStatus::Done,
            ..Default::default()
        });
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 11,
            kind: StepKind::PlannerResponse,
            status: StepStatus::Done,
            has_content: true,
            has_tokens: true,
            cache_read_tokens: 200_000,
            input_tokens: 5_000,
            ..Default::default()
        });
        assert_eq!(st.undo_step_index, None);
        assert!(st.has_delta);
        assert_eq!(st.delta_tokens, 205_000);

        // /compact + internal system message + CHECKPOINT
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 12,
            kind: StepKind::UserInput,
            status: StepStatus::Done,
            is_compact_cmd: true,
            ..Default::default()
        });
        assert!(st.compact_in_flight);
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 13,
            kind: StepKind::SystemMessage,
            status: StepStatus::Done,
            ..Default::default()
        });
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 14,
            kind: StepKind::Checkpoint,
            status: StepStatus::Done,
            output_tokens: 18_000,
            ..Default::default()
        });
        assert!(!st.compact_in_flight);
        assert!(st.just_compressed);
        assert_eq!(st.undo_step_index, Some(11));
        assert!(st.has_delta);
        assert_eq!(st.delta_tokens, -187_000);
        assert_eq!(st.user_messages, 1);

        // Subsequent normal user message disables undo and unlocks Compact now
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 15,
            kind: StepKind::UserInput,
            status: StepStatus::Done,
            ..Default::default()
        });
        assert_eq!(st.undo_step_index, None);
        assert!(!st.just_compressed);
        assert!(st.has_delta);
        assert_eq!(st.delta_tokens, 0);
        assert_eq!(st.user_messages, 2);

        // Cancelled or errored tool call step must NOT leave is_busy stuck true
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 16,
            kind: StepKind::PlannerResponse,
            status: StepStatus::FailedOrCancelled,
            has_tool_calls: true,
            ..Default::default()
        });
        assert!(!st.is_busy);

        // Historical step rewind discards overwritten branch cleanly
        push_step_to_scan(&mut st, StepMetrics {
            step_index: 12,
            kind: StepKind::UserInput,
            status: StepStatus::Done,
            ..Default::default()
        });
        assert_eq!(st.user_messages, 2);
        assert_eq!(st.checkpoint_count, 0);
        assert_eq!(st.context_tokens, 205_000);
    }

    #[test]
    fn undo_cutoff_hides_reverted_checkpoint_until_new_steps_arrive() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_cutoff_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let conv = "f7a802c6-bf8d-4297-9f13-0b9b1f90c5f0";
        let t_path = transcript_path(&dir, conv);
        fs::create_dir_all(t_path.parent().unwrap()).unwrap();

        let initial_lines = concat!(
            r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:00:00Z","content":"hi"}"#, "\n",
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:00:05Z","input_tokens":5000,"cache_read_tokens":200000,"content":"ok"}"#, "\n",
            r#"{"step_index":2,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:01:00Z","content":"/compact"}"#, "\n",
            r#"{"step_index":3,"type":"CHECKPOINT","status":"DONE","created_at":"2026-11-20T10:01:05Z","output_tokens":4200}"#, "\n"
        );
        fs::write(&t_path, initial_lines).unwrap();
        let bytes_len = fs::metadata(&t_path).unwrap().len();

        let state = AppState {
            app_data: dir.clone(),
            notes_dir: dir.join("notes"),
            token: None,
            default_conv: None,
            ls_address: None,
            csrf_token: None,
            preload_js: String::new(),
            settings: Arc::new(Mutex::new(SettingsStore {
                chats: HashMap::new(),
                chat_order: Vec::new(),
                draft: Settings::default(),
                draft_updated_at: 0,
                has_pending_draft: false,
                cutoffs: HashMap::new(),
                cutoff_order: Vec::new(),
                update_checked_at: 0,
                latest_version: String::new(),
                rev: 1,
                path: dir.join("settings.json"),
            })),
            notes_lock: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Mutex::new(HashMap::new())),
            chat_titles: Arc::new(Mutex::new((0, HashMap::new()))),
            ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
            quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
            last_prune_at: Arc::new(Mutex::new(None)),
            search_gen: Arc::new(AtomicUsize::new(0)),
            done_sound_mp3: Arc::new(OnceLock::new()),
            active_conns: Arc::new(AtomicUsize::new(0)),
        };

        let before = update_chat_scan(&state, conv, &t_path, bytes_len, None);
        assert_eq!(before.context_tokens, 4_200);
        assert_eq!(before.checkpoint_count, 1);
        assert_eq!(before.undo_step_index, Some(1));

        let cutoff = Some(RevertCutoff { bytes_len, max_step: 1 });
        let after_undo = update_chat_scan(&state, conv, &t_path, bytes_len, cutoff);
        assert_eq!(after_undo.context_tokens, 205_000);
        assert_eq!(after_undo.checkpoint_count, 0);
        assert_eq!(after_undo.undo_step_index, None);

        // Sending a new message after undo appends beyond bytes_len and tracks normally
        let next_line = r#"{"step_index":2,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:02:00Z","content":"next"}"#.to_string() + "\n";
        let mut f = fs::OpenOptions::new().append(true).open(&t_path).unwrap();
        f.write_all(next_line.as_bytes()).unwrap();
        let new_len = fs::metadata(&t_path).unwrap().len();

        let after_msg = update_chat_scan(&state, conv, &t_path, new_len, cutoff);
        assert_eq!(after_msg.user_messages, 2);
        assert_eq!(after_msg.checkpoint_count, 0);
        assert_eq!(after_msg.context_tokens, 205_000);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_promotes_to_new_chat_without_touching_existing_chat() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_draft_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let old_conv = "11111111-1111-1111-1111-111111111111";
        let new_conv = "22222222-2222-2222-2222-222222222222";
        let old_t = transcript_path(&dir, old_conv);
        fs::create_dir_all(old_t.parent().unwrap()).unwrap();
        fs::write(
            &old_t,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","content":"a"}"#, "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","content":"b"}"#, "\n",
                r#"{"step_index":2,"type":"USER_INPUT","status":"DONE","content":"c"}"#, "\n"
            ),
        ).unwrap();

        let state = AppState {
            app_data: dir.clone(),
            notes_dir: dir.join("notes"),
            token: None,
            default_conv: None,
            ls_address: None,
            csrf_token: None,
            preload_js: String::new(),
            settings: Arc::new(Mutex::new(SettingsStore {
                chats: HashMap::new(),
                chat_order: Vec::new(),
                draft: Settings::default(),
                draft_updated_at: 0,
                has_pending_draft: false,
                cutoffs: HashMap::new(),
                cutoff_order: Vec::new(),
                update_checked_at: 0,
                latest_version: String::new(),
                rev: 1,
                path: dir.join("settings.json"),
            })),
            notes_lock: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Mutex::new(HashMap::new())),
            chat_titles: Arc::new(Mutex::new((0, HashMap::new()))),
            ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
            quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
            last_prune_at: Arc::new(Mutex::new(None)),
            search_gen: Arc::new(AtomicUsize::new(0)),
            done_sound_mp3: Arc::new(OnceLock::new()),
            active_conns: Arc::new(AtomicUsize::new(0)),
        };

        let (st1, _) = handle_post_settings(&state, br#"{"conversationId":"","auto_compact_enabled":true,"auto_compact_threshold":210000}"#);
        assert_eq!(st1, 200);
        let (st2, _) = handle_post_notes(&state, br#"{"conversationId":"","draft":"Draft prompt","cards":[{"id":1,"text":"Saved card"}],"next_id":2}"#);
        assert_eq!(st2, 200);

        // Opening old multi-message chat must NOT steal the draft
        maybe_promote_draft(&state, old_conv);
        assert!(read_notes_data(&state.notes_dir, old_conv).unwrap().is_empty());
        assert_eq!(read_notes_data(&state.notes_dir, DRAFT_CONV_ID).unwrap().draft, "Draft prompt");

        // Opening new chat promotes both settings and notes (draft + cards) and clears draft
        maybe_promote_draft(&state, new_conv);
        let promoted_notes = read_notes_data(&state.notes_dir, new_conv).unwrap();
        assert_eq!(promoted_notes.draft, "Draft prompt");
        assert_eq!(promoted_notes.cards.len(), 1);
        assert_eq!(promoted_notes.cards[0].text, "Saved card");
        assert!(read_notes_data(&state.notes_dir, DRAFT_CONV_ID).unwrap().is_empty());
        let store = state.settings.lock().unwrap();
        let promoted = store.get_for_chat(new_conv);
        assert!(promoted.auto_compact_enabled);
        assert_eq!(promoted.auto_compact_threshold, 210_000);
        assert!(!store.get_for_chat("").auto_compact_enabled);
        assert!(!store.has_pending_draft);

        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_untouched_draft_uses_current_default_250k_and_loopback_validation_works() {
        let legacy = br#"{"chats":{},"draft":{"auto_compact_enabled":false,"auto_compact_threshold":300000,"updated_at":0},"cutoffs":{}}"#;
        let parsed = parse_settings_store_data(legacy).unwrap();
        assert_eq!(parsed.draft, Settings::default());
        assert_eq!(parsed.draft.auto_compact_threshold, 250_000);
        assert_eq!(parsed.draft_updated_at, 0);

        let custom = br#"{"chats":{},"draft":{"auto_compact_enabled":false,"auto_compact_threshold":300000,"updated_at":1700000000},"cutoffs":{}}"#;
        let parsed_custom = parse_settings_store_data(custom).unwrap();
        assert_eq!(parsed_custom.draft.auto_compact_threshold, 300_000);
        assert_eq!(parsed_custom.draft_updated_at, 1_700_000_000);

        assert!(is_loopback_host_authority("127.0.0.1:18765"));
        assert!(is_loopback_host_authority("localhost:5387"));
        assert!(is_loopback_host_authority("[::1]:5387"));
        assert!(!is_loopback_host_authority("evil.com:80"));
        assert!(!is_loopback_host_authority("127.0.0.1:80\r\nX-Injected: 1"));
    }

    #[test]
    fn multiword_search_ranks_exact_and_close_matches_and_picks_tightest_snippet() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_rank_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let conv = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let t_path = transcript_path(&dir, conv);
        fs::create_dir_all(t_path.parent().unwrap()).unwrap();

        let filler = "word ".repeat(80); // 400 chars
        let lines = format!(
            concat!(
                r#"{{"step_index":0,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:00:00Z","content":"Older exact deploy pipeline match"}}"#, "\n",
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-21T10:00:00Z","content":"deploy {filler} far away pipeline {filler} but here pipeline and deploy stand close together"}}"#, "\n",
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T10:00:00Z","content":"Newest message with deploy {filler} and much later pipeline"}}"#, "\n"
            ),
            filler = filler
        );
        fs::write(&t_path, lines).unwrap();

        let (res, total) = search_transcript(&t_path, "deploy pipeline", None);
        assert_eq!(total, 3);
        assert_eq!(res.len(), 3);
        // Tier 0 (exact phrase) comes first even though it is older
        assert_eq!(res[0].step_index, 0);
        assert_eq!(res[0].rank, 0);
        // Tier 1 (close proximity) comes second and snippet targets the tight window
        assert_eq!(res[1].step_index, 1);
        assert_eq!(res[1].rank, 1);
        assert!(res[1].snippet.contains("pipeline and deploy stand close"));
        // Tier 2 (scattered) comes last despite being the newest
        assert_eq!(res[2].step_index, 2);
        assert_eq!(res[2].rank, 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_chats_search_merges_titles_current_badge_and_ranking() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_all_search_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let conv_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let conv_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let path_a = transcript_path(&dir, conv_a);
        let path_b = transcript_path(&dir, conv_b);
        fs::create_dir_all(path_a.parent().unwrap()).unwrap();
        fs::create_dir_all(path_b.parent().unwrap()).unwrap();

        fs::write(
            &path_a,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T10:00:00Z","content":"<USER_REQUEST>\nConfigure plugin build\n</USER_REQUEST>"}"#, "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:01:00Z","content":"Build release archive for Linux"}"#, "\n"
            ),
        )
        .unwrap();
        fs::write(
            &path_b,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-22T12:00:00Z","content":"<USER_REQUEST>\nVerify release archive for macOS\n</USER_REQUEST>"}"#, "\n"
            ),
        )
        .unwrap();

        let mut titles = HashMap::new();
        titles.insert(conv_a.to_string(), "Linux Release Chat".to_string());
        let conv_c = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let path_c = transcript_path(&dir, conv_c);
        fs::create_dir_all(path_c.parent().unwrap()).unwrap();
        fs::write(
            &path_c,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T08:00:00Z","content":"<USER_REQUEST>\nArchived release notes\n</USER_REQUEST>"}"#, "\n"
            ),
        )
        .unwrap();
        let cutoffs = HashMap::new();

        let (res, total) = search_all_transcripts(&dir, conv_b, "release", &cutoffs, &titles, None);
        assert_eq!(total, 3);
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].conv_id, conv_b);
        assert!(res[0].is_current);
        assert!(!res[0].chat_archived);
        assert_eq!(res[0].chat_title, "Verify release archive for macOS");
        assert_eq!(res[1].conv_id, conv_a);
        assert!(!res[1].is_current);
        assert!(!res[1].chat_archived);
        assert_eq!(res[1].chat_title, "Linux Release Chat");
        assert_eq!(res[2].conv_id, conv_c);
        assert!(!res[2].is_current);
        assert!(res[2].chat_archived);
        assert_eq!(res[2].chat_title, "Archived release notes");

        let empty_titles = HashMap::new();
        let (res_no_ls, _) = search_all_transcripts(&dir, conv_b, "release", &cutoffs, &empty_titles, None);
        assert!(res_no_ls.iter().all(|r| !r.chat_archived));

        let n_dir = dir.join("notes");
        fs::create_dir_all(&n_dir).unwrap();
        fs::write(
            n_dir.join(format!("{conv_c}.json")),
            r#"{"draft":"","next_id":3,"cards":[{"id":1,"text":"Remember release checklist for Linux","pinned":true},{"id":2,"text":"release notes scattered far away from archive word here: archive","pinned":false}]}"#,
        )
        .unwrap();
        let (note_res, note_total) = search_all_notes(&n_dir, &dir, conv_b, "release", &titles, None);
        assert_eq!(note_total, 2);
        assert_eq!(note_res[0].role, "note");
        assert_eq!(note_res[0].conv_id, conv_c);
        assert!(note_res[0].chat_archived);
        assert_eq!(note_res[0].chat_title, "Archived release notes");

        // Verify unified ranking: exact phrase in chat (rank 0) beats scattered note (rank 2)
        let state = AppState {
            app_data: dir.clone(),
            notes_dir: n_dir.clone(),
            token: None,
            default_conv: None,
            ls_address: None,
            csrf_token: None,
            preload_js: String::new(),
            settings: Arc::new(Mutex::new(SettingsStore {
                chats: HashMap::new(),
                chat_order: Vec::new(),
                draft: Settings::default(),
                draft_updated_at: 0,
                has_pending_draft: false,
                cutoffs: HashMap::new(),
                cutoff_order: Vec::new(),
                update_checked_at: 0,
                latest_version: String::new(),
                rev: 1,
                path: dir.join("settings.json"),
            })),
            notes_lock: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Mutex::new(HashMap::new())),
            chat_titles: Arc::new(Mutex::new((unix_now_secs(), titles.clone()))),
            ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
            quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
            last_prune_at: Arc::new(Mutex::new(None)),
            search_gen: Arc::new(AtomicUsize::new(0)),
            done_sound_mp3: Arc::new(OnceLock::new()),
            active_conns: Arc::new(AtomicUsize::new(0)),
        };
        let filler = " padding".repeat(35);
        fs::write(
            n_dir.join(format!("{conv_c}.json")),
            format!(r#"{{"draft":"","next_id":2,"cards":[{{"id":1,"text":"release {filler} archive","pinned":true}}]}}"#),
        )
        .unwrap();
        let (st_code, merged_json) = handle_get_search(&state, conv_b, "release archive", "all");
        assert_eq!(st_code, 200);
        let first_role_pos = merged_json.find(r#""role":"user""#).unwrap();
        let note_role_pos = merged_json.find(r#""role":"note""#).unwrap();
        assert!(first_role_pos < note_role_pos);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_tasks_keep_session_busy_and_subsequent_steps_clear_undo() {
        let mut st = ChatScanState::default();

        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":1,"type":"USER_INPUT","status":"DONE","content":"Run build and tests"}"#
        ).unwrap());
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":5000,"cache_read_tokens":250000,"tool_calls":[{"name":"run_command"}]}"#
        ).unwrap());
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":3,"type":"GENERIC","status":"DONE","content":"ok"}"#
        ).unwrap());
        // Native mid-turn checkpoint at step 4
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":4,"type":"CHECKPOINT","status":"DONE","output_tokens":7500}"#
        ).unwrap());
        let sum_at_cp = st.to_summary();
        assert_eq!(sum_at_cp.undo_step_index, Some(3));
        assert!(sum_at_cp.just_compressed);

        // Prose in PLANNER_RESPONSE mentioning task markers must NEVER trigger false bg tracking
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":20000,"cache_read_tokens":0,"content":"Note: Tool is running as a background task with task id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/task-999"}"#
        ).unwrap());
        let sum_after_resume = st.to_summary();
        assert_eq!(sum_after_resume.undo_step_index, None);
        assert!(!sum_after_resume.just_compressed);
        assert!(!sum_after_resume.is_busy);
        assert!(!sum_after_resume.has_bg_work);

        // Background task starts at step 6 (real GENERIC step)
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":6,"type":"GENERIC","status":"RUNNING","content":"Created At: 2026-11-23T19:55:48+03:00\nTool is running as a background task with task id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/task-6\nTask Description: cargo test\n"}"#
        ).unwrap());
        // Agent outputs "Waiting for tests" message with no tool calls (step 7)
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":7,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":2500,"cache_read_tokens":20000,"content":"Waiting for cargo test to finish."}"#
        ).unwrap());
        let sum_waiting_bg = st.to_summary();
        assert!(sum_waiting_bg.is_busy);
        assert!(sum_waiting_bg.has_bg_work);
        assert!(!sum_waiting_bg.turn_finished);
        assert_eq!(sum_waiting_bg.undo_step_index, None);

        // Background task finishes via SYSTEM_MESSAGE (step 8)
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":8,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-23T16:57:40Z sender=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/task-6 priority=MESSAGE_PRIORITY_HIGH content=Task id \"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/task-6\" finished with result:\nCommand exited with code 0.\n</SYSTEM_MESSAGE>"}"#
        ).unwrap());
        // Agent sends final reply after task finishes (step 9)
        push_step_to_scan(&mut st, parse_line(
            br#"{"step_index":9,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":3000,"cache_read_tokens":22500,"content":"All tests passed."}"#
        ).unwrap());
        let mut sum_final = st.to_summary();
        assert!(!sum_final.is_busy);
        assert!(!sum_final.has_bg_work);
        assert!(sum_final.turn_finished);

        // Live LS idle status clears stale busy/bg state (e.g. quota error or server restart)
        sum_final.is_busy = true;
        sum_final.has_bg_work = true;
        sum_final.compact_in_flight = true;
        apply_ls_live_to_summary(&mut sum_final, LsLiveConvStatus { known: true, is_running: false, has_bg_task: false });
        assert!(!sum_final.is_busy);
        assert!(!sum_final.has_bg_work);
        assert!(!sum_final.compact_in_flight);

        // Live LS background task forces busy state & hides Undo
        sum_final.undo_step_index = Some(3);
        apply_ls_live_to_summary(&mut sum_final, LsLiveConvStatus { known: true, is_running: false, has_bg_task: true });
        assert!(sum_final.is_busy);
        assert!(sum_final.has_bg_work);
        assert_eq!(sum_final.undo_step_index, None);
    }

    #[test]
    fn parse_quota_summary_extracts_gemini_and_other_buckets() {
        let raw = br#"{
            "response": {
                "groups": [
                    {
                        "displayName": "Gemini Models",
                        "buckets": [
                            {"displayName": "Weekly limit", "remainingFraction": 0.82, "resetTime": "2026-11-30T12:00:00Z"},
                            {"displayName": "5h limit", "remainingFraction": 0.003, "resetTime": "2026-11-24T23:40:00Z"}
                        ]
                    },
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {"displayName": "Weekly limit", "remainingFraction": 0.0, "resetTime": "2026-11-25T10:00:00Z"},
                            {"displayName": "5h limit", "remainingFraction": 1, "resetTime": ""}
                        ]
                    }
                ]
            }
        }"#;
        let parsed = parse_quota_summary_json(raw);
        assert!(parsed.has_any());
        let g = parsed.gemini.as_ref().unwrap();
        assert_eq!(g.weekly.as_ref().unwrap().pct, 82);
        assert_eq!(g.five_hour.as_ref().unwrap().pct, 1);
        let o = parsed.other.as_ref().unwrap();
        assert_eq!(o.weekly.as_ref().unwrap().pct, 0);
        assert_eq!(o.five_hour.as_ref().unwrap().pct, 100);
        let json = parsed.to_json();
        assert!(json.contains(r#""available":true"#));
        assert!(json.contains(r#""pct":82"#));
        assert!(json.contains(r#""pct":1"#));
        assert!(json.contains(r#""pct":0"#));
        assert!(json.contains(r#""pct":100"#));

        let empty = parse_quota_summary_json(br#"{"response":{}}"#);
        assert!(!empty.has_any());
        assert_eq!(empty.to_json(), r#"{"available":false}"#);
    }

    #[test]
    fn prune_deleted_chats_removes_orphans_and_keeps_live_and_archived() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_prune_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let live_id = "11111111-1111-1111-1111-111111111111";
        let archived_id = "22222222-2222-2222-2222-222222222222";
        let deleted_id = "33333333-3333-3333-3333-333333333333";

        fs::create_dir_all(dir.join("brain").join(live_id)).unwrap();
        fs::create_dir_all(dir.join("brain").join(archived_id)).unwrap();
        let notes_dir = dir.join("notes");
        fs::create_dir_all(&notes_dir).unwrap();

        fs::write(notes_dir.join(format!("{live_id}.json")), r#"{"draft":"","cards":[],"next_id":1}"#).unwrap();
        fs::write(notes_dir.join(format!("{archived_id}.json")), r#"{"draft":"","cards":[],"next_id":1}"#).unwrap();
        fs::write(notes_dir.join(format!("{deleted_id}.json")), r#"{"draft":"","cards":[],"next_id":1}"#).unwrap();
        fs::write(notes_dir.join(format!("{DRAFT_CONV_ID}.json")), r#"{"draft":"","cards":[],"next_id":1}"#).unwrap();

        let mut chats = HashMap::new();
        chats.insert(live_id.to_string(), Settings::default());
        chats.insert(archived_id.to_string(), Settings::default());
        chats.insert(deleted_id.to_string(), Settings::default());

        let state = AppState {
            app_data: dir.clone(),
            notes_dir: notes_dir.clone(),
            token: None,
            default_conv: None,
            ls_address: None,
            csrf_token: None,
            preload_js: String::new(),
            settings: Arc::new(Mutex::new(SettingsStore {
                chats,
                chat_order: vec![live_id.to_string(), archived_id.to_string(), deleted_id.to_string()],
                draft: Settings::default(),
                draft_updated_at: 0,
                has_pending_draft: false,
                cutoffs: HashMap::new(),
                cutoff_order: Vec::new(),
                update_checked_at: 0,
                latest_version: String::new(),
                rev: 1,
                path: dir.join("settings.json"),
            })),
            notes_lock: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Mutex::new(HashMap::new())),
            chat_titles: Arc::new(Mutex::new((0, HashMap::new()))),
            ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
            quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
            last_prune_at: Arc::new(Mutex::new(None)),
            search_gen: Arc::new(AtomicUsize::new(0)),
            done_sound_mp3: Arc::new(OnceLock::new()),
            active_conns: Arc::new(AtomicUsize::new(0)),
        };

        let mut live_map = HashMap::new();
        live_map.insert(live_id.to_string(), false);

        let pruned = prune_deleted_chats(&state, live_id, &live_map, Duration::ZERO);
        assert_eq!(pruned, 2); // deleted note file + deleted settings entry
        assert!(notes_dir.join(format!("{live_id}.json")).exists());
        assert!(notes_dir.join(format!("{archived_id}.json")).exists());
        assert!(notes_dir.join(format!("{DRAFT_CONV_ID}.json")).exists());
        assert!(!notes_dir.join(format!("{deleted_id}.json")).exists());

        let store = state.settings.lock().unwrap();
        assert!(store.chats.contains_key(live_id));
        assert!(store.chats.contains_key(archived_id));
        assert!(!store.chats.contains_key(deleted_id));
        drop(store);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_state_persists_valid_semver_and_clamps_future_timestamp() {
        let mut dir = env::temp_dir();
        dir.push(format!("antimetro_test_upd_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let state = AppState {
            app_data: dir.clone(),
            notes_dir: dir.join("notes"),
            token: None,
            default_conv: None,
            ls_address: None,
            csrf_token: None,
            preload_js: String::new(),
            settings: Arc::new(Mutex::new(SettingsStore {
                chats: HashMap::new(),
                chat_order: Vec::new(),
                draft: Settings::default(),
                draft_updated_at: 0,
                has_pending_draft: false,
                cutoffs: HashMap::new(),
                cutoff_order: Vec::new(),
                update_checked_at: 0,
                latest_version: String::new(),
                rev: 1,
                path: dir.join("settings.json"),
            })),
            notes_lock: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(HashMap::new())),
            scans: Arc::new(Mutex::new(HashMap::new())),
            chat_titles: Arc::new(Mutex::new((0, HashMap::new()))),
            ls_live: Arc::new(Mutex::new(LsLiveCache::default())),
            quota_cache: Arc::new(Mutex::new(QuotaCache::default())),
            last_prune_at: Arc::new(Mutex::new(None)),
            search_gen: Arc::new(AtomicUsize::new(0)),
            done_sound_mp3: Arc::new(OnceLock::new()),
            active_conns: Arc::new(AtomicUsize::new(0)),
        };

        let (bad_st, _) = handle_post_update_state(
            &state,
            br#"{"checked_at":1700000000,"latest_version":"javascript:alert(1)"}"#,
        );
        assert_eq!(bad_st, 400);

        let (ok_st, _) = handle_post_update_state(
            &state,
            br#"{"checked_at":999999999999,"latest_version":"v1.5.2"}"#,
        );
        assert_eq!(ok_st, 200);

        let loaded = load_settings_store_data(&dir.join("settings.json"));
        assert_eq!(loaded.latest_version, "1.5.2");
        assert!(loaded.update_checked_at > 0 && loaded.update_checked_at <= unix_now_secs() + 60);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_group_classifies_gemini_and_other_models() {
        assert_eq!(ModelGroup::from_model_name(""), ModelGroup::None);
        assert_eq!(ModelGroup::from_model_name("gemini-4-neon-medium"), ModelGroup::Gemini);
        assert_eq!(ModelGroup::from_model_name("flash"), ModelGroup::Gemini);
        assert_eq!(ModelGroup::from_model_name("claude-opus-5-5-medium"), ModelGroup::Other);
        assert_eq!(ModelGroup::from_model_name("gpt-oss-120b-medium"), ModelGroup::Other);

        let line = br#"{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","model":"gemini-4-neon-medium","input_tokens":1000,"cache_read_tokens":500}"#;
        let parsed = parse_line(line).expect("parse_line");
        assert_eq!(parsed.model_group, ModelGroup::Gemini);
    }

    #[test]
    fn context_breakdown_tracks_system_user_tools_replies_and_summary() {
        let lines = [
            r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","content":"hello world 1234567890"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":10000,"cache_read_tokens":0,"output_tokens":500,"tool_calls":[{"name":"read_file"}]}"#,
            r#"{"step_index":2,"type":"GENERIC","status":"DONE","content":"file output contents here"}"#,
            r#"{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":4500,"cache_read_tokens":10000,"output_tokens":300,"content":"done"}"#,
            r#"{"step_index":4,"type":"USER_INPUT","status":"DONE","content":"next question"}"#,
            r#"{"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":800,"cache_read_tokens":14500,"output_tokens":200,"content":"answer"}"#,
        ];
        let mut scan = ChatScanState::default();
        for raw in lines {
            let m = parse_line(raw.as_bytes()).expect("parse line");
            push_step_to_scan(&mut scan, m);
        }
        assert_eq!(scan.context_tokens, 15_300);
        assert_eq!(scan.breakdown.total(), 15_300);
        assert_eq!(scan.breakdown.tools, 4_000);
        assert_eq!(scan.breakdown.replies, 800);
        assert!(scan.breakdown.system > 9_000);
        assert!(scan.breakdown.user >= 500);

        let cp = parse_line(
            br#"{"step_index":6,"type":"CHECKPOINT","status":"DONE","output_tokens":2400}"#,
        )
        .expect("checkpoint");
        push_step_to_scan(&mut scan, cp);
        assert_eq!(scan.breakdown.summary, 2_400);
        assert_eq!(scan.breakdown.total(), 2_400);
    }
}
