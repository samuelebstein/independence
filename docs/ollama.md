# how to run a local model on your computer so you don't have to pay and give all your data to openai and anthropic

## what are ollama and qwen?

**ollama** is software that downloads and runs language models. it handles loading the model and gives you a chat interface in your terminal. it also offers cloud models, but we're not using those here. local models don't require an account or subscription and they run _ON_ your computer.

**qwen** is a family of models developed by alibaba's qwen team. the qwen3 models used here have downloadable weights released under apache 2.0. the weights are the numerical parameters learned during training.

ollama is the software running the model. qwen is the model. your mac's CPU/GPU does the computation.

## 1. install ollama

you need macos 14 or newer.

open "terminal" and run this:

```bash
curl -fsSL https://ollama.com/install.sh | sh
```

## 2. after the download, check the cli works

```bash
ollama --version
```

## 3. use ollama to download the model

```bash
ollama pull qwen3:8b
ollama run qwen3:8b
```

## 4. try it

at the chat prompt, try:

```text
explain how a diesel engine works. assume i'm curious but don't know much about engines. 
```

nice job. have fun.

if you want to exit, type:

```text
/bye
```

once the model is downloaded, you can turn off wi-fi and run it again:

```bash
ollama run qwen3:8b
```

you've proven the intelligence is on your computer. you downloaded the capability, not just an interface to a service.


# more stuff if you're curious

## save your session

while you're talking to a model, you can save the current conversation:

```text
/save diesel-engine-notes
```

ollama saves that conversation locally as a new model entry built on top of the model you're already using.

conceptually, it looks like this:

```text
qwen3:8b
├── model weights
└── base config

/save diesel-engine-notes

diesel-engine-notes
├── from: qwen3:8b
├── messages:
│   ├── user: "how does a diesel engine work?"
│   ├── assistant: "..."
│   ├── user: "how does internal combustion work?"
│   └── assistant: "..."
└── settings
```

when you load it again, ollama starts the model with those messages already included in its context:

```text
/load diesel-engine-notes
```

or:

```bash
ollama run diesel-engine-notes
```

you can see saved sessions/models with:

```bash
ollama list
```

and remove one with:

```bash
ollama rm diesel-engine-notes
```

## choosing a model

check your memory under apple menu → about this mac.

these are starting points for apple silicon macs:

| your mac's memory | model to try | approximate download |
| --- | --- | --- |
| 8 gb | `qwen3:1.7b` | 1.4 gb |
| 16 gb | `qwen3:8b` | 5.2 gb |
| 32 gb | `qwen3:14b` | 9.3 gb |
| 48 gb or more | `qwen3:30b-a3b` | 19 gb |

download size is not total memory usage. the running model needs additional memory, and macos and your other apps need room too. start smaller when in doubt.

### what do the names mean?

**1.7b, 8b, 14b:** roughly how many billions of parameters the model contains. bigger models generally know more and can do more, but need more memory and compute.

**30b-a3b:** a mixture-of-experts model. it has about 30 billion parameters overall, but only activates about 3 billion for each token it generates.

**thinking:** qwen3 can spend extra time reasoning before answering. adding `/no_think` tells it to skip that and answer more directly.

## useful ollama commands

```bash
# models you've downloaded
ollama list

# models currently running
ollama ps

# stop a model without deleting it
ollama stop qwen3:8b
```

## what this isn't yet

this downloads a model, not a browsable copy of wikipedia or a library of verified references. the model has knowledge encoded in its weights, but you can't inspect where every answer came from.

the next independence experiment is connecting the model to a local library: wikipedia, maps, textbooks, repair manuals, medical references, and other knowledge you actually have on your computer.

then instead of:

```text
question
  ↓
mysterious model
  ↓
answer
```

you could have:

```text
question
  ↓
local model
  ↓
local library
  ↓
retrieve relevant material
  ↓
answer + sources
```

## references

- [ollama mac installation guide](https://docs.ollama.com/macos)
- [ollama command reference](https://docs.ollama.com/cli)
- [qwen3 models on ollama](https://ollama.com/library/qwen3)
- [qwen3 introduction](https://qwenlm.github.io/blog/qwen3/)
```
