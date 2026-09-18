use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use clap::Parser;
use engram_config::{Config, ModelProfile};
use engram_hybrid::recall;
use engram_models::OpenAiCompatibleClient;
use serde::Serialize;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

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

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/v1/status", get(status))
        .route("/api/v1/config", get(config))
        .route("/api/v1/recall", get(recall_api))
        .with_state(Arc::new(AppState {
            config: args.config,
        }));
    let listener = tokio::net::TcpListener::bind(args.bind).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
