use core::str;
use std::sync::{Arc, RwLock};

pub trait Messaging {
    fn publish_message(&self, message: &str, user_id: &str) -> Result<(), Box<dyn std::error::Error>>;
    fn subscribe(
        &self,
        last_seen_timestamp: u64,
    ) -> Result<Vec<MessageEntry>, Box<dyn std::error::Error>>;
}

#[derive(Clone)]
pub struct MessageEntry {
    content: String,
    user_id: String,
    timestamp: u64,
}
pub struct MessagingService {
   pub messages_wall: RwLock<Vec<MessageEntry>>
}

impl Messaging for MessagingService {
    fn publish_message(&self, message: &str, user_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut wall = self.messages_wall.write().unwrap();
        let entry = MessageEntry {
            content: message.to_string(),
            user_id: user_id.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        };
        wall.push(entry);

        println!("messages size: {}", wall.len());
        Ok(())
    }
    fn subscribe(
        &self,
        last_seen_timestamp: u64,
    ) -> Result<Vec<MessageEntry>, Box<dyn std::error::Error>> {
        let missed_messages = self
            .messages_wall
            .read()
            .unwrap()
            .iter()
            .filter(|entry| entry.timestamp > last_seen_timestamp)
            .cloned() // TODO: this is losing perdformance, find a better way
            .collect::<Vec<MessageEntry>>();
        Ok(missed_messages)
    }
}
