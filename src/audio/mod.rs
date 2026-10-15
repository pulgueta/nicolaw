//! The audio of a hearing: capture, voice activity detection, segments, and transcription.

#[cfg(test)]
pub mod fake;
pub mod native;
pub mod pipeline;
pub mod segmenter;

use std::path::PathBuf;

use pipeline::Input;

/// All audio in the pipeline is 16 kHz mono `f32`.
pub const SAMPLE_RATE: u64 = 16_000;

/// The text of an open segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partial {
    /// Text that does not change any more.
    pub committed: String,
    /// Text that can still change.
    pub tentative: String,
}

/// Voice activity detection for one frame of audio.
pub trait Vad: Send {
    fn is_speech(&mut self, frame: &[f32]) -> Result<bool, AudioError>;
}

/// Speech to text, one segment at a time: `begin`, then `feed` for each block of audio, then
/// `finish`. A model that is not a streaming model gives no partial text.
pub trait Transcriber: Send {
    fn begin(&mut self) -> Result<(), AudioError>;
    /// Returns the new partial text, if it changed.
    fn feed(&mut self, audio: &[f32]) -> Result<Option<Partial>, AudioError>;
    /// Returns the final text of the segment.
    fn finish(&mut self) -> Result<String, AudioError>;
}

/// The model files for a hearing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPaths {
    pub live: PathBuf,
    pub vad: PathBuf,
    /// The model of the optional refine pass.
    pub refine: Option<PathBuf>,
}

/// The loaded models of a hearing.
pub struct Engines {
    pub vad: Box<dyn Vad>,
    pub transcriber: Box<dyn Transcriber>,
    pub refiner: Option<Box<dyn Transcriber>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Microphone {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// An open microphone. It sends 16 kHz mono frames to the processor input while it records.
pub trait Capture: Send {
    fn start(&mut self) -> Result<(), AudioError>;
    fn stop(&mut self);
}

/// What a hearing needs from the machine: microphones and models. The app uses
/// [`native::NativeBackend`]; tests use a fake. The functions block: do not call them on the
/// UI thread.
pub trait Backend: Send + Sync {
    fn microphones(&self) -> Result<Vec<Microphone>, AudioError>;
    fn load(&self, models: &ModelPaths) -> Result<Engines, AudioError>;
    /// Asks for the microphone permission if necessary, then opens the microphone. `device` is
    /// a [`Microphone::id`], or `None` for the system default.
    fn open_microphone(
        &self,
        device: Option<&str>,
        input: async_channel::Sender<Input>,
    ) -> Result<Box<dyn Capture>, AudioError>;
}

/// A problem that stops or interrupts a hearing recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// No live transcription model or no voice detection model is installed.
    NoModel,
    NoMicrophone,
    PermissionDenied,
    /// The model is in use by an other recording.
    Busy,
    ModelLoad,
    /// The microphone cannot open, or it stopped during the recording.
    Microphone,
    Transcription,
    Storage,
    /// The audio file cannot be written.
    AudioFile,
}

impl AudioError {
    /// The message for the user.
    pub fn message(self) -> &'static str {
        match self {
            Self::NoModel => {
                "No hay un modelo de voz instalado. Seleccione Descargar para instalarlo."
            }
            Self::NoMicrophone => {
                "Nicolaw no encuentra un micrófono. Conecte un micrófono y seleccione Iniciar."
            }
            Self::PermissionDenied => {
                "Nicolaw no tiene permiso para usar el micrófono. Abra Configuración del Sistema \
                 > Privacidad y seguridad > Micrófono y active el permiso."
            }
            Self::Busy => {
                "El modelo de voz está en uso. Finalice la otra audiencia y seleccione Iniciar."
            }
            Self::ModelLoad => {
                "Nicolaw no puede cargar el modelo de voz. Descargue el modelo de nuevo."
            }
            Self::Microphone => {
                "El micrófono dejó de funcionar. Nicolaw guardó la transcripción hasta este punto."
            }
            Self::Transcription => "La transcripción de un segmento falló.",
            Self::Storage => "Nicolaw no puede guardar la transcripción.",
            Self::AudioFile => "Nicolaw no puede guardar el audio de la audiencia.",
        }
    }
}
