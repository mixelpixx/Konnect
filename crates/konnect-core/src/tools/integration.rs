//! `integration` toolset — JLCPCB parts database, datasheet enrichment, and Freerouting discovery.
//!
//! JLCPCB tools query a local SQLite cache of the JLCPCB parts database.
//! Freerouting discovery locates the JAR and verifies the Java runtime. Konnect
//! does not advertise an autorouter until it has a real PCB-editor bridge.
//! Datasheet enrichment uses the LCSC HTTP API.
//!
//! The three network calls (JLCPCB database download, LCSC datasheet lookups)
//! go through `get_with_backoff`, which retries transient failures (network
//! errors, 429, 5xx) with exponential backoff before giving up.
//!
//! The three JLCPCB query tools (`search_jlcpcb_parts`, `get_jlcpcb_part`,
//! `suggest_jlcpcb_alternatives`) cache results in `ToolContext::jlcpcb_cache`
//! (5-minute TTL) to avoid re-running an identical SQLite query for repeated
//! lookups within a session. Responses carry a `"cached"` field so callers
//! can see whether a given result came from cache.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::{
    command::{commit_command, prepare_command, ItemId, SchematicCommand},
    parse_sexp,
    writer::{find_direct_child_blocks, read_consistent},
    SexpError, SexpNode,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const JLCPCB_FEED_BASE_URL: &str = "https://bouni.github.io/kicad-jlcpcb-tools";
const JLCPCB_FEED_SENTINEL: &str = "chunk_num_current_parts_fts5.txt";
const JLCPCB_FEED_ARCHIVE: &str = "current-parts-fts5.db.zip";
const JLCPCB_MAX_CHUNKS: usize = 64;
const JLCPCB_MAX_ARCHIVE_BYTES: u64 = 1_073_741_824;
const JLCPCB_MAX_DATABASE_BYTES: u64 = 8_589_934_592;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "download_jlcpcb_database",
            "Download or update the local JLCPCB component parts database cache (SQLite).",
            json!({
                "type": "object",
                "properties": {
                    "output_path": { "type": "string", "description": "Local path to store the SQLite database file (optional, uses config default)" },
                    "force": { "type": "boolean", "description": "Force re-download even if cache exists", "default": false }
                },
                "required": []
            }),
            |args, ctx| async move { handle_download_jlcpcb(args, ctx).await }
        ),
        tool!(
            "search_jlcpcb_parts",
            "Search the local JLCPCB component database by keyword, value, or category. The query is split on whitespace and a part matches only if every word appears, in any order, in its LCSC number, MPN, package, manufacturer or description; case-insensitive.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search words, separated by spaces. A part must contain every word, in any order, in its LCSC number, MPN, package, manufacturer or description (case-insensitive; % and _ are literal)" },
                    "category": { "type": "string", "description": "Component category filter (optional)" },
                    "basic_only": { "type": "boolean", "description": "Restrict to JLCPCB Basic Library parts only", "default": false },
                    "in_stock": { "type": "boolean", "description": "Only return parts currently in stock", "default": true },
                    "limit": { "type": "integer", "description": "Maximum number of results", "default": 20 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_jlcpcb_parts(args, ctx).await }
        ),
        tool!(
            "get_jlcpcb_part",
            "Retrieve full details for a single JLCPCB part by its LCSC part number.",
            json!({
                "type": "object",
                "properties": {
                    "lcsc_id": { "type": "string", "description": "LCSC part number (e.g. 'C14663')" }
                },
                "required": ["lcsc_id"]
            }),
            |args, ctx| async move { handle_get_jlcpcb_part(args, ctx).await }
        ),
        tool!(
            "suggest_jlcpcb_alternatives",
            "Suggest JLCPCB-stocked alternative parts for a given component value and footprint.",
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string", "description": "Component value (e.g. '100nF')" },
                    "footprint": { "type": "string", "description": "KiCAD footprint identifier" },
                    "max_price_usd": { "type": "number", "description": "Maximum unit price in USD (optional)" },
                    "limit": { "type": "integer", "description": "Maximum number of suggestions", "default": 5 }
                },
                "required": ["value", "footprint"]
            }),
            |args, ctx| async move { handle_suggest_alternatives(args, ctx).await }
        ),
        tool!(
            "get_jlcpcb_database_stats",
            "Return statistics about the local JLCPCB database cache: part count, last updated, file size.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_jlcpcb_stats(args, ctx).await }
        ),
        tool!(
            "enrich_datasheets",
            "Fetch and cache datasheet URLs for all components in a schematic using the LCSC API.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "overwrite_existing": { "type": "boolean", "description": "Replace existing Datasheet fields", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_enrich_datasheets(args, ctx).await }
        ),
        tool!(
            "get_datasheet_url",
            "Retrieve the datasheet URL for a component by MPN or LCSC ID.",
            json!({
                "type": "object",
                "properties": {
                    "mpn": { "type": "string", "description": "Manufacturer part number (optional)" },
                    "lcsc_id": { "type": "string", "description": "LCSC part number (optional)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_get_datasheet_url(args, ctx).await }
        ),
        tool!(
            "check_freerouting",
            "Locate a Freerouting installation, including KiCad PCM plugin directories, and verify that its Java runtime is available.",
            json!({
                "type": "object",
                "properties": {
                    "jar_path": { "type": "string", "description": freerouting_jar_path_description() }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_freerouting(args, ctx).await }
        ),
        tool!(
            "route_specctra_dsn",
            "Route an existing Specctra DSN through the local Freerouting JAR's native headless MCP server and create a new SES file. Uses the documented session/job state machine, never sends board data to a cloud service, and never replaces an existing output.",
            json!({
                "type": "object",
                "properties": {
                    "dsn_path": { "type": "string", "description": "Existing Specctra .dsn input" },
                    "ses_output_path": { "type": "string", "description": "New .ses output path; existing files are never replaced" },
                    "jar_path": { "type": "string", "description": freerouting_jar_path_description() },
                    "max_passes": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "optimizer_enabled": { "type": "boolean" },
                    "job_timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 86400 },
                    "overall_timeout_seconds": { "type": "integer", "minimum": 10, "maximum": 86400, "default": 900 }
                },
                "required": ["dsn_path", "ses_output_path"]
            }),
            |args, ctx| async move { handle_route_specctra_dsn(args, ctx).await }
        ),
    ]
}

// ─── JLCPCB database path helper ─────────────────────────────────────────────

fn default_jlcpcb_db_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("jlcpcb.db")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".konnect").join("jlcpcb.db")
    }
}

fn resolve_db_path(args: &serde_json::Value, ctx: &ToolContext) -> PathBuf {
    if let Some(p) = args["output_path"].as_str() {
        return PathBuf::from(p);
    }
    if let Some(p) = &ctx.config.jlcpcb_db_path {
        return p.clone();
    }
    default_jlcpcb_db_path()
}

// ─── Retry/backoff for external HTTP calls ────────────────────────────────────
//
// JLCPCB database download and LCSC datasheet lookups are the only genuinely
// networked calls in this toolset (everything else queries the local SQLite
// cache). Both are prone to transient failures — timeouts, connection resets,
// rate limiting — that a simple retry clears up without any user action.

/// Retry policy: 3 attempts total, exponential backoff starting at 300ms
/// (300ms, then 600ms between attempts).
const RETRY_MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Whether an HTTP status is worth retrying. 429 (rate limited) and 5xx
/// (server-side) are transient; other 4xx (404, 401, ...) are not — retrying
/// a "not found" or "unauthorized" wastes time and won't change the outcome.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Delay before the next attempt, given the attempt number just made (1-based).
fn backoff_delay(attempt: u32) -> std::time::Duration {
    RETRY_BASE_DELAY * 2u32.pow(attempt.saturating_sub(1))
}

/// GET `url` with retry/backoff for transient failures (network-level errors,
/// 429, and 5xx). Returns the last response/error once attempts are exhausted.
async fn get_with_backoff(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !is_transient_status(status) || attempt >= RETRY_MAX_ATTEMPTS {
                    return Ok(resp);
                }
                tracing::warn!(
                    "[BETA] {} returned {} (attempt {}/{}), retrying",
                    url,
                    status,
                    attempt,
                    RETRY_MAX_ATTEMPTS
                );
            }
            Err(e) => {
                if attempt >= RETRY_MAX_ATTEMPTS {
                    return Err(e.into());
                }
                tracing::warn!(
                    "[BETA] request to {} failed (attempt {}/{}): {}, retrying",
                    url,
                    attempt,
                    RETRY_MAX_ATTEMPTS,
                    e
                );
            }
        }
        tokio::time::sleep(backoff_delay(attempt)).await;
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn parse_jlcpcb_chunk_count(value: &str) -> anyhow::Result<usize> {
    let chunk_count = value
        .trim()
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid JLCPCB feed chunk count: {value:?}"))?;
    if !(1..=JLCPCB_MAX_CHUNKS).contains(&chunk_count) {
        anyhow::bail!(
            "JLCPCB feed chunk count {chunk_count} is outside the supported range 1..={JLCPCB_MAX_CHUNKS}"
        );
    }
    Ok(chunk_count)
}

async fn download_jlcpcb_archive(
    client: &reqwest::Client,
    archive_path: &Path,
) -> anyhow::Result<(usize, u64)> {
    let sentinel_url = format!("{JLCPCB_FEED_BASE_URL}/{JLCPCB_FEED_SENTINEL}");
    let sentinel_response = get_with_backoff(client, &sentinel_url).await?;
    if !sentinel_response.status().is_success() {
        anyhow::bail!(
            "Failed to read JLCPCB feed manifest: HTTP {}",
            sentinel_response.status()
        );
    }
    let chunk_count = parse_jlcpcb_chunk_count(&sentinel_response.text().await?)?;

    let mut archive = tokio::fs::File::create(archive_path).await?;
    let mut downloaded_bytes = 0u64;
    for chunk_number in 1..=chunk_count {
        let chunk_url = format!("{JLCPCB_FEED_BASE_URL}/{JLCPCB_FEED_ARCHIVE}.{chunk_number:03}");
        let mut response = get_with_backoff(client, &chunk_url).await?;
        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to download JLCPCB database chunk {chunk_number}/{chunk_count}: HTTP {}",
                response.status()
            );
        }

        while let Some(bytes) = response.chunk().await? {
            downloaded_bytes = downloaded_bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("JLCPCB archive size overflow"))?;
            if downloaded_bytes > JLCPCB_MAX_ARCHIVE_BYTES {
                anyhow::bail!(
                    "JLCPCB archive exceeds the {} byte safety limit",
                    JLCPCB_MAX_ARCHIVE_BYTES
                );
            }
            archive.write_all(&bytes).await?;
        }
    }
    archive.sync_all().await?;
    Ok((chunk_count, downloaded_bytes))
}

