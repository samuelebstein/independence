# Raspberry Pi 5: text-only local Independence

This is one complete installation path for running Independence with a
keyboard, terminal text output, a local Ollama server, and a small local text
model. It intentionally does **not** set up a microphone, speaker, camera,
Piper, HAL voice files, or Whisper weights.

## Reported working configuration

These are the reported working conditions for this guide, not universal
minimum requirements or a guarantee for every Raspberry Pi installation.

| Item | Reported working value |
| --- | --- |
| Computer | Raspberry Pi 5 with 4 GiB RAM and an approximately 32 GB microSD card |
| Operating system | Debian GNU/Linux 13 (trixie), ARM64/aarch64 |
| Kernel | `6.18.50+rpt-rpi-2712` |
| Independence revision | `156c506` |
| Rust compiler | `rustc 1.99.1 (58cf8f9c3 2026-10-13)` |
| Rust build tool | `cargo 1.99.1 (5f94df478 2026-08-27)` |
| Ollama | `0.40.10` |
| Text model | `gemma3:1b`, reported ID `97558784bdc9`, recorded download size 806 MB |

The recorded model download size is not its total memory requirement while it
is loaded and answering. This guide has not been run as a fresh-card acceptance
test; it records the successful configuration above and the current source
configuration at revision `156c506`.

## 1. Install system prerequisites

Run these commands in the Pi terminal. `sudo apt update` refreshes APT's
catalog of available packages. It does **not** upgrade the whole operating
system. The one `apt install` command then installs the complete known native
build prerequisite set before the first Cargo build.

```bash
sudo apt update
sudo apt install \
  git curl ca-certificates \
  build-essential pkg-config cmake \
  clang libclang-dev libasound2-dev \
  zstd
```

The installation changes system software and may download packages. Each item
is present for a specific reason:

- `git` obtains the repository and records which committed revision is checked
  out.
- `curl` downloads the Rust and Ollama installers.
- `ca-certificates` provides the trusted certificate store used to verify those
  HTTPS downloads.
- `build-essential` provides the standard C/C++ compilers, `make`, and
  development files used to build and link native dependencies.
- `pkg-config` tells Rust build scripts where installed native libraries live
  and which compiler/linker settings they require.
- `cmake` configures and coordinates the bundled whisper.cpp native build.
- `clang` installs C/C++ compiler tools alongside the binding toolchain.
- `libclang-dev` supplies the libclang library that `bindgen` uses to generate
  Rust declarations from C/C++ headers.
- `libasound2-dev` provides the ALSA development files used by the Linux audio
  dependencies, even though this text-only run will not open an audio device.
- `zstd` extracts the compressed Ollama distribution used by its Linux
  installer.

The demonstrated binding requirement is **libclang**. The standalone `clang`
compiler is installed as part of the coherent toolchain, but this guide does
not claim that it necessarily compiles whisper.cpp; the native build may use
GCC from `build-essential` instead. This is the consolidated working setup,
not a claim that every package is part of a mathematically minimal install.

## 2. Install Rust and rustfmt

Rustup is Rust's per-user toolchain manager. It installs the Rust compiler
(`rustc`), Cargo, and optional tools under your home directory. Run it as your
normal user—**not** with `sudo`.

The checked-out Independence revision has no `rust-toolchain.toml` or
`rust-toolchain` file that selects a repository-specific version. To reproduce
the reported configuration, install the explicit tested `1.99.1` toolchain and
add `rustfmt` before building:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt
rustup default 1.99.1
rustc --version
cargo --version
rustfmt --version
```

The first command downloads and runs Rust's official rustup installer, without
selecting a moving default toolchain. The line beginning with `.` loads
`$HOME/.cargo/env` into this terminal session; it adds Rustup's user-local
`$HOME/.cargo/bin` directory to `PATH` so the next commands can find `rustup`,
`rustc`, Cargo, and rustfmt. `rustup toolchain install` downloads exactly
`1.99.1` and its `rustfmt` component. `rustup default` changes the normal
user's default Rust toolchain to that explicit version. The three version
commands should report Rust/Cargo 1.99.1 and a rustfmt version.

`rustc` compiles Rust source, Cargo resolves/builds the package and its locked
dependencies, and rustfmt formats Rust source. A moving `stable` download is
not described here as a way to guarantee the recorded 1.99.1 version.

## 3. Clone Independence

Create a conventional code directory, clone committed source from GitHub, and
select the recorded working revision:

```bash
mkdir -p ~/code
git clone https://github.com/samuelebstein/independence.git \
  ~/code/independence
