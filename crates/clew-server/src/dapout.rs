//! The DAP side of clew-server's own debug adapters (`php-debug`,
//! `erlang-debug`): responses and events written a whole frame at a time,
//! numbered, and the program's end said once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use clew_core::dapwire::Utf8Carry;
use clew_core::framing::write_frame;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

/// The writer: shared by the request loop and the tasks that report a
/// stop, the program's output, its end.
#[derive(Clone)]
pub(crate) struct Out {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    seq: Arc<AtomicI64>,
    /// `terminated` is sent once, whoever notices the end first.
    terminated: Arc<AtomicBool>,
}

impl Out {
    pub(crate) fn new<O: AsyncWrite + Unpin + Send + 'static>(output: O) -> Out {
        Out {
            writer: Arc::new(Mutex::new(Box::new(output))),
            seq: Arc::new(AtomicI64::new(0)),
            terminated: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn send(&self, mut message: Value) {
        message["seq"] = json!(self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let mut writer = self.writer.lock().await;
        let _ = write_frame(&mut *writer, &message).await;
        let _ = writer.flush().await;
    }

    /// Answer `request`: its body, or why it failed.
    pub(crate) async fn respond(&self, request: &Value, body: Result<Value, String>) {
        let mut response = json!({
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
        });
        match body {
            Ok(body) => {
                response["success"] = json!(true);
                response["body"] = body;
            }
            Err(message) => {
                response["success"] = json!(false);
                response["message"] = json!(message);
            }
        }
        self.send(response).await;
    }

    pub(crate) async fn event(&self, event: &str, body: Value) {
        self.send(json!({ "type": "event", "event": event, "body": body }))
            .await;
    }

    pub(crate) async fn output(&self, category: &str, text: String) {
        self.event("output", json!({ "category": category, "output": text }))
            .await;
    }

    /// `exited` (with the program's code, when it is known) then
    /// `terminated` — once.
    pub(crate) async fn terminated(&self, code: Option<i32>) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(code) = code {
            self.event("exited", json!({ "exitCode": code })).await;
        }
        self.event("terminated", json!({})).await;
    }

    /// Whether the end was said.
    pub(crate) fn has_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }
}

/// Send what `stream` (the program's stdout or stderr) prints as `category`
/// output, as it comes, a character split across two reads kept whole.
pub(crate) async fn forward(
    mut stream: Box<dyn AsyncRead + Unpin + Send>,
    category: &'static str,
    out: Out,
) {
    let mut buf = [0u8; 4096];
    let mut text = Utf8Carry::default();
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(whole) = text.push(&buf[..n]) {
            out.output(category, whole).await;
        }
    }
    if let Some(rest) = text.finish() {
        out.output(category, rest).await;
    }
}

/// A DAP client over in-memory pipes, for driving an adapter in a test.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::Duration;

    use clew_core::framing::{read_message, write_frame};
    use serde_json::{Value, json};
    use tokio::io::BufReader;

    pub(crate) struct Client {
        to: tokio::io::DuplexStream,
        from: BufReader<tokio::io::DuplexStream>,
        seq: i64,
        /// What arrived while waiting for something else.
        pub(crate) seen: Vec<Value>,
    }

    impl Client {
        /// A client, and the adapter's input and output to serve it on.
        pub(crate) fn pipes() -> (Client, tokio::io::DuplexStream, tokio::io::DuplexStream) {
            let (to, adapter_in) = tokio::io::duplex(1 << 20);
            let (adapter_out, from) = tokio::io::duplex(1 << 20);
            let client = Client {
                to,
                from: BufReader::new(from),
                seq: 0,
                seen: Vec::new(),
            };
            (client, adapter_in, adapter_out)
        }

        pub(crate) async fn send(&mut self, command: &str, arguments: Value) -> i64 {
            self.seq += 1;
            let request = json!({
                "seq": self.seq, "type": "request", "command": command, "arguments": arguments,
            });
            write_frame(&mut self.to, &request).await.unwrap();
            self.seq
        }

        async fn next(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(60), read_message(&mut self.from))
                .await
                .expect("the adapter answers in time")
                .unwrap()
                .expect("the adapter is up")
        }

        /// The response to request `seq`.
        pub(crate) async fn answer(&mut self, seq: i64) -> Value {
            loop {
                let message = self.next().await;
                if message["type"] == "response" && message["request_seq"] == seq {
                    return message;
                }
                self.seen.push(message);
            }
        }

        /// Ask, and expect success: the body.
        pub(crate) async fn request(&mut self, command: &str, arguments: Value) -> Value {
            let seq = self.send(command, arguments).await;
            let answer = self.answer(seq).await;
            assert_eq!(answer["success"], true, "{command}: {answer}");
            answer["body"].clone()
        }

        /// The next `event`, from what already arrived first.
        pub(crate) async fn event(&mut self, event: &str) -> Value {
            if let Some(at) = self.seen.iter().position(|m| m["event"] == event) {
                return self.seen.remove(at);
            }
            loop {
                let message = self.next().await;
                if message["event"] == event {
                    return message;
                }
                self.seen.push(message);
            }
        }

        /// The next of any of `events`, from what already arrived first.
        pub(crate) async fn event_any(&mut self, events: &[&str]) -> Value {
            if let Some(at) = self
                .seen
                .iter()
                .position(|m| events.iter().any(|e| m["event"] == *e))
            {
                return self.seen.remove(at);
            }
            loop {
                let message = self.next().await;
                if events.iter().any(|e| message["event"] == *e) {
                    return message;
                }
                self.seen.push(message);
            }
        }

        /// Whatever arrives within `wait`, kept.
        pub(crate) async fn gather(&mut self, wait: Duration) {
            if let Ok(Ok(Some(message))) =
                tokio::time::timeout(wait, read_message(&mut self.from)).await
            {
                self.seen.push(message);
            }
        }

        /// The program's output so far.
        pub(crate) fn output(&self) -> String {
            self.seen
                .iter()
                .filter(|m| m["event"] == "output" && m["body"]["category"] == "stdout")
                .filter_map(|m| m["body"]["output"].as_str())
                .collect()
        }
    }
}