fn extract_jlcpcb_database(archive_path: &Path, output_path: &Path) -> anyhow::Result<()> {
    let archive_file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(archive_file)?;
    if archive.len() != 1 {
        anyhow::bail!(
            "JLCPCB archive must contain exactly one database file; found {} entries",
            archive.len()
        );
    }

    let mut database = archive.by_index(0)?;
    if database.is_dir() || database.enclosed_name().is_none() {
        anyhow::bail!("JLCPCB archive does not contain a safe database file");
    }
    if database.size() > JLCPCB_MAX_DATABASE_BYTES {
        anyhow::bail!(
            "JLCPCB database exceeds the {} byte safety limit",
            JLCPCB_MAX_DATABASE_BYTES
        );
    }

    let output_file = std::fs::File::create(output_path)?;
    let mut output = BufWriter::new(output_file);
    io::copy(&mut database, &mut output)?;
    output.into_inner()?.sync_all()?;
    Ok(())
}

fn validate_upstream_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(parts)")?;
    let columns: HashSet<String> = statement
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    let required = [
        "LCSC Part",
        "First Category",
        "Second Category",
        "MFR.Part",
        "Package",
        "Manufacturer",
        "Library Type",
        "Description",
        "Datasheet",
        "Price",
        "Stock",
    ];
    let missing: Vec<&str> = required
        .into_iter()
        .filter(|column| !columns.contains(*column))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "JLCPCB feed schema is missing required columns: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn build_konnect_jlcpcb_database(upstream_path: &Path, output_path: &Path) -> anyhow::Result<u64> {
    let upstream = rusqlite::Connection::open_with_flags(
        upstream_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    validate_upstream_schema(&upstream)?;
    drop(upstream);

    let mut output = rusqlite::Connection::open(output_path)?;
    output.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;",
    )?;
    let upstream_path = upstream_path.to_string_lossy().into_owned();
    output.execute(
        "ATTACH DATABASE ?1 AS upstream",
        rusqlite::params![upstream_path],
    )?;

    let transaction = output.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE components (
             LCSC TEXT NOT NULL PRIMARY KEY,
             MFR_Part TEXT NOT NULL,
             Package TEXT NOT NULL,
             Manufacturer TEXT NOT NULL,
             Library_Type TEXT NOT NULL,
             Description TEXT NOT NULL,
             Datasheet TEXT NOT NULL,
             Price REAL NOT NULL,
             Stock INTEGER NOT NULL,
             Category TEXT NOT NULL
         );
         PRAGMA user_version = 1;",
    )?;
    let imported = transaction.execute(
        "INSERT INTO components (
             LCSC, MFR_Part, Package, Manufacturer, Library_Type,
             Description, Datasheet, Price, Stock, Category
         )
         SELECT
             COALESCE(\"LCSC Part\", ''),
             COALESCE(\"MFR.Part\", ''),
             COALESCE(\"Package\", ''),
             COALESCE(\"Manufacturer\", ''),
             COALESCE(\"Library Type\", ''),
             COALESCE(\"Description\", ''),
             COALESCE(\"Datasheet\", ''),
             CASE
                 WHEN instr(COALESCE(\"Price\", ''), ':') > 0
                 THEN CAST(substr(\"Price\", instr(\"Price\", ':') + 1) AS REAL)
                 ELSE CAST(COALESCE(NULLIF(\"Price\", ''), '0') AS REAL)
             END,
             CAST(COALESCE(NULLIF(\"Stock\", ''), '0') AS INTEGER),
             CASE
                 WHEN COALESCE(\"First Category\", '') = '' THEN COALESCE(\"Second Category\", '')
                 WHEN COALESCE(\"Second Category\", '') = '' THEN \"First Category\"
                 ELSE \"First Category\" || ' / ' || \"Second Category\"
             END
         FROM upstream.parts",
        [],
    )?;
    transaction.execute_batch(
        "CREATE INDEX components_mfr_part_idx ON components(MFR_Part);
         CREATE INDEX components_package_idx ON components(Package);
         CREATE INDEX components_library_stock_idx ON components(Library_Type, Stock);
         CREATE INDEX components_category_idx ON components(Category);",
    )?;
    transaction.commit()?;
    output.execute_batch("DETACH DATABASE upstream; PRAGMA synchronous = FULL;")?;

    let part_count: i64 =
        output.query_row("SELECT COUNT(*) FROM components", [], |row| row.get(0))?;
    if imported == 0 || part_count <= 0 {
        anyhow::bail!("JLCPCB feed did not contain any components");
    }
    let quick_check: String = output.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if quick_check != "ok" {
        anyhow::bail!("Generated JLCPCB database failed validation: {quick_check}");
    }
    drop(output);
    Ok(part_count.try_into()?)
}

fn replace_jlcpcb_database(
    staged_path: &Path,
    destination: &Path,
    backup_path: &Path,
) -> anyhow::Result<()> {
    if !destination.exists() {
        std::fs::rename(staged_path, destination)?;
        return Ok(());
    }

    std::fs::rename(destination, backup_path)?;
    if let Err(install_error) = std::fs::rename(staged_path, destination) {
        if let Err(restore_error) = std::fs::rename(backup_path, destination) {
            anyhow::bail!(
                "Failed to install the new JLCPCB database ({install_error}) and restore the previous database ({restore_error})"
            );
        }
        return Err(install_error.into());
    }
    std::fs::remove_file(backup_path)?;
    Ok(())
}

async fn handle_download_jlcpcb(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    let force = args["force"].as_bool().unwrap_or(false);

    if db_path.exists() && !force {
        let meta = tokio::fs::metadata(&db_path).await?;
        return Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "status": "already_exists",
                "path": db_path.to_str().unwrap_or(""),
                "size_bytes": meta.len(),
                "note": "Use force=true to re-download"
            }))
            .unwrap(),
        ));
    }

    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let db_parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let work_dir = tempfile::Builder::new()
        .prefix("konnect-jlcpcb-")
        .tempdir_in(db_parent)?;
    let archive_path = work_dir.path().join(JLCPCB_FEED_ARCHIVE);
    let upstream_path = work_dir.path().join("upstream.db");
    let staged_path = work_dir.path().join("konnect.db");
    let backup_path = work_dir.path().join("previous.db");

    let (chunk_count, downloaded_bytes) = download_jlcpcb_archive(&client, &archive_path).await?;
    let (part_count, size_bytes) = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        move || -> anyhow::Result<(u64, u64)> {
            extract_jlcpcb_database(&archive_path, &upstream_path)?;
            let part_count = build_konnect_jlcpcb_database(&upstream_path, &staged_path)?;
            let size_bytes = std::fs::metadata(&staged_path)?.len();
            replace_jlcpcb_database(&staged_path, &db_path, &backup_path)?;
            Ok((part_count, size_bytes))
        }
    })
    .await??;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "path": db_path.to_str().unwrap_or(""),
            "size_bytes": size_bytes,
            "part_count": part_count,
            "source": JLCPCB_FEED_BASE_URL,
            "downloaded_chunks": chunk_count,
            "downloaded_bytes": downloaded_bytes
        }))
        .unwrap(),
    ))
}

/// Build a deterministic cache key from a tool name, the resolved DB path
/// (so pointing at a different `output_path` never serves stale results),
/// and the query parameters that affect the result set.
fn cache_key(tool: &str, db_path: &std::path::Path, parts: &[&str]) -> String {
    format!("{}|{}|{}", tool, db_path.display(), parts.join("|"))
}

/// Columns `search_jlcpcb_parts` matches every query word against, in the
/// order they appear in the SQL and in the response's `search_fields`.
const JLCPCB_SEARCH_FIELDS: [&str; 5] =
    ["Description", "MFR_Part", "LCSC", "Package", "Manufacturer"];

/// The query words a part must all contain: the single source for both the
/// SQL and the `tokens` evidence in the response.
fn jlcpcb_search_tokens(query: &str) -> Vec<String> {
    query.split_whitespace().map(String::from).collect()
}

async fn handle_search_jlcpcb_parts(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // Arguments before environment: a missing `query` is the caller's
    // mistake and is theirs to fix, whereas a missing database is transient
    // and unrelated. Checking the database first told someone who had simply
    // forgotten the query to go and download a 2.5M-part catalogue (#218).
    let query = match require_str(args, "query") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }
    let basic_only = args["basic_only"].as_bool().unwrap_or(false);
    let in_stock = args["in_stock"].as_bool().unwrap_or(true);
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let category = args["category"].as_str().map(String::from);

    let key = cache_key(
        "search_jlcpcb_parts",
        &db_path,
        &[
            &query,
            category.as_deref().unwrap_or(""),
            &basic_only.to_string(),
            &in_stock.to_string(),
            &limit.to_string(),
        ],
    );
    if let Some(cached) = ctx.jlcpcb_cache.get(&key) {
        let mut body = cached;
        body["cached"] = json!(true);
        return Ok(CallToolResult::text(serde_json::to_string(&body).unwrap()));
    }

    let tokens = jlcpcb_search_tokens(&query);
    let sql_tokens = tokens.clone();
    let results = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = rusqlite::Connection::open(&db_path)?;

        // The JLCPCB db schema has columns: LCSC, MFR_Part, Package, Solder_Joint,
        // Manufacturer, Library_Type, Description, Datasheet, Price, Stock
        //
        // Every whitespace-separated word must match somewhere in one of
        // the searchable columns, in any order (#432). `%`, `_` and `\` in a
        // word are matched literally, and every value is a bound parameter.
        let mut sql = String::from(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Datasheet, Price, Stock \
             FROM components WHERE 1=1",
        );
        let mut params: Vec<String> = Vec::new();
        for token in &sql_tokens {
            let escaped = token
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            params.push(format!("%{}%", escaped));
            let n = params.len();
            sql.push_str(&format!(
                " AND (Description LIKE ?{n} ESCAPE '\\' OR MFR_Part LIKE ?{n} ESCAPE '\\' \
                 OR LCSC LIKE ?{n} ESCAPE '\\' OR Package LIKE ?{n} ESCAPE '\\' \
                 OR Manufacturer LIKE ?{n} ESCAPE '\\')"
            ));
        }
        if basic_only {
            sql.push_str(" AND Library_Type = 'Basic'");
        }
        if in_stock {
            sql.push_str(" AND Stock > 0");
        }
        if let Some(ref cat) = category {
            params.push(format!("%{}%", cat));
            sql.push_str(&format!(" AND Category LIKE ?{}", params.len()));
        }
        // Stable order so a LIMIT page is the same on every run (#432).
        sql.push_str(&format!(" ORDER BY LCSC LIMIT {}", limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), row_to_part_json)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    })
    .await??;

    let body = json!({
        "query": args["query"].as_str().unwrap_or(""),
        "count": results.len(),
        "results": results,
        "tokens": tokens,
        "search_fields": JLCPCB_SEARCH_FIELDS
    });
    ctx.jlcpcb_cache.put(key, body.clone());

    let mut body = body;
    body["cached"] = json!(false);
    Ok(CallToolResult::text(serde_json::to_string(&body).unwrap()))
}

fn row_to_part_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    // The exact LCSC PDF URL for most parts. Omitted here once made a caller
    // unable to get a datasheet the table already held (#255); an empty cell
    // reads as null rather than "" so callers can test presence directly.
    let datasheet_url = match row.get::<_, String>(6).unwrap_or_default() {
        url if url.is_empty() => serde_json::Value::Null,
        url => json!(url),
    };
    Ok(json!({
        "lcsc": row.get::<_, String>(0).unwrap_or_default(),
        "mpn": row.get::<_, String>(1).unwrap_or_default(),
        "package": row.get::<_, String>(2).unwrap_or_default(),
        "manufacturer": row.get::<_, String>(3).unwrap_or_default(),
        "library_type": row.get::<_, String>(4).unwrap_or_default(),
        "description": row.get::<_, String>(5).unwrap_or_default(),
        "datasheet_url": datasheet_url,
        "price": row.get::<_, f64>(7).unwrap_or(0.0),
        "stock": row.get::<_, i64>(8).unwrap_or(0)
    }))
}

async fn handle_get_jlcpcb_part(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }
    let lcsc_id = require_str(args, "lcsc_id")
        .map_err(|e| anyhow::anyhow!("{:?}", e))?
        .to_string();

    let key = cache_key("get_jlcpcb_part", &db_path, &[&lcsc_id]);
    if let Some(mut cached) = ctx.jlcpcb_cache.get(&key) {
        cached["cached"] = json!(true);
        return Ok(CallToolResult::text(
            serde_json::to_string(&cached).unwrap(),
        ));
    }

    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Datasheet, Price, Stock \
             FROM components WHERE LCSC = ?1 LIMIT 1"
        )?;
            let mut rows = stmt.query_map(rusqlite::params![lcsc_id], row_to_part_json)?;
            Ok(rows.next().and_then(|r| r.ok()))
        })
        .await??;

    match result {
        Some(part) => {
            ctx.jlcpcb_cache.put(key, part.clone());
            let mut part = part;
            part["cached"] = json!(false);
            Ok(CallToolResult::text(serde_json::to_string(&part).unwrap()))
        }
        None => Ok(CallToolResult::error(format!(
            "Part not found in database: {}",
            args["lcsc_id"].as_str().unwrap_or("")
        ))),
    }
}

