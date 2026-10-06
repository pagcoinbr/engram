use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use engram_config::{Config, ModelProfile, recommended_embedders};
use engram_hybrid::recall;
use engram_models::OpenAiCompatibleClient;
use engram_vector::QdrantClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::SocketAddr,
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
struct Args {
    /// Path to engram.yaml. Resolved via engram-paths when omitted, so this binary
    /// works for any user instead of only the root-owned install it was written on.
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    config: PathBuf,
}

#[derive(Serialize)]
struct ModelStatus {
    role: String,
    provider: String,
    endpoint: String,
    configured_model: String,
    expected_dimension: Option<u32>,
    observed_model: Option<String>,
    observed_dimension: Option<usize>,
    index_compatible: Option<bool>,
    /// Whether this provider was actually contacted.
    ///
    /// Without it, a client cannot distinguish "healthy" from "never checked": an
    /// endpoint we decline to probe has no error, and Atlas rendered
    /// `endpoint && !error` as a green badge for a server nothing had spoken to.
    probed: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct StatusResponse {
    config_version: u32,
    models: Vec<ModelStatus>,
}

#[derive(serde::Deserialize)]
struct RecallQuery {
    q: String,
    slug: Option<String>,
    k: Option<usize>,
}

#[derive(Clone, Deserialize, Serialize)]
struct EditableConfig {
    backend: String,
    graph: GraphEdit,
    llama_cpp: LlamaCppEdit,
    embed: EmbedEdit,
}

#[derive(Clone, Deserialize, Serialize)]
struct LlamaCppEdit {
    url: String,
    model: String,
    /// `serde(default)` plus a lenient number reader: a browser `<input
    /// type="number">` yields a STRING, which used to fail deserialization with a
    /// 422 before any validation ran, so the field the user edited was the one that
    /// broke the save.
    #[serde(deserialize_with = "lenient_u64")]
    timeout_seconds: u64,
}

#[derive(Clone, Deserialize, Serialize)]
struct EmbedEdit {
    provider: String,
    url: String,
    model: String,
    #[serde(deserialize_with = "lenient_u32")]
    dim: u32,
    /// Editable because asymmetric embedding models are unusable without them and
    /// the UI previously had no way to set them.
    #[serde(default)]
    query_prefix: String,
    #[serde(default)]
    document_prefix: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct GraphEdit {
    backend: String,
}

/// Accept `600` or `"600"`. See [`LlamaCppEdit::timeout_seconds`].
fn lenient_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    use serde::de::Error;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| D::Error::custom("must be a non-negative whole number")),
        serde_json::Value::String(text) => text
            .trim()
            .parse()
            .map_err(|_| D::Error::custom("must be a whole number")),
        _ => Err(D::Error::custom("must be a number")),
    }
}

fn lenient_u32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    use serde::de::Error;
    u32::try_from(lenient_u64(deserializer)?).map_err(|_| D::Error::custom("value too large"))
}

#[derive(Deserialize)]
struct EditRequest {
    config: EditableConfig,
}

#[derive(Deserialize)]
struct SaveRequest {
    revision: String,
    config: EditableConfig,
}

#[derive(Serialize)]
struct EditorResponse {
    path: String,
    revision: String,
    config: serde_json::Value,
    editable: EditableConfig,
    read_only: bool,
}

#[derive(Serialize)]
struct ValidationResponse {
    valid: bool,
    config: EditableConfig,
    requires_reindex: bool,
}

