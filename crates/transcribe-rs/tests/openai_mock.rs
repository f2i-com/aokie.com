//! The OpenAI engine's requests and answers, checked against a server on 127.0.0.1.
//!
//! Nothing here reaches the network: the engine is pointed at a listener this file opens on a loopback
//! port, with an explicit API key so that `OPENAI_API_KEY` from the environment is never read or sent.
//! Each server answers one request and hands back what it received, so a test can say what the engine
//! put on the wire: the path, the `Authorization` header and every multipart field.
//!
//! What this cannot say is how the real OpenAI service answers. The canned replies below follow the
//! documented response shapes; `tests/openai.rs` is the one test that calls the real service and it is
//! ignored unless asked for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use async_openai::config::OpenAIConfig;
use transcribe_rs::remote::openai::{
    OpenAIEngine, OpenAIModel, OpenAIRequestParams, OpenAITimestampGranularity,
};
use transcribe_rs::{RemoteTranscriptionEngine, TranscribeError};

/// One multipart field: its name, the file name when it is a file, and its content.
struct Field {
    name: String,
    filename: Option<String>,
    content: String,
}

/// What the server received.
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn fields(&self) -> Vec<Field> {
        let content_type = self.header("content-type").expect("a content type");
        let boundary = content_type
            .split("boundary=")
            .nth(1)
            .expect("a multipart boundary")
            .trim_matches('"')
            .to_string();
        let body = String::from_utf8_lossy(&self.body).into_owned();
        let delimiter = format!("--{boundary}");
        let mut fields = Vec::new();
        for part in body.split(&delimiter).skip(1) {
            if part.starts_with("--") {
                break; // the closing delimiter
            }
            let part = part.strip_prefix("\r\n").unwrap_or(part);
            let (head, content) = part.split_once("\r\n\r\n").expect("a part with headers");
            let disposition = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-disposition"))
                .expect("a content disposition");
            fields.push(Field {
                name: quoted(disposition, "name=\"").expect("a field name"),
                filename: quoted(disposition, "filename=\""),
                content: content.strip_suffix("\r\n").unwrap_or(content).to_string(),
            });
        }
        fields
    }

    /// The value of a text field; panics unless exactly one field has that name.
    fn text(&self, name: &str) -> String {
        let mut found = self.fields().into_iter().filter(|field| field.name == name);
        let field = found.next().unwrap_or_else(|| panic!("no field {name}"));
        assert!(found.next().is_none(), "field {name} appears twice");
        field.content
    }

    fn has(&self, name: &str) -> bool {
        self.fields().iter().any(|field| field.name == name)
    }
}

/// The text between `key` and the next quote, with `key` ending in a quote character.
fn quoted(line: &str, key: &str) -> Option<String> {
    // `name="` also matches inside `filename="`, so look for it at the start of a parameter.
    let start = line
        .match_indices(key)
        .map(|(index, _)| index)
        .find(|index| *index == 0 || matches!(line.as_bytes()[index - 1], b' ' | b';'))?
        + key.len();
    let rest = &line[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn read_chunked(reader: &mut impl BufRead) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line).expect("a chunk size");
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap(), 16)
            .expect("a hexadecimal chunk size");
        if size == 0 {
            loop {
                let mut trailer = String::new();
                reader
                    .read_line(&mut trailer)
                    .expect("the end of the chunks");
                if trailer.trim().is_empty() {
                    return body;
                }
            }
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk).expect("a chunk");
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).expect("a chunk terminator");
    }
}

/// Serves exactly one request on 127.0.0.1 and answers with `status` and `reply`. Returns the API base
/// URL to give the engine and the receiver for what the server saw. Every read has a ten second limit.
fn serve_once(status: &'static str, reply: &'static str) -> (String, mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("a connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));

        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("a request line");
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("a header line");
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            let (key, value) = line.split_once(':').expect("a header");
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }

        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        let body = if find("transfer-encoding").is_some_and(|value| value.contains("chunked")) {
            read_chunked(&mut reader)
        } else {
            let length: usize = find("content-length")
                .expect("a content length or chunked encoding")
                .parse()
                .expect("a numeric content length");
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("the request body");
            body
        };

        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .expect("the reply");
        stream.flush().ok();
        let _ = send.send(Captured {
            request_line: request_line.trim_end().to_string(),
            headers,
            body,
        });
    });
    (base, receive)
}

fn engine(base: &str) -> OpenAIEngine<OpenAIConfig> {
    OpenAIEngine::with_config(
        OpenAIConfig::new()
            .with_api_base(base)
            .with_api_key("test-key"),
    )
}

/// A one-tenth-second silent WAV in a directory of its own, removed when dropped.
struct Wav {
    dir: PathBuf,
    path: PathBuf,
}

impl Wav {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "transcribe-rs-openai-mock-{}-{test}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let path = dir.join("clip.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("a WAV file");
        for _ in 0..1_600 {
            writer.write_sample(0i16).expect("a sample");
        }
        writer.finalize().expect("a finished WAV file");
        Wav { dir, path }
    }
}