async fn handle_suggest_alternatives(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // Arguments before environment — see `handle_search_jlcpcb_parts`.
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::error(
            "JLCPCB database not found. Run download_jlcpcb_database first.",
        ));
    }
    let max_price = args["max_price_usd"].as_f64();
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;

    // Extract package from footprint (e.g. "Resistor_SMD:R_0402" → "0402")
    let package_hint = footprint
        .split(':')
        .next_back()
        .unwrap_or("")
        .split('_')
        .next_back()
        .unwrap_or("")
        .to_string();

    let key = cache_key(
        "suggest_jlcpcb_alternatives",
        &db_path,
        &[
            &value,
            &footprint,
            &max_price.map(|v| v.to_string()).unwrap_or_default(),
            &limit.to_string(),
        ],
    );
    if let Some(cached) = ctx.jlcpcb_cache.get(&key) {
        let mut body = cached;
        body["cached"] = json!(true);
        return Ok(CallToolResult::text(serde_json::to_string(&body).unwrap()));
    }

    let results = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = rusqlite::Connection::open(&db_path)?;
        let like_val = format!("%{}%", value);
        let like_pkg = format!("%{}%", package_hint);

        let mut sql = String::from(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Datasheet, Price, Stock \
             FROM components WHERE Description LIKE ?1 AND Package LIKE ?2 AND Stock > 0"
        );
        if let Some(max_p) = max_price {
            sql.push_str(&format!(" AND Price <= {}", max_p));
        }
        sql.push_str(&format!(" ORDER BY Price ASC LIMIT {}", limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params![like_val, like_pkg], row_to_part_json)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    })
    .await??;

    let body = json!({
        "value": args["value"].as_str().unwrap_or(""),
        "footprint": args["footprint"].as_str().unwrap_or(""),
        "alternatives": results
    });
    ctx.jlcpcb_cache.put(key, body.clone());

    let mut body = body;
    body["cached"] = json!(false);
    Ok(CallToolResult::text(serde_json::to_string(&body).unwrap()))
}

async fn handle_jlcpcb_stats(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "exists": false,
                "note": "Run download_jlcpcb_database to fetch the parts database"
            }))
            .unwrap(),
        ));
    }

    let meta = tokio::fs::metadata(&db_path).await?;
    let size_bytes = meta.len();

    let count = tokio::task::spawn_blocking({
        let db_path = db_path.clone();
        move || -> anyhow::Result<i64> {
            let conn = rusqlite::Connection::open(&db_path)?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM components", [], |r| r.get(0))?;
            Ok(count)
        }
    })
    .await??;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "exists": true,
            "path": db_path.to_str().unwrap_or(""),
            "size_bytes": size_bytes,
            "part_count": count
        }))
        .unwrap(),
    ))
}

async fn handle_enrich_datasheets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let overwrite = args["overwrite_existing"].as_bool().unwrap_or(false);

    let read_path = sch_path.clone();
    let content = tokio::task::spawn_blocking(move || read_consistent(&read_path)).await??;

    // Only placed, top-level symbol instances are enrichment targets. Library
    // definitions, quoted text, and nested decoys must not enter the target
    // set merely because they contain the same property spelling.
    let symbols = match validated_datasheet_symbols(&content) {
        Ok(symbols) => symbols,
        Err(error) => return Ok(error.into_result()),
    };
    let mut lcsc_ids: Vec<String> = symbols
        .iter()
        .map(|symbol| symbol.lcsc_id.clone())
        .collect();
    lcsc_ids.sort();
    lcsc_ids.dedup();

    if lcsc_ids.is_empty() {
        return Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "updated": 0,
                "note": "No LCSC property found in schematic components"
            }))
            .unwrap(),
        ));
    }

    // Query LCSC API for datasheet URLs
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut urls = HashMap::new();

    for lcsc_id in &lcsc_ids {
        let url = format!(
            "https://wmsc.lcsc.com/ftps/wm/product/detail?productCode={}",
            lcsc_id
        );
        if let Ok(resp) = get_with_backoff(&client, &url).await {
            if resp.status().is_success() {
                if let Ok(json_resp) = resp.json::<serde_json::Value>().await {
                    if let Some(datasheet_url) = json_resp
                        .pointer("/result/dataManualUrl")
                        .and_then(|v| v.as_str())
                    {
                        urls.insert(lcsc_id.clone(), datasheet_url.to_owned());
                    }
                }
            }
        }
    }

    let plan = match plan_datasheet_enrichment(&content, &urls, overwrite) {
        Ok(plan) => plan,
        Err(error) => return Ok(error.into_result()),
    };

    let observed_updates = if let Some((command, planned)) = plan {
        if let Err(error) = commit_command(&sch_path, &command) {
            if let Some(refusal) = datasheet_commit_refusal(&sch_path, &error) {
                return Ok(refusal);
            }
            return Err(error.into());
        }
        let committed = read_consistent(&sch_path)?;
        match observe_datasheet_updates(&committed, &planned) {
            Ok(observed) => observed,
            Err(error) => return Ok(error.into_result()),
        }
    } else {
        Vec::new()
    };

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "lcsc_ids_found": lcsc_ids.len(),
            "datasheets_enriched": observed_updates.len(),
            "schematic": sch_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

#[derive(Debug, Clone)]
struct DatasheetSymbolTarget {
    uuid: String,
    reference: String,
    unit: u32,
    lcsc_id: String,
    datasheet: Option<String>,
}

#[derive(Debug, Clone)]
struct PlannedDatasheetUpdate {
    target: DatasheetSymbolTarget,
    datasheet_url: String,
}

#[derive(Debug)]
enum DatasheetTargetError {
    Ambiguous {
        target: String,
        candidates: Vec<String>,
    },
    Stale {
        target: String,
        reason: String,
    },
}

impl DatasheetTargetError {
    fn stale(target: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Stale {
            target: target.into(),
            reason: reason.into(),
        }
    }

    fn from_sexp(target: impl Into<String>, error: SexpError) -> Self {
        Self::stale(target, error.to_string())
    }

    fn into_result(self) -> CallToolResult {
        match self {
            Self::Ambiguous { target, candidates } => {
                let reason = format!(
                    "more than one candidate was observed: {}",
                    candidates.join(", ")
                );
                CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: target.clone(),
                        reason: reason.clone(),
                    },
                    format!("cannot safely enrich {target}: {reason}"),
                )
            }
            Self::Stale { target, reason } => CallToolResult::error_kind(
                ToolErrorKind::StaleTarget {
                    target: target.clone(),
                    reason: reason.clone(),
                },
                format!("cannot safely enrich {target}: {reason}"),
            ),
        }
    }
}

