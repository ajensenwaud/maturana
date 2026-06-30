//! On-the-fly capability "forge" — Maturana's self-mutation primitive.
//!
//! An agent that has been granted `capabilities.self_forge` can author a new
//! capability *while it is running*, hand it to the host, and use it the same
//! turn — the WebAssembly analogue of pi.dev's "ask it to build, then use it
//! live". The agent writes a module in WebAssembly text (assembled here with no
//! external toolchain) or hands over a base64 `.wasm` binary; this module
//! compiles it, registers it in the agent's private registry, and runs it in
//! the exact same fuel/epoch/memory/WASI sandbox as any other Maturana tool.
//! Nothing the agent forges escapes that sandbox.

use anyhow::Context;
use base64::Engine as _;

use super::{
    is_wasm_module, run_tool, Capabilities, ResourceLimits, ToolManifest, ToolRegistry,
    ToolRunResult,
};

/// How the agent delivered the module source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeFormat {
    /// WebAssembly text format, assembled here — no external toolchain needed,
    /// so an agent can write a capability inline and run it immediately.
    Wat,
    /// A base64-encoded, already-compiled `.wasm` binary.
    WasmBase64,
}

impl ForgeFormat {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "wat" | "wast" | "text" => Ok(Self::Wat),
            "wasm" | "wasm-base64" | "base64" | "binary" => Ok(Self::WasmBase64),
            other => anyhow::bail!("unknown forge source format '{other}' (use 'wat' or 'wasm')"),
        }
    }
}

/// Compile an agent-supplied module to WebAssembly bytes.
pub fn compile(format: ForgeFormat, source: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = match format {
        ForgeFormat::Wat => {
            wat::parse_str(source).context("failed to assemble WAT source to WebAssembly")?
        }
        ForgeFormat::WasmBase64 => base64::engine::general_purpose::STANDARD
            .decode(source.trim())
            .context("failed to base64-decode the wasm module")?,
    };
    if !is_wasm_module(&bytes) {
        anyhow::bail!("compiled output is not a WebAssembly module (missing \\0asm header)");
    }
    Ok(bytes)
}

/// What a forge produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgeOutcome {
    pub name: String,
    /// Size of the compiled module in bytes.
    pub bytes: usize,
    pub run: ToolRunResult,
}

/// A parsed/validated forge request.
pub struct ForgeSpec<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub format: ForgeFormat,
    pub source: &'a str,
    pub input: &'a str,
    pub capabilities: Capabilities,
    pub limits: ResourceLimits,
}

/// Compile + register (in `registry`, the agent's private forge space) + run, in
/// one shot. The agent gets the run result and the capability persists under its
/// name so it (or a later turn) can re-run it via the normal tool path.
pub fn forge_and_run(registry: &ToolRegistry, spec: ForgeSpec<'_>) -> anyhow::Result<ForgeOutcome> {
    let wasm = compile(spec.format, spec.source)?;
    let bytes = wasm.len();
    let manifest = ToolManifest {
        name: spec.name.to_string(),
        version: "forged".to_string(),
        description: spec.description.to_string(),
        wasm: "module.wasm".to_string(),
        capabilities: spec.capabilities,
        limits: spec.limits,
        input_schema: serde_json::Value::Null,
        output_schema: serde_json::Value::Null,
    };
    let stored = registry.register(&manifest, &wasm)?;
    let run = run_tool(registry, &stored.name, spec.input)?;
    Ok(ForgeOutcome {
        name: stored.name,
        bytes,
        run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAT module an LLM could plausibly write: read up to 64 bytes of JSON-ish
    /// stdin, ignore it, and write a fixed line to stdout via WASI `fd_write`.
    const HELLO_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; iovec at 0: ptr=8, len=14 ; message "forged: alive\n" at 8
  (data (i32.const 0) "\08\00\00\00\0e\00\00\00")
  (data (i32.const 8) "forged: alive\n")
  (func (export "_start")
    (drop (call $fd_write
      (i32.const 1)   ;; stdout
      (i32.const 0)   ;; iovec base
      (i32.const 1)   ;; iovec count
      (i32.const 24)  ;; nwritten out
    ))))
"#;

    #[test]
    fn compile_rejects_garbage_and_accepts_wat() {
        assert!(compile(ForgeFormat::Wat, "(module)").is_ok());
        assert!(compile(ForgeFormat::Wat, "this is not wat").is_err());
        // A base64 blob that is not a wasm module is rejected by the header check.
        let junk = base64::engine::general_purpose::STANDARD.encode(b"not wasm");
        assert!(compile(ForgeFormat::WasmBase64, &junk).is_err());
    }

    #[test]
    fn format_parsing() {
        assert_eq!(ForgeFormat::parse("WAT").unwrap(), ForgeFormat::Wat);
        assert_eq!(ForgeFormat::parse("wasm").unwrap(), ForgeFormat::WasmBase64);
        assert!(ForgeFormat::parse("ruby").is_err());
    }

    #[test]
    fn forge_compiles_registers_and_runs_a_wat_capability() {
        let dir = std::env::temp_dir().join(format!("maturana-forge-{}", uuid::Uuid::new_v4()));
        let registry = ToolRegistry::new(&dir);
        let outcome = forge_and_run(
            &registry,
            ForgeSpec {
                name: "hello-forge",
                description: "forged in a test",
                format: ForgeFormat::Wat,
                source: HELLO_WAT,
                input: "{}",
                capabilities: Capabilities::default(),
                limits: ResourceLimits::default(),
            },
        )
        .expect("forge should compile, register, and run");
        assert_eq!(outcome.name, "hello-forge");
        assert!(outcome.run.ok, "stderr: {}", outcome.run.stderr);
        assert!(outcome.run.stdout.contains("forged: alive"));
        // The capability persists and is re-runnable by name.
        assert!(registry.load("hello-forge").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
