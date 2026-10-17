use crate::protocol::{Action, Event};

pub fn decide(event: &Event) -> Action {
    match event {
        Event::Silence => Action::Wait,

        Event::Heard(text) if text.trim().is_empty() => Action::Wait,

        Event::Heard(text) => Action::Say(format!("You said: {text}")),
    }
}

#[cfg(test)]
mod tests {
    use super::decide;
    use crate::protocol::{Action, Event};

    #[test]
    fn silence_does_not_request_speech() {
        let action = decide(&Event::Silence);

        assert_eq!(action, Action::Wait);
    }

    #[test]
    fn whitespace_is_not_a_question() {
        let event = Event::Heard(String::from("   "));

        let action = decide(&event);

        assert_eq!(action, Action::Wait);
    }

    #[test]
    fn heard_words_are_preserved_in_the_reply() {
        let event = Event::Heard(String::from("what tree is that?"));

        let action = decide(&event);

        assert_eq!(
            action,
            Action::Say(String::from("You said: what tree is that?"))
        );
    }
}