cd ~/code/independence
git checkout 156c506
git rev-parse --short HEAD
```

`mkdir -p` creates `~/code` if it does not already exist. `git clone` retrieves
the repository's committed code and history into `~/code/independence`; it does
not install system packages. `cd` makes that repository the working directory.
`git checkout 156c506` selects the reported working commit, and
`git rev-parse --short HEAD` should print `156c506`. Keep this directory as the
place to run the remaining commands.

## 4. Build Independence

From `~/code/independence`, build the optimized release executable:

```bash
cargo build --release --locked -j 1
```

`--release` builds optimized code. `--locked` tells Cargo to preserve the
dependency resolution recorded in `Cargo.lock`, rather than updating it.
`-j 1` limits Cargo to one build job at a time; it can lower peak contention on
a small Pi, but it is not a hard memory limit. Cargo may download source
dependencies during this first build.

The resulting executable is:

```text
target/release/independence
```

`Cargo.toml` states the package and direct dependency requirements;
`Cargo.lock` records the exact resolved dependency graph used by this revision.
`cargo run` builds if necessary and then launches the program. Launching
`./target/release/independence` directly runs the already-built executable and
does not start another build.

The build compiles the Rust integrations for audio and local Whisper/native
code. That does not download Whisper speech-model weights, require Piper, or
need a physical audio device for the text-only run below.

## 5. Install and start Ollama

Ollama is separate system software that serves the local model. Unlike Rustup,
which installs under the current user's home directory, Ollama's Linux
installer places system software and creates its service. The
[official Linux documentation](https://docs.ollama.com/linux#installing-specific-versions)
documents `OLLAMA_VERSION` for selecting a specific release. Use it here so
the command requests the tested `0.40.10`, rather than the installer’s current
latest release:

```bash
curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.10 sh
sudo systemctl enable --now ollama
systemctl is-active ollama
ollama --version
```

The installer downloads and installs Ollama and may request elevated access for
system locations. `sudo systemctl enable --now ollama` starts the Ollama system
service now and enables it to start at boot. `systemctl is-active ollama` should
print `active`. `ollama --version` should report `0.40.10`. Independence uses
the service through its existing local default endpoint; do not also start a
separate `ollama serve` process for this guide.

## 6. Download and test the text model

Ollama is the inference software. Gemma is the separately downloaded model
that Ollama will run. Download the recorded small text model, inspect the local
catalog, then ask it one direct test question:

```bash
ollama pull gemma3:1b
ollama list
ollama run gemma3:1b "Say hello in one short sentence."
```

`ollama pull` downloads `gemma3:1b`; the reported successful download was
806 MB. `ollama list` should show the local model, with reported ID
`97558784bdc9` in the recorded configuration. `ollama run` loads the model and
prints a short local response. This is the first command in the guide that
performs model inference.

## 7. Run Independence

Return to the repository root if needed, then launch the release binary with a
one-command model override:

```bash
OLLAMA_MODEL=gemma3:1b ./target/release/independence
```

The equivalent development command is:

```bash
OLLAMA_MODEL=gemma3:1b cargo run --release --locked -j 1
```

`OLLAMA_MODEL=gemma3:1b` applies only to that one launch. It does not change
Independence's source default, which remains `qwen3-vl:4b-instruct`, or change
Ollama's global configuration. The application uses its existing local Ollama
endpoint default.

At the Independence prompt, try a typed question and then a follow-up:

```text
What is the primary gas in Earth's atmosphere?
Why is it more common than oxygen?
/quit
```

The first two lines should receive local text answers using the same in-memory
conversation. `/quit` is the application’s supported normal exit command.

## Verification boundary

This guide was checked against the current repository configuration at
`156c506`: its `OLLAMA_MODEL` override, default local endpoint, release build
path, and `/quit` command. The documented Ollama version-selection mechanism
was checked against the official Linux documentation. It was not executed as a
fresh Raspberry Pi installation during this documentation change.

A future clean-install acceptance test is simple: follow these steps in order,
receive a local answer from Independence, ask the follow-up, and exit normally
with `/quit`.
