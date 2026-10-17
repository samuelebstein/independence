#[derive(Debug)]
pub enum Event {
    Heard(String),
    Silence,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Say(String),
    Wait,
}
