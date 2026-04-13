use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct HelloMessage {
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum HelloReply {
    Ok(String),
    Err(String),
}
