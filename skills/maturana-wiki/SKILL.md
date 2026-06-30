# maturana-wiki

Use this skill when adding shared markdown context to Maturana's LLM-wiki store
or retrieving relevant chunks for an agent.

The wiki is deliberately plain markdown: chunked, inspectable, editable, and
diffable. Do not introduce a vector database or hidden context service for the
MVP.

## Grounding

1. Read `AGENTS.md` first.
2. Identify whether the request is initialize, ingest, search, or debug
   context loading.
3. Inspect current wiki state:
   - `.maturana/wiki/INDEX.md`
   - `.maturana/wiki/chunks/`
4. For agent-specific context issues, inspect the latest channel context
   manifest:
   - `.maturana/agents/<agent-id>/channels/telegram/<chat-id>.context.json`
5. Prefer source markdown files that are stable, human-readable, and safe to
   share across agents.

## Preflight

- Confirm the source file exists and is safe to share across intended agents.
- Scan for obvious secrets, OAuth state, private keys, and raw API tokens before
  ingesting.
- Confirm chunk size preserves readable markdown sections.
- Confirm retrieval should use current message plus recent transcript terms
  rather than loading the whole wiki.
- Confirm `/new` should reload durable memory/wiki context without deleting it.

## Decision Path

- Wiki missing: run `wiki init`.
- New shared document: run `wiki ingest` with a clear title.
- Large or poorly split markdown: tune `--chunk-chars`; keep chunks readable.
- Retrieval question: run `wiki search` first, then inspect matching chunk
  files directly if needed.
- Channel follow-up misses context: inspect the context manifest and verify the
  relevant chunk terms appear in the current message or recent transcript.
- Sensitive content: do not ingest secrets, OAuth credentials, private tokens,
  or material outside the intended agent context.

## Actions

Initialize:

```powershell
maturana wiki init
```

This creates:

- `.maturana/wiki/INDEX.md`
- `.maturana/wiki/chunks/`

Ingest:

```powershell
maturana wiki ingest .\docs\some-file.md --title Some-File
```

Use `--chunk-chars` only when the default chunking is too coarse or too small:

```powershell
maturana wiki ingest .\docs\long-file.md --title Long-File --chunk-chars 2400
```

Search:

```powershell
maturana wiki search "network policy" --limit 5
```

## Evidence

Before claiming success, collect:

- `INDEX.md` exists and references the ingested title/source.
- Chunk files exist under `.maturana/wiki/chunks/`.
- Chunk filenames are stable slug/id names and contents include markdown
  frontmatter with title, source, and chunk number.
- `wiki search` returns expected chunks for representative terms.
- For channel context, the latest `.context.json` lists the loaded wiki chunk
  path, character count, matched terms, term sources, and context policy.

## Recovery

- Search returns nothing: simplify the query to concrete nouns from the source
  document.
- Too many irrelevant chunks: re-ingest with a clearer title or smaller
  `--chunk-chars`.
- Chunking split a section badly: adjust headings in the source markdown or
  re-ingest with a larger chunk size.
- Context manifest did not include an expected chunk: verify `.context.json`
  has the expected term under `wiki_term_sources` from either
  `current_message` or `recent_transcript`, then search the wiki manually.
- Accidentally ingested sensitive content: remove the affected chunk files,
  edit `INDEX.md`, and rotate any exposed credentials.

## Boundaries

- Do not ingest raw secrets, OAuth auth files, API tokens, or private keys.
- Do not add a vector database for the MVP.
- Do not hide wiki state behind an opaque service; markdown files are the
  source of truth.
- Do not make guest harnesses mutate shared wiki files without an explicit
  skill/tool action.
- Do not use wiki chunks as a substitute for `MATURANA.md`, `AGENTS.md`, or
  `SOUL.md`; those remain the agent contract and behavior files.
