# maturana-plugin

Use this skill when discovering, validating, or designing first-party or
third-party Maturana plugins.

Plugins are the extension boundary for Maturana features. A plugin may declare
skills, tools, commands, channel adapters, provider adapters, web/API features,
MCP bundles, or guest runtime integrations. The manifest declares what exists;
Maturana still owns validation, permissions, installation, and execution policy.
Plugin enablement is host-owned state under `<home>/plugins/config.json`; do not
write operational state into third-party manifests.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/plugins.md` for the current manifest contract and search roots.
3. Inspect existing plugins with `maturana plugin list`.
4. Inspect existing `skills/`, `tools/`, and Rust commands to avoid duplicating
   a feature that already exists.
5. Identify any filesystem, egress, or secret permissions the plugin would need.
6. Inspect whether the plugin or target feature is enabled with
   `maturana plugin inspect <name>`.
7. For first-party command-family work, inspect `maturana-builtins`; built-in
   top-level commands are gated by that plugin's feature enablement, except the
   core `maturana plugin` command.

## Preflight

- Confirm the requested feature belongs in a plugin, skill, tool, or MCP bundle
  rather than directly in Maturana core.
- Confirm the plugin manifest contains no raw secrets or OAuth auth state.
- Confirm all manifest paths are relative to the plugin root.
- Confirm command entrypoints live under `commands/` or inside a tool path
  declared by the same plugin.
- Confirm any declared egress hosts and secret names are narrow.
- Confirm a local validation command can prove the manifest before deployment.
- Confirm the plugin is enabled only after validation succeeds.

## Decision Path

- Human workflow or agent-facing procedure: declare a `skill` feature.
- Executable side effect: declare a `tool` feature and keep the tool contract
  narrow.
- External protocol or server: declare an `mcp`, `channel`, `web`, or
  `provider` feature as appropriate.
- Host lifecycle operation: prefer a Rust-owned operation exposed through
  `maturana-ops`; do not add broad shell execution.
- Third-party plugin: keep permissions explicit and validate before installing
  or deploying anything.

## Actions

List discovered plugins:

```powershell
maturana plugin list
```

Inspect one plugin:

```powershell
maturana plugin inspect <name>
```

Validate a plugin directory or manifest:

```powershell
maturana plugin validate <plugin-dir-or-manifest>
```

Install a validated local plugin into the active Maturana home:

```powershell
maturana plugin install <plugin-dir-or-manifest>
maturana plugin install <plugin-dir-or-manifest> --enable
```

Show the active search roots:

```powershell
maturana plugin roots
```

Show enabled plugin assets after feature gates are applied:

```powershell
maturana plugin assets
maturana plugin assets --kind skill
```

Enable or disable a plugin:

```powershell
maturana plugin enable <name>
maturana plugin disable <name>
```

Enable or disable a single feature:

```powershell
maturana plugin enable <name> --feature <feature>
maturana plugin disable <name> --feature <feature>
```

Install enabled plugin-declared skills into Codex's native skill root:

```powershell
maturana skill codex-prompts
```

Maturana installs built-in skills first, then enabled plugin skills. If a plugin
skill name would shadow an existing skill, the install fails and the conflict
must be fixed in the plugin manifest.

Plugin skills, tools, and commands may declare `feature = "<feature-name>"`.
Those assets are active only when the referenced feature is enabled.

First-party command families are declared in the `maturana-builtins` plugin.
Disabling one of its features disables the corresponding built-in top-level
commands before command dispatch. The `maturana plugin` command remains
always available as the modular-core escape hatch so a disabled feature can be
re-enabled without hand-editing `<home>/plugins/config.json`.

Inspect the built-in command catalog when command gates matter:

```powershell
maturana plugin inspect maturana-builtins
maturana plugin assets --kind command
```

Use `--json` for machine-readable output when another tool or report needs the
result.

## Evidence

Before claiming success, collect:

- The plugin root and manifest path.
- `maturana plugin validate <path>` output showing whether the manifest is
  valid.
- The list of declared features, skills, tools, commands, and permissions.
- The active asset list from `maturana plugin assets` when feature gates matter.
- The effective plugin and feature enablement from
  `maturana plugin inspect <name>`.
- For built-in command-family changes, the `maturana-builtins` command asset and
  feature gate that controls the top-level command.
- The `effective_permissions` field from plugin inspect output; it must be empty
  for disabled or invalid plugins and narrow for enabled plugins.
- `maturana skill codex-prompts` output when the plugin contributes Codex
  skills.
- Confirmation that plugin paths are relative and remain inside the plugin
  directory.
- Confirmation that command entrypoints are descriptor paths under `commands/`
  or are backed by declared tool paths.
- Confirmation that secrets are referenced by name only, not embedded.

## Recovery

- Plugin not found: run `maturana plugin roots` and place the plugin under one
  of the listed roots, or install it with `maturana plugin install <path>`.
- Manifest parse fails: fix TOML/JSON syntax before changing feature code.
- Path validation fails: move the asset inside the plugin directory and use a
  relative path.
- Permission too broad: narrow filesystem, egress, or secret declarations before
  installation.
- Invalid plugin accidentally enabled: `maturana plugin disable <name>`, fix the
  manifest, validate, then enable again.
- Skill install conflict: rename the plugin skill or disable the plugin; do not
  shadow first-party skills.
- Feature belongs in core: keep only the stable contract in core and move the
  feature behavior back into the plugin.
- Built-in command disabled unexpectedly: use `maturana plugin inspect
  maturana-builtins`, then re-enable the owning feature with `maturana plugin
  enable maturana-builtins --feature <feature>`.

## Boundaries

- Do not store raw secrets, tokens, or OAuth auth state in plugin manifests.
- Do not use scripts as a new product control plane.
- Do not create generic host command runners for plugins.
- Do not install or deploy invalid plugins.
- Do not use `--force` unless intentionally replacing a local installed plugin.
- Do not let plugin skills shadow first-party Codex skills.
- Do not bypass the skill/tool deployment paths for guest capabilities.
- Do not make non-core built-in command families bypass `maturana-builtins`
  feature gates.
