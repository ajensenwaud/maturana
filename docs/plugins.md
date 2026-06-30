# Maturana Plugins

Plugins are the extension boundary for Maturana features. A plugin is a
directory with a manifest that declares the skills, tools, commands, channel
adapters, provider adapters, or web/API features it contributes. The manifest is
metadata only: Maturana still owns validation, permission checks, installation,
and execution policy.

This is the first step toward a leaner core: core crates provide contracts and
safe runtime primitives; feature bundles live as plugins that can be first-party
or third-party.

## Search Roots

`maturana plugin list` discovers plugins from:

1. `<repo>/plugins`
2. `<home>/plugins`

`<home>` is the active Maturana home, usually `.maturana`.

Inspect the active roots with:

```powershell
maturana plugin roots
```

Install a third-party plugin from a local directory or manifest into the active
home root:

```powershell
maturana plugin install <plugin-dir-or-manifest>
maturana plugin install <plugin-dir-or-manifest> --enable
```

Install validates the manifest first, refuses invalid plugins, and copies the
plugin tree to `<home>/plugins/<name>`. Use `--force` only to replace an already
installed plugin with the same name.

## Manifest Files

Each plugin directory may contain one of:

- `MATURANA_PLUGIN.toml`
- `plugin.toml`
- `.maturana-plugin/plugin.toml`
- `plugin.json`
- `.maturana-plugin/plugin.json`

TOML is preferred for hand-authored plugins.

## Enablement State

Plugin manifests declare defaults. Local operator choices live in:

```text
<home>/plugins/config.json
```

The config file is host-owned Maturana state. Do not put enablement state inside
third-party plugin manifests.

A plugin is enabled when either:

- the plugin was explicitly enabled with `maturana plugin enable <name>`
- one of its features has `default_enabled = true`
- one of its features was explicitly enabled with
  `maturana plugin enable <name> --feature <feature>`

An explicit `maturana plugin disable <name>` disables the plugin and all of its
features until it is re-enabled.

Plugin status includes `effective_permissions`, which is empty for disabled or
invalid plugins and mirrors the manifest permissions only when the plugin is
enabled and valid. This is the permission set security review tooling should
inspect before deployment or feature enablement.

## Built-In Command Gates

First-party command families are cataloged by the `maturana-builtins` plugin in
the workspace `plugins/` root. This makes built-in features and third-party
features use the same manifest, validation, enablement, and asset-inspection
model.

At CLI startup, Maturana maps the requested top-level command to the
corresponding `maturana-builtins` command asset. If that asset references a
disabled feature, dispatch is refused before the command handler runs. For
example, disabling the `channels` feature disables `maturana channel`,
`maturana notify`, and `maturana tui`.

The `maturana plugin` command is intentionally part of the modular core and is
always available, even though it is listed in the built-ins catalog for
inspection. Operators must be able to re-enable a disabled feature without
editing `<home>/plugins/config.json` by hand.

If the `maturana-builtins` catalog is absent, unknown, or missing a command
entry, Maturana stays permissive for backwards compatibility and installation
recovery. If the catalog exists but is invalid, command dispatch fails until the
catalog is fixed.

`web-cockpit` is currently declared with `default_enabled = false`, so
`maturana web` is enabled only after the feature is explicitly enabled:

```powershell
maturana plugin enable maturana-builtins --feature web-cockpit
```

## Codex Skill Installation

`maturana skill codex-prompts` installs the built-in `skills/` tree and every
skill declared by enabled, valid plugins. Plugin skill paths are resolved inside
the plugin root and are copied to the Codex skill destination under the
manifest-declared skill name.

Plugin skill names must not shadow existing first-party skills or another
enabled plugin skill. Maturana refuses the install instead of silently replacing
a skill.

Skills, tools, and commands may set `feature = "<feature-name>"`. When present,
the asset is active only when that feature is enabled. Assets without a
`feature` field follow the plugin's overall enablement.

## Minimal Manifest

```toml
name = "example"
version = "0.1.0"
description = "Example plugin that contributes a skill and a tool"

[[features]]
name = "example-skill"
kind = "skill"
description = "Adds an example agent-facing workflow"
entrypoint = "skills/example/SKILL.md"
default_enabled = true

[[features]]
name = "example-tool"
kind = "tool"
description = "Adds a narrow executable helper"
entrypoint = "tools/example-tool"
default_enabled = false

[[skills]]
name = "example"
path = "skills/example/SKILL.md"
description = "Example Codex skill installed when the plugin is enabled"
feature = "example-skill"

[[tools]]
name = "example-tool"
path = "tools/example-tool"
description = "A narrow executable helper"
feature = "example-tool"

[[commands]]
name = "example-command"
description = "A declared host command entrypoint; Maturana owns execution policy"
entrypoint = "commands/example-command.toml"
feature = "example-tool"

[permissions]
egress = ["api.example.com"]
secrets = ["example/api-key"]
filesystem = ["/workspace"]
```

Feature `kind` is intentionally an open string. First-party and third-party
plugins can declare new feature families without requiring a core enum change.
Common kinds are `skill`, `tool`, `command`, `channel`, `provider`, `web`,
`mcp`, `secret-provider`, `host-op`, and `guest-runtime`.

## Commands

Command entrypoints are declarations, not permission to execute arbitrary host
programs. A command entrypoint must be a relative path that resolves inside the
plugin and either:

- lives under `commands/`, as a descriptor Maturana can inspect
- lives inside a tool path already declared by the same plugin

This keeps third-party command metadata path-safe while still allowing a plugin
to expose a command backed by its own declared tool.

```powershell
maturana plugin list
maturana plugin inspect <name>
maturana plugin validate <plugin-dir-or-manifest>
maturana plugin install <plugin-dir-or-manifest>
maturana plugin install <plugin-dir-or-manifest> --enable
maturana plugin roots
maturana plugin assets
maturana plugin assets --kind skill
maturana plugin enable <name>
maturana plugin disable <name>
maturana plugin enable <name> --feature <feature>
maturana plugin disable <name> --feature <feature>
```

Pass `--json` to each command for machine-readable output.

## Boundaries

- A plugin manifest declares capabilities; it does not grant execution by
  itself.
- Enabling a plugin records host state; it does not bypass validation,
  permission checks, or guest deployment.
- `maturana plugin install` copies only validated local plugin trees into
  `<home>/plugins`; it does not execute plugin code.
- Enabled plugin skills are installed into Codex only through
  `maturana skill codex-prompts`.
- Plugin paths must be relative to the plugin root.
- Raw secrets must never be stored in plugin manifests or source.
- Effective permissions come from enabled, valid plugins only; disabled or
  invalid plugins must not contribute filesystem, egress, or secret grants.
- Host operations should call Rust-owned Maturana operations, not broad shell
  runners.
- Guest capabilities should deploy through the existing skill/tool deployment
  path.

## Current Status

The manifest/discovery contract is implemented in `maturana-plugin`, with
shared host operations in `maturana-ops`. The CLI and web cockpit both use the
same catalog. Enabled assets are resolved through feature gates before Codex
skill installation or operator inspection. Local third-party plugin installation
is available through the shared ops layer. The `maturana-builtins` first-party
plugin now catalogs Rust-owned built-in command families as plugin command
assets, and CLI dispatch enforces those built-in feature gates while keeping
`maturana plugin` as the always-on core escape hatch. Further refactor work
should continue moving feature-family implementation out of CLI/core modules and
behind first-party plugin bundles while keeping the core contracts small.