fn direct_property_values(node: &SexpNode, name: &str) -> Vec<String> {
    node.find_all("property")
        .into_iter()
        .filter(|property| property.get(1).and_then(SexpNode::as_str) == Some(name))
        .filter_map(|property| property.get(2).and_then(SexpNode::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn one_identity_value(
    values: Vec<String>,
    field: &str,
    item: &str,
) -> Result<String, DatasheetTargetError> {
    match values.as_slice() {
        [value] if !value.trim().is_empty() => Ok(value.clone()),
        [] | [_] => Err(DatasheetTargetError::stale(
            item,
            format!("{field} is missing or empty"),
        )),
        _ => Err(DatasheetTargetError::Ambiguous {
            target: format!("{field} on {item}"),
            candidates: values,
        }),
    }
}

fn validated_datasheet_symbols(
    content: &str,
) -> Result<Vec<DatasheetSymbolTarget>, DatasheetTargetError> {
    let ranges = find_direct_child_blocks(content, "kicad_sch");
    if ranges.is_empty() {
        return Err(DatasheetTargetError::stale(
            "schematic document",
            "the kicad_sch root is missing or malformed",
        ));
    }

    let mut uuid_owners: HashMap<String, Vec<String>> = HashMap::new();
    let mut symbols = Vec::new();
    for (start, end) in ranges {
        let node = parse_sexp(&content[start..end])
            .map_err(|error| DatasheetTargetError::from_sexp("schematic item", error))?;
        let kind = node.head().unwrap_or("unknown");
        if let Some(uuid) = node
            .find("uuid")
            .and_then(|uuid| uuid.get(1))
            .and_then(SexpNode::as_str)
        {
            uuid_owners
                .entry(uuid.to_owned())
                .or_default()
                .push(format!("{kind} at byte {start}"));
        }
        if kind != "symbol" {
            continue;
        }

        let lcsc = direct_property_values(&node, "LCSC");
        if lcsc.is_empty() {
            continue;
        }
        let item = format!("symbol at byte {start}");
        let lcsc_id = one_identity_value(lcsc, "LCSC", &item)?;
        let reference = one_identity_value(
            direct_property_values(&node, "Reference"),
            "Reference",
            &item,
        )?;
        let uuid = node
            .find("uuid")
            .and_then(|uuid| uuid.get(1))
            .and_then(SexpNode::as_str)
            .filter(|uuid| !uuid.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| DatasheetTargetError::stale(&item, "UUID is missing or empty"))?;
        let units = node.find_all("unit");
        let unit = match units.as_slice() {
            [] => 1,
            [unit] => unit
                .get(1)
                .and_then(SexpNode::as_str)
                .and_then(|unit| unit.parse::<u32>().ok())
                .filter(|unit| *unit > 0)
                .ok_or_else(|| DatasheetTargetError::stale(&item, "unit is invalid"))?,
            _ => {
                return Err(DatasheetTargetError::Ambiguous {
                    target: format!("unit on {item}"),
                    candidates: units
                        .iter()
                        .filter_map(|unit| unit.get(1).and_then(SexpNode::as_str))
                        .map(ToOwned::to_owned)
                        .collect(),
                })
            }
        };
        let datasheets = direct_property_values(&node, "Datasheet");
        let datasheet = match datasheets.as_slice() {
            [] => None,
            [value] => Some(value.clone()),
            _ => {
                return Err(DatasheetTargetError::Ambiguous {
                    target: format!("Datasheet on {reference} unit {unit}"),
                    candidates: datasheets,
                })
            }
        };
        symbols.push(DatasheetSymbolTarget {
            uuid,
            reference,
            unit,
            lcsc_id,
            datasheet,
        });
    }

    for symbol in &symbols {
        if let Some(owners) = uuid_owners
            .get(&symbol.uuid)
            .filter(|owners| owners.len() > 1)
        {
            return Err(DatasheetTargetError::Ambiguous {
                target: format!("schematic UUID {}", symbol.uuid),
                candidates: owners.clone(),
            });
        }
    }

    let mut placed: HashMap<(String, u32), Vec<String>> = HashMap::new();
    for symbol in &symbols {
        placed
            .entry((symbol.reference.clone(), symbol.unit))
            .or_default()
            .push(symbol.uuid.clone());
    }
    if let Some(((reference, unit), uuids)) = placed.iter().find(|(_, uuids)| uuids.len() > 1) {
        return Err(DatasheetTargetError::Ambiguous {
            target: format!("symbol {reference} unit {unit}"),
            candidates: uuids.clone(),
        });
    }

    Ok(symbols)
}

fn plan_datasheet_enrichment(
    content: &str,
    urls: &HashMap<String, String>,
    overwrite: bool,
) -> Result<Option<(SchematicCommand, Vec<PlannedDatasheetUpdate>)>, DatasheetTargetError> {
    let symbols = validated_datasheet_symbols(content)?;
    let mut edited = content.to_owned();
    let mut planned = Vec::new();
    let path = Path::new("datasheet-enrichment.kicad_sch");

    for symbol in symbols {
        let Some(datasheet_url) = urls.get(&symbol.lcsc_id) else {
            continue;
        };
        let Some(existing) = &symbol.datasheet else {
            continue;
        };
        if (!overwrite && existing != "~" && !existing.is_empty()) || existing == datasheet_url {
            continue;
        }
        let id = ItemId::new(symbol.uuid.clone())
            .map_err(|error| DatasheetTargetError::from_sexp(&symbol.reference, error))?;
        let command = SchematicCommand::set_property(
            &edited,
            id,
            "Datasheet",
            datasheet_url,
            format!(
                "enrich datasheet for {} unit {}",
                symbol.reference, symbol.unit
            ),
        )
        .map_err(|error| DatasheetTargetError::from_sexp(&symbol.reference, error))?;
        edited = prepare_command(path, &edited, &command)
            .map_err(|error| DatasheetTargetError::from_sexp(&symbol.reference, error))?
            .0;
        planned.push(PlannedDatasheetUpdate {
            target: symbol,
            datasheet_url: datasheet_url.clone(),
        });
    }

    if planned.is_empty() {
        return Ok(None);
    }
    let ids = planned
        .iter()
        .map(|update| ItemId::new(update.target.uuid.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DatasheetTargetError::from_sexp("datasheet targets", error))?;
    let command = SchematicCommand::replace_items_from_document(
        content,
        &edited,
        ids,
        "enrich schematic datasheets",
    )
    .map_err(|error| DatasheetTargetError::from_sexp("datasheet targets", error))?
    .requiring_unchanged_document();
    Ok(Some((command, planned)))
}

fn observe_datasheet_updates(
    content: &str,
    planned: &[PlannedDatasheetUpdate],
) -> Result<Vec<serde_json::Value>, DatasheetTargetError> {
    let symbols = validated_datasheet_symbols(content)?;
    let mut observed = Vec::with_capacity(planned.len());
    for update in planned {
        let target = symbols
            .iter()
            .find(|symbol| symbol.uuid == update.target.uuid)
            .ok_or_else(|| {
                DatasheetTargetError::stale(
                    format!("symbol UUID {}", update.target.uuid),
                    "the committed symbol is missing",
                )
            })?;
        if target.reference != update.target.reference
            || target.unit != update.target.unit
            || target.lcsc_id != update.target.lcsc_id
        {
            return Err(DatasheetTargetError::stale(
                format!("symbol UUID {}", update.target.uuid),
                "committed identity differs from the planned reference, unit, or LCSC ID",
            ));
        }
        if target.datasheet.as_deref() != Some(&update.datasheet_url) {
            return Err(DatasheetTargetError::stale(
                format!("symbol {} unit {}", target.reference, target.unit),
                format!(
                    "committed Datasheet was {:?}, expected {}",
                    target.datasheet, update.datasheet_url
                ),
            ));
        }
        observed.push(json!({
            "uuid": target.uuid,
            "reference": target.reference,
            "unit": target.unit,
            "lcsc_id": target.lcsc_id,
            "datasheet_url": target.datasheet
        }));
    }
    Ok(observed)
}

fn datasheet_commit_refusal(path: &Path, error: &SexpError) -> Option<CallToolResult> {
    let reason = match error {
        SexpError::Conflict { .. } => "the schematic changed after enrichment was planned",
        SexpError::ItemConflict { reason, .. } => reason,
        SexpError::KiCadEditorLocked { .. } => {
            "KiCad owns the schematic; use a live editor mutation or close the document"
        }
        _ => return None,
    };
    Some(CallToolResult::error_kind(
        ToolErrorKind::StaleTarget {
            target: path.display().to_string(),
            reason: reason.to_owned(),
        },
        format!(
            "cannot commit datasheet enrichment for {}: {reason}",
            path.display()
        ),
    ))
}

/// The datasheet URL from the local JLCPCB catalog, keyed by LCSC ID or MPN.
/// The database Konnect already downloads carries the exact LCSC PDF URL in
/// its `Datasheet` column for most parts, so a hit here answers without a
/// network round trip. `Ok(None)` is an ordinary miss; `Err` means the
/// database could not be read at all.
async fn datasheet_from_catalog(
    args: &serde_json::Value,
    ctx: &ToolContext,
    lcsc_id: Option<&str>,
    mpn: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let db_path = resolve_db_path(args, ctx);
    if !db_path.exists() {
        return Ok(None);
    }
    let lcsc_id = lcsc_id.map(String::from);
    let mpn = mpn.map(String::from);
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let conn = rusqlite::Connection::open(&db_path)?;
        let (sql, param): (&str, String) = match (&lcsc_id, &mpn) {
            // An LCSC ID is the primary key shape, so it wins when both are
            // given — same precedence the web path used.
            (Some(id), _) => (
                "SELECT Datasheet FROM components WHERE LCSC = ?1 LIMIT 1",
                id.clone(),
            ),
            (None, Some(mpn)) => (
                "SELECT Datasheet FROM components WHERE MFR_Part = ?1 LIMIT 1",
                mpn.clone(),
            ),
            // Unreachable: the caller rejects both-absent before calling.
            (None, None) => return Ok(None),
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query_map(rusqlite::params![param], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?.filter(|url| !url.is_empty()))
    })
    .await?
}

async fn handle_get_datasheet_url(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let mpn = args["mpn"].as_str();
    let lcsc_id = args["lcsc_id"].as_str();

    if mpn.is_none() && lcsc_id.is_none() {
        return Ok(CallToolResult::error("Provide either 'mpn' or 'lcsc_id'"));
    }

    // The local catalog first: it already holds exact URLs, so a hit saves a
    // network round trip and works offline. Only a miss falls through to the
    // live LCSC API.
    if let Ok(Some(url)) = datasheet_from_catalog(args, ctx, lcsc_id, mpn).await {
        return Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "mpn": mpn,
                "lcsc_id": lcsc_id,
                "datasheet_url": url,
                "source": "local_catalog"
            }))
            .unwrap(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // Try LCSC API with lcsc_id first
    if let Some(id) = lcsc_id {
        let url = format!(
            "https://wmsc.lcsc.com/ftps/wm/product/detail?productCode={}",
            id
        );
        if let Ok(resp) = get_with_backoff(&client, &url).await {
            if resp.status().is_success() {
                if let Ok(json_resp) = resp.json::<serde_json::Value>().await {
                    if let Some(ds_url) = json_resp
                        .pointer("/result/dataManualUrl")
                        .and_then(|v| v.as_str())
                    {
                        return Ok(CallToolResult::text(
                            serde_json::to_string(&json!({
                                "lcsc_id": id,
                                "datasheet_url": ds_url,
                                "source": "lcsc_api"
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }
        }
    }

    // Both sources missed — say which were tried, not just that the answer
    // is null (#255).
    let mut tried = vec![];
    if resolve_db_path(&serde_json::Value::Null, ctx).exists() {
        tried.push("local catalog");
    } else {
        tried.push("local catalog (database not downloaded)");
    }
    if lcsc_id.is_some() {
        tried.push("LCSC API");
    }
    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "mpn": mpn,
            "lcsc_id": lcsc_id,
            "datasheet_url": null,
            "note": format!("Datasheet not found via {}", tried.join(" or "))
        }))
        .unwrap(),
    ))
}

// ─── Freerouting ──────────────────────────────────────────────────────────────

fn is_freerouting_jar(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().contains("freerouting"))
}

fn find_freerouting_jar_below(root: &Path, remaining_depth: usize) -> Option<PathBuf> {
    if is_freerouting_jar(root) {
        return Some(root.to_path_buf());
    }
    if remaining_depth == 0 || !root.is_dir() {
        return None;
    }

    let mut entries = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();

    entries
        .iter()
        .find(|path| is_freerouting_jar(path))
        .cloned()
        .or_else(|| {
            entries.into_iter().find_map(|path| {
                let file_type = std::fs::symlink_metadata(&path).ok()?.file_type();
                file_type
                    .is_dir()
                    .then(|| find_freerouting_jar_below(&path, remaining_depth - 1))
                    .flatten()
            })
        })
}

/// Where Freerouting discovery looks when the caller passes no `jar_path`, in
/// the order it looks. The `jar_path` descriptions and the `searched` evidence
/// are both built from this table, so what the schema says and what the
/// search does cannot drift apart. There is no configuration setting: the
/// schema once said "uses config default" for one that never existed (#787).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FreeroutingLocation {
    KicadThirdPartyVariable,
    KicadPluginFolder,
    WorkingDirectory,
    SystemDirectory,
    PathDirectory,
}

impl FreeroutingLocation {
    /// Every location, in search order.
    const ALL: [Self; 5] = [
        Self::KicadThirdPartyVariable,
        Self::KicadPluginFolder,
        Self::WorkingDirectory,
        Self::SystemDirectory,
        Self::PathDirectory,
    ];

    fn kind(self) -> &'static str {
        match self {
            Self::KicadThirdPartyVariable => "kicad_3rd_party_variable",
            Self::KicadPluginFolder => "kicad_plugin_folder",
            Self::WorkingDirectory => "working_directory",
            Self::SystemDirectory => "system_directory",
            Self::PathDirectory => "path_directory",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::KicadThirdPartyVariable => {
                "the KICAD10_3RD_PARTY, KICAD9_3RD_PARTY and KICAD8_3RD_PARTY directories"
            }
            Self::KicadPluginFolder => {
                "the KiCad 10, 9 and 8 plugin folders under the home directory \
                 (Documents/KiCad/<version>/3rdparty/plugins and \
                 .local/share/kicad/<version>/3rdparty/plugins)"
            }
            Self::WorkingDirectory => "freerouting.jar in the server's working directory",
            Self::SystemDirectory => "/usr/local/lib/freerouting and /opt/freerouting",
            Self::PathDirectory => "every directory on PATH",
        }
    }
}

fn freerouting_jar_path_description() -> String {
    let locations = FreeroutingLocation::ALL
        .iter()
        .map(|location| location.description())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Path to a Freerouting JAR (optional). When omitted, Konnect searches, in order: \
         {locations}. There is no configuration setting for it."
    )
}

