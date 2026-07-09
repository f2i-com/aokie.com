use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aokie_ai::runtimes::onnx_tts::OnnxTtsRuntime;
use aokie_ai::runtimes::parakeet_onnx::ParakeetOnnxRuntime;
use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_PORT: u16 = 17_920;
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

const STT_MODEL_ID: &str = "parakeet-tdt-0.6b";
const TTS_MODEL_ID: &str = "pocket-tts";

pub struct VoiceServer {
    paths: ModelPaths,
    max_body_bytes: usize,
    stt: Mutex<Option<ParakeetOnnxRuntime>>,
    tts: Mutex<Option<OnnxTtsRuntime>>,
}

impl VoiceServer {
    pub fn from_app_data(max_body_bytes: usize) -> Result<Self, String> {
        let app_data = aokie_core::paths::app_data_dir()
            .ok_or_else(|| "no app_data_dir for the voice models".to_string())?;
        Ok(Self::new(
            ModelPaths::from_app_data_dir(app_data),
            max_body_bytes,
        ))
    }

    pub fn new(paths: ModelPaths, max_body_bytes: usize) -> Self {
        Self {
            paths,
            max_body_bytes,
            stt: Mutex::new(None),
            tts: Mutex::new(None),
        }
    }

    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    fn transcribe(&self, samples_16k: &[f32]) -> Result<String, AppError> {
        if !self.paths.stt_files_present() {
            return Err(AppError::new(
                503,
                format!(
                    "STT model files are not present under {}",
                    self.paths.stt_dir.display()
                ),
            ));
        }

        let mut guard = self
            .stt
            .lock()
            .map_err(|_| AppError::new(500, "STT engine lock poisoned"))?;
        if guard.is_none() {
            ensure_ort_dylib();
            let started = Instant::now();
            let rt = ParakeetOnnxRuntime::load(
                &self.paths.stt_dir.join("encoder.int8.onnx"),
                &self.paths.stt_dir.join("decoder_joint.int8.onnx"),
                &self.paths.stt_dir.join("tokenizer.model"),
                2,
            )
            .map_err(|e| AppError::new(500, format!("STT load failed: {e}")))?;
            eprintln!("[aokie-voice-server] STT loaded in {:?}", started.elapsed());
            *guard = Some(rt);
        }

        let rt = guard
            .as_mut()
            .ok_or_else(|| AppError::new(500, "STT engine unavailable after load"))?;
        rt.transcribe(samples_16k)
            .map(|s| s.trim().to_string())
            .map_err(|e| AppError::new(500, format!("STT transcribe failed: {e}")))
    }

    fn synthesize_wav(&self, input: &str, voice: &str) -> Result<Vec<u8>, AppError> {
        if !self.paths.tts_files_present() {
            return Err(AppError::new(
                503,
                format!(
                    "TTS model files are not present under {}",
                    self.paths.tts_dir.display()
                ),
            ));
        }

        let mut guard = self
            .tts
            .lock()
            .map_err(|_| AppError::new(500, "TTS engine lock poisoned"))?;
        if guard.is_none() {
            ensure_ort_dylib();
            let started = Instant::now();
            let rt = OnnxTtsRuntime::open(&self.paths.tts_dir)
                .map_err(|e| AppError::new(500, format!("TTS load failed: {e}")))?;
            eprintln!("[aokie-voice-server] TTS loaded in {:?}", started.elapsed());
            *guard = Some(rt);
        }

        let rt = guard
            .as_mut()
            .ok_or_else(|| AppError::new(500, "TTS engine unavailable after load"))?;
        let sample_rate = rt.sample_rate();
        let mut f32_samples = Vec::new();
        rt.synthesize_stream(input, voice, |chunk, _rate| {
            f32_samples.extend_from_slice(chunk);
            true
        })
        .map_err(|e| AppError::new(500, format!("TTS synthesize failed: {e}")))?;

        let pcm: Vec<i16> = f32_samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        write_pcm16_wav(&pcm, sample_rate)
            .map_err(|e| AppError::new(500, format!("WAV encode failed: {e}")))
    }
}

