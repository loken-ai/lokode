use uuid::Uuid;

use crate::backend::wire::Message;

/// Conversation state for a single TUI session.
pub struct Session {
    pub id: String,
    pub history: Vec<Message>,
}

impl Session {
    pub fn new(system_prompt: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            history: vec![Message::system(system_prompt)],
        }
    }

    pub fn push(&mut self, m: Message) {
        self.history.push(m);
    }
}