/// One place discovery looked, and how many directory levels below it.
#[derive(Debug, Clone, PartialEq)]
struct FreeroutingSearchRoot {
    location: FreeroutingLocation,
    path: PathBuf,
    depth: usize,
}

impl FreeroutingSearchRoot {
    fn evidence(&self) -> serde_json::Value {
        json!({
            "kind": self.location.kind(),
            "path": self.path,
            "max_depth": self.depth,
            "exists": self.path.exists(),
        })
    }
}

fn freerouting_search_roots() -> Vec<FreeroutingSearchRoot> {
    freerouting_search_roots_from(
        |name| std::env::var_os(name),
        &std::env::current_dir().unwrap_or_default(),
    )
}

/// The search roots for a given environment, so a test can supply one without
/// touching the process's own.
fn freerouting_search_roots_from(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    working_directory: &Path,
) -> Vec<FreeroutingSearchRoot> {
    let mut roots: Vec<FreeroutingSearchRoot> = Vec::new();
    // `HOME` and `USERPROFILE` are often the same directory (Git Bash on
    // Windows sets both), and searching a root twice only repeats it in the
    // evidence.
    let mut push = |location, path: PathBuf, depth| {
        if !roots.iter().any(|root| root.path == path) {
            roots.push(FreeroutingSearchRoot {
                location,
                path,
                depth,
            })
        }
    };

    for variable in ["KICAD10_3RD_PARTY", "KICAD9_3RD_PARTY", "KICAD8_3RD_PARTY"] {
        if let Some(path) = var(variable) {
            push(
                FreeroutingLocation::KicadThirdPartyVariable,
                PathBuf::from(path),
                5,
            );
        }
    }

    let plugins =
        |base: PathBuf, version: &str| base.join(version).join("3rdparty").join("plugins");
    if let Some(home) = var("HOME").map(PathBuf::from) {
        for version in ["10.0", "9.0", "8.0"] {
            let documents = home.join("Documents").join("KiCad");
            let share = home.join(".local").join("share").join("kicad");
            push(
                FreeroutingLocation::KicadPluginFolder,
                plugins(documents, version),
                5,
            );
            push(
                FreeroutingLocation::KicadPluginFolder,
                plugins(share, version),
                5,
            );
        }
    }

    #[cfg(target_os = "windows")]
    if let Some(profile) = var("USERPROFILE").map(PathBuf::from) {
        for version in ["10.0", "9.0", "8.0"] {
            push(
                FreeroutingLocation::KicadPluginFolder,
                plugins(profile.join("Documents").join("KiCad"), version),
                5,
            );
        }
    }

    push(
        FreeroutingLocation::WorkingDirectory,
        working_directory.join("freerouting.jar"),
        0,
    );
    for directory in ["/usr/local/lib/freerouting", "/opt/freerouting"] {
        push(
            FreeroutingLocation::SystemDirectory,
            PathBuf::from(directory),
            3,
        );
    }
    if let Some(path) = var("PATH") {
        for directory in std::env::split_paths(&path) {
            push(FreeroutingLocation::PathDirectory, directory, 1);
        }
    }
    roots
}

/// What looking for the Freerouting JAR found.
#[derive(Debug, PartialEq)]
enum FreeroutingJar {
    Found(PathBuf),
    /// The caller passed `jar_path`, and it is not a file.
    SuppliedPathIsNotAFile(PathBuf),
    /// Discovery searched every root and found no JAR.
    NotFound(Vec<FreeroutingSearchRoot>),
}

fn find_freerouting_jar(args: &serde_json::Value) -> FreeroutingJar {
    if let Some(path) = args["jar_path"].as_str() {
        let path = PathBuf::from(path);
        return if path.is_file() {
            FreeroutingJar::Found(path)
        } else {
            FreeroutingJar::SuppliedPathIsNotAFile(path)
        };
    }

    let roots = freerouting_search_roots();
    match roots
        .iter()
        .find_map(|root| find_freerouting_jar_below(&root.path, root.depth))
    {
        Some(jar) => FreeroutingJar::Found(jar),
        None => FreeroutingJar::NotFound(roots),
    }
}

fn command_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod datasheet_enrichment_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use std::io::Write;

    const KICAD_STRUCTURAL_FIXTURE: &str =
        include_str!("../../tests/fixtures/structural_scans_kicad10.kicad_sch");

    fn symbol(reference: &str, unit: u32, uuid: Option<&str>, lcsc: &str) -> String {
        let uuid = uuid
            .map(|uuid| format!("\r\n\t\t(uuid \"{uuid}\")"))
            .unwrap_or_default();
        format!(
            "\t(symbol\r\n\t\t(lib_id \"Device:R\")\r\n\t\t(at 10 20 0)\r\n\t\t(unit {unit})\r\n\t\t(property \"Reference\" \"{reference}\")\r\n\t\t(property \"Datasheet\" \"~\")\r\n\t\t(property \"Notes\" \"decoy\"\r\n\t\t\t(property \"LCSC\" \"C-DECOY\")\r\n\t\t)\r\n\t\t(property \"LCSC\" \"{lcsc}\"){uuid}\r\n\t)\r\n"
        )
    }

    fn schematic(body: &str) -> String {
        format!("(kicad_sch\r\n\t(version 20231120)\r\n{body})\r\n")
    }

    fn urls(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(id, url)| ((*id).to_owned(), (*url).to_owned()))
            .collect()
    }

    #[test]
    fn tab_crlf_multi_unit_symbols_update_atomically_and_read_back() {
        let decoy = "\t(lib_symbols\r\n\t\t(symbol \"decoy\"\r\n\t\t\t(property \"LCSC\" \"C-LIBRARY\")\r\n\t\t)\r\n\t)\r\n";
        let original = schematic(&format!(
            "{decoy}{}{}",
            symbol("U1", 1, Some("unit-1"), "C1"),
            symbol("U1", 2, Some("unit-2"), "C1")
        ));
        let Some((command, planned)) = plan_datasheet_enrichment(
            &original,
            &urls(&[("C1", "https://example.com/u1.pdf")]),
            false,
        )
        .unwrap() else {
            panic!("expected enrichment plan");
        };
        assert_eq!(planned.len(), 2);

        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(original.as_bytes()).unwrap();
        file.flush().unwrap();
        let outcome = commit_command(file.path(), &command).unwrap();
        assert!(outcome.changed);

        let committed = std::fs::read_to_string(file.path()).unwrap();
        let observed = observe_datasheet_updates(&committed, &planned).unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0]["reference"], "U1");
        assert_eq!(observed[0]["unit"], 1);
        assert_eq!(observed[1]["unit"], 2);
        assert!(observed
            .iter()
            .all(|update| update["datasheet_url"] == "https://example.com/u1.pdf"));
        assert!(committed.contains("\r\n\t\t(property \"Notes\" \"decoy\"\r\n"));
        assert!(committed.contains("(property \"LCSC\" \"C-LIBRARY\")"));
        assert!(committed.contains("(property \"LCSC\" \"C-DECOY\")"));
    }

    #[test]
    fn kicad_authored_symbol_is_enriched_structurally_and_read_back() {
        let Some((command, planned)) = plan_datasheet_enrichment(
            KICAD_STRUCTURAL_FIXTURE,
            &urls(&[("C25804", "https://example.com/c25804.pdf")]),
            false,
        )
        .unwrap() else {
            panic!("expected enrichment plan");
        };
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].target.reference, "R1");

        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(KICAD_STRUCTURAL_FIXTURE.as_bytes()).unwrap();
        file.flush().unwrap();
        let outcome = commit_command(file.path(), &command).unwrap();
        assert!(outcome.changed);

        let committed = std::fs::read_to_string(file.path()).unwrap();
        let observed = observe_datasheet_updates(&committed, &planned).unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0]["reference"], "R1");
        assert_eq!(observed[0]["lcsc_id"], "C25804");
        assert_eq!(
            observed[0]["datasheet_url"],
            "https://example.com/c25804.pdf"
        );
        parse_sexp(&committed).expect("enriched KiCad fixture must still parse");
    }

    #[test]
    fn nested_or_library_lcsc_text_is_not_a_symbol_target() {
        let content = schematic(
            "\t(lib_symbols\n\t\t(symbol \"X\" (property \"LCSC\" \"C1\"))\n\t)\n\t(text \"(property LCSC C1)\" (at 0 0) (uuid \"text-1\"))\n",
        );
        assert!(validated_datasheet_symbols(&content).unwrap().is_empty());
        assert!(plan_datasheet_enrichment(
            &content,
            &urls(&[("C1", "https://example.com/decoy.pdf")]),
            false
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn missing_uuid_is_a_stale_target() {
        let content = schematic(&symbol("R1", 1, None, "C1"));
        let error = validated_datasheet_symbols(&content).unwrap_err();
        assert!(matches!(error, DatasheetTargetError::Stale { .. }));
    }

    #[test]
    fn duplicate_reference_and_unit_is_a_stale_refusal() {
        let content = schematic(&format!(
            "{}{}",
            symbol("U1", 1, Some("first"), "C1"),
            symbol("U1", 1, Some("second"), "C2")
        ));
        let error = validated_datasheet_symbols(&content).unwrap_err();
        assert!(matches!(&error, DatasheetTargetError::Ambiguous { .. }));
        assert_eq!(
            extract_error_kind(&error.into_result()).as_deref(),
            Some("stale_target")
        );
    }

    #[test]
    fn duplicate_uuid_is_a_stale_refusal_even_across_item_kinds() {
        let content = schematic(&format!(
            "{}\t(junction (at 1 2) (uuid \"shared\"))\r\n",
            symbol("R1", 1, Some("shared"), "C1")
        ));
        let error = validated_datasheet_symbols(&content).unwrap_err();
        assert!(matches!(&error, DatasheetTargetError::Ambiguous { .. }));
        assert_eq!(
            extract_error_kind(&error.into_result()).as_deref(),
            Some("stale_target")
        );
    }

    #[test]
    fn stale_document_revision_refuses_without_overwriting() {
        let original = schematic(&symbol("R1", 1, Some("r1"), "C1"));
        let Some((command, _)) = plan_datasheet_enrichment(
            &original,
            &urls(&[("C1", "https://example.com/r1.pdf")]),
            false,
        )
        .unwrap() else {
            panic!("expected enrichment plan");
        };
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        let newer = original.replace("(at 10 20 0)", "(at 11 20 0)");
        file.write_all(newer.as_bytes()).unwrap();
        file.flush().unwrap();

        let error = commit_command(file.path(), &command).unwrap_err();
        let refusal = datasheet_commit_refusal(file.path(), &error).unwrap();
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("stale_target")
        );
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), newer);
    }

    #[test]
    fn overwrite_policy_and_noop_counts_are_truthful() {
        let original = schematic(&symbol("R1", 1, Some("r1"), "C1")).replace(
            "(property \"Datasheet\" \"~\")",
            "(property \"Datasheet\" \"https://existing.example/r1.pdf\")",
        );
        let replacements = urls(&[("C1", "https://new.example/r1.pdf")]);
        assert!(plan_datasheet_enrichment(&original, &replacements, false)
            .unwrap()
            .is_none());
        assert!(plan_datasheet_enrichment(&original, &replacements, true)
            .unwrap()
            .is_some());

        let same = urls(&[("C1", "https://existing.example/r1.pdf")]);
        assert!(plan_datasheet_enrichment(&original, &same, true)
            .unwrap()
            .is_none());
    }
}