#[derive(Debug, Clone)]
pub struct ModelPaths {
    stt_dir: PathBuf,
    tts_dir: PathBuf,
}

impl ModelPaths {
    pub fn from_app_data_dir(app_data: impl Into<PathBuf>) -> Self {
        let app_data = app_data.into();
        Self {
            stt_dir: app_data.join("models").join("parakeet"),
            tts_dir: app_data.join("models").join("pocket_tts_onnx"),
        }
    }

    pub fn stt_dir(&self) -> &Path {
        &self.stt_dir
    }

    pub fn tts_dir(&self) -> &Path {
        &self.tts_dir
    }

    pub fn stt_files_present(&self) -> bool {
        [
            "encoder.int8.onnx",
            "decoder_joint.int8.onnx",
            "tokenizer.model",
        ]
        .iter()
        .all(|name| self.stt_dir.join(name).is_file())
    }

    pub fn tts_files_present(&self) -> bool {
        let bundle_path = self.tts_dir.join("bundle.json");
        if !bundle_path.is_file() {
            return false;
        }

        let tokenizer_ok = std::fs::read_to_string(&bundle_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("tokenizer_file")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .map(|name| self.tts_dir.join(name).is_file())
            .unwrap_or_else(|| self.tts_dir.join("tokenizer.model").is_file());
        if !tokenizer_ok {
            return false;
        }

        [
            "text_conditioner",
            "flow_lm_main",
            "flow_lm_flow",
            "mimi_decoder",
            "mimi_encoder",
        ]
        .iter()
        .all(|stem| {
            self.tts_dir.join(format!("{stem}_int8.onnx")).is_file()
                || self.tts_dir.join(format!("{stem}.onnx")).is_file()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn wav(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "audio/wav",
            body,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct AppError {
    status: u16,
    message: String,
}

impl AppError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn response(self) -> HttpResponse {
        error_response(self.status, self.message)
    }
}

pub fn handle_request(
    server: &VoiceServer,
    method: &str,
    url: &str,
    headers: &[Header],
    body: &[u8],
) -> HttpResponse {
    if body.len() > server.max_body_bytes() {
        return error_response(413, "request body exceeds 32 MiB limit");
    }

    let path = url.split('?').next().unwrap_or(url);
    match (method, path) {
        ("GET", "/health") => health_response(server),
        ("GET", "/v1/models") => models_response(),
        ("POST", "/v1/audio/transcriptions") => {
            handle_transcriptions(server, headers, body).unwrap_or_else(AppError::response)
        }
        ("POST", "/v1/audio/speech") => {
            handle_speech(server, headers, body).unwrap_or_else(AppError::response)
        }
        _ => error_response(404, "unknown route"),
    }
}

fn health_response(server: &VoiceServer) -> HttpResponse {
    HttpResponse::json(
        200,
        json!({
            "status": "ok",
            "stt": server.paths.stt_files_present(),
            "tts": server.paths.tts_files_present(),
        }),
    )
}

fn models_response() -> HttpResponse {
    HttpResponse::json(
        200,
        json!({
            "object": "list",
            "data": [
                { "id": STT_MODEL_ID, "object": "model" },
                { "id": TTS_MODEL_ID, "object": "model" }
            ]
        }),
    )
}

fn handle_transcriptions(
    server: &VoiceServer,
    headers: &[Header],
    body: &[u8],
) -> Result<HttpResponse, AppError> {
    let content_type = header_value(headers, "content-type").unwrap_or("");
    let wav_bytes = if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        parse_multipart_file(body, content_type)?
    } else {
        if !content_type.is_empty()
            && !content_type
                .to_ascii_lowercase()
                .starts_with("application/json")
        {
            return Err(AppError::new(
                400,
                "transcriptions require application/json or multipart/form-data",
            ));
        }
        let request: TranscriptionJson = serde_json::from_slice(body)
            .map_err(|e| AppError::new(400, format!("malformed JSON: {e}")))?;
        let encoded = request
            .audio
            .as_deref()
            .or(request.file.as_deref())
            .ok_or_else(|| AppError::new(400, "missing audio or file field"))?;
        decode_audio_field(encoded)
            .map_err(|e| AppError::new(400, format!("invalid audio field: {e}")))?
    };

    let samples = decode_wav_to_f32_16k(&wav_bytes)
        .map_err(|e| AppError::new(400, format!("invalid WAV: {e}")))?;
    let text = server.transcribe(&samples)?;
    Ok(HttpResponse::json(200, json!({ "text": text })))
}

fn handle_speech(
    server: &VoiceServer,
    headers: &[Header],
    body: &[u8],
) -> Result<HttpResponse, AppError> {
    let content_type = header_value(headers, "content-type").unwrap_or("");
    if !content_type.is_empty()
        && !content_type
            .to_ascii_lowercase()
            .starts_with("application/json")
    {
        return Err(AppError::new(400, "speech requires application/json"));
    }

    let request: SpeechJson = serde_json::from_slice(body)
        .map_err(|e| AppError::new(400, format!("malformed JSON: {e}")))?;
    let input = request
        .input
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(400, "missing input field"))?;
    if let Some(format) = request.response_format.as_deref() {
        if !format.eq_ignore_ascii_case("wav") {
            return Err(AppError::new(
                400,
                "only response_format \"wav\" is supported",
            ));
        }
    }

    let voice = request.voice.as_deref().unwrap_or("");
    Ok(HttpResponse::wav(server.synthesize_wav(input, voice)?))
}

#[derive(Debug, Deserialize)]
struct TranscriptionJson {
    audio: Option<String>,
    file: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpeechJson {
    input: Option<String>,
    voice: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
    response_format: Option<String>,
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

pub fn decode_audio_field(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = value.trim();
    let payload = if trimmed
        .get(..5)
        .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
        .unwrap_or(false)
    {
        let comma = trimmed
            .find(',')
            .ok_or_else(|| "data URL has no comma separator".to_string())?;
        let metadata = &trimmed[..comma];
        if !metadata.to_ascii_lowercase().contains(";base64") {
            return Err("data URL is not base64 encoded".to_string());
        }
        &trimmed[comma + 1..]
    } else {
        trimmed
    };
    decode_base64(payload)
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut sextets: Vec<Option<u8>> = Vec::new();
    let mut saw_padding = false;
    for b in input.bytes().filter(|b| !b.is_ascii_whitespace()) {
        match b {
            b'=' => {
                saw_padding = true;
                sextets.push(None);
            }
            _ if saw_padding => return Err("base64 data after padding".to_string()),
            _ => sextets.push(Some(base64_value(b)?)),
        }
    }

    match sextets.len() % 4 {
        0 => {}
        2 => {
            sextets.push(None);
            sextets.push(None);
        }
        3 => sextets.push(None),
        _ => return Err("invalid base64 length".to_string()),
    }

    let mut out = Vec::with_capacity(sextets.len() / 4 * 3);
    for chunk in sextets.chunks_exact(4) {
        let a = chunk[0].ok_or_else(|| "invalid base64 padding".to_string())?;
        let b = chunk[1].ok_or_else(|| "invalid base64 padding".to_string())?;
        out.push((a << 2) | (b >> 4));
        match (chunk[2], chunk[3]) {
            (Some(c), Some(d)) => {
                out.push(((b & 0x0f) << 4) | (c >> 2));
                out.push(((c & 0x03) << 6) | d);
            }
            (Some(c), None) => {
                out.push(((b & 0x0f) << 4) | (c >> 2));
            }
            (None, None) => {}
            (None, Some(_)) => return Err("invalid base64 padding".to_string()),
        }
    }
    Ok(out)
}

fn base64_value(b: u8) -> Result<u8, String> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' | b'-' => Ok(62),
        b'/' | b'_' => Ok(63),
        _ => Err(format!("invalid base64 byte 0x{b:02x}")),
    }
}

pub fn decode_wav_to_f32_16k(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let cursor = Cursor::new(bytes);
    let mut reader = hound::WavReader::new(cursor).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(format!(
            "expected 16-bit PCM WAV, got {:?} {} bits",
            spec.sample_format, spec.bits_per_sample
        ));
    }
    if spec.channels != 1 && spec.channels != 2 {
        return Err(format!(
            "expected mono or stereo WAV, got {} channels",
            spec.channels
        ));
    }
    if spec.sample_rate == 0 {
        return Err("sample rate must be non-zero".to_string());
    }

    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let channels = spec.channels as usize;
    if samples.len() % channels != 0 {
        return Err("WAV sample count is not frame-aligned".to_string());
    }

    let mono = if channels == 1 {
        samples
            .iter()
            .map(|&s| s as f32 / 32768.0)
            .collect::<Vec<_>>()
    } else {
        samples
            .chunks_exact(2)
            .map(|frame| ((frame[0] as f32) + (frame[1] as f32)) / (2.0 * 32768.0))
            .collect::<Vec<_>>()
    };
    Ok(resample_linear(&mono, spec.sample_rate, 16_000))
}

pub fn write_pcm16_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    if sample_rate == 0 {
        return Err("sample rate must be non-zero".to_string());
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).map_err(|e| e.to_string())?;
        for &sample in samples {
            writer.write_sample(sample).map_err(|e| e.to_string())?;
        }
        writer.finalize().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

fn parse_multipart_file(body: &[u8], content_type: &str) -> Result<Vec<u8>, AppError> {
    let boundary = parse_boundary(content_type)
        .ok_or_else(|| AppError::new(400, "multipart request is missing boundary"))?;
    let marker = format!("--{boundary}").into_bytes();
    let mut cursor = 0usize;

    while let Some(marker_start) = find_subslice(&body[cursor..], &marker).map(|i| cursor + i) {
        let mut part_start = marker_start + marker.len();
        if body.get(part_start..part_start + 2) == Some(b"--") {
            break;
        }
        if body.get(part_start..part_start + 2) != Some(b"\r\n") {
            return Err(AppError::new(400, "malformed multipart boundary"));
        }
        part_start += 2;

        let header_end_rel = find_subslice(&body[part_start..], b"\r\n\r\n")
            .ok_or_else(|| AppError::new(400, "multipart part has no header terminator"))?;
        let header_end = part_start + header_end_rel;
        let data_start = header_end + 4;
        let next_marker = find_subslice(&body[data_start..], &marker)
            .map(|i| data_start + i)
            .ok_or_else(|| AppError::new(400, "multipart part has no closing boundary"))?;
        let mut data_end = next_marker;
        if data_end >= 2 && body.get(data_end - 2..data_end) == Some(b"\r\n") {
            data_end -= 2;
        }

        let headers = std::str::from_utf8(&body[part_start..header_end])
            .map_err(|_| AppError::new(400, "multipart headers are not UTF-8"))?;
        let lower_headers = headers.to_ascii_lowercase();
        if lower_headers.contains("content-disposition:")
            && (lower_headers.contains("name=\"file\"") || lower_headers.contains("filename="))
        {
            return Ok(body[data_start..data_end].to_vec());
        }

        cursor = next_marker;
    }

    Err(AppError::new(400, "multipart request has no file part"))
}

fn parse_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        Some(value.trim().trim_matches('"').to_string())
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn error_response(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(
        status,
        json!({
            "error": {
                "message": message.into(),
                "type": "invalid_request_error"
            }
        }),
    )
}

fn ensure_ort_dylib() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["onnxruntime.dll", "onnxruntime_1.25.0.dll"] {
                let dll = dir.join(name);
                if dll.exists() {
                    std::env::set_var("ORT_DYLIB_PATH", &dll);
                    eprintln!("[aokie-voice-server] ORT_DYLIB_PATH -> {}", dll.display());
                    return;
                }
            }
        }
    }
}

