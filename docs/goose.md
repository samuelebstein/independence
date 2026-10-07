# how to run an open source ai agent so you don't have to use claude or codex

## what is goose?

**goose** is an open source ai agent that runs on your computer. it is the harness around the model: it gives an llm access to tools like your filesystem and shell so the model can actually read files, edit code, and run commands instead of just responding with text.

we're going to use goose with **ollama + qwen**, so the agent and the model can both run locally.

## 1. make sure ollama works

```bash
ollama ps
```

if you haven't set up ollama yet, see [ollama.md](ollama.md).

## 2. install goose

```bash
curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh | bash
```

check that it worked:

```bash
goose --version
```

## 3. connect goose to your local model

```bash
goose configure
```

choose:

```text
manual configuration
→ configure providers
→ ollama
```

use:

```text
host: http://localhost:11434
model: qwen3:8b
```

you do not need an api key for your local ollama server.

## 4. use it inside a project

```bash
cd /path/to/your/project
goose session
```

then try something simple:

```text
read README.md and summarize it
```

or:

```text
read README.md and fix obvious spelling mistakes. don't change anything else.
```

goose can now ask qwen what to do and use tools on your computer to actually do it.

conceptually:

```text
you
 ↓
goose
 ↓
qwen
 ↓
ollama
 ↓
your computer

goose gives the model access to:
├── files
├── shell
├── code
└── other tools/extensions
```

## a warning

local models are much less capable than claude or codex at agentic work right now. `qwen3:8b` can also be pretty slow, especially if ollama is running it on your cpu.

this is mostly an experiment to see what it feels like to have the entire agent stack running locally.

## references
[goose repository](https://github.com/aaif-goose/goose)  
[goose installation guide](https://github.com/aaif-goose/goose/blob/main/documentation/docs/getting-started/installation.md)  
[goose provider configuration](https://github.com/aaif-goose/goose/blob/main/documentation/docs/getting-started/providers.md)  
[goose quickstart](https://github.com/aaif-goose/goose/blob/main/documentation/docs/quickstart.md)
