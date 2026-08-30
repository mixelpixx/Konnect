//! Read-only runtime and installation provenance for the serving process.

use crate::tools::ServerConfig;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const PCM_IDENTIFIER: &str = "com.github.mixelpixx.konnect";

pub(crate) async fn collect(config: &ServerConfig) -> Value {
    let running_version = env!("CARGO_PKG_VERSION");
    let executable_path = std::env::current_exe().ok();
    let installation = executable_path
        .as_deref()
        .map(classify_installation)
        .unwrap_or_else(InstallSource::unavailable);

    let binary_probe = match executable_path.as_deref() {
        Some(path) => probe_command_version(path, VersionCommand::Konnect).await,
        None => VersionProbe::unavailable(),
    };
    let newer_than_running = binary_probe
        .version
        .as_deref()
        .and_then(|version| stable_version_cmp(version, running_version))
        .map(|ordering| ordering == Ordering::Greater);

    let kicad_cli_path = crate::kicad_install::find_cli(&config.kicad_cli);
    let kicad_probe = match kicad_cli_path.as_deref() {
        Some(path) => probe_command_version(path, VersionCommand::KiCad).await,
        None => VersionProbe::not_found(),
    };

    let ipc_endpoint = if config.ipc_address.trim().is_empty() {
        None
    } else {
        Some(redact_endpoint(config.ipc_address.trim()))
    };

    json!({
        "build": {
            "version": running_version,
            "commit": option_env!("KONNECT_BUILD_COMMIT"),
            "commit_source": option_env!("KONNECT_BUILD_COMMIT_SOURCE"),
            "working_tree_state": "not_recorded",
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "target_os": std::env::consts::OS,
            "target_arch": std::env::consts::ARCH,
        },
        "runtime": {
            "executable_path": executable_path.as_deref().map(display_path),
        },
        "installation": {
            "source": installation.name,
            "evidence": installation.evidence,
            "manifest_path": installation.manifest_path.as_deref().map(display_path),
            "binary_on_disk": {
                "probe_status": binary_probe.status,
                "version": binary_probe.version,
                "newer_than_running": newer_than_running,
            },
        },
        "kicad": {
            "cli_path": kicad_cli_path.as_deref().map(display_path),
            "probe_status": kicad_probe.status,
            "version": kicad_probe.version,
        },
        "ipc": {
            "configured": ipc_endpoint.is_some(),
            "source": "resolved_server_config",
            "endpoint": ipc_endpoint,
        },
        "restart_guidance": restart_guidance(installation.name, newer_than_running),
    })
}

#[derive(Debug)]
struct InstallSource {
    name: &'static str,
    evidence: &'static str,
    manifest_path: Option<PathBuf>,
}

impl InstallSource {
    fn unavailable() -> Self {
        Self {
            name: "unknown",
            evidence:
                "The serving executable path could not be resolved; no install source was inferred.",
            manifest_path: None,
        }
    }
}

fn classify_installation(executable_path: &Path) -> InstallSource {
    let manifest_path = executable_path
        .parent()
        .and_then(Path::parent)
        .map(|plugin_dir| plugin_dir.join("plugin.json"));

    if let Some(path) = manifest_path.filter(|path| is_konnect_pcm_manifest(path)) {
        return InstallSource {
            name: "kicad_pcm",
            evidence:
                "A sibling KiCad executable-plugin manifest has Konnect's exact public identifier.",
            manifest_path: Some(path),
        };
    }

    InstallSource {
        name: "unknown",
        evidence: "No verified KiCad PCM manifest was found beside this executable; standalone and source builds are intentionally not guessed from path names.",
        manifest_path: None,
    }
}

fn is_konnect_pcm_manifest(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    manifest.get("identifier").and_then(Value::as_str) == Some(PCM_IDENTIFIER)
        && manifest
            .get("runtime")
            .and_then(|runtime| runtime.get("type"))
            .and_then(Value::as_str)
            == Some("exec")
}

#[derive(Clone, Copy)]
enum VersionCommand {
    Konnect,
    KiCad,
}

struct VersionProbe {
    status: &'static str,
    version: Option<String>,
}

impl VersionProbe {
    fn unavailable() -> Self {
        Self {
            status: "executable_path_unavailable",
            version: None,
        }
    }

    fn not_found() -> Self {
        Self {
            status: "not_found",
            version: None,
        }
    }
}

async fn probe_command_version(path: &Path, command_kind: VersionCommand) -> VersionProbe {
    let mut command = Command::new(path);
    command.arg("--version").kill_on_drop(true);
    let output = match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return VersionProbe {
                status: "launch_failed",
                version: None,
            };
        }
        Err(_) => {
            return VersionProbe {
                status: "timed_out",
                version: None,
            };
        }
    };

    if !output.status.success() {
        return VersionProbe {
            status: "nonzero_exit",
            version: None,
        };
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty());

    let version = match command_kind {
        VersionCommand::Konnect => line.and_then(parse_konnect_version).map(str::to_string),
        VersionCommand::KiCad => line.and_then(sanitize_version_line),
    };
    VersionProbe {
        status: if version.is_some() {
            "ok"
        } else {
            "unrecognized_output"
        },
        version,
    }
}

