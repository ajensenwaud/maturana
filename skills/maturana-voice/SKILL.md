# maturana-voice

Use this skill when an agent needs speech: transcribe an audio file to text
(STT) or synthesize speech from text (TTS). It calls ElevenLabs or OpenAI audio
endpoints through the **pipelock proxy** (keys stay host-side), or a **local
Whisper** sidecar when audio must not leave the box. Audio files live under
`/workspace`.

## Grounding

1. Read `AGENTS.md` first; confirm the task is speech (STT/TTS), not text the
   agent should just write.
2. Confirm the agent opted in: `capabilities.voice: true` in `MATURANA.md`, plus
   the egress + injection block for the chosen provider:

```yaml
capabilities:
  voice: true
network:
  egress_allowlist: [api.openai.com, api.elevenlabs.io]
  proxy:
    inject_headers:
      - { host: api.openai.com, header: Authorization, source: "pipelock:openai/api-key", prefix: "Bearer " }
      - { host: api.elevenlabs.io, header: xi-api-key, source: "pipelock:elevenlabs/api-key" }
```

3. Confirm the relevant key (`pipelock:openai/api-key` or
   `pipelock:elevenlabs/api-key`) is set host-side.

## Preflight

- Confirm `HTTPS_PROXY` is exported in the guest so calls are key-injected; send
  NO auth header yourself.
- Confirm input/output audio paths are under `/workspace`.
- For STT, confirm the audio format is supported (wav/mp3/m4a) and within the
  provider's size limit.
- Confirm the content is appropriate to send to the chosen provider; for
  sensitive audio prefer the local Whisper sidecar (no egress).

## Decision Path

- Transcribe audio → text (cloud): OpenAI `/v1/audio/transcriptions` (whisper-1).
- Transcribe locally (no egress, privacy): the local Whisper sidecar
  (`whisper`/`faster-whisper` if installed in the guest).
- Synthesize natural speech: ElevenLabs `/v1/text-to-speech/<voice_id>` (best
  quality) or OpenAI `/v1/audio/speech`.
- Bulk or real-time streaming: out of scope here; report the limitation.

## Actions

STT (OpenAI):

```bash
curl -fsS https://api.openai.com/v1/audio/transcriptions \
  -F model=whisper-1 -F file=@/workspace/clip.wav
```

STT (local, no egress):

```bash
whisper /workspace/clip.wav --model base --output_dir /workspace --output_format txt
```

TTS (ElevenLabs → mp3 in /workspace):

```bash
curl -fsS -X POST https://api.elevenlabs.io/v1/text-to-speech/<VOICE_ID> \
  -H "content-type: application/json" \
  --data '{"text":"<TEXT>","model_id":"eleven_turbo_v2_5"}' \
  --output /workspace/speech.mp3
```

Report the transcript text or the saved audio path.

## Evidence

Before claiming success, collect:

- The HTTP call returned 200 (or the local tool exited 0).
- For STT: a non-empty transcript was produced and matches the audio's language.
- For TTS: the output file exists at `/workspace` with non-zero size and the
  expected audio container.
- For cloud calls, the proxy audit shows an allowed provider host and no key
  appears in the guest env, command line, or transcript.

## Recovery

- `401`/`403`: the provider key injection is missing — set the pipelock secret
  and confirm `inject_headers`, then retry.
- Connection refused / proxy denial: the provider host isn't allowlisted — add
  it and re-apply the spec.
- Local `whisper` not found: it isn't installed in the guest — fall back to the
  cloud STT path or report it.
- Garbled transcript: try a larger Whisper model or confirm the audio sample
  rate; for TTS, shorten the text or pick a different voice.

## Boundaries

- Do not place provider keys in the prompt, command line, saved files, or any
  guest file — the proxy injects them.
- Do not send private or sensitive audio to a cloud provider when the local
  Whisper path is available and appropriate.
- Do not reach a voice provider outside the pipelock proxy / egress allowlist.
- Do not write audio or transcripts outside `/workspace`.