async fn run_java_command(
    command: &mut tokio::process::Command,
) -> Result<std::process::Output, String> {
    command.kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(10), command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("Java command timed out after 10 seconds".to_string()),
    }
}

async fn handle_check_freerouting(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    match find_freerouting_jar(args) {
        FreeroutingJar::SuppliedPathIsNotAFile(path) => Ok(CallToolResult::json(&json!({
            "available": false,
            "engine_found": false,
            "native_mcp_available": false,
            "bridge_available": false,
            "checked_path": path,
            "note": format!("jar_path is not a file: {}", path.display())
        }))),
        FreeroutingJar::NotFound(roots) => Ok(CallToolResult::json(&json!({
            "available": false,
            "engine_found": false,
            "native_mcp_available": false,
            "bridge_available": false,
            "searched_locations": roots.iter().map(FreeroutingSearchRoot::evidence).collect::<Vec<_>>(),
            "note": "No Freerouting JAR was found in any searched location. Install Freerouting \
                     (KiCad's Plugin and Content Manager, or \
                     https://github.com/freerouting/freerouting/releases), or pass jar_path."
        }))),
        FreeroutingJar::Found(jar_path) => {
            let mut java_command = tokio::process::Command::new("java");
            java_command.arg("-version");
            let java = match run_java_command(&mut java_command).await {
                Ok(output) => output,
                Err(error) => {
                    return Ok(CallToolResult::json(&json!({
                        "available": false,
                        "engine_found": true,
                        "native_mcp_available": false,
                        "bridge_available": false,
                        "jar_path": jar_path,
                        "java_available": false,
                        "note": error
                    })));
                }
            };
            if !java.status.success() {
                return Ok(CallToolResult::json(&json!({
                    "available": false,
                    "engine_found": true,
                    "native_mcp_available": false,
                    "bridge_available": false,
                    "jar_path": jar_path,
                    "java_available": false,
                    "java_output": command_output(&java),
                    "note": "Freerouting was found, but Java did not start successfully"
                })));
            }

            let mut freerouting_command = tokio::process::Command::new("java");
            freerouting_command.args(["-jar", jar_path.to_str().unwrap_or(""), "--version"]);
            let freerouting = run_java_command(&mut freerouting_command).await;
            let (version_checked, version_output) = match freerouting {
                Ok(output) => (output.status.success(), command_output(&output)),
                Err(error) => (false, error),
            };

            let bridge = crate::freerouting_mcp::probe_local(&jar_path).await;
            Ok(CallToolResult::json(&json!({
                "available": true,
                "engine_found": true,
                "native_mcp_available": bridge.native_mcp_available,
                "bridge_available": bridge.bridge_available,
                "jar_path": jar_path,
                "java_available": true,
                "java_output": command_output(&java),
                "version_checked": version_checked,
                "version_output": version_output,
                "server_protocol_version": bridge.server_protocol_version,
                "native_mcp_tool_count": bridge.tool_count,
                "native_mcp_diagnostics": bridge.diagnostics,
                "bridge_error": bridge.error
            })))
        }
    }
}

async fn handle_route_specctra_dsn(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let dsn = get_path(args, "dsn_path")?;
    let ses_output = get_path(args, "ses_output_path")?;
    let jar = match find_freerouting_jar(args) {
        FreeroutingJar::Found(jar) => jar,
        FreeroutingJar::SuppliedPathIsNotAFile(path) => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("jar_path is not a file: {}", path.display()),
            ));
        }
        FreeroutingJar::NotFound(roots) => {
            return Ok(CallToolResult::error(format!(
                "Freerouting JAR not found in {} searched locations; install Freerouting or \
                 pass jar_path. check_freerouting lists every location searched.",
                roots.len()
            )));
        }
    };
    let max_passes = args["max_passes"]
        .as_u64()
        .map(u32::try_from)
        .transpose()
        .map_err(|_| anyhow::anyhow!("max_passes is too large"))?;
    let job_timeout_seconds = args["job_timeout_seconds"].as_u64();
    let overall_timeout_seconds = args["overall_timeout_seconds"].as_u64().unwrap_or(900);
    let settings = crate::freerouting_mcp::RouteSettings {
        max_passes,
        optimizer_enabled: args["optimizer_enabled"].as_bool(),
        job_timeout_seconds,
        poll_interval: std::time::Duration::from_secs(3),
        overall_timeout: std::time::Duration::from_secs(overall_timeout_seconds),
    };
    let evidence = crate::freerouting_mcp::route_local(&jar, &dsn, &ses_output, &settings)
        .await
        .map_err(|error| anyhow::anyhow!("Freerouting MCP routing failed: {error:#}"))?;
    Ok(CallToolResult::json(&json!({
        "success": true,
        "method": "local_freerouting_native_mcp",
        "engine": {
            "name": "Freerouting",
            "jar_path": jar,
            "execution": "local"
        },
        "native_mcp": {
            "used": true,
            "server_protocol_version": evidence.server_protocol_version
        },
        "bridge": {
            "mode": "dsn_ses_file_round_trip",
            "cloud_used": false
        },
        "artifacts": {
            "dsn_path": dsn,
            "ses_output_path": ses_output,
            "diagnostics_path": evidence.diagnostics_path
        },
        "session_id": evidence.session_id,
        "job_id": evidence.job_id,
        "final_state": evidence.final_state,
        "poll_count": evidence.poll_count,
        "elapsed_seconds": evidence.elapsed_seconds,
        "ses_bytes": evidence.ses_bytes
    })))
}

#[cfg(test)]
mod freerouting_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn finds_versioned_jar_inside_pcm_plugin_tree() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = temp.path().join("app_freerouting_kicad-plugin").join("lib");
        std::fs::create_dir_all(&plugin).unwrap();
        let jar = plugin.join("freerouting-2.3.0.jar");
        std::fs::write(&jar, b"fixture").unwrap();

        assert_eq!(find_freerouting_jar_below(temp.path(), 5), Some(jar));
    }

    #[tokio::test]
    async fn explicit_missing_jar_reports_each_readiness_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.jar");
        assert_eq!(
            find_freerouting_jar(&json!({ "jar_path": missing })),
            FreeroutingJar::SuppliedPathIsNotAFile(missing.clone())
        );

        let result = handle_check_freerouting(&json!({ "jar_path": missing }), &test_ctx())
            .await
            .unwrap();
        let body = response_json(&result);
        assert_eq!(body["available"], false);
        assert_eq!(body["engine_found"], false);
        assert_eq!(body["native_mcp_available"], false);
        assert_eq!(body["bridge_available"], false);
        // The caller's path is what was checked; discovery never ran, so
        // there is no search to report and no download advice to give.
        assert_eq!(body["checked_path"], json!(missing));
        assert_eq!(
            body["note"],
            json!(format!("jar_path is not a file: {}", missing.display()))
        );
        assert!(body.get("searched_locations").is_none(), "{body}");
    }

    /// A full environment, so every location the search can visit is present.
    fn every_location_environment(root: &Path) -> HashMap<&'static str, std::ffi::OsString> {
        let path = std::env::join_paths([root.join("bin-a"), root.join("bin-b")]).unwrap();
        HashMap::from([
            ("KICAD10_3RD_PARTY", root.join("k10").into_os_string()),
            ("KICAD9_3RD_PARTY", root.join("k9").into_os_string()),
            ("KICAD8_3RD_PARTY", root.join("k8").into_os_string()),
            ("HOME", root.join("home").into_os_string()),
            ("USERPROFILE", root.join("profile").into_os_string()),
            ("PATH", path),
        ])
    }

    /// #787: the `jar_path` description is the documentation of the search.
    /// The search visits locations in the order the description lists them,
    /// visits every location it lists, and the description promises no
    /// configuration setting.
    #[test]
    fn the_search_visits_the_documented_locations_in_the_documented_order() {
        let temp = tempfile::tempdir().unwrap();
        let env = every_location_environment(temp.path());
        let roots =
            freerouting_search_roots_from(|name| env.get(name).cloned(), &temp.path().join("cwd"));

        let visited: Vec<FreeroutingLocation> = roots.iter().map(|root| root.location).collect();
        assert!(
            visited.windows(2).all(|pair| pair[0] <= pair[1]),
            "search order differs from the documented order: {visited:?}"
        );
        let mut distinct = visited.clone();
        distinct.dedup();
        assert_eq!(distinct, FreeroutingLocation::ALL);

        let description = freerouting_jar_path_description();
        let positions: Vec<usize> = FreeroutingLocation::ALL
            .iter()
            .map(|location| {
                description
                    .find(location.description())
                    .unwrap_or_else(|| panic!("{location:?} is not documented: {description}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{description}"
        );
        assert!(!description.contains("config default"), "{description}");
        assert!(description.contains("There is no configuration setting"));

        // The working-directory root is absolute, so `searched` names the
        // file that was actually checked.
        assert!(roots.contains(&FreeroutingSearchRoot {
            location: FreeroutingLocation::WorkingDirectory,
            path: temp.path().join("cwd").join("freerouting.jar"),
            depth: 0,
        }));
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.location == FreeroutingLocation::PathDirectory)
                .map(|root| root.path.clone())
                .collect::<Vec<_>>(),
            [temp.path().join("bin-a"), temp.path().join("bin-b")]
        );
    }

    /// A directory reached twice is searched, and reported, once, under the
    /// location that reached it first.
    #[test]
    fn a_root_reached_twice_is_searched_once() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared").into_os_string();
        let path = std::env::join_paths([
            temp.path().join("bin"),
            temp.path().join("bin"),
            PathBuf::from(&shared),
        ])
        .unwrap();
        let env = HashMap::from([
            ("KICAD10_3RD_PARTY", shared.clone()),
            ("KICAD9_3RD_PARTY", shared.clone()),
            ("PATH", path),
        ]);
        let roots = freerouting_search_roots_from(|name| env.get(name).cloned(), temp.path());

        let mut paths: Vec<&PathBuf> = roots.iter().map(|root| &root.path).collect();
        let total = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), total, "{roots:?}");
        assert_eq!(
            roots
                .iter()
                .find(|root| root.path.as_os_str() == shared)
                .map(|root| root.location),
            Some(FreeroutingLocation::KicadThirdPartyVariable)
        );
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.location == FreeroutingLocation::PathDirectory)
                .count(),
            1
        );
    }

    /// Both tools that take `jar_path` describe it the same way, through the
    /// served `tools/list`.
    #[tokio::test]
    async fn served_jar_path_descriptions_state_the_search() {
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let response = handler
            .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .expect("tools/list receives a response");
        let tools = response.result.expect("result")["tools"].clone();
        for name in ["check_freerouting", "route_specctra_dsn"] {
            let tool = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} is not listed"));
            assert_eq!(
                tool["inputSchema"]["properties"]["jar_path"]["description"],
                json!(freerouting_jar_path_description()),
                "{name}"
            );
        }
    }

    /// `route_specctra_dsn` names the missing file instead of telling a caller
    /// who passed `jar_path` to pass `jar_path`.
    #[tokio::test]
    async fn served_route_refuses_a_jar_path_that_is_not_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.jar");
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "route_specctra_dsn",
                    "arguments": {
                        "dsn_path": temp.path().join("board.dsn").display().to_string(),
                        "ses_output_path": temp.path().join("board.ses").display().to_string(),
                        "jar_path": missing.display().to_string()
                    }
                }
            }))
            .await
            .expect("tools/call receives a response");
        let result = response.result.expect("result");
        assert_eq!(result["isError"], json!(true), "{result}");
        let body: serde_json::Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["error"]["kind"], "file_not_found", "{body}");
        assert_eq!(body["error"]["path"], json!(missing.display().to_string()));
        assert_eq!(
            body["message"],
            json!(format!("jar_path is not a file: {}", missing.display()))
        );
    }

    #[test]
    fn ignores_unrelated_jars() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("other.jar"), b"fixture").unwrap();
        assert_eq!(find_freerouting_jar_below(temp.path(), 5), None);
    }

    #[tokio::test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    async fn installed_engine_reports_each_observed_readiness_boundary() {
        let jar = PathBuf::from(std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR"));
        let result = handle_check_freerouting(&json!({ "jar_path": jar }), &test_ctx())
            .await
            .unwrap();
        let body = response_json(&result);
        assert_eq!(body["available"], true);
        assert_eq!(body["engine_found"], true);
        assert_eq!(body["native_mcp_available"], true);
        assert_eq!(body["bridge_available"], true);
        assert!(body["native_mcp_tool_count"].as_u64().unwrap() >= 6);
        assert!(body["server_protocol_version"].as_str().is_some());
    }
}