fn parse_konnect_version(line: &str) -> Option<&str> {
    let version = line.strip_prefix("konnect ")?.trim();
    (!version.is_empty() && !version.chars().any(char::is_whitespace)).then_some(version)
}

fn sanitize_version_line(line: &str) -> Option<String> {
    let value: String = line
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect();
    (!value.is_empty()).then_some(value)
}

fn stable_version_cmp(candidate: &str, running: &str) -> Option<Ordering> {
    fn stable_triplet(version: &str) -> Option<[u64; 3]> {
        if version.contains(['-', '+']) {
            return None;
        }
        let values = version
            .strip_prefix('v')
            .unwrap_or(version)
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        (values.len() == 3).then(|| [values[0], values[1], values[2]])
    }

    Some(stable_triplet(candidate)?.cmp(&stable_triplet(running)?))
}

fn redact_endpoint(endpoint: &str) -> String {
    let (without_fragment, had_fragment) = endpoint
        .split_once('#')
        .map_or((endpoint, false), |(head, _)| (head, true));
    let (without_query, had_query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, false), |(head, _)| (head, true));

    let without_credentials = if let Some((scheme, rest)) = without_query.split_once("://") {
        if let Some((_, authority_and_path)) = rest.split_once('@') {
            format!("{scheme}://[redacted]@{authority_and_path}")
        } else {
            without_query.to_string()
        }
    } else {
        without_query.to_string()
    };

    if had_query || had_fragment {
        format!("{without_credentials} [query/fragment redacted]")
    } else {
        without_credentials
    }
}

fn restart_guidance(source: &str, newer_than_running: Option<bool>) -> Vec<String> {
    let mut guidance = Vec::new();
    if newer_than_running == Some(true) {
        guidance.push(
            "A newer binary is proven at the serving executable path; restart the process before relying on the new build."
                .to_string(),
        );
    }

    #[cfg(target_os = "windows")]
    guidance.push(
        "Windows: exit every MCP client or KiCad session that launched Konnect, then reopen the owning application; running executables may remain locked during an update."
            .to_string(),
    );
    #[cfg(target_os = "macos")]
    guidance.push(
        "macOS: restart the MCP client that launched Konnect; if KiCad launched it, quit and reopen KiCad after the update."
            .to_string(),
    );
    #[cfg(target_os = "linux")]
    guidance.push(
        "Linux: restart the MCP client that launched Konnect; if KiCad launched it, stop the plugin server or quit and reopen KiCad after the update."
            .to_string(),
    );
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    guidance.push(
        "Restart the MCP client or KiCad session that launched Konnect after replacing the binary."
            .to_string(),
    );

    if source == "kicad_pcm" {
        guidance.push(
            "KiCad PCM install detected: complete the Plugin and Content Manager update, then restart KiCad and any separately configured MCP client."
                .to_string(),
        );
    }
    guidance.push(
        "Call get_installation_info again after restart and verify the serving version, commit, and executable path."
            .to_string(),
    );
    guidance
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_pcm_manifest_is_required_for_pcm_classification() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_dir = temp.path().join("plugins");
        let bin_dir = plugin_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let executable = bin_dir.join(if cfg!(windows) {
            "konnect.exe"
        } else {
            "konnect"
        });
        std::fs::write(&executable, b"").unwrap();

        assert_eq!(classify_installation(&executable).name, "unknown");
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"identifier":"someone.else","runtime":{"type":"exec"}}"#,
        )
        .unwrap();
        assert_eq!(classify_installation(&executable).name, "unknown");
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"identifier":"com.github.mixelpixx.konnect","runtime":{"type":"exec"}}"#,
        )
        .unwrap();

        let source = classify_installation(&executable);
        assert_eq!(source.name, "kicad_pcm");
        assert_eq!(source.manifest_path, Some(plugin_dir.join("plugin.json")));
    }

    #[test]
    fn endpoint_redaction_removes_credentials_query_and_fragment() {
        assert_eq!(
            redact_endpoint("tcp://user:secret@127.0.0.1:9000/api?token=hidden#detail"),
            "tcp://[redacted]@127.0.0.1:9000/api [query/fragment redacted]"
        );
        assert_eq!(
            redact_endpoint("ipc:///tmp/kicad/api.sock"),
            "ipc:///tmp/kicad/api.sock"
        );
    }

    #[test]
    fn newer_claim_requires_comparable_stable_versions() {
        assert_eq!(
            stable_version_cmp("0.12.0", "0.11.9"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            stable_version_cmp("0.11.0", "0.11.0"),
            Some(Ordering::Equal)
        );
        assert_eq!(stable_version_cmp("0.11.0-beta.1", "0.10.0"), None);
        assert_eq!(stable_version_cmp("not-a-version", "0.11.0"), None);
    }

    #[test]
    fn embedded_commit_is_hex_when_available() {
        if let Some(commit) = option_env!("KONNECT_BUILD_COMMIT") {
            assert!((7..=64).contains(&commit.len()));
            assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn restart_guidance_names_the_current_platform() {
        let guidance = restart_guidance("unknown", None).join("\n");
        #[cfg(target_os = "windows")]
        assert!(guidance.contains("Windows:"));
        #[cfg(target_os = "macos")]
        assert!(guidance.contains("macOS:"));
        #[cfg(target_os = "linux")]
        assert!(guidance.contains("Linux:"));
    }
}