impl Drop for Wav {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn received(receive: &mpsc::Receiver<Captured>) -> Captured {
    receive
        .recv_timeout(Duration::from_secs(10))
        .expect("the server saw a request")
}

#[tokio::test]
async fn a_gpt_4o_request_carries_the_key_the_model_and_the_options_and_reads_a_json_reply() {
    let (base, receive) = serve_once(
        "200 OK",
        r#"{"text":"hello from the mock","usage":{"type":"tokens","input_tokens":10,"output_tokens":3,"total_tokens":13}}"#,
    );
    let wav = Wav::new("gpt4o");

    let result = engine(&base)
        .transcribe_file(
            &wav.path,
            OpenAIRequestParams::builder()
                .model(OpenAIModel::Gpt4oMiniTranscribe)
                .language("en".to_string())
                .prompt("a lecture about audio".to_string())
                .temperature(0.0f32)
                .build()
                .unwrap(),
        )
        .await
        .expect("a transcription");

    assert_eq!(result.text, "hello from the mock");
    assert!(result.segments.is_none());

    let request = received(&receive);
    assert_eq!(
        request.request_line,
        "POST /v1/audio/transcriptions HTTP/1.1"
    );
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert!(
        request
            .header("content-type")
            .is_some_and(|value| value.starts_with("multipart/form-data; boundary=")),
        "the body is multipart form data"
    );
    assert_eq!(request.text("model"), "gpt-4o-mini-transcribe");
    assert_eq!(request.text("response_format"), "json");
    assert_eq!(request.text("language"), "en");
    assert_eq!(request.text("prompt"), "a lecture about audio");
    assert_eq!(request.text("temperature"), "0");
    assert!(
        !request.has("timestamp_granularities[]"),
        "the gpt-4o models take no timestamp granularity"
    );
    let file = request
        .fields()
        .into_iter()
        .find(|field| field.name == "file")
        .expect("a file part");
    assert_eq!(file.filename.as_deref(), Some("clip.wav"));
    assert!(file.content.starts_with("RIFF"), "the file part is the WAV");
}

#[tokio::test]
async fn whisper_asks_for_verbose_json_with_word_timestamps_and_returns_the_words() {
    let (base, receive) = serve_once(
        "200 OK",
        r#"{"language":"english","duration":1.0,"text":"hello world","words":[{"word":"hello","start":0.0,"end":0.4},{"word":"world","start":0.5,"end":0.9}],"usage":{"type":"duration","seconds":1.0}}"#,
    );
    let wav = Wav::new("words");

    let result = engine(&base)
        .transcribe_file(
            &wav.path,
            OpenAIRequestParams::builder()
                .model(OpenAIModel::Whisper1)
                .timestamp_granularity(OpenAITimestampGranularity::Word)
                .build()
                .unwrap(),
        )
        .await
        .expect("a transcription");

    assert_eq!(result.text, "hello world");
    let words = result.segments.expect("word segments");
    assert_eq!(words.len(), 2);
    assert_eq!(words[0].text, "hello");
    assert_eq!(words[1].text, "world");
    assert!((words[0].start - 0.0).abs() < 1e-6 && (words[0].end - 0.4).abs() < 1e-6);
    assert!((words[1].start - 0.5).abs() < 1e-6 && (words[1].end - 0.9).abs() < 1e-6);

    let request = received(&receive);
    assert_eq!(
        request.request_line,
        "POST /v1/audio/transcriptions HTTP/1.1"
    );
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.text("model"), "whisper-1");
    assert_eq!(request.text("response_format"), "verbose_json");
    assert_eq!(request.text("timestamp_granularities[]"), "word");
    assert!(!request.has("language") && !request.has("prompt") && !request.has("temperature"));
}

#[tokio::test]
async fn whisper_segment_timestamps_come_back_as_segments() {
    let (base, receive) = serve_once(
        "200 OK",
        r#"{"language":"english","duration":2.0,"text":"one two","segments":[{"id":0,"seek":0,"start":0.0,"end":1.0,"text":"one","tokens":[1],"temperature":0.0,"avg_logprob":-0.1,"compression_ratio":1.0,"no_speech_prob":0.01},{"id":1,"seek":0,"start":1.0,"end":2.0,"text":"two","tokens":[2],"temperature":0.0,"avg_logprob":-0.1,"compression_ratio":1.0,"no_speech_prob":0.01}],"usage":{"type":"duration","seconds":2.0}}"#,
    );
    let wav = Wav::new("segments");

    let result = engine(&base)
        .transcribe_file(
            &wav.path,
            OpenAIRequestParams::builder()
                .model(OpenAIModel::Whisper1)
                .timestamp_granularity(OpenAITimestampGranularity::Segment)
                .build()
                .unwrap(),
        )
        .await
        .expect("a transcription");

    let segments = result.segments.expect("segments");
    assert_eq!(
        segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert!((segments[1].start - 1.0).abs() < 1e-6 && (segments[1].end - 2.0).abs() < 1e-6);
    assert_eq!(
        received(&receive).text("timestamp_granularities[]"),
        "segment"
    );
}

#[tokio::test]
async fn an_api_error_comes_back_as_an_inference_error_with_the_services_message() {
    let (base, receive) = serve_once(
        "401 Unauthorized",
        r#"{"error":{"message":"Incorrect API key provided: test-key.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#,
    );
    let wav = Wav::new("error");

    let error = engine(&base)
        .transcribe_file(&wav.path, OpenAIRequestParams::default())
        .await
        .expect_err("the service refused the key");

    match error {
        TranscribeError::Inference(message) => {
            assert!(
                message.contains("Incorrect API key provided"),
                "the service's message is kept: {message}"
            );
        }
        other => panic!("expected an inference error, got {other:?}"),
    }
    assert_eq!(
        received(&receive).header("authorization"),
        Some("Bearer test-key")
    );
}