pub fn resolve_port<I>(args: I, env_port: Option<&str>) -> Result<u16, String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into).skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--port" {
            let value = iter
                .next()
                .ok_or_else(|| "--port requires a numeric value".to_string())?;
            return parse_port(&value, "--port");
        }
        if let Some(value) = arg.strip_prefix("--port=") {
            return parse_port(value, "--port");
        }
        return Err(format!("unknown argument {arg:?}"));
    }

    if let Some(value) = env_port {
        if !value.trim().is_empty() {
            return parse_port(value, "AOKIE_VOICE_PORT");
        }
    }
    Ok(DEFAULT_PORT)
}

fn parse_port(value: &str, source: &str) -> Result<u16, String> {
    value
        .trim()
        .parse::<u16>()
        .map_err(|e| format!("{source} must be a TCP port number: {e}"))
}

pub fn run_from_env() -> Result<(), String> {
    let env_port = std::env::var("AOKIE_VOICE_PORT").ok();
    let port = resolve_port(std::env::args(), env_port.as_deref())?;
    let server = VoiceServer::from_app_data(MAX_BODY_BYTES)?;
    run_http(server, port)
}

pub fn run_http(server: VoiceServer, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("[aokie-voice-server] listening on http://127.0.0.1:{port}");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("[aokie-voice-server] accept failed: {e}");
                continue;
            }
        };

        let response = match read_http_request(&mut stream, server.max_body_bytes()) {
            Ok(request) => handle_request(
                &server,
                &request.method,
                &request.url,
                &request.headers,
                &request.body,
            ),
            Err(err) => err.response(),
        };

        if let Err(e) = write_http_response(&mut stream, response) {
            eprintln!("[aokie-voice-server] respond failed: {e}");
        }
    }
    Ok(())
}

