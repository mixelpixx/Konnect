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

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::{read_consistent, write_atomic_if_unchanged};
use serde_json::json;
use std::collections::HashSet;
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
            "Search the local JLCPCB component database by keyword, value, or category.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (MPN, description, or value)" },
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
                    "jar_path": { "type": "string", "description": "Path to freerouting.jar (optional, uses config default)" }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_freerouting(args, ctx).await }
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

    let results = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = rusqlite::Connection::open(&db_path)?;

        // The JLCPCB db schema has columns: LCSC, MFR_Part, Package, Solder_Joint,
        // Manufacturer, Library_Type, Description, Datasheet, Price, Stock
        let mut sql = String::from(
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
             FROM components WHERE (Description LIKE ?1 OR MFR_Part LIKE ?1)"
        );
        if basic_only {
            sql.push_str(" AND Library_Type = 'Basic'");
        }
        if in_stock {
            sql.push_str(" AND Stock > 0");
        }
        if let Some(ref _cat) = category {
            sql.push_str(" AND Category LIKE ?2");
        }
        sql.push_str(&format!(" LIMIT {}", limit));

        let like_query = format!("%{}%", query);
        let mut stmt = conn.prepare(&sql)?;

        let rows: Vec<serde_json::Value> = if category.is_some() {
            let cat_like = format!("%{}%", category.as_deref().unwrap_or(""));
            stmt.query_map(rusqlite::params![like_query, cat_like], row_to_part_json)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map(rusqlite::params![like_query], row_to_part_json)?
                .filter_map(|r| r.ok())
                .collect()
        };
        Ok(rows)
    })
    .await??;

    let body = json!({
        "query": args["query"].as_str().unwrap_or(""),
        "count": results.len(),
        "results": results
    });
    ctx.jlcpcb_cache.put(key, body.clone());

    let mut body = body;
    body["cached"] = json!(false);
    Ok(CallToolResult::text(serde_json::to_string(&body).unwrap()))
}

