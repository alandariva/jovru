//! The canonical request of one round: everything the model is given,
//! in a form the application shows before it is sent.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One round's request. It is what the user reviews; the transport turns
/// it into the provider's wire format without adding content to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
}

/// A turn of the conversation, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        text: String,
    },
    /// What the model answered, with the tools it asked for.
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    /// The application's answer to one tool call.
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

impl Message {
    /// The text this turn carries, as the model reads it.
    pub fn len(&self) -> usize {
        match self {
            Self::User { text } => text.len(),
            Self::Assistant { text, tool_calls } => {
                text.len()
                    + tool_calls
                        .iter()
                        .map(|call| call.name.len() + call.arguments.to_string().len())
                        .sum::<usize>()
            }
            Self::Tool { content, .. } => content.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A tool the model may call, with the JSON Schema of its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A call the model asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A rough count of the tokens a request takes, about four characters per
/// token. It is shown as an estimate; the provider reports the real count
/// after the round.
pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    let tools: usize = request
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.input_schema.to_string().len())
        .sum();
    let messages: usize = request.messages.iter().map(Message::len).sum();
    let characters = request.system.len() + tools + messages;
    (characters as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_serialize_with_tagged_roles_and_estimate_from_their_text() {
        let request = ModelRequest {
            model: "m".into(),
            system: "abcd".into(),
            messages: vec![
                Message::User {
                    text: "abcdefgh".into(),
                },
                Message::Assistant {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "1".into(),
                        name: "t".into(),
                        arguments: json!({}),
                    }],
                },
                Message::Tool {
                    call_id: "1".into(),
                    name: "t".into(),
                    content: "abc".into(),
                },
            ],
            tools: Vec::new(),
            max_output_tokens: 10,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][2]["role"], "tool");
        // 4 + 8 + (1 + 2) + 3 = 18 characters.
        assert_eq!(estimate_tokens(&request), 5);
    }
}
