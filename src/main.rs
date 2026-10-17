mod protocol;
mod think;

use protocol::{Action, Event};

fn main() {
    let events = [
        Event::Heard(String::from("what tree is that?")),
        Event::Silence,
        Event::Heard(String::from("   ")),
    ];

    for event in events {
        let action = think::decide(&event);

        eprintln!("event: {event:?}");
        eprintln!("action: {action:?}");

        match action {
            Action::Say(text) => {
                println!("independence: {text}");
            }

            Action::Wait => {}
        }
    }
}
