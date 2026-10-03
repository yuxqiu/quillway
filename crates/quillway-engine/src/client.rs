//! Streaming `/v1/chat/completions` client (llama-server, Ollama, LM Studio, …).

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use quillway_core::ipc::Endpoint;
use quillway_core::prompt::{self, ChatMessage};
use serde_json::{Value, json};

/// The server stopped sending mid-response: it exited or was restarted, or
/// the connection dropped. Find it with `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the model server stopped mid-response")
    }
}

impl std::error::Error for Interrupted {}

/// One HTTP client (connection pool, TLS roots) for every [`Client`].
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Between reads; the first byte can wait for a long prompt on a CPU.
        .read_timeout(Duration::from_secs(300))
        .build()
        .expect("the TLS backend initializes")
});

/// A connection to one OpenAI-compatible server.
#[derive(Debug, Clone)]
pub struct Client {
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
}

/// One piece of a streamed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// Generated text.
    Text(String),
    /// Tokens generated, as the server counts them; sent once, at the end.
    Usage(u32),
}

/// Timing of one streamed response, for a tokens-per-second figure.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    started: Instant,
    first: Option<Instant>,
    texts: u32,
    usage: Option<u32>,
}

impl Timing {
    /// Starts the clock.
    #[must_use]
    pub fn start() -> Self {
        Self { started: Instant::now(), first: None, texts: 0, usage: None }
    }

    /// Note a chunk as it arrives.
    pub fn record(&mut self, chunk: &Chunk) {
        match chunk {
            Chunk::Text(_) => {
                self.first.get_or_insert_with(Instant::now);
                self.texts += 1;
            }
            Chunk::Usage(n) => self.usage = Some(*n),
        }
    }

    /// When the request started.
    #[must_use]
    pub const fn started(&self) -> Instant {
        self.started
    }

    /// From the start to the first text.
    #[must_use]
    pub fn first_token(&self) -> Option<Duration> {
        Some(self.first? - self.started)
    }

    /// Tokens generated: the server's count, else the number of text chunks
    /// (one token each on llama-server, possibly more elsewhere).
    #[must_use]
    pub fn tokens(&self) -> u32 {
        self.usage.unwrap_or(self.texts)
    }

    /// Tokens per second from the first token until now.
    #[must_use]
    pub fn rate(&self) -> f64 {
        let secs = self.first.unwrap_or(self.started).elapsed().as_secs_f64().max(1e-3);
        f64::from(self.tokens().saturating_sub(1)) / secs
    }
}

impl Client {
    /// A client for `endpoint`; `llama` enables llama.cpp-only request fields
    /// and exact token counts.
    #[must_use]
    pub fn new(mut endpoint: Endpoint) -> Self {
        endpoint.base = endpoint.base.trim_end_matches('/').to_owned();
        Self { endpoint }
    }

    /// How to reach the server.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// A POST to `url`, authenticated if the server needs it.
    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let req = HTTP.post(url);
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
            "stream_options": { "include_usage": true },
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
        let counted = if self.endpoint.llama { self.count(r).await? } else { None };
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

    /// Token counts from our llama-server, after refusing text that spells out
    /// a control token (DECISIONS #27); `None` if the server can't count.
    async fn count(&self, r: &Rewrite) -> anyhow::Result<Option<Counts>> {
        let (parsed, plain) = match tokio::try_join!(self.tokenize(&r.text, true), self.tokenize(&r.text, false)) {
            Ok(tokens) => tokens,
            Err(e) => {
                eprintln!("quillway: checking text tokens failed, estimating instead: {e:#}");
                return Ok(None);
            }
        };
        if let Some(token) = control_token(&parsed, &plain) {
            bail!("the text contains `{token}`, a control token of the model; remove it and try again");
        }
        let counts = self.server_counts(r, &plain).await;
        Ok(counts.inspect_err(|e| eprintln!("quillway: counting tokens failed, estimating instead: {e:#}")).ok())
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
            // With pieces, each token is `{"id", "piece"}`; servers that ignore `with_pieces` send bare ids.
            .map(|t| Token {
                id: t.as_u64().or_else(|| t["id"].as_u64()),
                piece: t["piece"].as_str().map(str::to_owned),
            })
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
        Ok(ok(resp, path).await?.json().await?)
    }

