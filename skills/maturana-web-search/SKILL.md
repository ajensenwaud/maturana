# maturana-web-search

Use this skill when an agent needs current information from the public web —
facts newer than its training data, live documentation, prices, news, or
anything the knowledge graph and wiki do not hold. It searches via **Brave
Search** or **Tavily**; API keys stay on the host and are injected by the
pipelock proxy, never entering the guest.

## Grounding

1. Read `AGENTS.md` first; confirm the request actually needs live web data
   (prefer the knowledge graph / wiki for ingested or durable knowledge).
2. Check the environment: search from a guest requires the pipelock proxy
   (`HTTPS_PROXY` set by the worker env) and the search hosts in the agent's
   `network.egress_allowlist`.
3. On the host (operator paths), `maturana search "query" --provider
   brave|tavily` resolves keys from pipelock directly.

## Preflight

- Confirm the agent spec allowlists the provider host
  (`api.search.brave.com` and/or `api.tavily.com`) and injects its key:

```yaml
network:
  egress_allowlist:
    - api.search.brave.com
    - api.tavily.com
  proxy:
    inject_headers:
      - { host: api.search.brave.com, header: X-Subscription-Token, source: "pipelock:brave/api-key" }
      - { host: api.tavily.com, header: Authorization, source: "pipelock:tavily/api-key", prefix: "Bearer " }
```

- Confirm the keys exist on the host: `maturana pipelock set brave/api-key
  <key>` / `maturana pipelock set tavily/api-key <key>`.
- Confirm the query contains concrete terms; do not search for secrets,
  personal identifiers, or anything from the agent's private context.

## Decision Path

- Need ranked web results with snippets: use **Brave**.
- Need answer-oriented research extracts: use **Tavily**.
- Provider returns 401/403: the key injection is missing — fix the spec
  block and host secret, do not paste keys into the guest.
- Information already ingested into the knowledge graph: query the graph
  (maturana-graph skill) instead of the web.
- Page content needed (not just snippets): follow up with the
  maturana-browse skill on an allowlisted host.

## Actions

From the guest, call the APIs through the proxy. The proxy injects the keys —
send **no** Authorization or token header yourself:

```bash
curl -fsS "https://api.search.brave.com/res/v1/web/search?q=rust+axum&count=5" \
  -H "Accept: application/json"
```

```bash
curl -fsS -X POST "https://api.tavily.com/search" \
  -H "content-type: application/json" \
  --data '{"query": "rust axum websocket tutorial", "max_results": 5}'
```

From the host (operator):

```bash
maturana search "rust axum websocket tutorial" --provider brave --count 5
maturana search "agent orchestration platforms" --provider tavily --json
```

Read `web.results[].{title,url,description}` (Brave) or
`results[].{title,url,content}` (Tavily) and cite the URLs you used.

## Evidence

Before claiming success, collect:

- The HTTP call returned 200 with a parseable JSON body.
- At least one result with a non-empty title and URL was extracted, or the
  empty result set was reported honestly.
- The pipelock proxy audit log (`.maturana/audit/pipelock-proxy.jsonl`)
  shows the request was allowed to the provider host.
- No API key appears anywhere in the guest: not in env, not in the command
  line, not in the transcript.

## Recovery

- `401`/`403`: key missing or wrong — set `brave/api-key` / `tavily/api-key`
  in pipelock on the host and confirm the spec's `inject_headers` block
  (Tavily needs `prefix: "Bearer "`).
- Connection refused / proxy denial: the provider host is not in
  `network.egress_allowlist` — add it and re-apply the spec.
- `429`: rate limited — back off, lower `count`, or switch provider.
- Empty results: broaden or rephrase the query with concrete nouns; try the
  other provider before concluding the information does not exist.

## Boundaries

- Do not place API keys in the guest: no env vars, no headers typed by the
  agent, no keys in prompts or transcripts — the proxy injects them.
- Do not search for secrets, credentials, or private personal data, and do
  not paste private context into a web search query.
- Do not bypass the pipelock proxy or the egress allowlist to reach a search
  provider directly.
- Do not treat search snippets as ground truth for high-stakes claims;
  fetch and read the source (maturana-browse) before relying on it.
