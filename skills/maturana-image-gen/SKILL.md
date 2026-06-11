# maturana-image-gen

Use this skill when an agent needs to generate an image from a text prompt —
illustrations, diagrams-as-images, mockups, social assets. It calls a
contemporary image model (OpenAI `gpt-image-1`, DALL·E 3 as fallback) through
the **pipelock proxy**, so the API key stays host-side and never enters the
guest. Generated images are written to `/workspace`.

## Grounding

1. Read `AGENTS.md` first; confirm the request is for a generated image (not a
   web search for an existing one, not a chart the agent could draw in code).
2. Confirm the agent opted in: `capabilities.image_gen: true` in `MATURANA.md`
   and the egress + injection block:

```yaml
capabilities:
  image_gen: true
network:
  egress_allowlist: [api.openai.com]
  proxy:
    inject_headers:
      - { host: api.openai.com, header: Authorization, source: "pipelock:openai/api-key", prefix: "Bearer " }
```

3. Confirm `pipelock:openai/api-key` is set host-side.

## Preflight

- Confirm `HTTPS_PROXY` is exported in the guest (the worker env sets it) so the
  call is injected with the key — send NO Authorization header yourself.
- Confirm the output directory is `/workspace` (the writable mount the host can
  retrieve from).
- Confirm the prompt has no personal data, secrets, or another person's likeness
  you lack consent for.
- Confirm the requested size/format is one the model supports (1024x1024 etc.).

## Decision Path

- General image from a prompt: `gpt-image-1` via `/v1/images/generations`.
- Need a specific older style or `gpt-image-1` unavailable: fall back to
  `dall-e-3`.
- Editing an existing image / inpainting: use the images **edits** endpoint with
  the source file, not generations.
- The user wants a chart/plot of data: stop — draw it in code (matplotlib etc.),
  don't image-generate it.

## Actions

Generate and save (the proxy injects the key; b64 is decoded to a PNG):

```bash
curl -fsS https://api.openai.com/v1/images/generations \
  -H "content-type: application/json" \
  --data '{"model":"gpt-image-1","prompt":"<PROMPT>","size":"1024x1024","n":1}' \
  | python3 -c 'import sys,json,base64; d=json.load(sys.stdin); open("/workspace/out.png","wb").write(base64.b64decode(d["data"][0]["b64_json"]))'
```

Report the saved path (`/workspace/out.png`) and a one-line description of what
was generated.

## Evidence

Before claiming success, collect:

- The HTTP call returned 200 and a `data[0].b64_json` (or `url`) payload.
- The decoded file exists at the `/workspace` path with a non-zero size and a
  PNG signature.
- The proxy audit (`.maturana/audit/<id>-pipelock-proxy.jsonl`) shows an allowed
  `api.openai.com` request.
- No API key appears in the guest env, command line, transcript, or the saved
  file's metadata.

## Recovery

- `401`/`403`: the key injection is missing — set `pipelock:openai/api-key`
  host-side and confirm the spec `inject_headers` (Bearer prefix), then retry.
- Connection refused / proxy denial: `api.openai.com` is not allowlisted — add
  it and re-apply the spec.
- `400 content_policy`/moderation: rephrase the prompt to comply; do not retry
  the same prompt repeatedly.
- `429`: back off and retry once; if persistent, report the rate limit rather
  than hammering.

## Boundaries

- Do not put the API key in the prompt, the saved file, the command line, or any
  guest file — the proxy injects it.
- Do not generate images of real identifiable people without explicit consent,
  nor sexual, violent, or deceptive content.
- Do not bypass the pipelock proxy / egress allowlist to reach the image API
  directly.
- Do not write generated images outside `/workspace`.