#[cfg(test)]
mod retry_backoff_tests {
    use super::*;

    /// End-to-end check against a real (hand-rolled) flaky HTTP server: two
    /// 503s followed by a 200 should be retried through to success, with
    /// real backoff delays elapsed in between — not just the status-code
    /// decision logic in isolation.
    #[tokio::test]
    async fn get_with_backoff_recovers_after_transient_failures() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for resp in [
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                socket.write_all(resp.as_bytes()).await.unwrap();
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/x", addr);

        let start = std::time::Instant::now();
        let resp = get_with_backoff(&client, &url).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        // Two retries at 300ms + 600ms = 900ms minimum before the 3rd (successful) attempt.
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "expected backoff delays to have elapsed, got {:?}",
            elapsed
        );
    }

    /// A persistent (non-transient) failure should return immediately after
    /// the first attempt — no wasted retries on a 404.
    #[tokio::test]
    async fn get_with_backoff_does_not_retry_client_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            // If get_with_backoff retried, it would try to accept() again here
            // and this task would hang until the test times out.
        });

        let client = reqwest::Client::new();
        let url = format!("http://{}/x", addr);

        let start = std::time::Instant::now();
        let resp = get_with_backoff(&client, &url).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "expected no retry delay for a 404, took {:?}",
            elapsed
        );
    }

    #[test]
    fn transient_on_rate_limit_and_server_errors() {
        assert!(is_transient_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_transient_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_transient_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_transient_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn not_transient_on_client_errors() {
        // Retrying a 404/401/403/400 wastes time — the request itself is
        // wrong, not the server having a bad moment.
        assert!(!is_transient_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_transient_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_transient_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_transient_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn not_transient_on_success() {
        assert!(!is_transient_status(reqwest::StatusCode::OK));
        assert!(!is_transient_status(reqwest::StatusCode::NO_CONTENT));
    }

    #[test]
    fn backoff_delay_doubles_each_attempt() {
        assert_eq!(backoff_delay(1), std::time::Duration::from_millis(300));
        assert_eq!(backoff_delay(2), std::time::Duration::from_millis(600));
        assert_eq!(backoff_delay(3), std::time::Duration::from_millis(1200));
    }

    #[test]
    fn backoff_delay_never_panics_on_zero_attempt() {
        // attempt is 1-based in normal use, but the saturating_sub guards
        // against an accidental 0 causing an underflow panic.
        assert_eq!(backoff_delay(0), std::time::Duration::from_millis(300));
    }
}

#[cfg(test)]
mod jlcpcb_database_tests {
    use super::*;
    use std::io::Write;

    fn seed_upstream_database(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("open upstream database");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE parts USING fts5 (
                 \"LCSC Part\",
                 \"First Category\",
                 \"Second Category\",
                 \"MFR.Part\",
                 \"Package\",
                 \"Manufacturer\",
                 \"Library Type\",
                 \"Description\",
                 \"Datasheet\",
                 \"Price\",
                 \"Stock\"
             );
             INSERT INTO parts VALUES (
                 'C14663', 'Resistors', 'Chip Resistor - Surface Mount',
                 'RC0402FR-0710KL', '0402', 'YAGEO', 'Basic',
                 '10k resistor 0402', 'https://example.com/datasheet.pdf',
                 '1-9:0.012,10-99:0.008', '5000'
             );",
        )
        .expect("seed upstream database");
    }

    #[test]
    fn parses_bounded_chunk_count() {
        assert_eq!(parse_jlcpcb_chunk_count("3\n").unwrap(), 3);
        assert!(parse_jlcpcb_chunk_count("0").is_err());
        assert!(parse_jlcpcb_chunk_count("65").is_err());
        assert!(parse_jlcpcb_chunk_count("not-a-number").is_err());
    }

    #[test]
    fn extracts_single_database_from_feed_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("parts.db.zip");
        let output_path = dir.path().join("upstream.db");
        let archive_file = std::fs::File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(archive_file);
        archive
            .start_file(
                "current-parts-fts5.db",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        archive.write_all(b"SQLite database bytes").unwrap();
        archive.finish().unwrap();

        extract_jlcpcb_database(&archive_path, &output_path).unwrap();

        assert_eq!(
            std::fs::read(output_path).unwrap(),
            b"SQLite database bytes"
        );
    }

    #[test]
    fn builds_stable_components_database_from_current_feed_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let upstream_path = dir.path().join("upstream.db");
        let output_path = dir.path().join("konnect.db");
        seed_upstream_database(&upstream_path);

        let part_count = build_konnect_jlcpcb_database(&upstream_path, &output_path).unwrap();
        assert_eq!(part_count, 1);

        let conn = rusqlite::Connection::open(output_path).expect("open generated database");
        let row: (String, f64, i64, String) = conn
            .query_row(
                "SELECT LCSC, Price, Stock, Category FROM components",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, "C14663");
        assert!((row.1 - 0.012).abs() < f64::EPSILON);
        assert_eq!(row.2, 5000);
        assert_eq!(row.3, "Resistors / Chip Resistor - Surface Mount");
    }

    #[test]
    fn rejects_an_upstream_schema_missing_required_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let upstream_path = dir.path().join("upstream.db");
        let output_path = dir.path().join("konnect.db");
        let conn = rusqlite::Connection::open(&upstream_path).expect("open upstream database");
        conn.execute("CREATE TABLE parts (\"LCSC Part\" TEXT)", [])
            .unwrap();
        drop(conn);

        let error = build_konnect_jlcpcb_database(&upstream_path, &output_path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing required columns"));
    }

    #[test]
    fn replaces_existing_database_and_removes_temporary_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("jlcpcb.db");
        let staged = dir.path().join("new.db");
        let backup = dir.path().join("previous.db");
        std::fs::write(&destination, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        replace_jlcpcb_database(&staged, &destination, &backup).unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), b"new");
        assert!(!backup.exists());
    }
}

