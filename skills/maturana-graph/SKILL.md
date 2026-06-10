# maturana-graph

Use this skill when an agent should read from or write to **MaturanaGraph** — the
shared knowledge graph queried with **GraphRAG**. It is the structured,
multi-hop counterpart to the plain `maturana-wiki` markdown store: entities and
relationships the agent can traverse and reason over, not just keyword-matched
chunks.

MaturanaGraph is a from-scratch property-graph engine that runs as a host
service. The host never calls a model — **all extraction and embedding happen
inside the agent VM**. The agent turns text into entities and vectors; the graph
service only stores them and does graph + vector math.

## Grounding

1. Read `AGENTS.md` and `MATURANA.md` first; confirm `knowledge_graph.enabled`.
2. Decide whether the request is **retrieve** (answer using existing knowledge),
   **ingest** (record new facts/relationships), or **debug** (inspect state).
3. Confirm the graph is wired for this agent by checking the environment:
   - `MATURANA_GRAPH_URL` — service base URL (resolved to the host gateway,
     e.g. `http://172.30.10.9:47835`).
   - `MATURANA_GRAPH_TOKEN` — bearer token sent as `x-maturana-graph-token`.
   - `MATURANA_GRAPH_NAME` — the named graph this agent reads/writes
     (`personal`, `team`, or the agent id); multiple agents can share one name.
4. If any are unset, the graph is not enabled — fall back to `maturana-wiki` and
   do not attempt graph calls. Enable it with
   `knowledge_graph: { enabled: true, graph: <name> }` in the agent's `MATURANA.md`.

## Preflight

- Health-check before relying on the service:
  `curl -fsS "$MATURANA_GRAPH_URL/health"` returns `{"ok":true}`.
- Confirm extraction and embedding will run **in this VM** (harness LLM +
  embeddings API over the governed egress), never on the host.
- For retrieval, confirm the query embedding uses the **same model and
  dimension** as the embeddings stored at ingest time.
- For ingest, scan the source for secrets, OAuth state, private keys, and raw
  tokens before writing — the named graph is visible to every agent that
  addresses it.
- Confirm node ids are stable strings, so re-upserting the same id updates in
  place rather than duplicating.

## Decision Path

- Question answerable from prior knowledge: **retrieve** with `/graph/query`.
- New freeform facts the agent extracted: **ingest** entities + edges with
  `/graph/upsert`.
- Whole document (PDF/PPTX/DOCX/MD/TXT/HTML): use the host-side
  `maturana graph ingest` parser instead of hand-building nodes.
- Empty or thin retrieval: broaden `query_terms`, raise `depth`/`k`, or verify
  the knowledge exists via `/graph/stats`.
- Graph not enabled (env unset): fall back to `maturana-wiki`.
- Sensitive knowledge: write it to an agent-private graph name, never a shared
  one.

## Actions

Retrieve (GraphRAG) — embed the question in the VM, then query. Send
`query_embedding` (vector search), `query_terms` (keyword seeds), or both;
`depth` controls hop expansion. Read `result.rendered_context` and reason over
it, citing entities by name (`result.subgraph` and `result.scored` hold the raw
nodes/edges and per-node scores):

```bash
curl -fsS -X POST "$MATURANA_GRAPH_URL/graph/query" \
  -H "content-type: application/json" \
  -H "x-maturana-graph-token: $MATURANA_GRAPH_TOKEN" \
  --data "$(jq -nc --arg g "$MATURANA_GRAPH_NAME" --argjson emb "$QUERY_EMBEDDING_JSON" \
    '{graph:$g, query_terms:["roadmap","q3"], query_embedding:$emb, k:8, depth:2, max_nodes:60}')"
```

Ingest extracted knowledge — upsert nodes (ids you choose, text in `props` as
`text`/`summary`/`description`, optional `embedding` float array) and edges that
connect node ids by `type`:

```bash
curl -fsS -X POST "$MATURANA_GRAPH_URL/graph/upsert" \
  -H "content-type: application/json" \
  -H "x-maturana-graph-token: $MATURANA_GRAPH_TOKEN" \
  --data "$(jq -nc --arg g "$MATURANA_GRAPH_NAME" '{
    graph:$g,
    nodes:[
      {id:"person:ada",   labels:["Person"],  props:{name:"Ada",   summary:"Lead engineer"}},
      {id:"project:atlas", labels:["Project"], props:{name:"Atlas", summary:"Graph rollout"}}
    ],
    edges:[ {id:"e1", from:"person:ada", to:"project:atlas", type:"LEADS"} ]
  }')"
```

Ingest documents — parse, chunk, and structure a file or directory host-side
(`Document`→`Chunk` with `CONTAINS`/`NEXT` edges), immediately keyword-searchable:

```bash
maturana graph ingest ./docs/handbook.pdf --graph "$MATURANA_GRAPH_NAME"
maturana graph ingest ./docs/ --graph "$MATURANA_GRAPH_NAME" --recursive
```

Inspect — `POST /graph/stats` with `{graph:"<name>"}` returns node/edge counts.

## Evidence

Before claiming success, collect:

- `/health` returns `{"ok":true}` and the upsert/query call returned HTTP 200.
- After an upsert, `POST /graph/stats` (`{graph:<name>}`) shows node and edge
  counts increased by the expected amounts.
- A retrieval for representative terms returns a non-empty `rendered_context`
  that names the expected entities and relationships.
- For document ingest, `maturana graph ingest` reports the chunk count and a
  follow-up query surfaces text from those chunks.
- Vector retrieval ranks the semantically-closest node first when a
  `query_embedding` is supplied (not just keyword matches).

## Recovery

- `401 unauthorized`: the token header is missing or wrong — re-read
  `MATURANA_GRAPH_TOKEN`; every path except `/health` requires it.
- `400 invalid graph name`: names are `[A-Za-z0-9._-]`, ≤128 chars, no `..` —
  pass `MATURANA_GRAPH_NAME` verbatim.
- Empty `rendered_context`: broaden `query_terms` to concrete nouns, raise
  `depth`/`k`, or confirm the knowledge was ingested via `/graph/stats`.
- Vector search misses: the query embedding must use the same model and
  dimension as the stored node embeddings; a mismatch makes cosine meaningless.
- Connection refused: the host graph service is not running — it is opt-in and
  supervised only when a graph token exists on the host.

## Boundaries

- Do not ask the host to call a model; entity extraction and embedding stay in
  the VM, and only finished nodes/edges/vectors are sent to the service.
- Do not ingest secrets, OAuth state, private keys, or raw API tokens into the
  graph.
- Do not write sensitive knowledge to a shared graph name (`personal`, `team`);
  anything there is readable by every agent that addresses that name — scope it
  to an agent-private graph instead.
- Do not treat the graph as a replacement for `MATURANA.md`, `AGENTS.md`, or
  `SOUL.md`; those remain the agent contract and behavior files.
- Do not mix embedding models or dimensions across ingest and query; the engine
  does exact cosine and will not reconcile mismatched vector spaces.
