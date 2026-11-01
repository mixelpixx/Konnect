pub mod meta_tools;
pub mod registry;

use crate::tools::ToolDef;
use std::collections::{HashMap, HashSet};
use tokio::sync::RwLock;

/// Tracks which toolsets are currently loaded in the session.
pub struct ToolRouter {
    /// All registered toolset definitions
    registry: &'static [ToolsetMeta],
    /// Names of currently active toolsets
    active: RwLock<HashSet<String>>,
    /// Flat map of tool_name → ToolDef for fast dispatch
    loaded_tools: RwLock<HashMap<String, ToolDef>>,
}

/// Static metadata for a toolset (not the tools themselves).
#[derive(Debug, Clone)]
pub struct ToolsetMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub tool_count: usize,
}

impl ToolRouter {
    pub fn new() -> Self {
        ToolRouter {
            registry: registry::ALL_TOOLSETS,
            active: RwLock::new(HashSet::new()),
            loaded_tools: RwLock::new(HashMap::new()),
        }
    }

    pub fn all_toolsets(&self) -> &'static [ToolsetMeta] {
        self.registry
    }

    pub async fn load(&self, name: &str) -> Option<Vec<ToolDef>> {
        let defs = registry::tools_for(name)?;
        let mut active = self.active.write().await;
        let mut loaded = self.loaded_tools.write().await;
        active.insert(name.to_string());
        for def in &defs {
            loaded.insert(def.name.to_string(), def.clone());
        }
        Some(defs)
    }

    /// Load the starter kit — a minimal set of toolsets that every session needs.
    ///
    /// This is what runs at server startup. Additional toolsets are loaded on demand
    /// by the LLM calling `load_toolset(name)`. Keeping the baseline small means
    /// `tools/list` costs ~2K tokens instead of ~23K.
    pub async fn load_starter_kit(&self) {
        for name in registry::STARTER_KIT {
            let _ = self.load(name).await;
        }
    }

    /// Load **every** toolset, so the very first `tools/list` is complete.
    ///
    /// This exists for MCP clients that cache the initial tool list and never
    /// re-fetch it on `notifications/tools/list_changed`. For those, a tool
    /// that is not in the first listing can never be called: `load_toolset`
    /// reports the names it loaded but returns no schemas, so the client has
    /// nothing to invoke and `auto_load_toolsets` never gets a chance to fire
    /// — it only helps a caller that already knows the tool name (#134, #169).
    ///
    /// The cost is the whole point of the router: a complete listing is ~25K
    /// tokens per `tools/list` instead of ~2K. Off by default; opt in only if
    /// your client needs it.
    pub async fn load_all(&self) {
        for ts in self.registry {
            let _ = self.load(ts.name).await;
        }
    }

    /// Find which toolset a tool name belongs to, whether or not that toolset
    /// is currently loaded. Used to give the LLM an actionable error when it
    /// calls a tool whose toolset hasn't been loaded yet.
    pub fn find_toolset_for_tool(&self, tool_name: &str) -> Option<&'static str> {
        for ts in self.registry {
            if let Some(defs) = registry::tools_for(ts.name) {
                if defs.iter().any(|d| d.name == tool_name) {
                    return Some(ts.name);
                }
            }
        }
        None
    }

    pub async fn unload(&self, name: &str) -> bool {
        let defs = match registry::tools_for(name) {
            Some(d) => d,
            None => return false,
        };
        let mut active = self.active.write().await;
        let mut loaded = self.loaded_tools.write().await;
        active.remove(name);
        for def in &defs {
            loaded.remove(def.name);
        }
        true
    }

    pub async fn active_names(&self) -> Vec<String> {
        self.active.read().await.iter().cloned().collect()
    }

    pub async fn get_tool(&self, name: &str) -> Option<ToolDef> {
        self.loaded_tools.read().await.get(name).cloned()
    }

    /// Return all currently active ToolDefs for use in MCP tool listings.
    pub async fn active_tools(&self) -> Vec<ToolDef> {
        self.loaded_tools.read().await.values().cloned().collect()
    }
}

