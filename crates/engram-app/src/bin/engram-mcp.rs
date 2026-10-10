use clap::Parser;
use engram_hybrid::recall;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    /// Default store for calls that do not name one. Resolved from the environment
    /// and the session's working directory when omitted.
    // allow_hyphen_values because EVERY engram slug starts with '-' (it is a
    // path with separators replaced), so clap read `--slug -home-alice` as a
    // missing value followed by an unknown flag. The documented invocation was
    // unusable as typed.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// The agent identity this server serves, fixed for the life of the process.
    /// Required on an install that defines tenants.
    #[arg(long, env = "ENGRAM_TENANT")]
    tenant: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = engram_paths::config_path(args.config);
    // An MCP server is started by the client inside the project directory, so the
    // cwd-derived store is the right default; "-root" was correct on one machine.
    //
    // Resolved once, here, and fatally: a server that started without knowing
    // its identity would have to decide per call, and the only safe answer then
    // is to refuse every call — better to not come up at all, where the client
    // surfaces the error to the operator.
    // Resolve the IDENTITY here, fatally; defer the STORE to the tools that
    // need one.
    //
    // Resolving both up front was wrong: the wiki tools are vault operations
    // keyed on the tenant and use no slug at all, yet a tenant owning several
    // memory stores with none matching the launch directory made the whole
    // server refuse to start — so `wiki_search` was unavailable because
    // `memory_recall` could not have picked a default. A slug failure now
    // surfaces only on the tool that needs a slug.
    //
    // The identity still follows the launch directory's store when --tenant is
    // absent, so one global registration serves every identity on the host.
    let derived = engram_paths::resolve_slug_in_cwd(None);
    let tenant = match engram_config::Config::load(&config)
        .map_err(|e| e.to_string())
        .and_then(|loaded| {
            engram_tenant::resolve_tenant(
                &loaded,
                args.tenant.as_deref(),
                &derived,
                args.slug.as_deref(),
            )
            .map_err(|e| e.to_string())
        }) {
        Ok(tenant) => tenant,
        Err(error) => {
            eprintln!("engram-mcp: {error}");
            std::process::exit(2);
        }
    };
    let default_slug = args.slug.clone().unwrap_or(derived);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = match request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "initialize" => ok(
                id,
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"engram-rust","version":"0.1.0"}}),
            ),
            "tools/list" => ok(
                id,
                json!({"tools":[
                    {"name":"memory_recall","description":"Primary memory recall. Uses the configured graph backend; Graphiti compatibility preserves Graphiti ordering exactly.","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"slug":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":20}},"required":["q"]}},
                    {"name":"memory_recall_hybrid","description":"Alias for memory_recall, retained for existing callers.","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"slug":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":20}},"required":["q"]}},
                    {"name":"wiki_search","description":"Search this agent's Obsidian vault. Returns matching SECTIONS with their breadcrumb and path; use wiki_fetch to read the surrounding document.","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":20}},"required":["q"]}},
                    {"name":"wiki_fetch","description":"Read a vault document, or one of its sections, within a character budget. Use after wiki_search to get full context rather than a fragment.","inputSchema":{"type":"object","properties":{"path":{"type":"string","description":"vault-relative path, as returned by wiki_search"},"heading":{"type":"string","description":"optional section name to narrow to"},"max_chars":{"type":"integer","minimum":500,"maximum":200000}},"required":["path"]}},
                    {"name":"wiki_write","description":"File a note into this agent's own writable area of the vault. Only paths under the agent subtree are permitted; curated pages are read-only. Markdown only. Credentials are redacted before the file is written.","inputSchema":{"type":"object","properties":{"path":{"type":"string","description":"vault-relative path under the agent subtree, ending in .md"},"content":{"type":"string"},"mode":{"type":"string","enum":["create","append","replace"],"description":"create (default) refuses to overwrite; append adds; replace overwrites"}},"required":["path","content"]}}
                ]}),
            ),
            "tools/call" => {
                call(
                    id,
                    request.get("params").unwrap_or(&Value::Null),
                    &config,
                    &tenant,
                    &default_slug,
                )
                .await
            }
            _ => err(id, -32601, "method not found"),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}