fn row_to_part_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    Ok(json!({
        "lcsc": row.get::<_, String>(0).unwrap_or_default(),
        "mpn": row.get::<_, String>(1).unwrap_or_default(),
        "package": row.get::<_, String>(2).unwrap_or_default(),
        "manufacturer": row.get::<_, String>(3).unwrap_or_default(),
        "library_type": row.get::<_, String>(4).unwrap_or_default(),
        "description": row.get::<_, String>(5).unwrap_or_default(),
        "price": row.get::<_, f64>(6).unwrap_or(0.0),
        "stock": row.get::<_, i64>(7).unwrap_or(0)
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
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
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
            "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Price, Stock \
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

    // Find all LCSC property values in the schematic
    let mut lcsc_ids: Vec<String> = Vec::new();
    let mut search = content.as_str();
    while let Some(pos) = search.find("(property \"LCSC\" \"") {
        let after = &search[pos + 18..];
        if let Some(end) = after.find('"') {
            lcsc_ids.push(after[..end].to_string());
        }
        search = &search[pos + 1..];
    }
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

    let mut enriched = 0usize;
    let mut new_content = content.clone();

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
                        // Find components with this LCSC ID and update their Datasheet property.
                        // Pattern: find (property "LCSC" "CxxxID") → walk back to symbol block →
                        // find (property "Datasheet" "...") and replace the URL.
                        let lcsc_pat = format!(r#"(property "LCSC" "{}")"#, lcsc_id);
                        let mut search_from = 0usize;
                        while let Some(lcsc_pos) = new_content[search_from..]
                            .find(&lcsc_pat)
                            .map(|i| i + search_from)
                        {
                            // Find the enclosing symbol block
                            let before = &new_content[..lcsc_pos];
                            if let Some(sym_start) = before.rfind("\n  (symbol") {
                                let sym_block = &new_content[sym_start..];
                                // Find Datasheet property within this symbol
                                let ds_pat = r#"(property "Datasheet" ""#;
                                if let Some(ds_offset) = sym_block.find(ds_pat) {
                                    let ds_abs = sym_start + ds_offset + ds_pat.len();
                                    if let Some(ds_end) = new_content[ds_abs..].find('"') {
                                        let existing = &new_content[ds_abs..ds_abs + ds_end];
                                        if overwrite || existing == "~" || existing.is_empty() {
                                            new_content = format!(
                                                "{}{}{}",
                                                &new_content[..ds_abs],
                                                datasheet_url,
                                                &new_content[ds_abs + ds_end..]
                                            );
                                            enriched += 1;
                                        }
                                    }
                                }
                            }
                            search_from = lcsc_pos + 1;
                        }
                    }
                }
            }
        }
    }

    // Write back if anything changed
    if enriched > 0 {
        write_atomic_if_unchanged(&sch_path, &content, &new_content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "lcsc_ids_found": lcsc_ids.len(),
            "datasheets_enriched": enriched,
            "schematic": sch_path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_get_datasheet_url(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let mpn = args["mpn"].as_str();
    let lcsc_id = args["lcsc_id"].as_str();

    if mpn.is_none() && lcsc_id.is_none() {
        return Ok(CallToolResult::error("Provide either 'mpn' or 'lcsc_id'"));
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
                                "datasheet_url": ds_url
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "mpn": mpn,
            "lcsc_id": lcsc_id,
            "datasheet_url": null,
            "note": "Datasheet not found via LCSC API"
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

fn freerouting_search_roots() -> Vec<(PathBuf, usize)> {
    let mut roots = Vec::new();

    for variable in ["KICAD10_3RD_PARTY", "KICAD9_3RD_PARTY", "KICAD8_3RD_PARTY"] {
        if let Some(path) = std::env::var_os(variable) {
            roots.push((PathBuf::from(path), 5));
        }
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for version in ["10.0", "9.0", "8.0"] {
            roots.push((
                home.join("Documents")
                    .join("KiCad")
                    .join(version)
                    .join("3rdparty")
                    .join("plugins"),
                5,
            ));
            roots.push((
                home.join(".local")
                    .join("share")
                    .join("kicad")
                    .join(version)
                    .join("3rdparty")
                    .join("plugins"),
                5,
            ));
        }
    }

    #[cfg(target_os = "windows")]
    if let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        for version in ["10.0", "9.0", "8.0"] {
            roots.push((
                profile
                    .join("Documents")
                    .join("KiCad")
                    .join(version)
                    .join("3rdparty")
                    .join("plugins"),
                5,
            ));
        }
    }

    roots.extend([
        (PathBuf::from("freerouting.jar"), 0),
        (PathBuf::from("/usr/local/lib/freerouting"), 3),
        (PathBuf::from("/opt/freerouting"), 3),
    ]);
    if let Some(path) = std::env::var_os("PATH") {
        roots.extend(std::env::split_paths(&path).map(|directory| (directory, 1)));
    }
    roots
}

fn find_freerouting_jar(args: &serde_json::Value) -> Option<PathBuf> {
    if let Some(path) = args["jar_path"].as_str() {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }

    freerouting_search_roots()
        .into_iter()
        .find_map(|(root, depth)| find_freerouting_jar_below(&root, depth))
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

async fn run_java_command(
    command: &mut tokio::process::Command,
) -> Result<std::process::Output, String> {
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
    let jar = find_freerouting_jar(args);

    match jar {
        None => Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "available": false,
                "note": "freerouting.jar not found. Download from https://github.com/freerouting/freerouting/releases"
            }))
            .unwrap(),
        )),
        Some(jar_path) => {
            let mut java_command = tokio::process::Command::new("java");
            java_command.arg("-version");
            let java = match run_java_command(&mut java_command).await {
                Ok(output) => output,
                Err(error) => {
                    return Ok(CallToolResult::json(&json!({
                        "available": false,
                        "jar_path": jar_path,
                        "java_available": false,
                        "note": error
                    })));
                }
            };
            if !java.status.success() {
                return Ok(CallToolResult::json(&json!({
                    "available": false,
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

            Ok(CallToolResult::json(&json!({
                "available": true,
                "jar_path": jar_path,
                "java_available": true,
                "java_output": command_output(&java),
                "version_checked": version_checked,
                "version_output": version_output
            })))
        }
    }
}

#[cfg(test)]
mod freerouting_tests {
    use super::*;

    #[test]
    fn finds_versioned_jar_inside_pcm_plugin_tree() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = temp.path().join("app_freerouting_kicad-plugin").join("lib");
        std::fs::create_dir_all(&plugin).unwrap();
        let jar = plugin.join("freerouting-2.3.0.jar");
        std::fs::write(&jar, b"fixture").unwrap();

        assert_eq!(find_freerouting_jar_below(temp.path(), 5), Some(jar));
    }

    #[test]
    fn explicit_missing_jar_does_not_claim_availability() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.jar");
        assert_eq!(find_freerouting_jar(&json!({ "jar_path": missing })), None);
    }

    #[test]
    fn ignores_unrelated_jars() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("other.jar"), b"fixture").unwrap();
        assert_eq!(find_freerouting_jar_below(temp.path(), 5), None);
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
                Library_Type TEXT, Description TEXT, Price REAL, Stock INTEGER,
                Category TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO components VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                "C14663",
                "RC0402FR-0710KL",
                "0402",
                "YAGEO",
                "Basic",
                "10k resistor 0402",
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

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }
}
