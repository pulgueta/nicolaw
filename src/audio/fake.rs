//! Fake voice detection and transcription for tests.

use std::sync::{Arc, Mutex};

use super::pipeline::Input;
use super::{
    AudioError, Backend, Capture, Engines, Microphone, ModelPaths, Partial, Transcriber, Vad,
};

/// A machine with one microphone. The test sends the audio: [`FakeBackend::input`] is the
/// processor input while the fake microphone records.
#[derive(Clone)]
pub struct FakeBackend {
    /// The final text of each segment.
    pub text: String,
    pub input: Arc<Mutex<Option<async_channel::Sender<Input>>>>,
    /// The models that the last `load` received.
    pub loaded: Arc<Mutex<Option<ModelPaths>>>,
}

impl FakeBackend {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            input: Arc::default(),
            loaded: Arc::default(),
        }
    }

    /// Sends one frame of audio, as the microphone does. Speech is 0.5, silence is 0.0.
    pub fn send(&self, level: f32, frames: usize) {
        let input = self.input.lock().unwrap();
        let input = input
            .as_ref()
            .expect("the fake microphone is not recording");
        for _ in 0..frames {
            input.try_send(Input::Frame(vec![level; 512])).unwrap();
        }
    }
}

impl Backend for FakeBackend {
    fn microphones(&self) -> Result<Vec<Microphone>, AudioError> {
        Ok(vec![Microphone {
            id: "fake".into(),
            name: "Micrófono de prueba".into(),
            is_default: true,
        }])
    }

    fn load(&self, models: &ModelPaths) -> Result<Engines, AudioError> {
        *self.loaded.lock().unwrap() = Some(models.clone());
        Ok(Engines {
            vad: Box::new(FakeVad),
            transcriber: Box::new(FakeTranscriber::new(&self.text)),
            refiner: None,
        })
    }

    fn open_microphone(
        &self,
        _device: Option<&str>,
        input: async_channel::Sender<Input>,
    ) -> Result<Box<dyn Capture>, AudioError> {
        Ok(Box::new(FakeCapture {
            input,
            slot: self.input.clone(),
        }))
    }
}

struct FakeCapture {
    input: async_channel::Sender<Input>,
    slot: Arc<Mutex<Option<async_channel::Sender<Input>>>>,
}

impl Capture for FakeCapture {
    fn start(&mut self) -> Result<(), AudioError> {
        *self.slot.lock().unwrap() = Some(self.input.clone());
        Ok(())
    }

    fn stop(&mut self) {
        self.slot.lock().unwrap().take();
    }
}

/// Speech is any frame with a sample above 0.1. Tests send frames of 0.5 for speech and of 0.0
/// for silence.
pub struct FakeVad;

impl Vad for FakeVad {
    fn is_speech(&mut self, frame: &[f32]) -> Result<bool, AudioError> {
        Ok(frame.iter().any(|sample| sample.abs() > 0.1))
    }
}

/// Shows "<first words>" + " <last word>" while the segment is open, then returns `text`.
/// An empty `text` is a segment with no words.
pub struct FakeTranscriber {
    pub text: String,
    /// Audio samples of the open segment.
    pub samples: usize,
}

impl FakeTranscriber {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            samples: 0,
        }
    }
}

impl Transcriber for FakeTranscriber {
    fn begin(&mut self) -> Result<(), AudioError> {
        self.samples = 0;
        Ok(())
    }

    fn feed(&mut self, audio: &[f32]) -> Result<Option<Partial>, AudioError> {
        self.samples += audio.len();
        let words: Vec<&str> = self.text.split_whitespace().collect();
        let Some((last, first)) = words.split_last() else {
            return Ok(None);
        };
        Ok(Some(Partial {
            committed: first.join(" "),
            tentative: format!(" {last}"),
        }))
    }

    fn finish(&mut self) -> Result<String, AudioError> {
        Ok(self.text.clone())
    }
}
