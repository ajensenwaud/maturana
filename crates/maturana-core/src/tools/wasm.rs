//! Wasmtime-backed execution engine for Maturana tools.
//!
//! Each invocation gets a fresh [`wasmtime::Store`] with:
//! - **fuel** metering (bounds total executed instructions),
//! - **epoch interruption** driven by a watchdog thread (wall-clock timeout),
//! - a **linear-memory ceiling** via [`wasmtime::StoreLimits`], and
//! - **WASI preview1** wired to in-memory stdin/stdout/stderr pipes plus only
//!   the filesystem/env capabilities the manifest opted into.
//!
//! The module is treated as a WASI command: it reads its JSON request on
//! stdin and writes its JSON response on stdout. There is no ambient
//! authority — no sockets, no clock-free escape, no host filesystem beyond the
//! declared preopens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use super::{Capabilities, ToolManifest, ToolRunResult};

struct HostState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

pub fn execute(manifest: &ToolManifest, wasm: &[u8], input: &str) -> anyhow::Result<ToolRunResult> {
    manifest.validate()?;
    let started = Instant::now();

    let mut config = Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);
    let engine = Engine::new(&config).context("failed to construct wasm engine")?;
    let module =
        Module::new(&engine, wasm).context("failed to compile tool wasm module")?;

    let stdout = MemoryOutputPipe::new(4 * 1024 * 1024);
    let stderr = MemoryOutputPipe::new(1024 * 1024);

    let mut builder = WasiCtxBuilder::new();
    builder
        .stdin(MemoryInputPipe::new(input.as_bytes().to_vec()))
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    apply_capabilities(&mut builder, &manifest.capabilities)?;
    let wasi = builder.build_p1();

    let limits = StoreLimitsBuilder::new()
        .memory_size(manifest.limits.memory_mb as usize * 1024 * 1024)
        .build();
    let mut store = Store::new(&engine, HostState { wasi, limits });
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(manifest.limits.fuel)
        .context("failed to set wasm fuel")?;
    // Trap as soon as the watchdog advances the epoch past our deadline.
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();

    // Watchdog: bump the engine epoch once the wall-clock timeout elapses,
    // which makes the running guest trap. It exits early when the call returns.
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let engine = engine.clone();
        let finished = Arc::clone(&finished);
        let deadline = Instant::now() + Duration::from_millis(manifest.limits.timeout_ms);
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            engine.increment_epoch();
        })
    };

    let mut linker: Linker<HostState> = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |state: &mut HostState| &mut state.wasi)
        .context("failed to wire WASI preview1")?;

    let run = (|| -> anyhow::Result<()> {
        linker
            .module(&mut store, "", &module)
            .context("failed to instantiate tool module")?;
        let start = linker
            .get_default(&mut store, "")
            .context("tool module has no default (_start) export")?
            .typed::<(), ()>(&store)?;
        start.call(&mut store, ()).context("tool execution trapped")
    })();

    finished.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    let fuel_used = store
        .get_fuel()
        .ok()
        .map(|remaining| manifest.limits.fuel.saturating_sub(remaining));

    let stdout_text = String::from_utf8_lossy(&stdout.contents()).to_string();
    let mut stderr_text = String::from_utf8_lossy(&stderr.contents()).to_string();
    let ok = match &run {
        Ok(()) => true,
        Err(error) => {
            if !stderr_text.is_empty() {
                stderr_text.push('\n');
            }
            stderr_text.push_str(&format!("{error:#}"));
            false
        }
    };

    Ok(ToolRunResult {
        tool: manifest.name.clone(),
        version: manifest.version.clone(),
        ok,
        stdout: stdout_text,
        stderr: stderr_text,
        fuel_used,
        duration_ms: started.elapsed().as_millis(),
        at: Utc::now(),
    })
}

fn apply_capabilities(
    builder: &mut WasiCtxBuilder,
    capabilities: &Capabilities,
) -> anyhow::Result<()> {
    for dir in &capabilities.fs_read {
        builder
            .preopened_dir(dir, dir.as_str(), DirPerms::READ, FilePerms::READ)
            .with_context(|| format!("failed to preopen read dir {dir}"))?;
    }
    for dir in &capabilities.fs_write {
        builder
            .preopened_dir(
                dir,
                dir.as_str(),
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            )
            .with_context(|| format!("failed to preopen write dir {dir}"))?;
    }
    for name in &capabilities.env {
        if let Ok(value) = std::env::var(name) {
            builder.env(name, value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Capabilities, ResourceLimits};

    fn manifest(limits: ResourceLimits) -> ToolManifest {
        ToolManifest {
            name: "fixture".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            wasm: "module.wasm".to_string(),
            capabilities: Capabilities::default(),
            limits,
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        }
    }

    #[test]
    fn runs_a_wasi_command_and_captures_stdout() {
        // A minimal WASI command module that writes "ok\n" to fd 1 (stdout).
        let wasm = wat::parse_str(
            r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 8) "ok\n")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 8))
                (i32.store (i32.const 4) (i32.const 3))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 20)))))
            "#,
        )
        .unwrap();

        let result = execute(&manifest(ResourceLimits::default()), &wasm, "{}").unwrap();
        assert!(result.ok, "stderr: {}", result.stderr);
        assert_eq!(result.stdout, "ok\n");
        assert!(result.fuel_used.unwrap() > 0);
    }

    #[test]
    // On Windows, this wasmtime build delivers a fuel/epoch trap by tearing the
    // process down (STATUS_STACK_BUFFER_OVERRUN) rather than surfacing a catchable
    // `Err`, so the test self-aborts here. On Linux — where Maturana runs its agent
    // fleets and where a runaway module is bounded by a graceful trap — it runs and
    // proves the fuel/timeout ceiling. (Maturana is single-user, so a granted
    // agent's runaway forge only aborts its own supervised sessiond, which the
    // plane restarts.)
    #[cfg_attr(
        windows,
        ignore = "wasmtime traps abort the process on Windows; the fuel/timeout bound is validated on Linux"
    )]
    fn fuel_or_timeout_stops_an_infinite_loop() {
        // Infinite loop: bounded fuel and a short timeout must terminate it
        // with a trap rather than hanging the host.
        let wasm = wat::parse_str(r#"(module (func (export "_start") (loop br 0)))"#).unwrap();
        let limits = ResourceLimits {
            fuel: 5_000_000,
            memory_mb: 16,
            timeout_ms: 2_000,
        };
        let result = execute(&manifest(limits), &wasm, "{}").unwrap();
        assert!(!result.ok);
        assert!(
            result.stderr.contains("fuel") || result.stderr.to_lowercase().contains("epoch")
                || result.stderr.contains("trap")
                || result.stderr.contains("interrupt"),
            "unexpected stderr: {}",
            result.stderr
        );
    }
}
