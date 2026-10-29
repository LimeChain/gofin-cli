//! Opt-in NDJSON observations. Output backpressure never stalls financial work.
#[cfg(test)]
use gofin_domain::execution_progress::Stage;
use gofin_domain::{GoFinError, execution_progress::Event};
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::mpsc::{SyncSender, sync_channel},
    thread::JoinHandle,
};

pub struct JsonStream {
    context: Option<Value>,
    sender: SyncSender<Value>,
    writer: JoinHandle<std::io::Result<()>>,
}

impl JsonStream {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout())
    }

    pub(crate) fn new(mut output: impl Write + Send + 'static) -> Self {
        let (sender, receiver) = sync_channel::<Value>(32);
        let writer = std::thread::spawn(move || {
            for event in receiver {
                serde_json::to_writer(&mut output, &event)?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
            Ok(())
        });
        Self {
            context: None,
            sender,
            writer,
        }
    }

    pub fn with_context(mut self, context: Value) -> Self {
        self.context = Some(context);
        self
    }

    fn contextualize(&self, mut event: Value) -> Value {
        if let Some(context) = &self.context {
            event["context"] = context.clone();
        }
        event
    }

    pub fn connecting(&self) {
        self.progress_event(json!({"type":"progress","stage":"connecting"}));
    }

    #[cfg(test)]
    pub fn progress(&self, stage: Stage) {
        self.progress_details(stage.into());
    }

    pub fn progress_details(&self, event: Event) {
        let mut value = serde_json::to_value(event).expect("fixed progress event serializes");
        value["type"] = json!("progress");
        self.progress_event(value);
    }

    fn progress_event(&self, event: Value) {
        // Observations may be dropped for a slow consumer; never block dispatch.
        let _ = self.sender.try_send(self.contextualize(event));
    }

    pub fn finish(self, result: Result<Value, GoFinError>) -> i32 {
        let exit_code = i32::from(result.is_err());
        // Financial work has finished before waiting for final output delivery.
        let sent = self
            .sender
            .send(self.contextualize(final_event(result)))
            .is_ok();
        drop(self.sender);
        let written = matches!(self.writer.join(), Ok(Ok(())));
        if sent && written { exit_code } else { 1 }
    }
}

pub fn final_event(result: Result<Value, GoFinError>) -> Value {
    match result {
        Ok(value) => json!({"type":"result","result":value}),
        Err(error) => json!({"type":"error","error":error}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_progress_writer_does_not_block_financial_work() {
        struct Blocked(mpsc::Receiver<()>);
        impl Write for Blocked {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (release, wait) = mpsc::channel();
        let stream = JsonStream::new(Blocked(wait));
        // More observations than the bounded queue can hold still return immediately.
        for _ in 0..100 {
            stream.progress(Stage::Preparing);
        }
        release.send(()).unwrap();
        assert_eq!(stream.finish(Ok(json!({"status":"completed"}))), 1);
    }

    #[test]
    fn error_is_one_terminal_event_with_original_recovery_details() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let stream = JsonStream::new(Capture(bytes.clone()));
        let error = GoFinError::new("EXPIRED", "approval", "Expired.", "Inspect the same plan.");
        assert_eq!(stream.finish(Err(error)), 1);
        let value: Value = serde_json::from_slice(&bytes.lock().unwrap()).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["next_action"], "Inspect the same plan.");
    }
}
