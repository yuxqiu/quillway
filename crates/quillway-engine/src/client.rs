//! Streaming `/v1/chat/completions` client (llama-server, Ollama, LM Studio, …).

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{Stream, StreamExt};
use quillway_core::ipc::Endpoint;
use quillway_core::prompt::{self, ChatMessage};
use serde_json::{Value, json};

/// A connection to one OpenAI-compatible server.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
}

/// One rewrite request.
#[derive(Debug, Clone)]
pub struct Rewrite {
    /// What to do, e.g. a preset's instruction.
    pub instruction: String,
    /// The text to rewrite.
    pub text: String,
    /// Sampling temperature.
    pub temperature: f32,
    /// Upper bound on generated tokens; `None` fills the context left after the prompt.
    pub max_tokens: Option<u32>,
}

impl Client {
    /// A client for `endpoint`; `llama` enables llama.cpp-only request fields
    /// and exact token counts.
    #[must_use]
    pub fn new(mut endpoint: Endpoint) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Between reads; the first byte can wait for a long prompt on a CPU.
            .read_timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        endpoint.base = endpoint.base.trim_end_matches('/').to_owned();
        Self { http, endpoint }
    }

    /// Whether this is our own llama-server.
    #[must_use]
    pub const fn is_llama(&self) -> bool {
        self.endpoint.llama
    }

    /// How to reach the server.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// A POST to `url`, authenticated if the server needs it.
    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.http.post(url);
        match &self.endpoint.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    fn body(&self, r: &Rewrite, max_tokens: u32) -> Value {
        let Endpoint { model, llama, sampling, .. } = &self.endpoint;
        let messages: Vec<ChatMessage> = prompt::build_messages(&r.instruction, &r.text);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "temperature": r.temperature,
            "top_p": sampling.top_p,
            "max_tokens": max_tokens,
            "stop": [prompt::STOP],
        });
        if *llama && let Value::Object(o) = &mut body {
            o.insert("top_k".into(), json!(sampling.top_k));
            o.insert("min_p".into(), json!(sampling.min_p));
            o.insert("cache_prompt".into(), json!(true));
            o.insert("chat_template_kwargs".into(), json!({ "enable_thinking": false }));
            // Rewrites repeat the input; penalties cause gratuitous synonym swaps.
            o.insert("repeat_penalty".into(), json!(1.0));
            o.insert("presence_penalty".into(), json!(0.0));
        }
        body
    }

    /// `max_tokens` for `r`: counted by our llama-server's tokenizer, estimated
    /// elsewhere or if the server can't count (an older `model.llama_server`).
    async fn budget(&self, r: &Rewrite) -> anyhow::Result<u32> {
        let counted = if self.is_llama() {
            match tokio::try_join!(self.tokenize(&r.text, true), self.tokenize(&r.text, false)) {
                Ok((parsed, plain)) => {
                    if let Some(token) = control_token(&parsed, &plain) {
                        bail!("the text contains `{token}`, a control token of the model; remove it and try again");
                    }
                    self.server_counts(r, &plain)
                        .await
                        .inspect_err(|e| eprintln!("quillway: counting tokens failed, estimating instead: {e:#}"))
                        .ok()
                }
                Err(e) => {
                    eprintln!("quillway: checking text tokens failed, estimating instead: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let (prompt_tokens, text_tokens) = counted.map_or_else(
            || (prompt::estimate_prompt(&r.instruction, &r.text), prompt::estimate_tokens(&r.text)),
            |c| (c.prompt, c.text),
        );
        let context = self.endpoint.context;
        prompt::budget(prompt_tokens, text_tokens, context).with_context(|| {
            format!(
                "the text is too long for the model's {context}-token context ({prompt_tokens} tokens with the prompt); \
                 shorten it or raise `model.context`"
            )
        })
    }

    async fn server_counts(&self, r: &Rewrite, plain: &[Token]) -> anyhow::Result<Counts> {
        let body = json!({
            "messages": prompt::build_messages(&r.instruction, &r.text),
            "chat_template_kwargs": { "enable_thinking": false },
        });
        let v = self.post_root("apply-template", &body).await?;
        let rendered = v["prompt"].as_str().context("apply-template: no prompt")?;
        let prompt = self.tokenize(rendered, true).await?;
        let len = |t: &[Token]| u32::try_from(t.len()).unwrap_or(u32::MAX);
        Ok(Counts { prompt: len(&prompt), text: len(plain) })
    }

    /// `text`'s tokens from llama-server; `parse_special` turns control-token text into control tokens.
    async fn tokenize(&self, text: &str, parse_special: bool) -> anyhow::Result<Vec<Token>> {
        let body = json!({ "content": text, "parse_special": parse_special, "with_pieces": true });
        let v = self.post_root("tokenize", &body).await?;
        let tokens = v["tokens"].as_array().context("tokenize: no tokens")?;
        Ok(tokens
            .iter()
            .map(|t| Token { id: t["id"].as_u64(), piece: t["piece"].as_str().map(str::to_owned) })
            .collect())
    }

    /// POST to a llama-server endpoint outside `/v1`.
    async fn post_root(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let base = &self.endpoint.base;
        let root = base.strip_suffix("/v1").unwrap_or(base);
        let resp = self
            .post(&format!("{root}/{path}"))
            .json(body)
            .send()
            .await
            .with_context(|| format!("connecting to {root}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{path}: {status}");
        }
        Ok(resp.json().await?)
    }

    /// Stream content deltas. Dropping the stream cancels the request.
    ///
    /// # Errors
    ///
    /// The text doesn't fit the context, or the server is unreachable or
    /// rejects the request; later failures arrive as stream items.
    pub async fn stream(
        &self,
        r: &Rewrite,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send + use<>> {
        let max_tokens = match r.max_tokens {
            Some(n) => n,
            None => self.budget(r).await?,
        };
        let base = &self.endpoint.base;
        let resp = self
            .post(&format!("{base}/chat/completions"))
            .json(&self.body(r, max_tokens))
            .send()
            .await
            .with_context(|| format!("connecting to {base}"))?;
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
                match bytes.next().await {
                    None => {
                        return Some((
                            Err(anyhow::anyhow!("incomplete response: stream ended before [DONE]")),
                            (bytes, buf, true),
                        ));
                    }
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    Some(Err(e)) => return Some((Err(e.into()), (bytes, buf, true))),
                }
            }
        }))
    }

    /// Run one tiny request, so GPU pipelines are built and the fixed prompt
    /// prefix is cached. It is cut at one token on purpose, so the first
    /// delta is success and the token-limit ending that follows is ignored.
    ///
    /// # Errors
    ///
    /// As [`Client::stream`], or the server fails before the first token.
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        let r = Rewrite { instruction: "Proofread.".into(), text: "ok".into(), temperature: 0.0, max_tokens: Some(1) };
        let mut s = std::pin::pin!(self.stream(&r).await?);
        match s.next().await {
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(e),
            None => bail!("warm-up produced no token"),
        }
    }

    /// Collect a whole response (CLI use).
    ///
    /// # Errors
    ///
    /// As [`Client::stream`], plus any error reported mid-stream.
    pub async fn complete(&self, r: &Rewrite) -> anyhow::Result<String> {
        let mut s = std::pin::pin!(self.stream(r).await?);
        let mut out = String::new();
        while let Some(d) = s.next().await {
            out.push_str(&d?);
        }
        if out.trim().is_empty() {
            bail!("the model returned nothing");
        }
        Ok(out)
    }
}

