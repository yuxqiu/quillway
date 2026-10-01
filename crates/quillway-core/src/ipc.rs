//! CLI ⇄ daemon protocol: one JSON request line, one JSON response line.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Show the popup, or hide it if visible.
    Toggle {
        input: Input,
    },
    Show {
        input: Input,
    },
    Hide,
    /// Re-read config/state and restart the model server.
    Reload,
    Status,
    Quit,
}

/// Where the text to rewrite comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "from", content = "text", rename_all = "snake_case")]
pub enum Input {
    /// The clipboard if it was copied recently, else an empty box to type in.
    Clipboard,
    /// Text sent by the caller (e.g. an editor via `--stdin`).
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status { visible: bool, model: String, engine: String },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request::Toggle { input: Input::Text("hi".into()) };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"cmd":"toggle","input":{"from":"text","text":"hi"}}"#);
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let s = serde_json::to_string(&Request::Show { input: Input::Clipboard }).unwrap();
        assert_eq!(s, r#"{"cmd":"show","input":{"from":"clipboard"}}"#);
    }
}