/// Note what is NOT a tool argument: the tenant.
///
/// It is fixed for the life of the process, from `--tenant` on the command line
/// the client launched. Exposing it in a tool schema would let the model choose
/// its own identity and read another agent's memories simply by asking — the
/// one thing the tenant boundary exists to prevent. `slug` IS exposed, and is
/// therefore validated against the tenant on every call.
async fn call(
    id: &Value,
    params: &Value,
    config: &Path,
    tenant: &engram_tenant::Tenant,
    default_slug: &str,
) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    match name {
        "memory_recall" | "memory_recall_hybrid" => {}
        "wiki_search" => return wiki_search_tool(id, arguments, config, tenant).await,
        "wiki_fetch" => return wiki_fetch_tool(id, arguments, tenant),
        "wiki_write" => return wiki_write_tool(id, arguments, tenant),
        _ => return err(id, -32602, "unknown tool"),
    }
    let Some(query) = arguments
        .get("q")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return err(id, -32602, "q is required");
    };
    // The model supplied this, so it is checked rather than trusted: a slug
    // belonging to another identity is refused, and the refusal is returned to
    // the model as a tool error so it can correct itself.
    let requested = arguments
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|slug| !slug.is_empty());
    let slug = match tenant.choose_slug(requested, default_slug) {
        Ok(slug) => slug,
        Err(error) => {
            return ok(
                id,
                json!({"content":[{"type":"text","text":error.to_string()}],"isError":true}),
            );
        }
    };
    let k = arguments
        .get("k")
        .and_then(Value::as_u64)
        .unwrap_or(6)
        .clamp(1, 20) as usize;
    match recall(config, tenant, &slug, query, k).await {
        Ok(result) => ok(
            id,
            json!({"content":[{"type":"text","text":serde_json::to_string(&result).unwrap()}]}),
        ),
        Err(error) => ok(
            id,
            json!({"content":[{"type":"text","text":error}],"isError":true}),
        ),
    }
}
fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn err(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

/// Search the vault. Note the absent `tenant` argument — see `call`.
async fn wiki_search_tool(
    id: &Value,
    arguments: &Value,
    config: &Path,
    tenant: &engram_tenant::Tenant,
) -> Value {
    let Some(query) = arguments
        .get("q")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return err(id, -32602, "q is required");
    };
    let k = arguments
        .get("k")
        .and_then(Value::as_u64)
        .unwrap_or(6)
        .clamp(1, 20) as usize;
    match engram_hybrid::wiki_search(config, tenant, query, k).await {
        Ok(result) => tool_text(id, serde_json::to_string(&result).unwrap_or_default()),
        Err(error) => tool_error(id, error),
    }
}

/// Read a document or section.
///
/// The default budget is deliberately generous: this tool exists BECAUSE a
/// truncated fragment is what the memory path already gives for a long document,
/// and the point of a wiki is to hand an LLM the whole relevant page.
fn wiki_fetch_tool(id: &Value, arguments: &Value, tenant: &engram_tenant::Tenant) -> Value {
    let Some(path) = arguments
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return err(id, -32602, "path is required");
    };
    let heading = arguments
        .get("heading")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let max_chars = arguments
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(24_000)
        .clamp(500, 200_000) as usize;
    match engram_hybrid::wiki_fetch(tenant, path.trim(), heading, max_chars) {
        Ok(text) => tool_text(id, text),
        Err(error) => tool_error(id, error),
    }
}

fn tool_text(id: &Value, text: String) -> Value {
    ok(id, json!({"content":[{"type":"text","text":text}]}))
}

fn tool_error(id: &Value, error: String) -> Value {
    ok(
        id,
        json!({"content":[{"type":"text","text":error}],"isError":true}),
    )
}

/// File a note into the agent subtree.
///
/// Every guard lives in `engram_hybrid::wiki_write`, not here: this is one of
/// several callers, and a check at the MCP boundary only would be absent from
/// the others.
fn wiki_write_tool(id: &Value, arguments: &Value, tenant: &engram_tenant::Tenant) -> Value {
    let Some(path) = arguments
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return err(id, -32602, "path is required");
    };
    let Some(content) = arguments.get("content").and_then(Value::as_str) else {
        return err(id, -32602, "content is required");
    };
    let mode = match engram_hybrid::WriteMode::parse(
        arguments.get("mode").and_then(Value::as_str).unwrap_or(""),
    ) {
        Ok(mode) => mode,
        Err(error) => return tool_error(id, error),
    };
    match engram_hybrid::wiki_write(tenant, path, content, mode) {
        Ok(report) => tool_text(id, report),
        Err(error) => tool_error(id, error),
    }
}
