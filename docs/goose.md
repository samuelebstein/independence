# goose setup instructions

**goose is similar to claude and codex in functionality but differs as it is open source, allowing transparency and community contributions.**

## check ollama status and configuration
```bash
ollama ps
```
if not running, go to [ollama.md](ollama.md) for setup instructions.

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

### 4. navigate to your workspace
```bash
cd /path/to/your/project
```
this directory will be your workspace for all goose operations.

### 5. start a goose session
```bash
goose session
```

## references

- [goose-docs.ai](https://goose-docs.ai/) contains tutorials on:
  - installing and configuring goose
  - using ollama with goose
  - advanced session management
  - troubleshooting common setup issues

