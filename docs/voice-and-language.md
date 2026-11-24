# Voice and language

Independence speaks with one Piper voice and listens with one Whisper model.
Both can be swapped at launch, which is enough to practise another language
(for example Spanish) entirely offline. Downloading the files is a one-time
setup step; nothing goes online while Independence runs (a voice that would
make Piper download something while speaking is refused; see the Chinese
pinyin voices below).

## The three settings

| What | Flag (`--voice …`) | Environment variable | Default |
| --- | --- | --- | --- |
| Piper voice | `--tts-model <file.onnx>` | `INDEPENDENCE_TTS_MODEL` (and optionally `INDEPENDENCE_TTS_CONFIG`) | `local-data/piper/hal/hal.onnx` |
| Whisper model | `--whisper-model <file.bin>` | `INDEPENDENCE_WHISPER_MODEL` | `local-data/whisper/ggml-base.en.bin` |
| Whisper language | `--whisper-language <code>` | `INDEPENDENCE_WHISPER_LANGUAGE` | `en` |

- A flag wins over its environment variable.
- A voice's config is the same path plus `.json`. With `--tts-model`, that
  config is always used, so a leftover `INDEPENDENCE_TTS_CONFIG` for another
  voice cannot be paired with it.
- The language is a code such as `en` or `es` (a full name like `spanish`
  also works), or `auto` to detect the language of each recording.
  whisper.cpp decides which languages exist; an unknown one stops the launch.
- Any language other than `en` needs a **multilingual** Whisper model
  (`ggml-base.bin`). The English-only models are named `*.en.bin`; pairing one
  with another language stops the launch with a message saying so, instead of
  quietly transcribing your Spanish as English.

These settings apply to `--voice`. Typed mode and `--dictate` are unchanged.

## Spanish setup

Download once, on the machine that will run Independence:

```bash
cd ~/code/independence

# A Spanish Piper voice (Mexican Spanish, high quality).
mkdir -p local-data/piper/es_MX-claude-high
curl -fL -o local-data/piper/es_MX-claude-high/es_MX-claude-high.onnx \
  https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_MX/claude/high/es_MX-claude-high.onnx
curl -fL -o local-data/piper/es_MX-claude-high/es_MX-claude-high.onnx.json \
  https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_MX/claude/high/es_MX-claude-high.onnx.json

# The multilingual Whisper model (same size and speed class as base.en).
curl -fL -o local-data/whisper/ggml-base.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin
```

Hear the voice before using it (`--length-scale` above 1 is slower):

```bash
echo "Hola. ¿Cómo estás?" | piper -m local-data/piper/es_MX-claude-high/es_MX-claude-high.onnx -f hola.wav && aplay hola.wav
```

Other Spanish voices in the same collection: `es_ES` davefx and sharvard
(Spain, medium), `es_AR` daniela (Argentina, high), `es_MX` ald (medium).
Each voice has its own license in its model card.

### Laptop

```bash
cargo run -- --voice \
  --tts-model local-data/piper/es_MX-claude-high/es_MX-claude-high.onnx \
  --whisper-model local-data/whisper/ggml-base.bin \
  --whisper-language auto
```

### Raspberry Pi (Hailo)

The launcher reads the same environment variables:

```bash
cd ~/code/independence
INDEPENDENCE_TTS_MODEL=$PWD/local-data/piper/es_MX-claude-high/es_MX-claude-high.onnx \
INDEPENDENCE_WHISPER_MODEL=$PWD/local-data/whisper/ggml-base.bin \
INDEPENDENCE_WHISPER_LANGUAGE=auto \
scripts/run-raspberry-pi-hailo.sh --check
```

Drop `--check` to start. `--check` runs every setup check (files, Piper,
Whisper model and language, button device, camera, bridge port), lists every
problem at once, and starts nothing.

## Chinese (Mandarin) setup

Whisper's multilingual model already understands Mandarin (`zh`); only the
voice is new. Piper has three Mandarin voices, all `zh_CN`:

