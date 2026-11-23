//! Phone event streams. A reconnect reads the latest answer snapshot for the same device and request.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

const ASK_INTERVAL: Duration = Duration::from_secs(15);
const STREAM_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Answer {
    pub cancel: Arc<AtomicBool>,
    state: Mutex<AnswerState>,
}

#[derive(Default)]
struct AnswerState {
    sequence: u64,
    text: String,
    done: bool,
    error: bool,
}

impl Answer {
    pub fn push(&self, delta: &str) {
        let mut state = self.state.lock().unwrap();
        if state.done { return; }
        state.text.push_str(delta);
        state.sequence += 1;
    }

    pub fn finish(&self, result: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.error = result.is_none();
        if let Some(text) = result { state.text = text; }
        state.done = true;
        state.sequence += 1;
    }

    fn snapshot(&self, after: u64, request: &str) -> Option<(u64, bool, Value)> {
        let state = self.state.lock().unwrap();
        (state.sequence > after || state.done).then(|| {
            (state.sequence, state.done, json!({
                "requestId": request, "sequence": state.sequence, "text": state.text,
                "done": state.done, "error": state.error,
            }))
        })
    }
}

struct Job {
    request: String,
    episode: String,
    answer: Arc<Answer>,
    started: Instant,
}

#[derive(Default)]
pub struct Events {
    jobs: Mutex<HashMap<u64, Job>>,
    asr: Mutex<HashMap<String, Value>>,
    streams: Arc<Mutex<HashMap<u64, usize>>>,
}

impl Events {
    pub fn start(&self, device: u64, episode: &str) -> Option<(String, Arc<Answer>)> {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.get(&device).is_some_and(|job| {
            job.started.elapsed() < ASK_INTERVAL ||
                (!job.answer.state.lock().unwrap().done && job.started.elapsed() < STREAM_TIMEOUT)
        }) {
            return None;
        }
        if let Some(job) = jobs.remove(&device) { job.answer.cancel.store(true, Ordering::Relaxed); }
        let request = uuid::Uuid::new_v4().to_string();
        let answer = Arc::new(Answer { cancel: Arc::new(AtomicBool::new(false)), state: Mutex::default() });
        jobs.insert(device, Job { request: request.clone(), episode: episode.into(), answer: answer.clone(), started: Instant::now() });
        Some((request, answer))
    }

    pub fn answer(&self, device: u64, request: &str, episode: &str) -> Option<Arc<Answer>> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(&device)?;
        (job.request == request && job.episode == episode).then(|| job.answer.clone())
    }

    pub fn remove(&self, device: u64) {
        if let Some(job) = self.jobs.lock().unwrap().remove(&device) {
            job.answer.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        for (_, job) in self.jobs.lock().unwrap().drain() {
            job.answer.cancel.store(true, Ordering::Relaxed);
        }
        self.asr.lock().unwrap().clear();
    }

    pub fn note_asr(&self, episode: &str, value: Value) {
        let mut entries = self.asr.lock().unwrap();
        if entries.len() >= 64 && !entries.contains_key(episode) { entries.clear(); }
        entries.insert(episode.to_owned(), value);
    }

    pub fn connect(&self, device: u64) -> Option<StreamSlot> {
        let mut streams = self.streams.lock().unwrap();
        if streams.values().sum::<usize>() >= 32 { return None; }
        let count = streams.entry(device).or_default();
        if *count >= 2 { return None; }
        *count += 1;
        Some(StreamSlot { device, streams: self.streams.clone() })
    }

    pub fn asr(&self, episode: &str) -> Option<Value> { self.asr.lock().unwrap().get(episode).cloned() }
}

pub struct StreamSlot {
    device: u64,
    streams: Arc<Mutex<HashMap<u64, usize>>>,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut streams = self.streams.lock().unwrap();
        if let Some(count) = streams.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 { streams.remove(&self.device); }
        }
    }
}

pub struct Stream {
    pub gateway: Arc<super::remote::Gateway>,
    pub device: u64,
    pub episode: String,
    pub question: Option<(String, Arc<Answer>)>,
    sequence: u64,
    last_asr: Option<Value>,
    started: Instant,
    access_expiry: Option<i64>,
    next_ping: Instant,
    done: bool,
    _slot: StreamSlot,
}

impl Stream {
    pub fn new(gateway: Arc<super::remote::Gateway>, device: u64, episode: String, question: Option<(String, Arc<Answer>)>, after: u64, slot: StreamSlot, access_expiry: Option<i64>) -> Self {
        Self { gateway, device, episode, question, sequence: after,
            last_asr: None, started: Instant::now(), access_expiry, next_ping: Instant::now(), done: false, _slot: slot }
    }

    fn frame(&mut self) -> Option<String> {
        loop {
            if self.done || self.started.elapsed() >= STREAM_TIMEOUT || !self.gateway.device_active(self.device)
                || self.access_expiry.is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp()) {
                return None;
            }
            if let Some((request, answer)) = &self.question {
                if let Some((sequence, done, data)) = answer.snapshot(self.sequence, request) {
                    self.sequence = sequence;
                    self.done = done;
                    return Some(format!("id: {sequence}\nevent: llm\ndata: {data}\n\n"));
                }
            }
            let asr = self.gateway.events.asr(&self.episode);
            if asr != self.last_asr {
                self.last_asr = asr;
                if let Some(data) = &self.last_asr { return Some(format!("event: asr\ndata: {data}\n\n")); }
            }
            if Instant::now() >= self.next_ping {
                self.next_ping = Instant::now() + Duration::from_secs(5);
                return Some(": connected\n\n".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Stream {
    pub fn write_to(mut self, mut writer: impl Write) {
        let result = (|| -> std::io::Result<()> {
            // tiny_http buffers ordinary bodies. Flush each SSE frame so the first delta arrives before completion.
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nTransfer-Encoding: chunked\r\n\r\n")?;
            writer.flush()?;
            while let Some(frame) = self.frame() {
                write!(writer, "{:x}\r\n", frame.len())?;
                writer.write_all(frame.as_bytes())?;
                writer.write_all(b"\r\n")?;
                writer.flush()?;
            }
            writer.write_all(b"0\r\n\r\n")?;
            writer.flush()
        })();
        // A broken connection may reconnect and catch up from the latest snapshot.
        let _ = result;
    }
}