#[derive(Serialize)]
struct SaveResponse {
    ok: bool,
    revision: String,
    backup: String,
    requires_reindex: bool,
    restart_required: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = engram_paths::config_path(args.config);
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/v1/status", get(status))
        .route("/api/v1/models/recommended", get(recommended_models))
        .route("/api/v1/index/status", get(index_status))
        .route("/api/v1/config", get(full_config))
        .route("/api/v1/config/editor", get(editor).put(save_editor))
        .route("/api/v1/config/editor/validate", post(validate_editor))
        .route("/api/v1/recall", get(recall_api))
        .with_state(Arc::new(AppState { config }));
    let listener = tokio::net::TcpListener::bind(args.bind).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn recommended_models() -> Json<Vec<engram_config::RecommendedEmbedder>> {
    Json(recommended_embedders())
}

#[derive(Serialize)]
struct IndexStatus {
    enabled: bool,
    local_enabled: bool,
    collection: String,
    dimension: u32,
    embedding_space: String,
    points: Option<u64>,
    error: Option<String>,
}

async fn index_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = match Config::load(&state.config) {
        Ok(config) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    // local_enabled is the documented master switch: with it off there is no index
    // work at all, whatever vector_store says.
    let mut status = IndexStatus {
        enabled: config.local_enabled && config.vector_store.enabled,
        local_enabled: config.local_enabled,
        collection: config.vector_store.collection.clone(),
        dimension: config.embed.dim,
        embedding_space: config.embedding_space_id(),
        points: None,
        error: None,
    };
    if status.enabled {
        match QdrantClient::from_config(&config).count(None).await {
            Ok(points) => status.points = Some(points),
            Err(error) => status.error = Some(error.to_string()),
        }
    }
    (StatusCode::OK, Json(status)).into_response()
}

async fn editor(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let revision = match revision(&state.config) {
        Ok(revision) => revision,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
    };
    match Config::load(&state.config) {
        Ok(config) => (
            StatusCode::OK,
            Json(EditorResponse {
                path: state.config.display().to_string(),
                revision,
                editable: editable(&config),
                config: redacted_file(&state.config).unwrap_or(serde_json::Value::Null),
                read_only: false,
            }),
        )
            .into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

async fn validate_editor(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EditRequest>,
) -> impl IntoResponse {
    match validate_edit(&state.config, request.config) {
        Ok((config, requires_reindex)) => (
            StatusCode::OK,
            Json(ValidationResponse {
                valid: true,
                config,
                requires_reindex,
            }),
        )
            .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

async fn save_editor(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SaveRequest>,
) -> impl IntoResponse {
    // Everything from here to the rename happens under an exclusive lock. The
    // revision check used to sit outside it, so two concurrent saves could both
    // pass the check and the second would silently discard the first.
    let _lock = match ConfigLock::acquire(&state.config) {
        Ok(lock) => lock,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    match revision(&state.config) {
        Ok(current) if current != request.revision => {
            return (
                StatusCode::CONFLICT,
                "configuration changed on disk; reload before saving",
            )
                .into_response();
        }
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
        Ok(_) => {}
    }
    // Report WHY a save was rejected. This used to collapse to "invalid
    // configuration", so Save gave no hint where Validate would have explained it.
    let (edit, requires_reindex) = match validate_edit(&state.config, request.config) {
        Ok(result) => result,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    match write_edit(&state.config, &edit) {
        Ok(backup) => (
            StatusCode::OK,
            Json(SaveResponse {
                ok: true,
                revision: revision(&state.config).unwrap_or_default(),
                backup,
                requires_reindex,
                restart_required: true,
            }),
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

fn editable(config: &Config) -> EditableConfig {
    EditableConfig {
        backend: config.backend.clone(),
        llama_cpp: LlamaCppEdit {
            url: config.llama_cpp.url.clone(),
            model: config.llama_cpp.model.clone(),
            timeout_seconds: config.llama_cpp.timeout_seconds,
        },
        embed: EmbedEdit {
            provider: config.embed.provider.clone(),
            url: config.embed.url.clone(),
            model: config.embed.model.clone(),
            dim: config.embed.dim,
            query_prefix: config.embed.query_prefix.clone(),
            document_prefix: config.embed.document_prefix.clone(),
        },
        graph: GraphEdit {
            backend: config.graph.backend.clone(),
        },
    }
}

fn validate_edit(path: &Path, mut edit: EditableConfig) -> Result<(EditableConfig, bool), String> {
    let current = Config::load(path).map_err(|error| error.to_string())?;
    edit.backend = edit.backend.trim().to_string();
    edit.llama_cpp.url = edit.llama_cpp.url.trim_end_matches('/').to_string();
    edit.embed.url = edit.embed.url.trim_end_matches('/').to_string();
    edit.llama_cpp.model = edit.llama_cpp.model.trim().to_string();
    edit.embed.model = edit.embed.model.trim().to_string();
    edit.embed.provider = edit.embed.provider.trim().to_lowercase();
    edit.graph.backend = edit.graph.backend.trim().to_lowercase();
    if !matches!(
        edit.backend.as_str(),
        "ollama" | "claude" | "ccg" | "llama_cpp"
    ) {
        return Err("unsupported backend".into());
    }
    if !matches!(
        edit.embed.provider.as_str(),
        "" | "ollama" | "fastembed" | "llama_cpp" | "openai"
    ) {
        return Err("unsupported embedding provider".into());
    }
    if !matches!(edit.graph.backend.as_str(), "native" | "graphiti_compat") {
        return Err("unsupported graph backend".into());
    }
    if edit.embed.dim == 0 || edit.llama_cpp.timeout_seconds == 0 {
        return Err("embedding dimension and timeout must be positive".into());
    }
    let candidate = Config {
        backend: edit.backend.clone(),
        llama_cpp: engram_config::LlamaCpp {
            url: edit.llama_cpp.url.clone(),
            model: edit.llama_cpp.model.clone(),
            timeout_seconds: edit.llama_cpp.timeout_seconds,
            ..current.llama_cpp.clone()
        },
        embed: engram_config::Embed {
            provider: edit.embed.provider.clone(),
            url: edit.embed.url.clone(),
            model: edit.embed.model.clone(),
            dim: edit.embed.dim,
            query_prefix: edit.embed.query_prefix.clone(),
            document_prefix: edit.embed.document_prefix.clone(),
            ..current.embed.clone()
        },
        graph: engram_config::Graph {
            backend: edit.graph.backend.clone(),
            ..current.graph.clone()
        },
        ..current.clone()
    };
    candidate.validate().map_err(|error| error.to_string())?;
    // Reindex is required whenever the embedding SPACE changes, which is more than
    // the four fields previously compared — a prefix change silently moves every
    // query into a different region of the space.
    let reindex = candidate.embedding_space_id() != current.embedding_space_id();
    Ok((edit, reindex))
}

/// A content hash of the config, used for optimistic concurrency.
///
/// This used to swallow read errors into `""`. A client that also sent `""` then
/// passed the equality check, so an unreadable config disabled the protection
/// entirely; it is now an error.
fn revision(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes))[..16].to_string())
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

/// An exclusive lock around the read-validate-write sequence.
///
/// `flock(2)` on a persistent sibling lockfile, held by the kernel.
///
/// Two earlier designs were wrong in the same way. `O_EXCL` plus an
/// unlink-if-old stale breaker races with its own cure — A sees an old lock, its
/// owner exits, B creates a fresh one, and A's unlink deletes *B's* lock, putting
/// two writers in the critical section. Adding an owner token and re-reading it
/// before the unlink narrows that window but cannot close it: read-then-unlink is
/// still two syscalls with no atomicity between them, in acquire and in `Drop`
/// alike.
///
/// `flock` has no such window. Ownership lives in the kernel, not in the
/// directory entry, so there is nothing to check-then-act on; the lock is
/// released automatically when the fd closes, including on crash or `SIGKILL`,
/// which removes the need for a staleness heuristic at all. The lockfile is never
/// unlinked, so no process can delete a file another process holds.
#[derive(Debug)]
struct ConfigLock {
    /// Held only for its side effect: closing this file releases the flock,
    /// which is why there is no `Drop` impl and nothing to unlink.
    _file: fs::File,
}

impl ConfigLock {
    /// How long to wait for a concurrent save before reporting contention.
    /// Comfortably longer than a legitimate save (read, validate, fsync, rename).
    const WAIT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(20);

    fn acquire(config: &Path) -> Result<Self, String> {
        Self::acquire_within(config, Self::WAIT)
    }

    fn acquire_within(config: &Path, wait: Duration) -> Result<Self, String> {
        let path = config.with_extension("yaml.lock");
        // Opened, never removed: the file is just a handle for the kernel lock.
        //
        // Group/other-writable, unlike the config itself. The file holds no
        // content — all the state is the kernel's — so there is nothing to
        // protect, and because it is persistent a restrictive mode outlives
        // whoever created it: once the config changes hands to a service user,
        // that user can own the config and the directory and still not be able to
        // open a lockfile left by the old owner.
        //
        // `OpenOptions::mode` is not enough on its own — it is masked by the
        // process umask, so the usual `0022` turns `0666` into `0644` and the
        // lockout is unchanged. The mode is therefore set explicitly, through the
        // open file descriptor rather than the path: a path-based `chmod` races
        // with anything that replaces the entry between create and chmod, and
        // would then re-permission the wrong file.
        let file = match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o666)
            .open(&path)
        {
            Ok(file) => {
                // As the creator, failing to widen the mode is a real failure, not
                // a detail to swallow: the lock would be acquired at the very mode
                // this exists to avoid, and the next owner is locked out with no
                // sign of why.
                file.set_permissions(fs::Permissions::from_mode(0o666))
                    .map_err(|error| {
                        format!("could not set the mode on {}: {error}", path.display())
                    })?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|error| {
                        format!(
                            "could not open the config lock {}: {error}. If it was left by \
                         another user, remove it — it carries no state.",
                            path.display()
                        )
                    })?;
                // Migrate a lockfile created before this mode was used. Best
                // effort by necessity — it may belong to another user, in which
                // case we could still open it and the lock works regardless.
                let _ = file.set_permissions(fs::Permissions::from_mode(0o666));
                file
            }
            Err(error) => {
                return Err(format!("could not create {}: {error}", path.display()));
            }
        };
        let deadline = SystemTime::now() + wait;
        loop {
            // SAFETY: a valid fd owned by `file` for the duration of the call.
            let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if locked == 0 {
                return Ok(Self { _file: file });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(format!("could not lock configuration: {error}"));
            }
            if SystemTime::now() >= deadline {
                return Err("configuration is locked by another save; try again".into());
            }
            std::thread::sleep(Self::POLL);
        }
    }

    /// Whether the lock is contended, probed through a *separate* open file
    /// description — for tests, which cannot observe a kernel lock on disk.
    ///
    /// `flock` locks are held per open file description, not per process, so this
    /// probe conflicts with a live `ConfigLock` exactly as another process's would.
    /// It is still a weaker claim than a true cross-process test: it demonstrates
    /// the mechanism, not a second `execve`.
    #[cfg(test)]
    fn is_contended(config: &Path) -> Result<bool, String> {
        let path = config.with_extension("yaml.lock");
        let file = fs::File::options()
            .write(true)
            .open(&path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        // SAFETY: a valid fd owned by `file` for the duration of both calls.
        unsafe {
            if libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
                return Ok(false);
            }
        }
        // Only contention means "held"; anything else is a real failure and must
        // not be reported as a healthy lock.
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err(format!("probe failed: {error}"))
        }
    }
}

// Dropping the File closes the fd, which releases the flock. No unlink, so there
// is no way to delete a lock another process is holding.

fn write_edit(path: &Path, edit: &EditableConfig) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let mode = metadata.permissions().mode();
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let mut raw: serde_yaml::Value =
        serde_yaml::from_slice(&bytes).map_err(|error| error.to_string())?;
    let root = raw
        .as_mapping_mut()
        .ok_or("configuration must be a YAML mapping")?;
    root.insert("backend".into(), edit.backend.clone().into());
    // Merge per section, leaving every key we do not model — including the api_key
    // entries, which the edit payload deliberately has no field for — untouched.
    for (section, values) in [
        ("llama_cpp", serde_yaml::to_value(&edit.llama_cpp)),
        ("embed", serde_yaml::to_value(&edit.embed)),
        ("graph", serde_yaml::to_value(&edit.graph)),
    ] {
        let section_map = root
            .entry(section.into())
            .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()))
            .as_mapping_mut()
            .ok_or("configuration section must be a mapping")?;
        for (key, value) in values
            .map_err(|error| error.to_string())?
            .as_mapping()
            .ok_or("invalid edit")?
        {
            section_map.insert(key.clone(), value.clone());
        }
    }

    let parent = path.parent().ok_or("configuration path has no parent")?;
    let unique = unique_suffix();
    let backup_dir = parent.join("backups/config");
    fs::create_dir_all(&backup_dir).map_err(|error| error.to_string())?;
    // Unique to the nanosecond and the pid. Second-granularity names collided
    // between concurrent saves, and the Python fallback wrote a different naming
    // scheme into the same directory.
    let backup = backup_dir.join(format!("engram.yaml.{unique}.bak"));
    write_private(&backup, &bytes, mode)?;

    let temp = parent.join(format!(
        "{}.{unique}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let rendered = serde_yaml::to_string(&raw).map_err(|error| error.to_string())?;
    write_private(&temp, rendered.as_bytes(), mode)?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })?;
    // fsync the directory too: without it the rename itself can be lost on a crash,
    // leaving no config where there used to be a valid one.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(backup.display().to_string())
}

/// Write, preserving the source mode, and fsync before returning.
fn write_private(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    // create_new honours the umask, so set the mode explicitly as well.
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:09}.{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id()
    )
}

/// The whole config file, with secret-looking values masked.
///
/// Atlas shows this under a "Full configuration / Secrets redacted" heading. The
/// typed `Config` is neither: it models about six of the file's twenty-odd
/// sections, and nothing redacted anything on this path. Serving the real file
/// through the shared detector makes both halves of that label true.
fn redacted_file(path: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    let (masked, _) = engram_secrets::redact(&text);
    let value: serde_yaml::Value = serde_yaml::from_str(&masked).ok()?;
    serde_json::to_value(value).ok()
}

async fn full_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Load first, so an invalid config still reports as invalid rather than being
    // echoed back as if it were fine.
    if let Err(error) = Config::load(&state.config) {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    match redacted_file(&state.config) {
        Some(value) => (StatusCode::OK, Json(value)).into_response(),
        None => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "could not read configuration".to_string(),
        )
            .into_response(),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = match Config::load(&state.config) {
        Ok(config) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let mut models = Vec::new();
    for profile in config.profiles() {
        models.push(probe(&config, profile).await);
    }
    (
        StatusCode::OK,
        Json(StatusResponse {
            config_version: config.config_version,
            models,
        }),
    )
        .into_response()
}

async fn recall_api(
    Query(query): Query<RecallQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // A `-root` default made this endpoint search a store that only exists on one
    // machine; resolve it the same way every other entry point does.
    let slug = engram_paths::resolve_slug(query.slug.as_deref());
    match recall(
        &state.config,
        &slug,
        &query.q,
        query.k.unwrap_or(6).clamp(1, 20),
    )
    .await
    {
        Ok(output) => (StatusCode::OK, Json(output)).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

async fn probe(config: &Config, profile: ModelProfile) -> ModelStatus {
    let mut status = ModelStatus {
        role: profile.role.into(),
        provider: profile.provider.clone(),
        endpoint: profile.endpoint.clone(),
        configured_model: profile.model.clone(),
        expected_dimension: profile.expected_dimension,
        observed_model: None,
        observed_dimension: None,
        index_compatible: None,
        probed: false,
        error: None,
    };
    // Only OpenAI-compatible endpoints can be probed with this client. Everything
    // else is reported honestly as unprobed rather than as healthy.
    let openai_compatible = matches!(profile.provider.as_str(), "llama_cpp" | "openai")
        || (profile.role == "embedding" && config.embed_provider() == "llama_cpp");
    if !openai_compatible || profile.endpoint.is_empty() {
        return status;
    }
    status.probed = true;
    let key = if profile.role == "embedding" {
        config.embed.api_key.clone()
    } else {
        config.llama_cpp.api_key.clone()
    };
    match OpenAiCompatibleClient::new(profile.endpoint).map(|client| {
        client
            .with_api_key(key.present())
            .with_timeout(config.embed_timeout_seconds())
    }) {
        Ok(client) if profile.role == "embedding" => match client
            .probe(&profile.model, profile.expected_dimension)
            .await
        {
            Ok(report) => {
                status.observed_model = report.observed_model;
                status.observed_dimension = report.observed_dimension;
                status.index_compatible = report.index_compatible;
            }
            Err(error) => status.error = Some(error.to_string()),
        },
        Ok(client) => match client.models().await {
            Ok(models) => status.observed_model = models.into_iter().next(),
            Err(error) => status.error = Some(error.to_string()),
        },
        Err(error) => status.error = Some(error.to_string()),
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locking is exclusive between independent opens, released on drop, and
    /// never deletes the lockfile.
    ///
    /// The lockfile is a handle for a kernel lock, not the lock itself. Two
    /// earlier designs tried to encode ownership in the directory entry and raced
    /// on check-then-unlink; there is nothing here to race on. Scope: `flock` is
    /// per open file description, so the probe here conflicts exactly as another
    /// process's would — but this is still an in-process demonstration of the
    /// mechanism, not a cross-process test.
    #[test]
    fn the_config_lock_is_exclusive_between_independent_opens() {
        let dir = std::env::temp_dir().join(format!("engram-lock-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("engram.yaml");
        fs::write(&config, "embed: {dim: 1024}\n").unwrap();
        let lock_path = config.with_extension("yaml.lock");

        let held = ConfigLock::acquire(&config).unwrap();
        assert!(ConfigLock::is_contended(&config).unwrap());

        // Genuinely group/other-writable, so a later service user can still open
        // the persistent lockfile. `OpenOptions::mode` alone does not achieve
        // this: the usual 0022 umask silently turns 0666 into 0644 and the
        // ownership-change lockout stays exactly as it was.
        let mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o666,
            "lockfile mode is {mode:o}, umask was not overcome"
        );

        // A lockfile left at a restrictive mode by an earlier version is widened
        // on the next acquisition, so an upgraded install does not inherit the
        // lockout permanently.
        drop(held);
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).unwrap();
        let held = ConfigLock::acquire(&config).unwrap();
        let mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o666,
            "an existing lockfile was not migrated ({mode:o})"
        );

        // A second acquisition does not succeed, waits out its window rather
        // than failing instantly, and gives up rather than hanging. Tested with
        // a short window so CI does not pay the production one.
        let window = Duration::from_millis(200);
        let waited = SystemTime::now();
        let error =
            ConfigLock::acquire_within(&config, window).expect_err("lock was not exclusive");
        let elapsed = waited.elapsed().unwrap();
        assert!(error.contains("locked by another save"), "{error}");
        assert!(elapsed >= window, "gave up early: {elapsed:?}");
        assert!(elapsed < window * 10, "did not give up: {elapsed:?}");
        // ...and the production window is long enough to outlast a real save.
        assert!(ConfigLock::WAIT >= Duration::from_secs(5));

        // Releasing hands it on, and the file survives — nothing unlinks it, so
        // no process can remove a lock another process holds.
        drop(held);
        assert!(lock_path.exists(), "the lockfile must persist");
        assert!(
            !ConfigLock::is_contended(&config).unwrap(),
            "drop must release the lock"
        );
        let next = ConfigLock::acquire(&config).expect("released lock is reusable");
        drop(next);
        let _ = fs::remove_dir_all(&dir);
    }

    fn edit() -> EditableConfig {
        EditableConfig {
            backend: "llama_cpp".into(),
            graph: GraphEdit {
                backend: "graphiti_compat".into(),
            },
            llama_cpp: LlamaCppEdit {
                url: "http://ai/v1".into(),
                model: "qwen".into(),
                timeout_seconds: 600,
            },
            embed: EmbedEdit {
                provider: "llama_cpp".into(),
                url: "http://e/v1".into(),
                model: "bge-m3".into(),
                dim: 1024,
                query_prefix: String::new(),
                document_prefix: String::new(),
            },
        }
    }

    fn scratch(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("engram-app-test-{name}-{}", unique_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engram.yaml");
        fs::write(&path, body).unwrap();
        path
    }

    /// A browser number input sends a string. That used to 422 the whole save.
    #[test]
    fn numeric_editor_fields_accept_strings_from_the_browser() {
        let request: SaveRequest = serde_json::from_str(
            r#"{"revision":"abc","config":{"backend":"llama_cpp",
                "graph":{"backend":"native"},
                "llama_cpp":{"url":"http://ai/v1","model":"q","timeout_seconds":"600"},
                "embed":{"provider":"llama_cpp","url":"http://e/v1","model":"bge-m3","dim":"1024"}}}"#,
        )
        .expect("string numbers must deserialize");
        assert_eq!(request.config.llama_cpp.timeout_seconds, 600);
        assert_eq!(request.config.embed.dim, 1024);

        // and real numbers still work
        let request: EditRequest = serde_json::from_str(
            r#"{"config":{"backend":"llama_cpp","graph":{"backend":"native"},
                "llama_cpp":{"url":"http://ai/v1","model":"q","timeout_seconds":600},
                "embed":{"provider":"llama_cpp","url":"http://e/v1","model":"bge-m3","dim":1024}}}"#,
        )
        .unwrap();
        assert_eq!(request.config.embed.dim, 1024);

        // junk is still rejected, just with a message instead of a silent 422
        assert!(
            serde_json::from_str::<EditRequest>(
                r#"{"config":{"backend":"llama_cpp","graph":{"backend":"native"},
                "llama_cpp":{"url":"http://ai/v1","model":"q","timeout_seconds":"soon"},
                "embed":{"provider":"llama_cpp","url":"http://e/v1","model":"bge-m3","dim":1024}}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_prefix_change_requires_a_reindex() {
        let path = scratch(
            "prefix",
            "backend: llama_cpp\nllama_cpp: {url: 'http://ai/v1', model: qwen, timeout_seconds: 600}\nembed: {provider: llama_cpp, url: 'http://e/v1', model: bge-m3, dim: 1024}\n",
        );
        let (_, reindex) = validate_edit(&path, edit()).unwrap();
        assert!(!reindex, "an unchanged embedding space needs no reindex");

        let mut changed = edit();
        changed.embed.query_prefix = "query: ".into();
        let (_, reindex) = validate_edit(&path, changed).unwrap();
        assert!(reindex, "changing a prefix moves every query in the space");

        let mut changed = edit();
        changed.embed.model = "qwen3-embedding-0.6b".into();
        let (_, reindex) = validate_edit(&path, changed).unwrap();
        assert!(
            reindex,
            "a same-dimension model swap still changes the space"
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The save path must leave credentials, comments-adjacent keys and unmodelled
    /// sections alone. The api_key below has no editor field at all; losing it would
    /// silently unauthenticate every model call.
    #[test]
    fn saving_preserves_unmodelled_keys_and_credentials() {
        let path = scratch(
            "preserve",
            "backend: llama_cpp\nlocal_enabled: true\n\
             llama_cpp: {url: 'http://ai/v1', model: qwen, timeout_seconds: 600, api_key: 'sk-keepme'}\n\
             embed: {provider: llama_cpp, url: 'http://e/v1', model: bge-m3, dim: 1024}\n\
             telegram: {bot_token: 'keep-this-too'}\n",
        );
        let mut next = edit();
        next.embed.dim = 768;
        write_edit(&path, &next).unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("sk-keepme"), "credential dropped: {saved}");
        assert!(
            saved.contains("keep-this-too"),
            "unmodelled section dropped: {saved}"
        );
        assert!(
            saved.contains("local_enabled"),
            "master switch dropped: {saved}"
        );
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.embed.dim, 768, "the edit did not apply");
        assert_eq!(reloaded.llama_cpp.api_key.present(), Some("sk-keepme"));
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_unreadable_config_is_an_error_not_an_empty_revision() {
        let missing = std::env::temp_dir().join("engram-app-test-absent/engram.yaml");
        assert!(
            revision(&missing).is_err(),
            "a missing config must not hash to \"\", which any client could match"
        );
    }

    #[test]
    fn the_full_config_view_masks_secrets_across_every_section() {
        let path = scratch(
            "redact",
            "backend: llama_cpp\n\
             llama_cpp: {url: 'http://ai/v1', api_key: 'sk-abcdefghijklmnopqrstuv'}\n\
             telegram: {bot_token: '123456:AAEkjhsdfkjhsdfkjhsdfkjhsdfkjhsdfkjh'}\n",
        );
        let rendered = serde_json::to_string(&redacted_file(&path).unwrap()).unwrap();
        assert!(
            !rendered.contains("sk-abcdefghijklmnopqrstuv"),
            "{rendered}"
        );
        assert!(rendered.contains("backend"), "non-secret keys must survive");
        // and unmodelled sections are present, which the typed view dropped entirely
        assert!(rendered.contains("telegram"), "{rendered}");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
