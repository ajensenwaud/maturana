//! On-the-fly WASM tool framework.
//!
//! Maturana agents can author, build, register, and run their own tools at
//! runtime without rebuilding the host. A tool is a single WebAssembly module
//! plus a declarative [`ToolManifest`]. The host (or a guest) runs the module
//! in a sandbox with no ambient authority: the manifest's [`Capabilities`]
//! are the *only* things granted, resource use is bounded by fuel, memory, and
//! a wall-clock timeout, and the module talks to the outside world through a
//! narrow stdin/stdout JSON contract.
//!
//! The execution engine ([`crate::tools::wasm`]) is behind the `wasm-runtime`
//! feature so the control plane (manifests, capability policy, the on-disk
//! registry, and the Telegram build/run animation) stays dependency-light and
//! always testable. When the feature is off, [`run_tool`] returns a clear
//! "engine not built in" error rather than silently doing nothing.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(feature = "wasm-runtime")]
pub mod wasm;

/// On-the-fly capability forge (self-mutation). Needs the WAT assembler + the
/// execution engine, so it rides the same feature as `wasm`.
#[cfg(feature = "wasm-runtime")]
pub mod forge;

/// What a tool is allowed to touch. Default is the empty set: a freshly
/// authored tool is pure compute (stdin -> stdout) until the author opts in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Capabilities {
    /// Host directories the tool may read, mapped into the guest as the same
    /// relative names under a preopened root. Empty means no filesystem.
    pub fs_read: Vec<String>,
    /// Host directories the tool may write.
    pub fs_write: Vec<String>,
    /// Environment variable names passed through (values only, never secrets
    /// unless the name resolves through pipelock at call time).
    pub env: Vec<String>,
    /// Outbound network hosts the tool may reach. Empty means no network.
    /// Enforced by the egress proxy, not by WASI, which has no sockets.
    pub net: Vec<String>,
}

impl Capabilities {
    pub fn is_pure(&self) -> bool {
        self.fs_read.is_empty()
            && self.fs_write.is_empty()
            && self.env.is_empty()
            && self.net.is_empty()
    }
}

/// Hard ceiling on a tool's declared linear-memory limit, so a manifest cannot
/// request an absurd allocation that exhausts host memory at instantiation.
pub const MAX_TOOL_MEMORY_MB: u32 = 4096;

/// Resource ceilings applied to every invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResourceLimits {
    /// Wasmtime fuel units; bounds total executed instructions.
    pub fuel: u64,
    /// Linear memory ceiling in megabytes.
    pub memory_mb: u32,
    /// Wall-clock timeout in milliseconds (enforced via epoch interruption).
    pub timeout_ms: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            fuel: 2_000_000_000,
            memory_mb: 256,
            timeout_ms: 30_000,
        }
    }
}

/// The durable description of a tool. Serialized as `tool.json` next to the
/// `.wasm` module inside the registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Path to the compiled module, relative to the manifest's directory.
    pub wasm: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub limits: ResourceLimits,
    /// Free-form JSON Schema describing the stdin payload (documentation only).
    #[serde(default)]
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub output_schema: serde_json::Value,
}

impl ToolManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !is_valid_tool_name(&self.name) {
            anyhow::bail!(
                "invalid tool name '{}': use lowercase letters, digits, and dashes",
                self.name
            );
        }
        if self.version.trim().is_empty() {
            anyhow::bail!("tool '{}' is missing a version", self.name);
        }
        if self.wasm.trim().is_empty() {
            anyhow::bail!("tool '{}' is missing a wasm module path", self.name);
        }
        if self.wasm.contains("..") || self.wasm.starts_with('/') || self.wasm.contains('\\') {
            anyhow::bail!(
                "tool '{}' wasm path must stay inside its directory",
                self.name
            );
        }
        if self.limits.timeout_ms == 0 {
            anyhow::bail!("tool '{}' timeout_ms must be greater than zero", self.name);
        }
        if self.limits.memory_mb == 0 || self.limits.memory_mb > MAX_TOOL_MEMORY_MB {
            anyhow::bail!(
                "tool '{}' memory_mb must be between 1 and {MAX_TOOL_MEMORY_MB}",
                self.name
            );
        }
        // A preopened directory is a real grant of host filesystem authority, so
        // reject traversal sequences that would let a declared grant escape the
        // directory it names. (Operators still vet the absolute roots a tool may
        // request via the security-review skill.)
        for dir in self
            .capabilities
            .fs_read
            .iter()
            .chain(&self.capabilities.fs_write)
        {
            if dir.split(|c| c == '/' || c == '\\').any(|seg| seg == "..") {
                anyhow::bail!(
                    "tool '{}' capability path '{dir}' must not contain '..'",
                    self.name
                );
            }
        }
        Ok(())
    }
}

pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Outcome of one tool invocation, recorded for audit and for the
/// self-improvement trajectory log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRunResult {
    pub tool: String,
    pub version: String,
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub fuel_used: Option<u64>,
    pub duration_ms: u128,
    pub at: DateTime<Utc>,
}

/// On-disk tool registry rooted under `<home>/tools/<name>/`.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    root: PathBuf,
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tool_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn manifest_path(&self, name: &str) -> PathBuf {
        self.tool_dir(name).join("tool.json")
    }

    /// Register a tool by copying its compiled module into the registry and
    /// writing a normalized manifest. Returns the resolved manifest.
    pub fn register(
        &self,
        manifest: &ToolManifest,
        wasm_bytes: &[u8],
    ) -> anyhow::Result<ToolManifest> {
        manifest.validate()?;
        if !is_wasm_module(wasm_bytes) {
            anyhow::bail!(
                "tool '{}' module is not a WebAssembly binary (missing \\0asm header)",
                manifest.name
            );
        }
        let dir = self.tool_dir(&manifest.name);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

        let wasm_name = "module.wasm";
        fs::write(dir.join(wasm_name), wasm_bytes)
            .with_context(|| format!("failed to write module for {}", manifest.name))?;

        let mut stored = manifest.clone();
        stored.wasm = wasm_name.to_string();
        fs::write(
            self.manifest_path(&manifest.name),
            serde_json::to_string_pretty(&stored)?,
        )
        .with_context(|| format!("failed to write manifest for {}", manifest.name))?;
        Ok(stored)
    }

    pub fn load(&self, name: &str) -> anyhow::Result<ToolManifest> {
        // `register` validates `manifest.name` before writing, but the read
        // paths (`load`/`run_tool`/`wasm_bytes`) take a caller-supplied lookup
        // name that is joined straight onto the registry root. Reject anything
        // that is not a plain tool name so `tool run ../../secret` (or, on
        // Windows, an absolute path that replaces the root entirely) cannot read
        // outside the registry.
        if !is_valid_tool_name(name) {
            anyhow::bail!("invalid tool name '{name}'");
        }
        let path = self.manifest_path(name);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("tool '{name}' is not registered ({})", path.display()))?;
        let manifest: ToolManifest = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse manifest {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn wasm_bytes(&self, manifest: &ToolManifest) -> anyhow::Result<Vec<u8>> {
        let path = self.tool_dir(&manifest.name).join(&manifest.wasm);
        fs::read(&path).with_context(|| format!("failed to read module {}", path.display()))
    }

    pub fn list(&self) -> anyhow::Result<Vec<ToolManifest>> {
        let mut tools = Vec::new();
        if !self.root.exists() {
            return Ok(tools);
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(manifest) = self.load(&name) {
                tools.push(manifest);
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }
}

/// First four bytes of every WebAssembly module: `\0asm`.
pub fn is_wasm_module(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == b"\0asm"
}

/// Run a registered tool against a JSON `input`.
///
/// With the `wasm-runtime` feature this executes the module in a sandbox; the
/// control-plane build (default) returns an explanatory error so callers can
/// still register, list, and reason about tools.
pub fn run_tool(registry: &ToolRegistry, name: &str, input: &str) -> anyhow::Result<ToolRunResult> {
    let manifest = registry.load(name)?;
    let wasm = registry.wasm_bytes(&manifest)?;
    run_manifest(&manifest, &wasm, input)
}

#[cfg(feature = "wasm-runtime")]
pub fn run_manifest(
    manifest: &ToolManifest,
    wasm: &[u8],
    input: &str,
) -> anyhow::Result<ToolRunResult> {
    wasm::execute(manifest, wasm, input)
}

#[cfg(not(feature = "wasm-runtime"))]
pub fn run_manifest(
    _manifest: &ToolManifest,
    _wasm: &[u8],
    _input: &str,
) -> anyhow::Result<ToolRunResult> {
    anyhow::bail!(
        "wasm execution engine not built in; rebuild with `--features wasm-runtime` to run tools"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_wasm() -> Vec<u8> {
        // Minimal valid module header: `\0asm` + version 1.
        vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    }

    fn registry() -> ToolRegistry {
        let dir = std::env::temp_dir().join(format!("maturana-tools-{}", uuid::Uuid::new_v4()));
        ToolRegistry::new(dir)
    }

    #[test]
    fn names_are_validated() {
        assert!(is_valid_tool_name("weather-fetch"));
        assert!(is_valid_tool_name("t1"));
        assert!(!is_valid_tool_name("Weather"));
        assert!(!is_valid_tool_name("-bad"));
        assert!(!is_valid_tool_name("bad-"));
        assert!(!is_valid_tool_name("with space"));
    }

    #[test]
    fn default_capabilities_are_empty() {
        assert!(Capabilities::default().is_pure());
    }

    #[test]
    fn register_round_trips_manifest_and_module() {
        let reg = registry();
        let manifest = ToolManifest {
            name: "echo-json".to_string(),
            version: "0.1.0".to_string(),
            description: "echoes its input".to_string(),
            wasm: "build/echo.wasm".to_string(),
            capabilities: Capabilities::default(),
            limits: ResourceLimits::default(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::Value::Null,
        };
        let stored = reg.register(&manifest, &tiny_wasm()).unwrap();
        // The stored manifest points at the normalized in-registry module name.
        assert_eq!(stored.wasm, "module.wasm");

        let loaded = reg.load("echo-json").unwrap();
        assert_eq!(loaded.name, "echo-json");
        assert_eq!(loaded.version, "0.1.0");
        assert!(loaded.capabilities.is_pure());
        assert_eq!(reg.wasm_bytes(&loaded).unwrap(), tiny_wasm());

        let listed = reg.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "echo-json");
        let _ = fs::remove_dir_all(reg.root());
    }

    #[test]
    fn register_rejects_non_wasm_bytes() {
        let reg = registry();
        let manifest = ToolManifest {
            name: "bogus".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            wasm: "m.wasm".to_string(),
            capabilities: Capabilities::default(),
            limits: ResourceLimits::default(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        };
        let error = reg.register(&manifest, b"not wasm at all").unwrap_err();
        assert!(error.to_string().contains("WebAssembly"));
        let _ = fs::remove_dir_all(reg.root());
    }

    #[test]
    fn manifest_validation_rejects_path_escape() {
        let manifest = ToolManifest {
            name: "ok".to_string(),
            version: "1".to_string(),
            description: String::new(),
            wasm: "../escape.wasm".to_string(),
            capabilities: Capabilities::default(),
            limits: ResourceLimits::default(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn load_rejects_traversal_name() {
        let reg = registry();
        // No registry on disk needed: the name guard must fire before any path
        // is built, so a traversal/absolute lookup never touches the filesystem.
        for bad in ["../secret", "..\\secret", "/etc/passwd", "a/b", "C:\\x"] {
            let error = reg.load(bad).unwrap_err();
            assert!(
                error.to_string().contains("invalid tool name"),
                "expected name rejection for {bad:?}, got: {error}"
            );
        }
    }

    #[test]
    fn validate_rejects_capability_traversal_and_bad_memory() {
        let mut manifest = ToolManifest {
            name: "ok".to_string(),
            version: "1".to_string(),
            description: String::new(),
            wasm: "m.wasm".to_string(),
            capabilities: Capabilities {
                fs_read: vec!["../../etc".to_string()],
                ..Default::default()
            },
            limits: ResourceLimits::default(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        };
        assert!(manifest.validate().is_err());

        manifest.capabilities = Capabilities::default();
        manifest.limits.memory_mb = 0;
        assert!(manifest.validate().is_err());
        manifest.limits.memory_mb = MAX_TOOL_MEMORY_MB + 1;
        assert!(manifest.validate().is_err());
        manifest.limits.memory_mb = 64;
        assert!(manifest.validate().is_ok());
    }

    #[cfg(not(feature = "wasm-runtime"))]
    #[test]
    fn run_without_engine_explains_itself() {
        let reg = registry();
        let manifest = ToolManifest {
            name: "echo".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            wasm: "m.wasm".to_string(),
            capabilities: Capabilities::default(),
            limits: ResourceLimits::default(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        };
        reg.register(&manifest, &tiny_wasm()).unwrap();
        let error = run_tool(&reg, "echo", "{}").unwrap_err();
        assert!(error
            .to_string()
            .contains("wasm execution engine not built in"));
        let _ = fs::remove_dir_all(reg.root());
    }
}
