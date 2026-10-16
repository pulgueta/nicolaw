//! The real backend: microphone capture with `handy-recorder`, and voice detection and speech
//! to text with `transcribe-cpp` (ggml, Metal).

use std::path::Path;
use std::sync::Once;
use std::sync::mpsc;

use handy_recorder::{AudioChunk, Channels, ErrorKind, Permission, Recorder, RecorderConfig, Sink};
use transcribe_cpp::{Backend as Compute, Model, ModelOptions, RunOptions, StreamOptions};

use super::pipeline::Input;
use super::{
    AudioError, Backend, Capture, Engines, Microphone, ModelPaths, Partial, SAMPLE_RATE,
    Transcriber, Vad,
};

/// Samples per frame: the Silero VAD frame, 32 ms.
const FRAME_SAMPLES: usize = 512;
/// A frame with a speech probability at or above this value is speech.
const SPEECH_THRESHOLD: f32 = 0.5;

pub struct NativeBackend;

impl Backend for NativeBackend {
    fn microphones(&self) -> Result<Vec<Microphone>, AudioError> {
        let devices = handy_recorder::list_input_devices().map_err(|_| AudioError::Microphone)?;
        Ok(devices
            .into_iter()
            .filter(|device| !device.is_monitor)
            .map(|device| Microphone {
                id: device.id,
                name: device.name,
                is_default: device.is_default,
            })
            .collect())
    }

    fn load(&self, models: &ModelPaths) -> Result<Engines, AudioError> {
        Ok(Engines {
            vad: Box::new(SileroVad::load(&models.vad)?),
            transcriber: Box::new(NativeTranscriber::load(&models.live)?),
        })
    }

    fn load_refiner(&self, path: &Path) -> Result<Box<dyn Transcriber>, AudioError> {
        Ok(Box::new(NativeTranscriber::load(path)?))
    }

    fn open_microphone(
        &self,
        device: Option<&str>,
        input: async_channel::Sender<Input>,
    ) -> Result<Box<dyn Capture>, AudioError> {
        match handy_recorder::permission_status() {
            Permission::Denied => return Err(AudioError::PermissionDenied),
            Permission::NotDetermined if !request_access() => {
                return Err(AudioError::PermissionDenied);
            }
            _ => {}
        }
        let config = RecorderConfig {
            device: device.map(str::to_owned),
            sample_rate: Some(SAMPLE_RATE as u32),
            channels: Channels::MixToMono,
            frames_per_chunk: Some(FRAME_SAMPLES),
            take_headset: false,
        };
        let failed = input.clone();
        let recorder = Recorder::open_with_failure_handler(config, move |_| {
            failed.try_send(Input::Failed).ok();
        })
        .map_err(|error| match error.kind() {
            ErrorKind::PermissionDenied => AudioError::PermissionDenied,
            ErrorKind::DeviceUnavailable => AudioError::NoMicrophone,
            _ => AudioError::Microphone,
        })?;
        // A local diagnostic: the device that is open and the formats, not the audio.
        let info = recorder.info();
        eprintln!(
            "Microphone open: \"{}\" (requested: {}), device format {:?}, sink format {:?}, \
             {} frames per chunk",
            info.device.name,
            device.unwrap_or("system default"),
            info.device_format,
            info.format,
            info.frames_per_chunk,
        );
        Ok(Box::new(NativeCapture { recorder, input }))
    }
}

/// Asks the user for the microphone permission, and waits for the answer. Without the
/// permission, macOS gives silent audio.
#[cfg(target_os = "macos")]
fn request_access() -> bool {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};

    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return false;
    };
    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(move |granted: Bool| {
        sender.send(granted.as_bool()).ok();
    });
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &handler) };
    receiver.recv().unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn request_access() -> bool {
    true
}

/// Sends each chunk of microphone audio to the processor input.
struct ChannelSink(async_channel::Sender<Input>);