#[cfg(test)]
mod jlcpcb_cache_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Builds a temp SQLite file with a `components` table matching the
    /// schema the handlers query, seeded with one part.
    fn seed_test_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("jlcpcb.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.execute(
            "CREATE TABLE components (
                LCSC TEXT, MFR_Part TEXT, Package TEXT, Manufacturer TEXT,
                Library_Type TEXT, Description TEXT, Datasheet TEXT,
                Price REAL, Stock INTEGER, Category TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO components VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                "C14663",
                "RC0402FR-0710KL",
                "0402",
                "YAGEO",
                "Basic",
                "10k resistor 0402",
                "https://www.lcsc.com/datasheet/C14663.pdf",
                0.01,
                5000,
                "Resistors / Chip Resistor - Surface Mount"
            ],
        )
        .unwrap();
        (dir, db_path)
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_caches_repeated_query() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "query": "10k",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_search_jlcpcb_parts(&args, &ctx).await.unwrap();
        let second = handle_search_jlcpcb_parts(&args, &ctx).await.unwrap();

        let first_body = response_json(&first);
        let second_body = response_json(&second);
        assert_eq!(first_body["cached"], json!(false));
        assert_eq!(second_body["cached"], json!(true));
        assert_eq!(first_body["results"], second_body["results"]);
        assert_eq!(first_body["count"], json!(1));
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_different_query_is_a_cache_miss() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();

        let args_a = json!({ "query": "10k", "output_path": db_path.to_str().unwrap() });
        let args_b = json!({ "query": "100nF", "output_path": db_path.to_str().unwrap() });

        handle_search_jlcpcb_parts(&args_a, &ctx).await.unwrap();
        let second = handle_search_jlcpcb_parts(&args_b, &ctx).await.unwrap();

        assert_eq!(response_json(&second)["cached"], json!(false));
    }

    #[tokio::test]
    async fn get_jlcpcb_part_caches_repeated_lookup() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "lcsc_id": "C14663",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_get_jlcpcb_part(&args, &ctx).await.unwrap();
        let second = handle_get_jlcpcb_part(&args, &ctx).await.unwrap();

        assert_eq!(response_json(&first)["cached"], json!(false));
        assert_eq!(response_json(&second)["cached"], json!(true));
        assert_eq!(response_json(&first)["lcsc"], json!("C14663"));
    }

    #[tokio::test]
    async fn suggest_alternatives_caches_repeated_query() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();
        let args = json!({
            "value": "10k",
            "footprint": "Resistor_SMD:R_0402",
            "output_path": db_path.to_str().unwrap()
        });

        let first = handle_suggest_alternatives(&args, &ctx).await.unwrap();
        let second = handle_suggest_alternatives(&args, &ctx).await.unwrap();

        assert_eq!(response_json(&first)["cached"], json!(false));
        assert_eq!(response_json(&second)["cached"], json!(true));
    }

    /// Seeds parts whose search words are spread over different columns.
    fn seed_multi_word_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, db_path) = seed_test_db();
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        for (lcsc, mpn, package, mfr, desc) in [
            (
                "C1001",
                "PH-1x7",
                "2.54mm",
                "BOOMELE",
                "Pin Header 1x7 Through Hole Male",
            ),
            (
                "C1002",
                "PH-1x40",
                "2.54mm",
                "BOOMELE",
                "40P Pin Header Through Hole",
            ),
            (
                "C1003",
                "CAP-50",
                "0603",
                "Samsung",
                "50% duty_cycle capacitor",
            ),
            (
                "C1004",
                "CAP-51",
                "0603",
                "Samsung",
                "50 duty cycle capacitor",
            ),
        ] {
            conn.execute(
                "INSERT INTO components VALUES (?1, ?2, ?3, ?4, 'Extended', ?5, '', 0.1, 10, 'Misc')",
                rusqlite::params![lcsc, mpn, package, mfr, desc],
            )
            .unwrap();
        }
        (dir, db_path)
    }

    async fn search_lcsc_ids(db_path: &std::path::Path, query: &str) -> Vec<String> {
        let ctx = test_ctx();
        let args = json!({ "query": query, "output_path": db_path.to_str().unwrap() });
        let result = handle_search_jlcpcb_parts(&args, &ctx).await.unwrap();
        let mut ids: Vec<String> = response_json(&result)["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["lcsc"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    /// The defect in #432: the whole query was bound as one `LIKE '%query%'`
    /// phrase against Description/MFR_Part, so reordering, trimming or adding
    /// a word that is not in that exact order flipped a hit into zero results.
    #[tokio::test]
    async fn search_jlcpcb_parts_matches_every_token_in_any_order() {
        let (_dir, db_path) = seed_multi_word_db();
        assert_eq!(
            search_lcsc_ids(&db_path, "Pin Header 1x7").await,
            vec!["C1001"]
        );
        assert_eq!(
            search_lcsc_ids(&db_path, "  1x7   header pin ").await,
            vec!["C1001"]
        );
        assert_eq!(
            search_lcsc_ids(&db_path, "through hole pin header").await,
            vec!["C1001", "C1002"]
        );
        assert_eq!(
            search_lcsc_ids(&db_path, "40P Pin Header Through Hole").await,
            vec!["C1002"]
        );
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_tokens_may_span_columns() {
        let (_dir, db_path) = seed_multi_word_db();
        // Package, Manufacturer, LCSC and MFR_Part are all searchable.
        assert_eq!(
            search_lcsc_ids(&db_path, "boomele 2.54mm 1x7").await,
            vec!["C1001"]
        );
        assert_eq!(
            search_lcsc_ids(&db_path, "C1002 header").await,
            vec!["C1002"]
        );
        assert_eq!(
            search_lcsc_ids(&db_path, "PH-1x40 boomele").await,
            vec!["C1002"]
        );
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_absent_token_returns_no_row() {
        let (_dir, db_path) = seed_multi_word_db();
        assert!(search_lcsc_ids(&db_path, "pin header 1x7 nonexistentword")
            .await
            .is_empty());
    }

    /// The filters and LIMIT keep working alongside the tokenised match.
    /// (The query has no ORDER BY, before or after the fix.)
    #[tokio::test]
    async fn search_jlcpcb_parts_preserves_stock_basic_category_and_limit_filters() {
        let (_dir, db_path) = seed_multi_word_db();
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        for (lcsc, lib, desc, stock, cat) in [
            ("C2001", "Basic", "Widget gadget alpha", 10, "Gadgets"),
            ("C2002", "Extended", "Widget gadget beta", 10, "Gadgets"),
            ("C2003", "Basic", "Widget gadget gamma", 0, "Gadgets"),
            ("C2004", "Basic", "Widget gadget delta", 10, "Other"),
        ] {
            conn.execute(
                "INSERT INTO components VALUES (?1, 'MPN', 'PKG', 'MFR', ?2, ?3, '', 0.1, ?4, ?5)",
                rusqlite::params![lcsc, lib, desc, stock, cat],
            )
            .unwrap();
        }
        let ctx = test_ctx();
        let run = |extra: serde_json::Value| {
            let mut args =
                json!({ "query": "gadget widget", "output_path": db_path.to_str().unwrap() });
            for (k, v) in extra.as_object().unwrap() {
                args[k] = v.clone();
            }
            let ctx = &ctx;
            async move {
                let result = handle_search_jlcpcb_parts(&args, ctx).await.unwrap();
                let mut ids: Vec<String> = response_json(&result)["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["lcsc"].as_str().unwrap().to_string())
                    .collect();
                ids.sort();
                ids
            }
        };
        // in_stock defaults to true: C2003 (stock 0) is excluded.
        assert_eq!(run(json!({})).await, vec!["C2001", "C2002", "C2004"]);
        assert_eq!(
            run(json!({ "in_stock": false })).await,
            vec!["C2001", "C2002", "C2003", "C2004"]
        );
        assert_eq!(
            run(json!({ "basic_only": true })).await,
            vec!["C2001", "C2004"]
        );
        assert_eq!(
            run(json!({ "category": "Gadgets" })).await,
            vec!["C2001", "C2002"]
        );
        assert_eq!(
            run(json!({ "category": "Gadgets", "basic_only": true, "in_stock": false })).await,
            vec!["C2001", "C2003"]
        );
        assert_eq!(run(json!({ "limit": 2 })).await.len(), 2);
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_like_metacharacters_are_literal() {
        let (_dir, db_path) = seed_multi_word_db();
        assert_eq!(search_lcsc_ids(&db_path, "50%").await, vec!["C1003"]);
        assert_eq!(search_lcsc_ids(&db_path, "duty_cycle").await, vec!["C1003"]);
        assert!(search_lcsc_ids(&db_path, "%").await.len() == 1);
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    /// The defect in #255: the local catalog holds exact datasheet URLs, but
    /// get_datasheet_url only ever asked the live LCSC API and returned null.
    /// A catalog hit must answer directly — no network involved.
    #[tokio::test]
    async fn get_datasheet_url_answers_from_the_local_catalog() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();

        let by_lcsc = handle_get_datasheet_url(
            &json!({ "lcsc_id": "C14663", "output_path": db_path.to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        let body = response_json(&by_lcsc);
        assert_eq!(
            body["datasheet_url"],
            json!("https://www.lcsc.com/datasheet/C14663.pdf")
        );
        assert_eq!(body["source"], json!("local_catalog"));

        let by_mpn = handle_get_datasheet_url(
            &json!({ "mpn": "RC0402FR-0710KL", "output_path": db_path.to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            response_json(&by_mpn)["datasheet_url"],
            json!("https://www.lcsc.com/datasheet/C14663.pdf")
        );
    }

    /// The catalog projections hid the column too — a caller could not get a
    /// URL from part lookup either.
    #[tokio::test]
    async fn catalog_lookups_carry_the_datasheet_url() {
        let (_dir, db_path) = seed_test_db();
        let ctx = test_ctx();

        let part = handle_get_jlcpcb_part(
            &json!({ "lcsc_id": "C14663", "output_path": db_path.to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            response_json(&part)["datasheet_url"],
            json!("https://www.lcsc.com/datasheet/C14663.pdf")
        );

        let search = handle_search_jlcpcb_parts(
            &json!({ "query": "10k", "output_path": db_path.to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            response_json(&search)["results"][0]["datasheet_url"],
            json!("https://www.lcsc.com/datasheet/C14663.pdf")
        );
    }

    #[tokio::test]
    async fn search_jlcpcb_parts_limit_page_is_ordered_by_lcsc() {
        let (_dir, db_path) = seed_multi_word_db();
        let ctx = test_ctx();
        // Insert out of LCSC order so rowid order differs from LCSC order.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for lcsc in ["C3003", "C3001", "C3002"] {
            conn.execute(
                "INSERT INTO components VALUES (?1, 'ORD-1', '0805', 'Acme', 'Basic', 'ordercheck part', '', 0.1, 10, 'Misc')",
                rusqlite::params![lcsc],
            )
            .unwrap();
        }
        let args = json!({
            "query": "ordercheck",
            "limit": 2,
            "output_path": db_path.to_str().unwrap()
        });
        let body = response_json(&handle_search_jlcpcb_parts(&args, &ctx).await.unwrap());
        let ids: Vec<&str> = body["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["lcsc"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["C3001", "C3002"]);
    }

    #[tokio::test]
    async fn served_tools_call_reports_tokens_and_fields_on_miss_and_cache_hit() {
        let (_dir, db_path) = seed_multi_word_db();
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: Some(db_path.clone()),
            auto_load_toolsets: true,
            eager_toolsets: true,
        })
        .await
        .expect("handler builds");
        let call = || async {
            let response = handler
                .handle_message(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "search_jlcpcb_parts",
                        "arguments": {
                            "query": "  header   Pin 1x7 ",
                            "limit": 5
                        }
                    }
                }))
                .await
                .expect("response");
            let result = response.result.expect("result");
            serde_json::from_str::<serde_json::Value>(
                result["content"][0]["text"].as_str().expect("text"),
            )
            .expect("json body")
        };
        let fresh = call().await;
        let hit = call().await;
        assert_eq!(fresh["cached"], json!(false));
        assert_eq!(hit["cached"], json!(true));
        for body in [&fresh, &hit] {
            assert_eq!(body["tokens"], json!(["header", "Pin", "1x7"]));
            assert_eq!(
                body["search_fields"],
                json!(["Description", "MFR_Part", "LCSC", "Package", "Manufacturer"])
            );
            assert_eq!(body["query"], json!("  header   Pin 1x7 "));
        }
        assert_eq!(fresh["results"], hit["results"]);
        assert!(fresh["count"].as_u64().unwrap() >= 1);
    }

    /// With no database at all the tool degrades to its old behaviour — web
    // lookup (unreachable in tests), then null with a note naming what was
    // tried rather than blaming only the API (#255).
    #[tokio::test]
    async fn a_catalog_miss_falls_through_and_the_note_names_the_sources() {
        let ctx = test_ctx(); // jlcpcb_db_path: None → default path absent

        let result = handle_get_datasheet_url(&json!({ "lcsc_id": "C99999999" }), &ctx)
            .await
            .unwrap();
        let body = response_json(&result);
        assert_eq!(body["datasheet_url"], serde_json::Value::Null);
        let note = body["note"].as_str().unwrap();
        assert!(note.contains("local catalog"), "{note}");
    }
}
