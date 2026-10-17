//! Terminal output for the prototype's `speak` capability.

use crate::protocol::Output;

pub const HELP: &str = r#"Independence local observation prototype

Type a question to ask the local Ollama model.
/look <path> | <question>  load one explicit JPEG/PNG and ask about it
/stop                      cancel the active application turn
/reset                     cancel active work and clear accepted scene/history
/help                      show this help
/quit                      exit

Paths are literal text, never shell commands. Relative paths resolve from the
directory where Independence was started; spaces are allowed. `|` separates a
path from its question, so a path containing `|` is not supported.

Only the last successfully accepted scene is used for follow-ups. A /look
photo replaces that scene only after its answer succeeds. Photos must be JPEG
or PNG and at most 10 MiB. History is in memory only and caps at six accepted
exchanges; use /reset or /look to begin a new scene.

This prototype has keyboard input and terminal output only: no microphone,
camera, speech synthesis, persistent memory, streaming, or autonomous tools.
/stop cancels this application's request task, but cannot prove that a
separate Ollama server has stopped its own computation immediately.
"#;

pub fn welcome() {
    eprintln!("Independence is ready. Type /help for commands.");
}

pub fn emit(output: Output) {
    match output {
        Output::Answer(answer) => println!("independence: {answer}"),
        Output::Diagnostic(message) => eprintln!("independence: {message}"),
        Output::Help => println!("{HELP}"),
    }
}
