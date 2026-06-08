# maturana-wiki

Use this skill when adding shared markdown context to Maturana's LLM-wiki store
or retrieving relevant chunks for an agent.

## Initialize

```powershell
.\scripts\maturana.ps1 wiki init
```

This creates:

- `.maturana/wiki/INDEX.md`
- `.maturana/wiki/chunks/`

## Ingest

```powershell
.\scripts\maturana.ps1 wiki ingest .\docs\some-file.md --title Some-File
```

The ingester splits markdown by headings and size, then writes chunk files under
`.maturana/wiki/chunks/`. Use `--chunk-chars` only when the default chunking is
too coarse or too small.

## Search

```powershell
.\scripts\maturana.ps1 wiki search "network policy" --limit 5
```

Search is deliberately simple keyword search for the MVP. Prefer plain markdown
and grep-friendly titles over a vector database at this stage.

## Guest Context

Agents should read `/wiki/INDEX.md` and relevant chunk files on demand. The
shared wiki must stay inspectable, editable, and diffable.
