//! Streaming `/v1/chat/completions` client (llama-server, Ollama, LM Studio, …).

use anyhow::{Context, bail};
use futures_util::{Stream, StreamExt};
use quillway_core::catalog::Sampling;
use quillway_core::prompt::{self, ChatMessage};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
    model: String,
    /// Talking to our own llama-server: send llama.cpp-specific fields.
    llama: bool,
}

/// One rewrite request.
#[derive(Debug, Clone)]
pub struct Rewrite {
    pub instruction: String,
    pub text: String,
    pub temperature: f32,
    pub sampling: Sampling,
    pub max_tokens: u32,
}

impl Client {
    pub fn new(base: String, api_key: Option<String>, model: String, llama: bool) -> Self {
        Self { http: reqwest::Client::new(), base: base.trim_end_matches('/').to_owned(), api_key, model, llama }
    }

    pub fn is_llama(&self) -> bool {
        self.llama
    }

    pub fn body(&self, r: &Rewrite) -> Value {
        let messages: Vec<ChatMessage> = prompt::build_messages(&r.instruction, &r.text);
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": r.temperature,
            "top_p": r.sampling.top_p,
            "max_tokens": r.max_tokens,
            "stop": [prompt::STOP],
        });
        if self.llama {
            let o = body.as_object_mut().expect("object");
            o.insert("top_k".into(), json!(r.sampling.top_k));
            o.insert("min_p".into(), json!(r.sampling.min_p));
            o.insert("cache_prompt".into(), json!(true));
            o.insert("chat_template_kwargs".into(), json!({ "enable_thinking": false }));
            // Rewrites repeat the input; penalties cause gratuitous synonym swaps.
            o.insert("repeat_penalty".into(), json!(1.0));
            o.insert("presence_penalty".into(), json!(0.0));
        }
        body
    }

    /// Stream content deltas. Dropping the stream cancels the request.
    pub async fn stream(
        &self,
        r: &Rewrite,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send + use<>> {
        let mut req = self.http.post(format!("{}/chat/completions", self.base)).json(&self.body(r));
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.with_context(|| format!("connecting to {}", self.base))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("{status}: {}", text.chars().take(300).collect::<String>());
        }
        let state = (resp.bytes_stream(), Vec::<u8>::new(), false);
        Ok(futures_util::stream::unfold(state, |(mut bytes, mut buf, done)| async move {
            if done {
                return None;
            }
            loop {
                // Drain complete lines before reading more bytes.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    match parse_sse_line(&String::from_utf8_lossy(&line)) {
                        Sse::Delta(d) => return Some((Ok(d), (bytes, buf, false))),
                        Sse::Done => return None,
                        Sse::Error(e) => return Some((Err(anyhow::anyhow!(e)), (bytes, buf, true))),
                        Sse::Skip => {}
                    }
                }
                match bytes.next().await? {
                    Ok(chunk) => buf.extend_from_slice(&chunk),
                    Err(e) => return Some((Err(e.into()), (bytes, buf, true))),
                }
            }
        }))
    }

    /// Collect a whole response (CLI use).
    pub async fn complete(&self, r: &Rewrite) -> anyhow::Result<String> {
        let mut s = std::pin::pin!(self.stream(r).await?);
        let mut out = String::new();
        while let Some(d) = s.next().await {
            out.push_str(&d?);
        }
        Ok(out)
    }
}

#[derive(Debug, PartialEq)]
enum Sse {
    Delta(String),
    Done,
    Error(String),
    Skip,
}

fn parse_sse_line(line: &str) -> Sse {
    let Some(data) = line.trim_end().strip_prefix("data:") else { return Sse::Skip };
    let data = data.trim_start();
    if data == "[DONE]" {
        return Sse::Done;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else { return Sse::Skip };
    if let Some(e) = v.get("error") {
        return Sse::Error(e.get("message").and_then(Value::as_str).unwrap_or("server error").to_owned());
    }
    match v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Sse::Delta(s.to_owned()),
        _ => Sse::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse() {
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#), Sse::Delta("Hi".into()));
        assert_eq!(parse_sse_line("data: [DONE]"), Sse::Done);
        assert_eq!(parse_sse_line(": keep-alive"), Sse::Skip);
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#), Sse::Skip);
        assert_eq!(parse_sse_line(r#"data: {"error":{"message":"boom"}}"#), Sse::Error("boom".into()));
    }

    #[test]
    fn llama_fields_only_for_llama_server() {
        let r = Rewrite {
            instruction: "x".into(),
            text: "y".into(),
            temperature: 0.2,
            sampling: Sampling { top_p: 0.8, top_k: 20, min_p: 0.0 },
            max_tokens: 128,
        };
        let ours = Client::new("http://h/v1".into(), None, "m".into(), true).body(&r);
        let byo = Client::new("http://h/v1".into(), None, "m".into(), false).body(&r);
        assert_eq!(ours["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(ours["top_k"], 20);
        assert!(byo.get("top_k").is_none() && byo.get("chat_template_kwargs").is_none());
        assert_eq!(byo["stop"][0], "</text>");
    }
}