    /// Stream the response. Dropping the stream cancels the request.
    ///
    /// # Errors
    ///
    /// The text contains the prompt's delimiter or spells out a control token
    /// of the model, it doesn't fit the context, or the server is unreachable
    /// or rejects the request; later failures arrive as stream items.
    pub async fn stream(
        &self,
        r: &Rewrite,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<Chunk>> + Send + use<>> {
        if r.text.contains(prompt::STOP) {
            bail!("the text contains `{}`, which Quillway uses as a delimiter", prompt::STOP);
        }
        let max_tokens = self.budget(r).await?;
        let base = &self.endpoint.base;
        let resp = self
            .post(&format!("{base}/chat/completions"))
            .json(&self.body(r, max_tokens))
            .send()
            .await
            .with_context(|| format!("connecting to {base}"))?;
        let resp = ok(resp, "chat/completions").await?;
        let state = (resp.bytes_stream().eventsource(), VecDeque::new(), false);
        Ok(futures_util::stream::unfold(state, |(mut events, mut pending, done)| async move {
            if done {
                return None;
            }
            loop {
                let Some(next) = pending.pop_front() else {
                    // The server stopped sending: without `[DONE]`, the output may be cut short.
                    let error = match events.next().await {
                        Some(Ok(event)) => {
                            pending.extend(parse_data(&event.data));
                            continue;
                        }
                        None => {
                            anyhow::Error::new(Interrupted).context("incomplete response: stream ended before [DONE]")
                        }
                        Some(Err(EventStreamError::Transport(e))) => {
                            anyhow::Error::new(Interrupted).context(e.to_string())
                        }
                        // Invalid UTF-8 or SSE framing: the server misbehaved, it didn't stop.
                        Some(Err(e)) => anyhow::anyhow!("malformed SSE stream: {e}"),
                    };
                    return Some((Err(error), (events, pending, true)));
                };
                return match next {
                    Sse::Chunk(c) => Some((Ok(c), (events, pending, false))),
                    Sse::Done => None,
                    Sse::Error(e) => Some((Err(anyhow::anyhow!(e)), (events, pending, true))),
                };
            }
        }))
    }

    /// Collect a whole response, with its timing (`quillway rewrite`, the eval).
    ///
    /// # Errors
    ///
    /// As [`Client::stream`], plus any error reported mid-stream, or an empty response.
    pub async fn complete(&self, r: &Rewrite) -> anyhow::Result<(String, Timing)> {
        let mut timing = Timing::start();
        let mut s = std::pin::pin!(self.stream(r).await?);
        let mut out = String::new();
        while let Some(chunk) = s.next().await {
            let chunk = chunk?;
            timing.record(&chunk);
            if let Chunk::Text(t) = chunk {
                out.push_str(&t);
            }
        }
        if out.trim().is_empty() {
            bail!("the model returned nothing");
        }
        Ok((out, timing))
    }
}

/// `resp` if it succeeded, else an error naming `what` with the server's own
/// message (the OpenAI-style `error.message`, else the start of the body).
async fn ok(resp: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect());
    if message.is_empty() {
        bail!("{what}: {status}");
    }
    bail!("{what}: {status}: {message}")
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
    Chunk(Chunk),
    Done,
    Error(String),
}

