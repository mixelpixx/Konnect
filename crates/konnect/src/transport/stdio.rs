use anyhow::Result;
use konnect_core::mcp::{handler::McpHandler, protocol::*};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Run the MCP server over STDIO (stdin/stdout).
/// All logging must go to stderr — stdout is reserved for the MCP protocol.
pub async fn run_stdio(handler: McpHandler) -> Result<()> {
    info!("Starting STDIO transport");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut stdout = stdout;
    let mut line = String::new();

    // Server-initiated notifications (e.g. tools/list_changed after
    // load_toolset / unload_toolset) are queued here by the handler and
    // flushed to stdout after the triggering message is handled. Without this
    // sink, the handler's notification reached only the HTTP/SSE transport and
    // was silently dropped on stdio, so stdio clients (Claude Code) never
    // re-fetched tools/list and tools added mid-session stayed uncallable
    // (issue #19). Notifications are only ever generated synchronously while
    // handling a request, so draining right after each message catches them —
    // no separate task or cancellation-prone select! over read_line is needed.
    let (notif_tx, mut notif_rx) = mpsc::channel::<String>(16);
    handler.register_notification_sink(notif_tx).await;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF — client disconnected
            info!("STDIO: EOF received, shutting down");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        debug!("STDIO recv: {}", trimmed);

        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(msg) => handler.handle_message(msg).await,
            Err(e) => {
                error!("Failed to parse JSON: {}", e);
                Some(JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError {
                        code: PARSE_ERROR,
                        message: format!("Parse error: {}", e),
                        data: None,
                    },
                ))
            }
        };

        if let Some(resp) = response {
            let mut json = serde_json::to_string(&resp)?;
            json.push('\n');
            debug!("STDIO send: {}", json.trim());
            stdout.write_all(json.as_bytes()).await?;
            stdout.flush().await?;
        }

        // Flush any notifications the handler queued while processing this
        // message, after the response so the client sees the response first.
        while let Ok(notif) = notif_rx.try_recv() {
            let mut json = notif;
            json.push('\n');
            debug!("STDIO send notification: {}", json.trim());
            stdout.write_all(json.as_bytes()).await?;
            stdout.flush().await?;
        }

        // The handler only queues a reload; the stdio transport owns the
        // irreversible step. Reaching this point proves the response and all
        // synchronous notifications were flushed, and we exec before reading
        // another request. That closes the old timer window where a second
        // mutation could be interrupted before its guards were dropped.
        #[cfg(unix)]
        if let Some(plan) = handler.take_reload_request() {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(&plan.executable)
                .args(&plan.arguments)
                .exec();
            return Err(anyhow::anyhow!(
                "reload_server failed to exec {}: {error}",
                plan.executable.display()
            ));
        }
    }

    Ok(())
}