impl Default for ToolRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_toolset_loads() {
        let router = ToolRouter::new();
        for meta in registry::ALL_TOOLSETS {
            assert!(
                router.load(meta.name).await.is_some(),
                "toolset '{}' failed to load",
                meta.name
            );
        }
        assert!(router.load("nonexistent_toolset").await.is_none());
    }

    #[tokio::test]
    async fn loaded_tools_reuse_their_compiled_input_schema() {
        let router = ToolRouter::new();
        router.load("project").await.expect("known toolset");

        let first = router
            .get_tool("create_project")
            .await
            .expect("loaded tool");
        let second = router
            .get_tool("create_project")
            .await
            .expect("loaded tool");
        assert!(std::sync::Arc::ptr_eq(
            &first.input_validator,
            &second.input_validator
        ));

        let catalogue_first = registry::tools_for("project").expect("registered toolset");
        let catalogue_second = registry::tools_for("project").expect("registered toolset");
        assert!(std::sync::Arc::ptr_eq(
            &catalogue_first[0].input_validator,
            &catalogue_second[0].input_validator
        ));
    }

    #[test]
    fn every_registered_input_schema_is_valid_draft_2020_12() {
        for toolset in registry::ALL_TOOLSETS {
            for tool in registry::tools_for(toolset.name).expect("registered toolset") {
                assert!(
                    jsonschema::draft202012::meta::is_valid(&tool.input_schema),
                    "{} has an invalid input schema",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn fixed_records_are_closed_and_only_reviewed_maps_are_extensible() {
        fn visit(value: &serde_json::Value, path: &str) {
            if value["type"] == "object"
                || value["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|t| t == "object"))
                || value.get("properties").is_some()
                || value.get("patternProperties").is_some()
            {
                let policy = value
                    .get("additionalProperties")
                    .expect("every record has an explicit policy");
                if policy != false {
                    assert!(
                        matches!(path,
                        "edit_schematic_component/properties/fields" |
                        "edit_schematic_component/properties/field_placements" |
                        "batch_edit_schematic_components/properties/edits/items/properties/fields" |
                        "create_symbol/properties/fields" |
                        "set_symbol_properties/properties/fields" |
                        "copy_routing_pattern/properties/net_map" |
                        "apply_template/properties/net_mappings"
                    ),
                        "unreviewed open input record: {path}"
                    );
                }
            }
            // Traverse schema positions, not object-valued defaults/examples.
            for keyword in [
                "properties",
                "patternProperties",
                "$defs",
                "definitions",
                "dependentSchemas",
            ] {
                if let Some(children) = value[keyword].as_object() {
                    for (key, child) in children {
                        visit(child, &format!("{path}/{keyword}/{key}"));
                    }
                }
            }
            for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
                if let Some(children) = value[keyword].as_array() {
                    for (index, child) in children.iter().enumerate() {
                        visit(child, &format!("{path}/{keyword}/{index}"));
                    }
                }
            }
            for keyword in [
                "items",
                "additionalProperties",
                "contains",
                "propertyNames",
                "not",
                "if",
                "then",
                "else",
                "unevaluatedProperties",
                "unevaluatedItems",
            ] {
                if let Some(child) = value.get(keyword) {
                    visit(child, &format!("{path}/{keyword}"));
                }
            }
        }
        for meta in registry::ALL_TOOLSETS {
            for tool in registry::tools_for(meta.name).unwrap() {
                visit(&tool.input_schema, tool.name);
            }
        }
        for tool in meta_tools::meta_tool_descriptions_for(true) {
            visit(&tool.input_schema, &tool.name);
        }
    }

    #[tokio::test]
    async fn starter_kit_loads_expected_toolsets_and_nothing_more() {
        let router = ToolRouter::new();
        router.load_starter_kit().await;
        let active: std::collections::HashSet<String> =
            router.active_names().await.into_iter().collect();
        for expected in registry::STARTER_KIT {
            assert!(
                active.contains(*expected),
                "starter kit missing toolset '{}'",
                expected
            );
        }
        // On-demand toolsets must not be auto-loaded
        assert!(!active.contains("pcb_board"));
        assert!(!active.contains("integration"));
        assert!(!active.contains("templates"));
    }

    /// The eager path exists so a client that never re-fetches `tools/list`
    /// still sees every tool. That only holds if `load_all` really loads all
    /// of them — a partial listing would leave exactly the silent
    /// uncallable-tool failure it is meant to cure (#134, #169).
    #[tokio::test]
    async fn load_all_activates_every_registered_toolset() {
        let router = ToolRouter::new();
        router.load_all().await;
        let active: std::collections::HashSet<String> =
            router.active_names().await.into_iter().collect();
        for ts in registry::ALL_TOOLSETS {
            assert!(active.contains(ts.name), "load_all missed '{}'", ts.name);
        }
        assert_eq!(active.len(), registry::ALL_TOOLSETS.len());

        // And the listing it produces is the full catalogue.
        let listed = router.active_tools().await.len();
        let registered: usize = registry::ALL_TOOLSETS.iter().map(|t| t.tool_count).sum();
        assert_eq!(
            listed, registered,
            "an eager tools/list must carry every registered tool"
        );
    }

    #[tokio::test]
    async fn find_toolset_for_tool_resolves_unloaded_tools() {
        let router = ToolRouter::new();
        router.load_starter_kit().await;
        // pcb_board is NOT in starter kit, but this lookup must still find it
        assert_eq!(
            router.find_toolset_for_tool("place_component"),
            Some("pcb_components")
        );
        assert_eq!(
            router.find_toolset_for_tool("route_trace"),
            Some("pcb_routing")
        );
        assert_eq!(router.find_toolset_for_tool("nonexistent_tool"), None);
    }

    // ─── Registry invariants ─────────────────────────────────────────────────
    //
    // These are the guardrails that protect future work:
    //
    // - The hand-written `tool_count` in ALL_TOOLSETS must match what
    //   `tools_for(name)` actually returns. Otherwise `list_toolboxes` lies
    //   to the LLM.
    // - No toolset grows past ~20 tools. Past that, split it — 20 tool
    //   descriptions at ~400 bytes each is already a 1.6KB payload in
    //   `tools/list` when loaded, and tool selection accuracy degrades.

    /// The cap above which a toolset must be split. If you hit this, either
    /// move tools to a sibling toolset or add a new one — don't raise this
    /// number without a conversation.
    const MAX_TOOLS_PER_TOOLSET: usize = 20;

    #[test]
    fn registry_tool_counts_match_reality() {
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name)
                .unwrap_or_else(|| panic!("tools_for({}) returned None", meta.name));
            assert_eq!(
                defs.len(),
                meta.tool_count,
                "registry declares tool_count={} for '{}' but tools_for() returned {} tools — \
                 update ALL_TOOLSETS in router/registry.rs",
                meta.tool_count,
                meta.name,
                defs.len()
            );
        }
    }

    #[test]
    fn no_toolset_has_duplicate_tool_names() {
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name).unwrap();
            let mut seen = std::collections::HashSet::new();
            for d in &defs {
                assert!(
                    seen.insert(d.name),
                    "duplicate tool name '{}' inside toolset '{}'",
                    d.name,
                    meta.name
                );
            }
        }
    }

    #[test]
    fn tool_names_unique_across_toolsets() {
        // Duplicate names across toolsets are a silent foot-gun: whichever
        // toolset is loaded last wins in `loaded_tools`, so behavior depends
        // on load order. Aliases that point at the same handler are fine; the
        // test fails on first occurrence so the committer has to decide.
        let mut owner: std::collections::HashMap<&'static str, &'static str> =
            std::collections::HashMap::new();
        let mut collisions = Vec::new();
        for meta in registry::ALL_TOOLSETS {
            let defs = registry::tools_for(meta.name).unwrap();
            for d in &defs {
                if let Some(prev) = owner.insert(d.name, meta.name) {
                    if prev != meta.name {
                        collisions.push(format!(
                            "'{}' declared in both '{}' and '{}'",
                            d.name, prev, meta.name
                        ));
                    }
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "tool name collisions across toolsets (last-loaded wins in the router):\n  {}",
            collisions.join("\n  ")
        );
    }

    #[test]
    fn no_toolset_exceeds_max_size() {
        for meta in registry::ALL_TOOLSETS {
            assert!(
                meta.tool_count <= MAX_TOOLS_PER_TOOLSET,
                "toolset '{}' has {} tools, which exceeds the soft cap of {}. \
                 Split it into two before bumping this cap.",
                meta.name,
                meta.tool_count,
                MAX_TOOLS_PER_TOOLSET
            );
        }
    }

    #[test]
    fn starter_kit_entries_are_all_valid_toolsets() {
        for name in registry::STARTER_KIT {
            assert!(
                registry::tools_for(name).is_some(),
                "STARTER_KIT references unknown toolset '{}'",
                name
            );
        }
    }
}
