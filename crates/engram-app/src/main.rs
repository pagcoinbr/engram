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
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "ENGRAM_CONFIG",
        default_value = "/root/.claude/engram.yaml"
    )]
    config: PathBuf,
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
    llama_cpp: LlamaCppEdit,
    embed: EmbedEdit,
}

#[derive(Clone, Deserialize, Serialize)]
struct LlamaCppEdit {
    url: String,
    model: String,
    timeout_seconds: u64,
}

#[derive(Clone, Deserialize, Serialize)]
struct EmbedEdit {
    provider: String,
    url: String,
    model: String,
    dim: u32,
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
    config: Config,
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
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/v1/status", get(status))
        .route("/api/v1/models/recommended", get(recommended_models))
        .route("/api/v1/index/status", get(index_status))
        .route("/api/v1/config", get(config))
        .route("/api/v1/config/editor", get(editor).put(save_editor))
        .route("/api/v1/config/editor/validate", post(validate_editor))
        .route("/api/v1/recall", get(recall_api))
        .with_state(Arc::new(AppState {
            config: args.config,
        }));
    let listener = tokio::net::TcpListener::bind(args.bind).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn recommended_models() -> Json<Vec<engram_config::RecommendedEmbedder>> {
    Json(recommended_embedders())
}

#[derive(Serialize)]
struct IndexStatus {
    enabled: bool,
    collection: String,
    dimension: u32,
    points: Option<u64>,
    error: Option<String>,
}

async fn index_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = match Config::load(&state.config) {
        Ok(config) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let mut status = IndexStatus {
        enabled: config.vector_store.enabled,
        collection: config.vector_store.collection.clone(),
        dimension: config.embed.dim,
        points: None,
        error: None,
    };
    if status.enabled {
        match QdrantClient::new(config.vector_store.url, config.vector_store.collection)
            .count(None)
            .await
        {
            Ok(points) => status.points = Some(points),
            Err(error) => status.error = Some(error.to_string()),
        }
    }
    (StatusCode::OK, Json(status)).into_response()
}

async fn editor(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match Config::load(&state.config) {
        Ok(config) => (
            StatusCode::OK,
            Json(EditorResponse {
                path: state.config.display().to_string(),
                revision: revision(&state.config),
                editable: editable(&config),
                config,
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
    if request.revision != revision(&state.config) {
        return (
            StatusCode::CONFLICT,
            "configuration changed on disk; reload before saving",
        )
            .into_response();
    }
    let Ok((edit, requires_reindex)) = validate_edit(&state.config, request.config) else {
        return (StatusCode::BAD_REQUEST, "invalid configuration").into_response();
    };
    match write_edit(&state.config, &edit) {
        Ok(backup) => (
            StatusCode::OK,
            Json(SaveResponse {
                ok: true,
                revision: revision(&state.config),
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
        },
    }
}

fn validate_edit(
    path: &PathBuf,
    mut edit: EditableConfig,
) -> Result<(EditableConfig, bool), String> {
    let current = Config::load(path).map_err(|error| error.to_string())?;
    edit.backend = edit.backend.trim().to_string();
    edit.llama_cpp.url = edit.llama_cpp.url.trim_end_matches('/').to_string();
    edit.embed.url = edit.embed.url.trim_end_matches('/').to_string();
    edit.llama_cpp.model = edit.llama_cpp.model.trim().to_string();
    edit.embed.model = edit.embed.model.trim().to_string();
    edit.embed.provider = edit.embed.provider.trim().to_lowercase();
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
    if edit.embed.dim == 0 || edit.llama_cpp.timeout_seconds == 0 {
        return Err("embedding dimension and timeout must be positive".into());
    }
    let candidate = Config {
        backend: edit.backend.clone(),
        llama_cpp: engram_config::LlamaCpp {
            url: edit.llama_cpp.url.clone(),
            model: edit.llama_cpp.model.clone(),
            timeout_seconds: edit.llama_cpp.timeout_seconds,
        },
        embed: engram_config::Embed {
            provider: edit.embed.provider.clone(),
            url: edit.embed.url.clone(),
            model: edit.embed.model.clone(),
            dim: edit.embed.dim,
            query_prefix: current.embed.query_prefix.clone(),
            document_prefix: current.embed.document_prefix.clone(),
        },
        ..current.clone()
    };
    candidate.validate().map_err(|error| error.to_string())?;
    let prior = editable(&current);
    let reindex = prior.embed.provider != edit.embed.provider
        || prior.embed.url != edit.embed.url
        || prior.embed.model != edit.embed.model
        || prior.embed.dim != edit.embed.dim;
    Ok((edit, reindex))
}

fn revision(path: &PathBuf) -> String {
    fs::read(path)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes))[..16].to_string())
        .unwrap_or_default()
}

fn write_edit(path: &PathBuf, edit: &EditableConfig) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let mut raw: serde_yaml::Value =
        serde_yaml::from_slice(&bytes).map_err(|error| error.to_string())?;
    let root = raw
        .as_mapping_mut()
        .ok_or("configuration must be a YAML mapping")?;
    root.insert("backend".into(), edit.backend.clone().into());
    for (section, values) in [
        ("llama_cpp", serde_yaml::to_value(&edit.llama_cpp)),
        ("embed", serde_yaml::to_value(&edit.embed)),
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
    let backup_dir = path
        .parent()
        .ok_or("configuration path has no parent")?
        .join("backups/config");
    fs::create_dir_all(&backup_dir).map_err(|error| error.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let backup = backup_dir.join(format!("engram.yaml.{stamp}.bak"));
    fs::write(&backup, &bytes).map_err(|error| error.to_string())?;
    let temp = path.with_extension("yaml.tmp");
    fs::write(
        &temp,
        serde_yaml::to_string(&raw).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::set_permissions(
        &temp,
        fs::Permissions::from_mode(metadata.permissions().mode()),
    )
    .map_err(|error| error.to_string())?;
    fs::rename(temp, path).map_err(|error| error.to_string())?;
    Ok(backup.display().to_string())
}

async fn config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match Config::load(&state.config) {
        Ok(config) => (StatusCode::OK, Json(config)).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = match Config::load(&state.config) {
        Ok(config) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    let mut models = Vec::new();
    for profile in config.profiles() {
        models.push(probe(profile).await);
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
    match recall(
        &state.config,
        query.slug.as_deref().unwrap_or("-root"),
        &query.q,
        query.k.unwrap_or(6).clamp(1, 20),
    )
    .await
    {
        Ok(output) => (StatusCode::OK, Json(output)).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

async fn probe(profile: ModelProfile) -> ModelStatus {
    let mut status = ModelStatus {
        role: profile.role.into(),
        provider: profile.provider.clone(),
        endpoint: profile.endpoint.clone(),
        configured_model: profile.model.clone(),
        expected_dimension: profile.expected_dimension,
        observed_model: None,
        observed_dimension: None,
        index_compatible: None,
        error: None,
    };
    if !matches!(profile.provider.as_str(), "llama_cpp" | "openai") || profile.endpoint.is_empty() {
        return status;
    }
    match OpenAiCompatibleClient::new(profile.endpoint) {
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
