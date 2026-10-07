# goose 
"goose is a general-purpose AI agent that runs on your machine. Not just for code — use it for research, writing, automation, data analysis, or anything you need to get done."

importantly, goose is open source aka not claude or codex!

## setup instructions

### 1. install goose cli (linux/mac)
```bash
curl -fsSL https://goose.sh/install | bash
```

### 2. verify installation
```bash
goose --version
```

### 3. configure goose for local ollama
```bash
goose configure
```

when prompted:
- choose "manual configuration"
- select "ollama" as the provider
- enter host: `http://localhost:11434` (press enter for default)
- select model: `qwen3:8b`

### 4. check ollama status and configuration
```bash
ollama ps
```
if not running, go to ollama.md for setup instructions.

### 7. start a goose session
```bash
goose session
```