impl Sink for ChannelSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        let real = chunk.valid_frames * chunk.channels as usize;
        self.0
            .try_send(Input::Frame(chunk.samples[..real].to_vec()))
            .ok();
    }
}

struct NativeCapture {
    recorder: Recorder<ChannelSink>,
    input: async_channel::Sender<Input>,
}

impl Capture for NativeCapture {
    fn start(&mut self) -> Result<(), AudioError> {
        self.recorder
            .start(ChannelSink(self.input.clone()))
            .map_err(|_| AudioError::Microphone)
    }

    fn stop(&mut self) {
        // The audio until the stop is already in the input.
        self.recorder.stop().ok();
    }

    fn device_name(&self) -> String {
        self.recorder.info().device.name.clone()
    }
}

/// The native library writes its log to stderr. Nicolaw does not show it.
fn quiet() {
    static QUIET: Once = Once::new();
    QUIET.call_once(transcribe_cpp::disable_logging);
}

fn load_error(error: transcribe_cpp::Error) -> AudioError {
    match error {
        transcribe_cpp::Error::Busy(_) => AudioError::Busy,
        _ => AudioError::ModelLoad,
    }
}

fn run_error(error: transcribe_cpp::Error) -> AudioError {
    match error {
        transcribe_cpp::Error::Busy(_) => AudioError::Busy,
        _ => AudioError::Transcription,
    }
}

/// Silero voice detection, on the CPU: it is too small to gain from the GPU.
pub struct SileroVad {
    session: transcribe_cpp::VadSession,
    speech: bool,
}

impl SileroVad {
    pub fn load(path: &Path) -> Result<Self, AudioError> {
        quiet();
        let options = ModelOptions {
            backend: Compute::Cpu,
            device: None,
        };
        let model = Model::load_with(path, &options).map_err(load_error)?;
        let info = model.vad_info().map_err(load_error)?;
        if info.sample_rate as u64 != SAMPLE_RATE || info.frame_samples as usize != FRAME_SAMPLES {
            return Err(AudioError::ModelLoad);
        }
        Ok(Self {
            session: model.vad_session().map_err(load_error)?,
            speech: false,
        })
    }
}

impl Vad for SileroVad {
    fn is_speech(&mut self, frame: &[f32]) -> Result<bool, AudioError> {
        let result = self.session.stream_feed(frame).map_err(run_error)?;
        // A short frame gives no probability yet: keep the last decision.
        if let Some(&probability) = result.probs.last() {
            self.speech = probability >= SPEECH_THRESHOLD;
        }
        Ok(self.speech)
    }
}

enum Command {
    Begin,
    Feed(Vec<f32>),
    Finish,
}

enum Reply {
    Nothing,
    Partial(Partial),
    Text(String),
}

