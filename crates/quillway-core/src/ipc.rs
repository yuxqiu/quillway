//! CLI ⇄ daemon protocol: one JSON request line, one JSON response line.

use serde::{Deserialize, Serialize};

use crate::catalog::Sampling;

/// A command from the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Show the popup, or hide it if visible. With [`Input::Text`], an open popup
    /// is kept and the request fails, so the text isn't lost.
    Toggle {
        /// Text to start from.
        input: Input,
    },
    /// Show the popup; a no-op if it is open, except that [`Input::Text`] then
    /// fails, so the text isn't lost.
    Show {
        /// Text to start from.
        input: Input,
    },
    /// Hide the popup.
    Hide,
    /// Re-read config/state and restart the model server.
    Reload,
    /// Report [`Response::Status`].
    Status,
    /// Start the model server if needed and report [`Response::Server`], so
    /// `quillway rewrite` shares it instead of loading a second copy.
    Connect,
    /// Stop the daemon.
    Quit,
}

/// Where the text to rewrite comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", content = "text", rename_all = "snake_case")]
pub enum Input {
    /// The clipboard if it was copied recently, else an empty box to type in.
    Clipboard,
    /// Text sent by the caller (e.g. an editor via `--stdin`).
    Text(String),
}

/// How to reach an OpenAI-compatible model server, and how to sample from its model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    /// OpenAI-compatible `/v1` URL.
    pub base: String,
    /// Bearer token.
    pub api_key: Option<String>,
    /// Model name for requests.
    pub model: String,
    /// Our own llama-server (exact token counts, llama.cpp fields).
    pub llama: bool,
    /// Context window in tokens.
    pub context: u32,
    /// Sampling defaults of the model.
    pub sampling: Sampling,
}

/// The daemon's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Done.
    Ok,
    /// Answer to [`Request::Status`].
    Status {
        /// Whether the popup is open.
        visible: bool,
        /// Active model's display name.
        model: String,
        /// Model server state, e.g. `ready`.
        engine: String,
    },
    /// Answer to [`Request::Connect`]: how to reach the daemon's model server.
    Server(Endpoint),
    /// The request failed.
    Error {
        /// What went wrong.
        message: String,
    },
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
        let server = Response::Server(Endpoint {
            base: "http://h/v1".into(),
            api_key: None,
            model: "m".into(),
            llama: true,
            context: 8192,
            sampling: Sampling::default(),
        });
        let s = serde_json::to_string(&server).unwrap();
        assert!(s.starts_with(r#"{"status":"server","base":"http://h/v1","#), "{s}");
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), server);
    }
}