struct Counts {
    /// The whole rendered prompt.
    prompt: u32,
    /// The text alone.
    text: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    id: Option<u64>,
    /// `None` when the token isn't valid UTF-8 on its own.
    piece: Option<String>,
}

/// The first control token in `parsed` (the text tokenized with control
/// tokens recognised) that `plain` spells out as ordinary text.
fn control_token(parsed: &[Token], plain: &[Token]) -> Option<String> {
    if parsed.iter().map(|t| t.id).eq(plain.iter().map(|t| t.id)) {
        return None;
    }
    let ordinary: std::collections::HashSet<_> = plain.iter().map(|t| t.id).collect();
    let mut new = parsed.iter().filter(|t| !ordinary.contains(&t.id));
    let first = new.clone().next();
    let token = new.find(|t| t.piece.as_deref().is_some_and(|p| p.starts_with(['<', '[']))).or(first);
    token.map(|t| t.piece.clone().unwrap_or_else(|| format!("token {}", t.id.unwrap_or_default())))
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
    if data.is_empty() {
        return Sse::Skip;
    }
    if data == "[DONE]" {
        return Sse::Done;
    }
    let v = match serde_json::from_str::<Value>(data) {
        Ok(v) => v,
        Err(e) => return Sse::Error(format!("malformed SSE data: {e}")),
    };
    if let Some(e) = v.get("error") {
        return Sse::Error(e.get("message").and_then(Value::as_str).unwrap_or("server error").to_owned());
    }
    // Only reasons that mean "cut short" fail: servers differ in how they name a
    // normal ending (`stop`, `eos_token`, `stop_sequence`, an empty string, …).
    match v.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
        Some("length") => return Sse::Error("the model stopped at its token limit; the rewrite is incomplete".into()),
        Some(reason @ ("content_filter" | "abort")) => {
            return Sse::Error(format!("the model stopped with finish reason {reason:?}; the rewrite is incomplete"));
        }
        _ => {}
    }
    match v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Sse::Delta(s.to_owned()),
        _ => Sse::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quillway_core::catalog::Sampling;

    fn client(base: &str, api_key: Option<&str>, llama: bool) -> Client {
        Client::new(Endpoint {
            base: base.into(),
            api_key: api_key.map(Into::into),
            model: "test".into(),
            llama,
            context: 8192,
            sampling: Sampling { top_p: 0.8, top_k: 20, min_p: 0.0 },
        })
    }

    type Requests = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    /// Answer each `POST <path>` with the first route whose path matches and
    /// whose needle is in the request body; record every request.
    async fn serve(routes: Vec<(&'static str, &'static str, String)>) -> (String, Requests) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Requests::default();
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = socket.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_owned();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut request_body = vec![0; content_length];
                reader.read_exact(&mut request_body).await.unwrap();
                let raw = String::from_utf8_lossy(&request_body).into_owned();
                seen.lock().unwrap().push((path.clone(), serde_json::from_str(&raw).unwrap()));
                let body = routes
                    .iter()
                    .find(|(p, needle, _)| *p == path && raw.contains(needle))
                    .map_or("", |(_, _, b)| b.as_str());
                let response =
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                writer.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, requests)
    }

    async fn serve_sse(body: &'static str) -> String {
        serve(vec![("/v1/chat/completions", "", body.to_owned())]).await.0
    }

    const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"}}]}\n\ndata: [DONE]\n\n";

    fn request() -> Rewrite {
        Rewrite { instruction: "Proofread".into(), text: "hello".into(), temperature: 0.2, max_tokens: Some(128) }
    }

    #[tokio::test]
    async fn incomplete_stream_is_an_error() {
        let base = serve_sse("data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n").await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");
    }

    #[tokio::test]
    async fn token_limit_is_an_error() {
        let base = serve_sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("token limit"), "{error}");
    }

    #[tokio::test]
    async fn non_stop_finish_reason_is_an_error() {
        let base = serve_sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("content_filter"), "{error}");
    }

    #[tokio::test]
    async fn other_normal_endings_are_accepted() {
        for reason in ["eos_token", "stop_sequence", ""] {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Done\"}},\"finish_reason\":\"{reason}\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let base = serve(vec![("/v1/chat/completions", "", body)]).await.0;
            let text = client(&base, None, false).complete(&request()).await.unwrap();
            assert_eq!(text, "Done", "{reason:?}");
        }
    }

    #[tokio::test]
    async fn malformed_data_after_a_delta_is_an_error() {
        let base = serve_sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n\
             data: {bad json}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("malformed"), "{error}");
    }

    #[tokio::test]
    async fn complete_stream_returns_text() {
        let base = serve_sse("data: {\"choices\":[{\"delta\":{\"content\":\"Complete\"}}]}\n\ndata: [DONE]\n\n").await;
        let text = client(&base, None, false).complete(&request()).await.unwrap();
        assert_eq!(text, "Complete");
    }

    #[tokio::test]
    async fn empty_rewrite_is_an_error() {
        let base = serve_sse("data: [DONE]\n\n").await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("nothing"), "{error}");
    }

    #[tokio::test]
    async fn llama_budget_uses_the_servers_token_counts() {
        let (base, requests) = serve(vec![
            ("/apply-template", "", "{\"prompt\":\"rendered\"}".to_owned()),
            ("/tokenize", "", tokens(&vec![1; 3000])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = Rewrite { max_tokens: None, ..request() };
        let text = client(&base, Some("k"), true).complete(&r).await.unwrap();
        assert_eq!(text, "Ok");
        let body = |path: &str| requests.lock().unwrap().iter().find(|(p, _)| p == path).unwrap().1.clone();
        // Both the rendered prompt and the text count 3000: room 5192, cap 6000.
        assert_eq!(body("/v1/chat/completions")["max_tokens"], 5192);
        assert_eq!(body("/apply-template")["chat_template_kwargs"]["enable_thinking"], false);
    }

    fn tokens(ids: &[u64]) -> String {
        let t: Vec<Value> =
            ids.iter().map(|id| json!({ "id": id, "piece": if *id == 9 { "<|im_end|>" } else { "x" } })).collect();
        json!({ "tokens": t }).to_string()
    }

    #[tokio::test]
    async fn text_spelling_out_a_control_token_is_rejected() {
        let (base, requests) = serve(vec![
            ("/apply-template", "", "{\"prompt\":\"rendered\"}".to_owned()),
            ("/tokenize", "\"parse_special\":false", tokens(&[1, 2, 3, 4, 5])),
            ("/tokenize", "", tokens(&[1, 9, 5])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = Rewrite { max_tokens: None, ..request() };
        let error = client(&base, None, true).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("`<|im_end|>`, a control token"), "{error}");
        assert!(requests.lock().unwrap().iter().all(|(p, _)| p != "/v1/chat/completions"));
    }

    #[tokio::test]
    async fn control_token_is_rejected_when_template_counting_is_unavailable() {
        let (base, requests) = serve(vec![
            ("/tokenize", "\"parse_special\":false", tokens(&[1, 2, 3, 4, 5])),
            ("/tokenize", "", tokens(&[1, 9, 5])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = Rewrite { max_tokens: None, ..request() };
        let error = client(&base, None, true).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("`<|im_end|>`, a control token"), "{error}");
        assert!(requests.lock().unwrap().iter().all(|(p, _)| p != "/v1/chat/completions"));
    }

    #[tokio::test]
    async fn a_server_that_cannot_count_falls_back_to_the_estimate() {
        // An older llama-server: no /apply-template (the mock answers it with an empty body).
        let (base, requests) = serve(vec![("/v1/chat/completions", "", DONE.to_owned())]).await;
        let r = Rewrite { max_tokens: None, ..request() };
        let text = client(&base, None, true).complete(&r).await.unwrap();
        assert_eq!(text, "Ok");
        let chat = requests.lock().unwrap().iter().find(|(p, _)| p == "/v1/chat/completions").unwrap().1.clone();
        assert_eq!(chat["max_tokens"], 1024);
    }

    #[test]
    fn control_token_needs_a_difference_between_tokenizations() {
        let t = |id, piece: &str| Token { id: Some(id), piece: Some(piece.into()) };
        assert_eq!(control_token(&[t(1, "a")], &[t(1, "a")]), None);
        assert_eq!(
            control_token(&[t(1, "a"), t(7, "a<"), t(9, "<|im_end|>")], &[t(2, "a<"), t(3, "|")]),
            Some("<|im_end|>".into())
        );
    }

    #[tokio::test]
    async fn text_too_long_for_the_context_is_rejected_before_generating() {
        let (base, requests) = serve(vec![]).await;
        let r = Rewrite { max_tokens: None, text: "x".repeat(30_000), ..request() };
        let error = client(&base, None, false).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("too long"), "{error}");
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn warm_up_ignores_its_own_token_limit() {
        let base = serve_sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        client(&base, None, true).warm_up().await.unwrap();
    }

    #[tokio::test]
    async fn warm_up_requires_a_token() {
        let base = serve_sse("data: [DONE]\n\n").await;
        let error = client(&base, None, true).warm_up().await.unwrap_err();
        assert!(error.to_string().contains("no token"), "{error}");
    }

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
        let r = Rewrite { instruction: "x".into(), text: "y".into(), temperature: 0.2, max_tokens: Some(128) };
        let ours = client("http://h/v1", None, true).body(&r, 128);
        let byo = client("http://h/v1", None, false).body(&r, 128);
        assert_eq!(ours["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(ours["top_k"], 20);
        assert!(byo.get("top_k").is_none() && byo.get("chat_template_kwargs").is_none());
        assert_eq!(byo["stop"][0], "</text>");
    }
}