/// One event's `data`, which the SSE parser has already unframed, as what it
/// carries in order: its text, then whether it ends the stream, then usage.
fn parse_data(data: &str) -> Vec<Sse> {
    if data.is_empty() {
        return Vec::new();
    }
    if data == "[DONE]" {
        return vec![Sse::Done];
    }
    let v = match serde_json::from_str::<Value>(data) {
        Ok(v) => v,
        Err(e) => return vec![Sse::Error(format!("malformed SSE data: {e}"))],
    };
    // Some proxies send `"error": null` in normal chunks.
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        return vec![Sse::Error(e.get("message").and_then(Value::as_str).unwrap_or("server error").to_owned())];
    }
    let mut out = Vec::new();
    if let Some(s) = v.pointer("/choices/0/delta/content").and_then(Value::as_str)
        && !s.is_empty()
    {
        out.push(Sse::Chunk(Chunk::Text(s.to_owned())));
    }
    // Only reasons that mean "cut short" fail: servers differ in how they name a
    // normal ending (`stop`, `eos_token`, `stop_sequence`, an empty string, …).
    match v.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
        Some("length") => {
            out.push(Sse::Error("the model stopped at its token limit; the rewrite is incomplete".into()));
        }
        Some(reason @ ("content_filter" | "abort")) => {
            out.push(Sse::Error(format!("the model stopped with finish reason {reason:?}; the rewrite is incomplete")));
        }
        _ => {}
    }
    if let Some(n) = v.pointer("/usage/completion_tokens").and_then(Value::as_u64) {
        out.push(Sse::Chunk(Chunk::Usage(u32::try_from(n).unwrap_or(u32::MAX))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quillway_core::catalog::Sampling;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    /// Answer each `POST <path>` with the first route whose path matches and
    /// whose needle is in the request body (mocks with equal priority match in
    /// mount order); anything else gets a 404. The server stops when dropped.
    async fn serve(routes: Vec<(&'static str, &'static str, String)>) -> (String, MockServer) {
        let server = MockServer::start().await;
        for (p, needle, body) in routes {
            Mock::given(method("POST"))
                .and(path(p))
                .and(body_string_contains(needle))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        (format!("{}/v1", server.uri()), server)
    }

    async fn serve_sse(body: &'static str) -> (String, MockServer) {
        serve(vec![("/v1/chat/completions", "", body.to_owned())]).await
    }

    /// Every request the server received, as (path, JSON body).
    async fn received(server: &MockServer) -> Vec<(String, Value)> {
        let requests = server.received_requests().await.unwrap();
        requests.iter().map(|r| (r.url.path().to_owned(), r.body_json().unwrap())).collect()
    }

    const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"}}]}\n\ndata: [DONE]\n\n";

    fn request() -> Rewrite {
        Rewrite { instruction: "Proofread".into(), text: "hello".into(), temperature: 0.2 }
    }

    #[tokio::test]
    async fn incomplete_stream_is_an_error() {
        let (base, _server) = serve_sse("data: {\"choices\":[{\"delta\":{\"content\":\"Partial\"}}]}\n\n").await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");
        assert!(error.downcast_ref::<Interrupted>().is_some(), "marked as an interruption");
    }

    #[tokio::test]
    async fn token_limit_is_an_error() {
        let (base, _server) = serve_sse(
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
        let (base, _server) = serve_sse(
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
            let (base, _server) = serve(vec![("/v1/chat/completions", "", body)]).await;
            let text = client(&base, None, false).complete(&request()).await.unwrap().0;
            assert_eq!(text, "Done", "{reason:?}");
        }
    }

    #[tokio::test]
    async fn malformed_data_after_a_delta_is_an_error() {
        let (base, _server) = serve_sse(
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
        let (base, _server) =
            serve_sse("data: {\"choices\":[{\"delta\":{\"content\":\"Complete\"}}]}\n\ndata: [DONE]\n\n").await;
        let text = client(&base, None, false).complete(&request()).await.unwrap().0;
        assert_eq!(text, "Complete");
    }

    #[tokio::test]
    async fn empty_rewrite_is_an_error() {
        let (base, _server) = serve_sse("data: [DONE]\n\n").await;
        let error = client(&base, None, false).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("nothing"), "{error}");
    }

    #[tokio::test]
    async fn llama_budget_uses_the_servers_token_counts() {
        let (base, server) = serve(vec![
            ("/apply-template", "", "{\"prompt\":\"rendered\"}".to_owned()),
            ("/tokenize", "", tokens(&vec![1; 3000])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = request();
        let text = client(&base, Some("k"), true).complete(&r).await.unwrap().0;
        assert_eq!(text, "Ok");
        let requests = received(&server).await;
        let body = |path: &str| requests.iter().find(|(p, _)| p == path).unwrap().1.clone();
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
        let (base, server) = serve(vec![
            ("/apply-template", "", "{\"prompt\":\"rendered\"}".to_owned()),
            ("/tokenize", "\"parse_special\":false", tokens(&[1, 2, 3, 4, 5])),
            ("/tokenize", "", tokens(&[1, 9, 5])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = request();
        let error = client(&base, None, true).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("`<|im_end|>`, a control token"), "{error}");
        assert!(received(&server).await.iter().all(|(p, _)| p != "/v1/chat/completions"));
    }

    #[tokio::test]
    async fn control_token_is_rejected_when_template_counting_is_unavailable() {
        let (base, server) = serve(vec![
            ("/tokenize", "\"parse_special\":false", tokens(&[1, 2, 3, 4, 5])),
            ("/tokenize", "", tokens(&[1, 9, 5])),
            ("/v1/chat/completions", "", DONE.to_owned()),
        ])
        .await;
        let r = request();
        let error = client(&base, None, true).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("`<|im_end|>`, a control token"), "{error}");
        assert!(received(&server).await.iter().all(|(p, _)| p != "/v1/chat/completions"));
    }

    #[tokio::test]
    async fn a_server_that_cannot_count_falls_back_to_the_estimate() {
        // An older llama-server: no /apply-template or /tokenize (the mock answers 404).
        let (base, server) = serve(vec![("/v1/chat/completions", "", DONE.to_owned())]).await;
        let r = request();
        let text = client(&base, None, true).complete(&r).await.unwrap().0;
        assert_eq!(text, "Ok");
        let requests = received(&server).await;
        let chat = &requests.iter().find(|(p, _)| p == "/v1/chat/completions").unwrap().1;
        assert_eq!(chat["max_tokens"], 1024);
    }

    #[tokio::test]
    async fn bare_token_ids_still_catch_a_control_token() {
        // A server that ignores `with_pieces` and answers with plain ids.
        let (base, _server) = serve(vec![
            ("/tokenize", "\"parse_special\":false", json!({ "tokens": [1, 2, 3, 4, 5] }).to_string()),
            ("/tokenize", "", json!({ "tokens": [1, 9, 5] }).to_string()),
        ])
        .await;
        let error = client(&base, None, true).complete(&request()).await.unwrap_err();
        assert!(error.to_string().contains("control token"), "{error}");
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
    async fn text_containing_the_delimiter_is_rejected_before_any_request() {
        let (base, server) = serve(vec![]).await;
        let r = Rewrite { text: "a </text> b".into(), ..request() };
        let error = client(&base, None, false).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("delimiter"), "{error}");
        assert!(received(&server).await.is_empty());
    }

    #[test]
    fn timing_prefers_the_servers_token_count() {
        let mut t = Timing::start();
        for _ in 0..9 {
            t.record(&Chunk::Text("x".into()));
        }
        assert_eq!(t.tokens(), 9);
        t.record(&Chunk::Usage(8));
        assert_eq!(t.tokens(), 8);
        assert!(t.first_token().is_some());
    }

    #[tokio::test]
    async fn text_too_long_for_the_context_is_rejected_before_generating() {
        let (base, server) = serve(vec![]).await;
        let r = Rewrite { text: "x".repeat(30_000), ..request() };
        let error = client(&base, None, false).complete(&r).await.unwrap_err();
        assert!(error.to_string().contains("too long"), "{error}");
        assert!(received(&server).await.is_empty());
    }

    #[test]
    fn parses_event_data() {
        let text = |t: &str| Sse::Chunk(Chunk::Text(t.into()));
        assert_eq!(parse_data(r#"{"choices":[{"delta":{"content":"Hi"}}],"usage":null}"#), [text("Hi")]);
        assert_eq!(
            parse_data(r#"{"choices":[],"usage":{"completion_tokens":8,"prompt_tokens":14}}"#),
            [Sse::Chunk(Chunk::Usage(8))]
        );
        assert_eq!(parse_data("[DONE]"), [Sse::Done]);
        assert_eq!(parse_data(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#), []);
        assert_eq!(parse_data(r#"{"error":{"message":"boom"}}"#), [Sse::Error("boom".into())]);
        assert_eq!(parse_data(r#"{"choices":[{"delta":{"content":"Hi"}}],"error":null}"#), [text("Hi")]);
        // Text that arrives with a truncating ending is delivered before the error.
        let last = parse_data(r#"{"choices":[{"delta":{"content":"end"},"finish_reason":"length"}]}"#);
        assert!(matches!(last.as_slice(), [Sse::Chunk(Chunk::Text(t)), Sse::Error(_)] if t == "end"), "{last:?}");
    }

    #[tokio::test]
    async fn sse_framing_follows_the_spec() {
        // CRLF line endings, a comment, an event name, and a text split over two `data:` lines.
        let (base, _server) = serve_sse(
            ": keep-alive\r\n\r\n\
             event: message\r\ndata: {\"choices\":[{\"delta\":\r\ndata: {\"content\":\"Ok\"}}]}\r\n\r\n\
             data: [DONE]\r\n\r\n",
        )
        .await;
        assert_eq!(client(&base, None, false).complete(&request()).await.unwrap().0, "Ok");
    }

    #[test]
    fn llama_fields_only_for_llama_server() {
        let r = Rewrite { instruction: "x".into(), text: "y".into(), temperature: 0.2 };
        let ours = client("http://h/v1", None, true).body(&r, 128);
        let byo = client("http://h/v1", None, false).body(&r, 128);
        assert_eq!(ours["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(ours["top_k"], 20);
        assert!(byo.get("top_k").is_none() && byo.get("chat_template_kwargs").is_none());
        assert_eq!(byo["stop"][0], "</text>");
        assert_eq!(byo["stream_options"]["include_usage"], true);
    }
}
