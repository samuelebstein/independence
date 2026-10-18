//! Terminal output for the prototype's `speak` capability.

use crate::protocol::Output;

pub const HELP: &str = r#"Independence local observation prototype

Type a question to ask the local Ollama model.
/look <path> | <question>  load one explicit JPEG/PNG and ask about it
/cameras                    list usable local V4L2 camera identifiers
/camera | <question>        capture from the only listed camera and ask
/camera <id> | <question>   capture from one explicitly selected camera
/stop                      cancel the active application turn
/reset                     cancel active work and clear accepted scene/history
/help                      show this help
/quit                      exit

Paths are literal text, never shell commands. Relative paths resolve from the
directory where Independence was started; spaces are allowed. `|` separates a
path from its question, so a path containing `|` is not supported.

Only the last successfully accepted scene is used for follow-ups. A /look or
/camera photo replaces that scene only after its answer succeeds. Camera
selection is checked before it supersedes active work. If multiple cameras are
listed, /camera requires an ID; it never guesses a /dev/video node. A camera
capture briefly opens the selected device, discards up to two warm-up frames,
keeps one validated JPEG in memory, then releases the device before Ollama is
contacted. Photos must be JPEG or PNG and at most 10 MiB. History is in memory
only and caps at six accepted exchanges; use /reset or /look to begin a new
scene.

Camera captures are not saved by Independence, but the original source file
for /look remains where it was.
This prototype has keyboard input and terminal output only: no microphone,
speech synthesis, persistent memory, continuous vision, streaming, or
autonomous tools. /stop cancels this application's request task, but cannot
prove that a separate Ollama server has stopped its own computation immediately.
"#;

pub fn welcome() {
    eprintln!("Independence is ready. Type /help for commands.");
}

pub fn emit(output: Output) {
    match output {
        Output::Answer(answer) => println!("independence: {answer}"),
        Output::Diagnostic(message) => eprintln!("independence: {message}"),
        Output::Cameras(cameras) => {
            if cameras.is_empty() {
                eprintln!(
                    "independence: no usable V4L2 cameras found; connect one, check permissions, then run /cameras"
                );
            } else {
                println!("independence: usable cameras:");
                for camera in cameras {
                    println!(
                        "  {}  {}  (/dev/video{})",
                        camera.id(),
                        camera.name(),
                        camera.id()
                    );
                }
            }
        }
        Output::Help => println!("{HELP}"),
    }
}
