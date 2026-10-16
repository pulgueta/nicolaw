//! Fake voice detection and transcription for tests.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::pipeline::Input;
use super::{
    AudioError, Backend, Capture, Engines, Microphone, ModelPaths, Partial, Transcriber, Vad,
};

/// A machine with microphones. The test sends the audio: [`FakeBackend::input`] is the
/// processor input while the fake microphone records.
#[derive(Clone)]
pub struct FakeBackend {
    /// The final text of each segment.
    pub text: String,
    /// The final text of each segment in the refine pass.
    pub refined: String,
    pub microphones: Vec<Microphone>,
    pub input: Arc<Mutex<Option<async_channel::Sender<Input>>>>,
    /// The models that the last `load` received.
    pub loaded: Arc<Mutex<Option<ModelPaths>>>,
    /// The device that the last `open_microphone` received: `Some(None)` is the system
    /// default.
    pub opened: Arc<Mutex<Option<Option<String>>>>,
    /// The model that the last `load_refiner` received, and true while it is loaded.
    pub refiner: Arc<Mutex<Option<std::path::PathBuf>>>,
    pub refiner_loaded: Arc<std::sync::atomic::AtomicBool>,
}

impl FakeBackend {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            refined: text.to_owned(),
            microphones: vec![Microphone {
                id: "fake".into(),
                name: "Micrófono de prueba".into(),
                is_default: true,
            }],
            input: Arc::default(),
            loaded: Arc::default(),
            opened: Arc::default(),
            refiner: Arc::default(),
            refiner_loaded: Arc::default(),
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
        Ok(self.microphones.clone())
    }

    fn load(&self, models: &ModelPaths) -> Result<Engines, AudioError> {
        *self.loaded.lock().unwrap() = Some(models.clone());
        Ok(Engines {
            vad: Box::new(FakeVad),
            transcriber: Box::new(FakeTranscriber::new(&self.text)),
        })
    }

    fn load_refiner(&self, path: &Path) -> Result<Box<dyn Transcriber>, AudioError> {
        *self.refiner.lock().unwrap() = Some(path.to_owned());
        Ok(Box::new(FakeRefiner {
            transcriber: FakeTranscriber::new(&self.refined),
            loaded: LoadedFlag::new(self.refiner_loaded.clone()),
        }))
    }

    fn open_microphone(
        &self,
        device: Option<&str>,
        input: async_channel::Sender<Input>,
    ) -> Result<Box<dyn Capture>, AudioError> {
        *self.opened.lock().unwrap() = Some(device.map(str::to_owned));
        let name = self
            .microphones
            .iter()
            .find(|microphone| match device {
                Some(id) => microphone.id == id,
                None => microphone.is_default,
            })
            .map(|microphone| microphone.name.clone())
            .ok_or(AudioError::NoMicrophone)?;
        Ok(Box::new(FakeCapture {
            input,
            slot: self.input.clone(),
            name,
        }))
    }
}

/// True from the load of the fake refine model until its drop.
struct LoadedFlag(Arc<std::sync::atomic::AtomicBool>);

impl LoadedFlag {
    fn new(flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Self(flag)
    }
}

impl Drop for LoadedFlag {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

struct FakeRefiner {
    transcriber: FakeTranscriber,
    loaded: LoadedFlag,
}

impl Transcriber for FakeRefiner {
    fn begin(&mut self) -> Result<(), AudioError> {
        let _ = &self.loaded;
        self.transcriber.begin()
    }

    fn feed(&mut self, audio: &[f32]) -> Result<Option<Partial>, AudioError> {
        self.transcriber.feed(audio)
    }

    fn finish(&mut self) -> Result<String, AudioError> {
        self.transcriber.finish()
    }
}

struct FakeCapture {
    input: async_channel::Sender<Input>,
    slot: Arc<Mutex<Option<async_channel::Sender<Input>>>>,
    name: String,
}

impl Capture for FakeCapture {
    fn start(&mut self) -> Result<(), AudioError> {
        *self.slot.lock().unwrap() = Some(self.input.clone());
        Ok(())
    }

    fn stop(&mut self) {
        self.slot.lock().unwrap().take();
    }

    fn device_name(&self) -> String {
        self.name.clone()
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
/// An empty `text` is a segment with no words. Like a streaming model, it refuses `begin` while
/// a segment is open.
pub struct FakeTranscriber {
    pub text: String,
    /// Audio samples of the open segment.
    pub samples: usize,
    /// The number of next `feed` calls that fail.
    pub failed_feeds: usize,
    open: bool,
}

impl FakeTranscriber {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            samples: 0,
            failed_feeds: 0,
            open: false,
        }
    }
}

impl Transcriber for FakeTranscriber {
    fn begin(&mut self) -> Result<(), AudioError> {
        if std::mem::replace(&mut self.open, true) {
            return Err(AudioError::Transcription);
        }
        self.samples = 0;
        Ok(())
    }

    fn feed(&mut self, audio: &[f32]) -> Result<Option<Partial>, AudioError> {
        if self.failed_feeds > 0 {
            self.failed_feeds -= 1;
            return Err(AudioError::Transcription);
        }
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
        self.open = false;
        Ok(self.text.clone())
    }
}