/// Speech to text with a `transcribe-cpp` model. A stream borrows its session, so one
/// dedicated thread owns the session and serves the calls. A streaming model gives partial
/// text; other models transcribe the full segment at `finish`.
pub struct NativeTranscriber {
    /// `None` only during drop.
    commands: Option<mpsc::Sender<Command>>,
    replies: mpsc::Receiver<Result<Reply, AudioError>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for NativeTranscriber {
    /// Stops the thread and waits for it, so that the model is free before the process exits.
    /// A Metal model that is still loaded at exit makes ggml abort.
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

impl NativeTranscriber {
    pub fn load(path: &Path) -> Result<Self, AudioError> {
        quiet();
        let model = Model::load(path).map_err(load_error)?;
        let capabilities = model.capabilities().map_err(load_error)?;
        let session = model.session().map_err(load_error)?;
        let options = RunOptions {
            language: capabilities
                .languages
                .iter()
                .any(|language| language == "es")
                .then(|| "es".to_owned()),
            ..RunOptions::default()
        };
        let streaming = capabilities.supports_streaming;
        let (commands, command_receiver) = mpsc::channel();
        let (reply_sender, replies) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("nicolaw-transcriber".into())
            .spawn(move || serve(session, options, streaming, command_receiver, reply_sender))
            .map_err(|_| AudioError::ModelLoad)?;
        Ok(Self {
            commands: Some(commands),
            replies,
            thread: Some(thread),
        })
    }

    fn call(&self, command: Command) -> Result<Reply, AudioError> {
        self.commands
            .as_ref()
            .and_then(|commands| commands.send(command).ok())
            .ok_or(AudioError::Transcription)?;
        self.replies.recv().map_err(|_| AudioError::Transcription)?
    }
}

impl Transcriber for NativeTranscriber {
    fn begin(&mut self) -> Result<(), AudioError> {
        self.call(Command::Begin).map(|_| ())
    }

    fn feed(&mut self, audio: &[f32]) -> Result<Option<Partial>, AudioError> {
        match self.call(Command::Feed(audio.to_vec()))? {
            Reply::Partial(partial) => Ok(Some(partial)),
            _ => Ok(None),
        }
    }

    fn finish(&mut self) -> Result<String, AudioError> {
        match self.call(Command::Finish)? {
            Reply::Text(text) => Ok(text),
            _ => Ok(String::new()),
        }
    }
}

type Replies = mpsc::Sender<Result<Reply, AudioError>>;

/// The transcriber thread. It stops when the transcriber is dropped.
fn serve(
    mut session: transcribe_cpp::Session,
    options: RunOptions,
    streaming: bool,
    commands: mpsc::Receiver<Command>,
    replies: Replies,
) {
    while let Ok(command) = commands.recv() {
        let reply = match command {
            Command::Begin if streaming => {
                serve_stream(&mut session, &options, &commands, &replies);
                continue;
            }
            Command::Begin => {
                serve_batch(&mut session, &options, &commands, &replies);
                continue;
            }
            // A feed or a finish without a segment.
            Command::Feed(_) | Command::Finish => Err(AudioError::Transcription),
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

/// Serves one segment with a stream.
fn serve_stream(
    session: &mut transcribe_cpp::Session,
    options: &RunOptions,
    commands: &mpsc::Receiver<Command>,
    replies: &Replies,
) {
    let mut stream = match session.stream(options, &StreamOptions::default()) {
        Ok(stream) => stream,
        Err(error) => {
            replies.send(Err(run_error(error))).ok();
            return;
        }
    };
    replies.send(Ok(Reply::Nothing)).ok();
    while let Ok(command) = commands.recv() {
        let reply = match command {
            Command::Feed(audio) => stream.feed(&audio).map_err(run_error).map(|update| {
                if update.committed_changed || update.tentative_changed {
                    let text = stream.text();
                    Reply::Partial(Partial {
                        committed: text.committed,
                        tentative: text.tentative,
                    })
                } else {
                    Reply::Nothing
                }
            }),
            Command::Finish => {
                let reply = stream
                    .finalize()
                    .map(|_| Reply::Text(stream.text().full))
                    .map_err(run_error);
                replies.send(reply).ok();
                return;
            }
            Command::Begin => Err(AudioError::Transcription),
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

/// Serves one segment with a model that is not a streaming model: the audio collects until
/// `finish`, then one run transcribes it.
fn serve_batch(
    session: &mut transcribe_cpp::Session,
    options: &RunOptions,
    commands: &mpsc::Receiver<Command>,
    replies: &Replies,
) {
    replies.send(Ok(Reply::Nothing)).ok();
    let mut audio = Vec::new();
    while let Ok(command) = commands.recv() {
        let reply = match command {
            Command::Feed(block) => {
                audio.extend_from_slice(&block);
                Ok(Reply::Nothing)
            }
            Command::Finish => {
                let reply = match session.run(&audio, options) {
                    Ok(transcript) => Ok(Reply::Text(transcript.text)),
                    // Too little audio has no words.
                    Err(transcribe_cpp::Error::InputTooShort(_)) => Ok(Reply::Text(String::new())),
                    Err(error) => Err(run_error(error)),
                };
                replies.send(reply).ok();
                return;
            }
            Command::Begin => Err(AudioError::Transcription),
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeTranscriber, SileroVad};
    use crate::audio::{Transcriber, Vad};
    use crate::models::{self, ModelDirs, SILERO_VAD};

    fn read_wav(path: &str) -> Vec<f32> {
        hound::WavReader::open(path)
            .unwrap()
            .into_samples::<i16>()
            .map(|sample| sample.unwrap() as f32 / i16::MAX as f32)
            .collect()
    }

    /// Transcribes a Spanish WAV file with an installed speech model (Nemotron Streaming 3.5,
    /// else Cohere Transcribe) from Handy's folders. Make the file and run the test with:
    ///
    /// ```sh
    /// say -v Monica -o /tmp/nicolaw-es.aiff "La audiencia empieza a las nueve de la mañana."
    /// afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/nicolaw-es.aiff /tmp/nicolaw-es.wav
    /// NICOLAW_TEST_WAV=/tmp/nicolaw-es.wav cargo test -- --ignored --nocapture real_model
    /// ```
    #[test]
    #[ignore = "needs an installed model and a WAV file"]
    fn real_model_transcribes_spanish_speech() {
        let wav = std::env::var("NICOLAW_TEST_WAV").expect("set NICOLAW_TEST_WAV");
        let model = models::discover(&ModelDirs::system())
            .into_iter()
            .filter(|status| status.model.live)
            .find_map(|status| status.path)
            .expect("no speech model is installed");
        let audio = read_wav(&wav);

        let mut transcriber = NativeTranscriber::load(&model).unwrap();
        transcriber.begin().unwrap();
        for block in audio.chunks(512) {
            if let Some(partial) = transcriber.feed(block).unwrap() {
                println!("partial: {}|{}", partial.committed, partial.tentative);
            }
        }
        let text = transcriber.finish().unwrap();

        println!("model: {}\ntext: {text}", model.display());
        assert!(text.to_lowercase().contains("audiencia"), "text: {text}");
    }

    /// Downloads the Silero VAD (1.2 MB) into a temporary cache, not the user's cache, then
    /// finds speech in a Spanish WAV file (see the test above) and no speech in silence. Needs
    /// the internet:
    ///
    /// ```sh
    /// NICOLAW_TEST_WAV=/tmp/nicolaw-es.wav cargo test -- --ignored --nocapture real_vad
    /// ```
    #[test]
    #[ignore = "needs the internet and a WAV file"]
    fn real_vad_downloads_and_finds_speech() {
        let wav = std::env::var("NICOLAW_TEST_WAV").expect("set NICOLAW_TEST_WAV");
        let root = std::env::temp_dir().join(format!("nicolaw-vad-{}", std::process::id()));
        let dirs = ModelDirs {
            hf_cache: root.join("hub"),
            handy: root.join("handy"),
        };
        let mut calls = Vec::new();

        let path =
            models::download(&SILERO_VAD, &dirs, |done, total| calls.push((done, total))).unwrap();
        let found = models::discover(&dirs)
            .into_iter()
            .find(|status| status.model.key == SILERO_VAD.key)
            .and_then(|status| status.path);
        let mut vad = SileroVad::load(&path).unwrap();
        let speech = read_wav(&wav)
            .as_chunks::<512>()
            .0
            .iter()
            .filter(|frame| vad.is_speech(frame.as_slice()).unwrap())
            .count();
        let silence = (0..30)
            .filter(|_| vad.is_speech(&[0.0; 512]).unwrap())
            .count();

        println!("progress calls: {}, last: {:?}", calls.len(), calls.last());
        println!("speech frames: {speech}, speech in silence: {silence}");
        assert_eq!(found.as_deref(), Some(path.as_path()));
        assert_eq!(calls.last().map(|(done, total)| done == total), Some(true));
        assert!(speech > 20);
        // The detector needs a few frames to see the end of speech.
        assert!(silence < 5);
    }
}
