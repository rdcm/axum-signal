use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct HelloMessage {
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub enum HelloReply {
    Ok(String),
    Err(String),
}