| Voice | Pronunciation | Extra download |
| --- | --- | --- |
| `huayan` (medium, x_low) | espeak-ng (`cmn`) | none |
| `chaowen` (medium) | pinyin, via the g2pW model | g2pW, once |
| `xiao_ya` (medium) | pinyin, via the g2pW model | g2pW, once |

**Start with `huayan`.** It works like the HAL and Spanish voices:

```bash
cd ~/code/independence
mkdir -p local-data/piper/zh_CN-huayan-medium
curl -fL -o local-data/piper/zh_CN-huayan-medium/zh_CN-huayan-medium.onnx \
  https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/huayan/medium/zh_CN-huayan-medium.onnx
curl -fL -o local-data/piper/zh_CN-huayan-medium/zh_CN-huayan-medium.onnx.json \
  https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/huayan/medium/zh_CN-huayan-medium.onnx.json
# (ggml-base.bin from the Spanish setup above is also the Chinese Whisper model.)

cargo run -- --voice \
  --tts-model local-data/piper/zh_CN-huayan-medium/zh_CN-huayan-medium.onnx \
  --whisper-model local-data/whisper/ggml-base.bin \
  --whisper-language zh
```

On the Pi, use `INDEPENDENCE_TTS_MODEL`, `INDEPENDENCE_WHISPER_MODEL`, and
`INDEPENDENCE_WHISPER_LANGUAGE=zh` with `scripts/run-raspberry-pi-hailo.sh`,
as in the Spanish example.

### The pinyin voices and g2pW

`chaowen` and `xiao_ya` turn characters into pinyin with g2pW, a separate
model. **If g2pW is missing, Piper downloads it from the internet the first
time the voice speaks.** Independence does not allow that: it runs Piper in
the voice's own folder and refuses to start with a pinyin voice unless
`g2pW/g2pw.onnx` is already there. Download it once, next to the voice:

```bash
voice=local-data/piper/zh_CN-chaowen-medium
mkdir -p "$voice/g2pW"
curl -fL -o "$voice/zh_CN-chaowen-medium.onnx" \
  https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/chaowen/medium/zh_CN-chaowen-medium.onnx
curl -fL -o "$voice/zh_CN-chaowen-medium.onnx.json" \
  https://huggingface.co/rhasspy/piper-voices/resolve/main/zh/zh_CN/chaowen/medium/zh_CN-chaowen-medium.onnx.json
curl -fL "https://huggingface.co/datasets/rhasspy/piper-checkpoints/resolve/main/zh/zh_CN/_resources/g2pw.tar.gz?download=true" |
  tar -xz -C "$voice/g2pW"
test -f "$voice/g2pW/g2pw.onnx" && echo "g2pW ready"
```

Caveats, not yet measured:

- Independence starts Piper once per sentence, so a pinyin voice also loads
  g2pW once per sentence. Expect it to be noticeably slower than `huayan`,
  especially on the Pi.
- Pinyin voices need a Piper new enough to know them (the laptop's 1.8.0
  does). Check the Pi's Piper with:
  `~/.local/share/independence/piper-venv/bin/python -c "from piper.config import PhonemeType; print(PhonemeType.PINYIN)"`

### Chinese text in Independence

- Answers are split into sentences at `。`, `！`, and `？` (no space needed),
  so Chinese answers stream sentence by sentence like English ones.
- Whisper sometimes writes Traditional characters even for Mandarin speech.
  The model reads both.

## Choosing `es` or `auto`

- `es`: best for Spanish-only practice. English you speak is forced into
  Spanish words.
- `auto`: Whisper detects the language of each recording, so you can ask one
  question in English and practise the next in Spanish. A single recording
  that mixes both languages may still come out garbled.

## What this does not do (yet)

- **The model is not told to tutor or to answer in Spanish.** Ask it ("From
  now on, answer only in Spanish."). Voice keeps the last six exchanges, so
  repeat the request after that.
- **One voice per run.** A Spanish voice reads any English answer with a
  Spanish accent; HAL reads Spanish badly.
- **Whisper corrects toward likely words.** It shows what you probably meant,
  so it cannot give pronunciation feedback.
- Multilingual `base` is slightly less accurate on English than `base.en`;
  `ggml-small.bin` is more accurate and several times slower (not measured on
  the Pi).
