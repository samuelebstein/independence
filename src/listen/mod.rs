//! Keyboard input for the prototype's `listen` capability.
//!
//! A dedicated standard thread performs blocking stdin reads. That keeps the
//! Tokio executor free to receive completions and cancellation commands. The
//! thread is intentionally not joined at shutdown: a portable blocked
//! `read_line` cannot be forced awake. It exits on EOF, on a closed channel,
//! or when the process exits, and it never owns inference work.

use std::{
    env,
    io::{self, BufRead},
    path::{Path, PathBuf},
    thread,
};

use tokio::sync::mpsc::Sender;

use crate::protocol::{Event, Input};

/// Starts the blocking keyboard adapter for this process.
pub fn spawn_stdin_reader(sender: Sender<Event>) -> io::Result<()> {
    let working_directory = env::current_dir()?;
    let _input_thread = thread::Builder::new()
        .name("independence-input".into())
        .spawn(move || read_stdin(sender, working_directory))?;

    Ok(())
}

fn read_stdin(sender: Sender<Event>, working_directory: PathBuf) {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();

    loop {
        match input.read_line(&mut line) {
            Ok(byte_count) => {
                let event = classify_read(byte_count, &line, &working_directory);
                line.clear();

                if sender.blocking_send(Event::Input(event)).is_err() || byte_count == 0 {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.blocking_send(Event::Input(Input::Malformed(format!(
                    "could not read terminal input: {error}"
                ))));
                let _ = sender.blocking_send(Event::Input(Input::EndOfInput));
                break;
            }
        }
    }
}

fn classify_read(byte_count: usize, line: &str, working_directory: &Path) -> Input {
    if byte_count == 0 {
        Input::EndOfInput
    } else {
        parse_line(line, working_directory)
    }
}

/// Parses one keyboard line without running a shell or expanding paths.
pub fn parse_line(line: &str, working_directory: &Path) -> Input {
    let line = line.trim();

    if line.is_empty() {
        return Input::Blank;
    }

    if !line.starts_with('/') {
        return Input::Ask(line.to_owned());
    }

    match line {
        "/stop" => return Input::Stop,
        "/reset" => return Input::Reset,
        "/quit" => return Input::Quit,
        "/help" => return Input::Help,
        _ => {}
    }

    if line.starts_with("/stop")
        || line.starts_with("/reset")
        || line.starts_with("/quit")
        || line.starts_with("/help")
    {
        return Input::Malformed("/stop, /reset, /quit, and /help do not accept arguments".into());
    }

    if line == "/look" {
        return Input::Malformed("/look needs `path | question`".into());
    }

    let Some(rest) = line.strip_prefix("/look") else {
        return Input::Malformed(format!("unknown command `{line}`; try /help"));
    };

    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return Input::Malformed(format!("unknown command `{line}`; try /help"));
    }

    parse_look(rest, working_directory)
}

fn parse_look(rest: &str, working_directory: &Path) -> Input {
    let Some((raw_path, raw_question)) = rest.split_once('|') else {
        return Input::Malformed("/look needs `path | question`".into());
    };

    let raw_path = raw_path.trim();
    let question = raw_question.trim();
    if raw_path.is_empty() {
        return Input::Malformed("/look needs a photo path before `|`".into());
    }
    if question.is_empty() {
        return Input::Malformed("/look needs a question after `|`".into());
    }

    let path = Path::new(raw_path);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        working_directory.join(path)
    };

    Input::Look {
        path,
        question: question.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::protocol::Input;

    use super::{classify_read, parse_line};

    #[test]
    fn look_keeps_spaces_in_a_literal_relative_path() {
        let working_directory = Path::new("/work/independence");
        assert_eq!(
            parse_line(
                "/look data/trees/coast live oak.png | What might this tree be?",
                working_directory
            ),
            Input::Look {
                path: working_directory.join("data/trees/coast live oak.png"),
                question: "What might this tree be?".into(),
            }
        );
    }

    #[test]
    fn malformed_look_is_not_a_question() {
        assert!(matches!(
            parse_line("/look tree.png", Path::new("/work")),
            Input::Malformed(message) if message.contains("path | question")
        ));
        assert!(matches!(
            parse_line("/look", Path::new("/work")),
            Input::Malformed(message) if message.contains("path | question")
        ));
    }

    #[test]
    fn eof_is_distinct_from_an_empty_submitted_line() {
        let working_directory = Path::new("/work");
        assert_eq!(classify_read(1, "\n", working_directory), Input::Blank);
        assert_eq!(classify_read(0, "", working_directory), Input::EndOfInput);
    }
}