struct RawHttpRequest {
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Vec<u8>,
}

fn read_http_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<RawHttpRequest, AppError> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| AppError::new(400, format!("failed to read request line: {e}")))?;
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    if request_line.is_empty() {
        return Err(AppError::new(400, "empty HTTP request"));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| AppError::new(400, "missing HTTP method"))?
        .to_string();
    let url = parts
        .next()
        .ok_or_else(|| AppError::new(400, "missing HTTP path"))?
        .to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| AppError::new(400, format!("failed to read header: {e}")))?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(AppError::new(400, "malformed HTTP header"));
        };
        headers.push(Header::new(name.trim(), value.trim()));
    }

    if header_value(&headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return Err(AppError::new(
            400,
            "chunked request bodies are not supported",
        ));
    }

    let content_length = match header_value(&headers, "content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|e| AppError::new(400, format!("invalid Content-Length: {e}")))?,
        None => 0,
    };
    if content_length > max_body_bytes {
        return Err(AppError::new(413, "request body exceeds 32 MiB limit"));
    }

    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|e| AppError::new(400, format!("failed to read request body: {e}")))?;

    Ok(RawHttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&response.body))
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "HTTP",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aokie-voice-server-test-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn test_server(max_body_bytes: usize) -> (TestDir, VoiceServer) {
        let tmp = TestDir::new();
        let server = VoiceServer::new(ModelPaths::from_app_data_dir(tmp.path()), max_body_bytes);
        (tmp, server)
    }

    fn json_header() -> Vec<Header> {
        vec![Header::new("Content-Type", "application/json")]
    }

    fn decode_json(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    fn make_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
        write_pcm16_wav(samples, sample_rate).unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn wav_decode_mono_pcm16_passthrough() {
        let wav = make_wav(&[0, 16_384, -16_384, 32_767], 16_000);
        let samples = decode_wav_to_f32_16k(&wav).unwrap();
        assert_eq!(samples.len(), 4);
        assert!((samples[0] - 0.0).abs() < 0.0001);
        assert!((samples[1] - 0.5).abs() < 0.0001);
        assert!((samples[2] + 0.5).abs() < 0.0001);
        assert!((samples[3] - (32_767.0 / 32_768.0)).abs() < 0.0001);
    }

    #[test]
    fn wav_decode_stereo_downmixes_to_mono() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for sample in [16_384i16, 0, 0, -16_384] {
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        let samples = decode_wav_to_f32_16k(&cursor.into_inner()).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.25).abs() < 0.0001);
        assert!((samples[1] + 0.25).abs() < 0.0001);
    }

    #[test]
    fn wav_decode_resamples_to_16khz() {
        let wav = make_wav(&[0, 8192, 16_384, 24_576], 8_000);
        let samples = decode_wav_to_f32_16k(&wav).unwrap();
        assert_eq!(samples.len(), 8);
        assert!((samples[0] - 0.0).abs() < 0.0001);
        assert!((samples[2] - 0.25).abs() < 0.0001);
        assert!((samples[4] - 0.5).abs() < 0.0001);
        assert!(samples.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn decodes_bare_base64_audio_field() {
        let bytes = b"RIFFtest";
        assert_eq!(decode_audio_field(&b64(bytes)).unwrap(), bytes);
    }

    #[test]
    fn decodes_data_url_audio_field() {
        let bytes = b"WAVEdata";
        let data_url = format!("data:audio/wav;base64,{}", b64(bytes));
        assert_eq!(decode_audio_field(&data_url).unwrap(), bytes);
    }

    #[test]
    fn audio_field_wins_over_file_field() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({
            "audio": format!("data:audio/wav;base64,{}", b64(&wav)),
            "file": "not-valid-base64@@",
            "model": STT_MODEL_ID
        })
        .to_string()
        .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("STT model files"),
            "audio should win over the malformed file alias"
        );
    }

    #[test]
    fn file_alias_accepts_bare_base64_wav() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "file": b64(&wav), "model": STT_MODEL_ID })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("STT model files"));
    }

    #[test]
    fn missing_audio_and_file_returns_json_4xx() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            br#"{"model":"x"}"#,
        );
        assert_eq!(response.status, 400);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing audio"));
    }

    #[test]
    fn malformed_json_returns_json_4xx() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            br#"{"audio":"nope""#,
        );
        assert_eq!(response.status, 400);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("malformed JSON"));
    }

    #[test]
    fn oversized_body_is_rejected() {
        let (_tmp, server) = test_server(8);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            b"123456789",
        );
        assert_eq!(response.status, 413);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn unknown_route_returns_404() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(&server, "GET", "/missing", &[], b"");
        assert_eq!(response.status, 404);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn wav_emit_round_trips_through_reader() {
        let input = [-32_768, -1, 0, 1, 32_767];
        let wav = write_pcm16_wav(&input, 24_000).unwrap();
        let mut reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 24_000);
        assert_eq!(spec.bits_per_sample, 16);
        let output = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn transcription_with_missing_models_returns_graceful_error() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "audio": b64(&wav), "model": STT_MODEL_ID })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("STT model files"));
    }

    #[test]
    fn speech_with_missing_models_returns_graceful_error() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello","voice":"alba","response_format":"wav"}"#,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TTS model files"));
    }

    #[test]
    fn parses_multipart_file_part() {
        let wav = make_wav(&[1, 2, 3], 16_000);
        let boundary = "abc123";
        let mut body = Vec::new();
        body.extend_from_slice(b"--abc123\r\n");
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
        body.extend_from_slice(&wav);
        body.extend_from_slice(b"\r\n--abc123--\r\n");
        let parsed =
            parse_multipart_file(&body, &format!("multipart/form-data; boundary={boundary}"))
                .unwrap();
        assert_eq!(parsed, wav);
    }

    #[test]
    fn health_checks_presence_without_loading_models() {
        let (tmp, server) = test_server(MAX_BODY_BYTES);
        fs::create_dir_all(tmp.path().join("models/parakeet")).unwrap();
        for name in [
            "encoder.int8.onnx",
            "decoder_joint.int8.onnx",
            "tokenizer.model",
        ] {
            fs::write(tmp.path().join("models/parakeet").join(name), b"x").unwrap();
        }
        let response = handle_request(&server, "GET", "/health", &[], b"");
        assert_eq!(response.status, 200);
        let value = decode_json(&response.body);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["stt"], true);
        assert_eq!(value["tts"], false);
    }
}
