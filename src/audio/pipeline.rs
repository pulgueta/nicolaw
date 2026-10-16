//! The processing of hearing audio, in two workers:
//!
//! - [`Storage`] writes the WAV file, counts the samples, gives the mic level, and checks the
//!   capture health. It is fast, so slow transcription never delays the audio file.
//! - [`Processor`] does voice detection, segments, live transcription, and the storage of the
//!   segments. A bounded queue feeds it. When the queue is full, the storage sends the position
//!   of the stored audio instead of a copy, and the processor reads that audio from the WAV
//!   file. No audio is lost for the transcription.
//!
//! Both are synchronous, so that tests drive them step by step. The app runs each one on a
//! background task.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read as _, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hound::{SampleFormat, WavSpec, WavWriter};

use super::segmenter::{Output, Segmenter};
use super::{AudioError, SAMPLE_RATE, Transcriber, Vad};
use crate::store::{Segment, Speaker, Store};

/// The WAV header is written again after this many samples (10 s), so that the file stays
/// readable if the app stops.
const FLUSH_SAMPLES: usize = 160_000;
/// The transcription queue keeps at most this many items (about 2 s of frames). When it is
/// full, the storage sends positions in the WAV file instead of copies of the audio.
pub const STT_QUEUE: usize = 64;

/// No frame for this time: the microphone gives no audio.
const NO_FRAMES: Duration = Duration::from_secs(3);
/// Only exact-zero samples for this time (3 s): the microphone gives no signal.
const SILENT_SAMPLES: u64 = 3 * SAMPLE_RATE;
/// Only quiet frames for this time (10 s): the signal is low.
const QUIET_SAMPLES: u64 = 10 * SAMPLE_RATE;
/// A frame with an RMS level below this value (-66 dBFS) is quiet.
const QUIET_RMS: f32 = 0.0005;

/// What the capture and the app send to the storage, in capture order.
#[derive(Debug)]
pub enum Input {
    /// 16 kHz mono audio.
    Frame(Vec<f32>),
    /// The speaker changes at this point of the audio.
    Speaker(Speaker),
    /// The recording pauses: the open segment ends.
    Pause,
    /// The recording continues after a pause.
    Resume,
    /// Check the capture health. The app sends it two times per second.
    Tick,
    /// The microphone stopped working.
    Failed,
}

/// What the storage sends to the processor, in capture order.
#[derive(Debug, PartialEq)]
pub enum Work {
    Frame(Vec<f32>),
    /// The samples `start..end` of the WAV file.
    Stored {
        start: u64,
        end: u64,
    },
    Speaker(Speaker),
    /// End the open segment: the recording paused, or the microphone stopped.
    Flush,
}

/// The state of the capture, independent of speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// No frame yet, for less than 3 s.
    Waiting,
    Ok,
    /// No frame for 3 s.
    NoFrames,
    /// Only exact-zero samples for 3 s.
    Silent,
    /// Samples that are not zero, but quiet, for 10 s.
    Quiet,
}

/// What the workers report to the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The mic level of the last frame, from 0 (silence) to 1 (full scale).
    Level(f32),
    /// The capture health changed.
    Health(Health),
    /// The text of the open segment.
    Partial {
        segment_id: i64,
        speaker: Speaker,
        t_start_ms: i64,
        committed: String,
        tentative: String,
    },
    /// A segment ended and is stored with its final text.
    Final(Segment),
    /// The transcription of a segment failed. It is stored without text, as a marker.
    Untranscribed(Segment),
    /// A segment ended without text and is not kept. Its partial text is no longer valid.
    Discarded {
        segment_id: i64,
    },
    /// The refine pass replaced the text of a segment.
    Refined {
        segment_id: i64,
        text: String,
    },
    /// The user corrected the speaker of a stored segment.
    SpeakerChanged {
        segment_id: i64,
    },
    /// The capture or the audio file stopped working: the recording cannot continue.
    Interrupted(AudioError),
    Error(AudioError),
}

/// The capture health from the frames that arrive.
pub struct HealthMeter {
    /// The start of the capture, or of the capture after a pause.
    since: Instant,
    last_frame: Option<Instant>,
    pub frames: u64,
    pub zero_frames: u64,
    /// The largest absolute sample.
    pub peak: f32,
    /// Samples in the current run of exact-zero frames.
    zero_run: u64,
    /// Samples in the current run of quiet (or exact-zero) frames.
    quiet_run: u64,
}

impl HealthMeter {
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            last_frame: None,
            frames: 0,
            zero_frames: 0,
            peak: 0.0,
            zero_run: 0,
            quiet_run: 0,
        }
    }

    pub fn frame(&mut self, frame: &[f32], now: Instant) {
        self.last_frame = Some(now);
        self.frames += 1;
        let peak = frame
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        self.peak = self.peak.max(peak);
        let samples = frame.len() as u64;
        if peak == 0.0 {
            self.zero_frames += 1;
            self.zero_run += samples;
            self.quiet_run += samples;
        } else if rms(frame) < QUIET_RMS {
            self.zero_run = 0;
            self.quiet_run += samples;
        } else {
            self.zero_run = 0;
            self.quiet_run = 0;
        }
    }

    /// Starts the checks again, for example when the recording continues after a pause.
    pub fn restart(&mut self, now: Instant) {
        self.since = now;
        self.last_frame = None;
        self.zero_run = 0;
        self.quiet_run = 0;
    }

    pub fn state(&self, now: Instant) -> Health {
        let last = self.last_frame.unwrap_or(self.since);
        if now.saturating_duration_since(last) >= NO_FRAMES {
            Health::NoFrames
        } else if self.last_frame.is_none() {
            Health::Waiting
        } else if self.zero_run >= SILENT_SAMPLES {
            Health::Silent
        } else if self.quiet_run >= QUIET_SAMPLES {
            Health::Quiet
        } else {
            Health::Ok
        }
    }
}

/// A destination for the WAV file. Tests use one that fails.
pub trait WavFile: Write + Seek + Send {}

impl<T: Write + Seek + Send> WavFile for T {}

/// Writes the hearing audio and sends it to the processor.
pub struct Storage {
    wav: WavWriter<Box<dyn WavFile>>,
    /// The position of the first sample in the file.
    data_offset: u64,
    unflushed: usize,
    /// The samples written so far: the clock of the recording.
    written: Arc<AtomicU64>,
    meter: HealthMeter,
    /// The last health that the storage reported.
    health: Option<Health>,
    /// False while the recording pauses, and after a failure.
    active: bool,
    /// True after a write error: the storage writes nothing more.
    failed: bool,
    stt: async_channel::Sender<Work>,
    /// The work that did not fit in the full queue, in order. Frames in it are positions in
    /// the file.
    pending: VecDeque<Work>,
}

impl Storage {
    /// Creates the WAV file of the hearing at `path`, with its folder.
    pub fn create(
        path: &Path,
        written: Arc<AtomicU64>,
        stt: async_channel::Sender<Work>,
        now: Instant,
    ) -> Result<Self, AudioError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|_| AudioError::AudioFile)?;
        }
        let file = File::create(path).map_err(|_| AudioError::AudioFile)?;
        let mut storage = Self::with_writer(Box::new(BufWriter::new(file)), written, stt, now)?;
        storage.wav.flush().map_err(|_| AudioError::AudioFile)?;
        storage.data_offset = fs::metadata(path).map_err(|_| AudioError::AudioFile)?.len();
        Ok(storage)
    }

    pub fn with_writer(
        writer: Box<dyn WavFile>,
        written: Arc<AtomicU64>,
        stt: async_channel::Sender<Work>,
        now: Instant,
    ) -> Result<Self, AudioError> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE as u32,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let wav = WavWriter::new(writer, spec).map_err(|_| AudioError::AudioFile)?;
        Ok(Self {
            wav,
            data_offset: 0,
            unflushed: 0,
            written,
            meter: HealthMeter::new(now),
            health: None,
            active: true,
            failed: false,
            stt,
            pending: VecDeque::new(),
        })
    }

    /// The position of the first sample in the WAV file.
    pub fn data_offset(&self) -> u64 {
        self.data_offset
    }

    /// True while some work waits because the processor queue is full.
    #[cfg(test)]
    pub fn behind(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn push(&mut self, input: Input, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        match input {
            Input::Frame(frame) => {
                if self.failed {
                    return events;
                }
                if let Err(error) = self.write(&frame) {
                    self.failed = true;
                    self.active = false;
                    events.push(Event::Interrupted(error));
                    return events;
                }
                self.meter.frame(&frame, now);
                events.push(Event::Level(level(&frame)));
                if self.active {
                    self.check(now, &mut events);
                }
                self.forward(Work::Frame(frame));
            }
            Input::Speaker(speaker) => self.forward(Work::Speaker(speaker)),
            Input::Pause => {
                self.active = false;
                self.forward(Work::Flush);
            }
            Input::Resume => {
                if !self.failed {
                    self.active = true;
                    self.health = None;
                    self.meter.restart(now);
                }
            }
            Input::Tick => {
                if self.active {
                    self.check(now, &mut events);
                }
                self.drain();
            }
            Input::Failed => {
                self.active = false;
                self.forward(Work::Flush);
                events.push(Event::Interrupted(AudioError::Microphone));
            }
        }
        events
    }

    /// Completes the WAV file. Returns the events and the work that is still waiting for the
    /// processor: send it in order.
    pub fn finish(mut self) -> (Vec<Event>, Vec<Work>) {
        let mut events = Vec::new();
        if self.wav.finalize().is_err() && !self.failed {
            events.push(Event::Error(AudioError::AudioFile));
        }
        (events, self.pending.drain(..).collect())
    }

    fn check(&mut self, now: Instant, events: &mut Vec<Event>) {
        let state = self.meter.state(now);
        if self.health != Some(state) {
            self.health = Some(state);
            events.push(Event::Health(state));
        }
    }

    fn write(&mut self, frame: &[f32]) -> Result<(), AudioError> {
        for &sample in frame {
            let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            self.wav
                .write_sample(sample)
                .map_err(|_| AudioError::AudioFile)?;
        }
        self.written
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        self.unflushed += frame.len();
        if self.unflushed >= FLUSH_SAMPLES {
            self.unflushed = 0;
            self.wav.flush().map_err(|_| AudioError::AudioFile)?;
        }
        Ok(())
    }

    /// Sends the work to the processor, after the work that waits. A frame that does not fit
    /// becomes a position in the file.
    fn forward(&mut self, work: Work) {
        self.drain();
        if self.pending.is_empty() {
            if let Err(async_channel::TrySendError::Full(work)) = self.stt.try_send(work) {
                self.queue(work);
            }
        } else {
            self.queue(work);
        }
    }

    fn queue(&mut self, work: Work) {
        let Work::Frame(frame) = work else {
            self.pending.push_back(work);
            return;
        };
        let end = self.wav.len() as u64;
        let start = end - frame.len() as u64;
        match self.pending.back_mut() {
            Some(Work::Stored { end: last, .. }) if *last == start => *last = end,
            _ => self.pending.push_back(Work::Stored { start, end }),
        }
    }

    /// Sends the waiting work while the queue has space.
    fn drain(&mut self) {
        let mut flushed = false;
        while let Some(work) = self.pending.pop_front() {
            if matches!(work, Work::Stored { .. }) && !flushed {
                // The processor reads these samples from the file.
                flushed = true;
                if self.wav.flush().is_err() {
                    self.failed = true;
                }
            }
            match self.stt.try_send(work) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(work)) => {
                    self.pending.push_front(work);
                    return;
                }
                Err(async_channel::TrySendError::Closed(_)) => {
                    self.pending.clear();
                    return;
                }
            }
        }
    }
}

/// Reads stored audio from the WAV file while the storage writes it.
pub struct WavSource {
    file: File,
    data_offset: u64,
}

impl WavSource {
    pub fn open(path: &Path, data_offset: u64) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            data_offset,
        })
    }

    fn read(&mut self, start: u64, end: u64) -> io::Result<Vec<f32>> {
        let mut bytes = vec![0; (end - start) as usize * 2];
        self.file
            .seek(SeekFrom::Start(self.data_offset + start * 2))?;
        self.file.read_exact(&mut bytes)?;
        Ok(bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair) as f32 / i16::MAX as f32)
            .collect())
    }
}

/// The segment that is open now.
struct Current {
    id: i64,
    speaker: Speaker,
    t_start_ms: i64,
    /// True after an error: the segment gets no more audio, and it ends as a marker without
    /// text.
    failed: bool,
}

pub struct Processor {
    hearing_id: i64,
    store: Arc<Mutex<Store>>,
    /// The speaker of the next segment. A [`Work::Speaker`] changes it.
    speaker: Speaker,
    vad: Box<dyn Vad>,
    transcriber: Box<dyn Transcriber>,
    /// The WAV file, for [`Work::Stored`].
    source: Option<WavSource>,
    /// The samples processed so far. Its difference to the storage clock is the delay.
    processed: Arc<AtomicU64>,
    segmenter: Segmenter,
    current: Option<Current>,
    /// True after a voice detection error, so that the error shows one time only.
    vad_failed: bool,
}

impl Processor {
    pub fn new(
        hearing_id: i64,
        store: Arc<Mutex<Store>>,
        speaker: Speaker,
        vad: Box<dyn Vad>,
        transcriber: Box<dyn Transcriber>,
        source: Option<WavSource>,
        processed: Arc<AtomicU64>,
    ) -> Self {
        Self {
            hearing_id,
            store,
            speaker,
            vad,
            transcriber,
            source,
            processed,
            segmenter: Segmenter::default(),
            current: None,
            vad_failed: false,
        }
    }

    pub fn push(&mut self, work: Work) -> Vec<Event> {
        let mut events = Vec::new();
        match work {
            Work::Frame(frame) => self.frame(&frame, &mut events),
            Work::Stored { start, end } => {
                let audio = match self.source.as_mut().map(|source| source.read(start, end)) {
                    Some(Ok(audio)) => audio,
                    _ => {
                        events.push(Event::Error(AudioError::AudioFile));
                        // Silence keeps the positions of the next segments correct.
                        vec![0.0; (end - start) as usize]
                    }
                };
                for frame in audio.chunks(512) {
                    self.frame(frame, &mut events);
                }
            }
            Work::Speaker(speaker) => {
                self.flush(&mut events);
                self.speaker = speaker;
            }
            Work::Flush => self.flush(&mut events),
        }
        events
    }

    /// Ends the open segment.
    pub fn finish(mut self) -> Vec<Event> {
        let mut events = Vec::new();
        self.flush(&mut events);
        events
    }

    fn frame(&mut self, frame: &[f32], events: &mut Vec<Event>) {
        let speech = match self.vad.is_speech(frame) {
            Ok(speech) => speech,
            Err(error) => {
                if !std::mem::replace(&mut self.vad_failed, true) {
                    events.push(Event::Error(error));
                }
                false
            }
        };
        for output in self.segmenter.push(frame, speech) {
            self.handle(output, events);
        }
        self.processed
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
    }

    /// Ends the open segment. The audio before this point is not pre-roll for the next one.
    fn flush(&mut self, events: &mut Vec<Event>) {
        if let Some(output) = self.segmenter.flush() {
            self.handle(output, events);
        }
    }

    fn handle(&mut self, output: Output, events: &mut Vec<Event>) {
        match output {
            Output::Start { t_ms } => self.start(t_ms, events),
            Output::Audio(audio) => self.feed(audio, events),
            Output::End { t_ms } => self.end(t_ms, events),
        }
    }

    fn start(&mut self, t_ms: i64, events: &mut Vec<Event>) {
        let speaker = self.speaker;
        let id = self
            .store
            .lock()
            .unwrap()
            .insert_segment(self.hearing_id, t_ms, speaker);
        let id = match id {
            Ok(id) => id,
            Err(_) => {
                events.push(Event::Error(AudioError::Storage));
                return;
            }
        };
        let failed = match self.transcriber.begin() {
            Ok(()) => false,
            Err(error) => {
                events.push(Event::Error(error));
                true
            }
        };
        self.current = Some(Current {
            id,
            speaker,
            t_start_ms: t_ms,
            failed,
        });
    }

    fn feed(&mut self, audio: Vec<f32>, events: &mut Vec<Event>) {
        let Some(current) = self.current.as_mut().filter(|current| !current.failed) else {
            return;
        };
        match self.transcriber.feed(&audio) {
            Ok(Some(partial)) => events.push(Event::Partial {
                segment_id: current.id,
                speaker: current.speaker,
                t_start_ms: current.t_start_ms,
                committed: partial.committed,
                tentative: partial.tentative,
            }),
            Ok(None) => {}
            Err(error) => {
                current.failed = true;
                events.push(Event::Error(error));
                // Close the stream, so that the next segment can begin.
                self.transcriber.finish().ok();
            }
        }
    }

    fn end(&mut self, t_end_ms: i64, events: &mut Vec<Event>) {
        let Some(current) = self.current.take() else {
            return;
        };
        let text = if current.failed {
            None
        } else {
            match self.transcriber.finish() {
                Ok(text) => Some(text.trim().to_owned()),
                Err(error) => {
                    events.push(Event::Error(error));
                    None
                }
            }
        };
        let segment = Segment {
            id: current.id,
            t_start_ms: current.t_start_ms,
            t_end_ms,
            text: text.clone().unwrap_or_default(),
            speaker: current.speaker,
            is_final: true,
            revision: 0,
            failed: text.is_none(),
        };
        let store = self.store.lock().unwrap();
        let stored = match text.as_deref() {
            // The transcription failed: the gap stays visible in the transcript.
            None => store.fail_segment(current.id, t_end_ms),
            // A segment with no words is noise: it is not kept.
            Some("") => store.delete_segment(current.id),
            Some(text) => store.finish_segment(current.id, text, t_end_ms),
        };
        drop(store);
        if stored.is_err() {
            events.push(Event::Error(AudioError::Storage));
        }
        match text.as_deref() {
            None => events.push(Event::Untranscribed(segment)),
            Some("") => events.push(Event::Discarded {
                segment_id: current.id,
            }),
            Some(_) if stored.is_ok() => events.push(Event::Final(segment)),
            Some(_) => {}
        }
    }
}

/// The audio of one segment, for the refine pass after the hearing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefineJob {
    pub hearing_id: i64,
    pub segment_id: i64,
    pub start_sample: u64,
    pub end_sample: u64,
}

/// The audio after the end of a segment that the refine pass also reads (300 ms), so that the
/// last word is complete.
const REFINE_TAIL: u64 = 4_800;

/// The refine jobs of a hearing: one for each final segment with text.
pub fn refine_jobs(store: &Store, hearing_id: i64) -> rusqlite::Result<Vec<RefineJob>> {
    Ok(store
        .segments(hearing_id)?
        .into_iter()
        .filter(|segment| segment.is_final && !segment.failed && !segment.text.is_empty())
        .map(|segment| RefineJob {
            hearing_id,
            segment_id: segment.id,
            start_sample: segment.t_start_ms as u64 * SAMPLE_RATE / 1000,
            end_sample: segment.t_end_ms as u64 * SAMPLE_RATE / 1000 + REFINE_TAIL,
        })
        .collect())
}

/// Transcribes the audio again with the refine model. Returns `None` when the refine pass
/// fails or gives no text. After a successful `begin`, the session always closes, also after a
/// feed error, so that the next job can begin.
pub fn refine(transcriber: &mut dyn Transcriber, audio: &[f32]) -> Option<String> {
    transcriber.begin().ok()?;
    let fed = transcriber.feed(audio);
    let finished = transcriber.finish();
    fed.ok()?;
    let text = finished.ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Refines the segments of a hearing from its WAV file, one after the other, until `cancel`
/// is set. A refined text replaces the text of its segment; the store keeps the live text.
/// Calls `on_event` for each refined segment. Returns the number of refined segments.
pub fn refine_hearing(
    transcriber: &mut dyn Transcriber,
    store: &Mutex<Store>,
    hearing_id: i64,
    wav: &Path,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> Result<usize, AudioError> {
    let jobs = refine_jobs(&store.lock().unwrap(), hearing_id).map_err(|_| AudioError::Storage)?;
    let mut reader = hound::WavReader::open(wav).map_err(|_| AudioError::AudioFile)?;
    let length = reader.duration() as u64;
    let mut refined = 0;
    for job in jobs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end = job.end_sample.min(length);
        if job.start_sample >= end {
            continue;
        }
        reader
            .seek(job.start_sample as u32)
            .map_err(|_| AudioError::AudioFile)?;
        let audio: Vec<f32> = reader
            .samples::<i16>()
            .take((end - job.start_sample) as usize)
            .map(|sample| sample.map(|sample| sample as f32 / i16::MAX as f32))
            .collect::<Result<_, _>>()
            .map_err(|_| AudioError::AudioFile)?;
        let Some(text) = refine(transcriber, &audio) else {
            continue;
        };
        store
            .lock()
            .unwrap()
            .set_segment_text(job.segment_id, &text)
            .map_err(|_| AudioError::Storage)?;
        refined += 1;
        on_event(Event::Refined {
            segment_id: job.segment_id,
            text,
        });
    }
    Ok(refined)
}

fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    (frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32).sqrt()
}

/// The RMS level of the frame on a 60 dB scale: 0 is -60 dBFS or less, 1 is full scale.
fn level(frame: &[f32]) -> f32 {
    let rms = rms(frame);
    if rms <= 0.0 {
        return 0.0;
    }
    ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{
        Event, Health, HealthMeter, Input, Processor, STT_QUEUE, Storage, WavSource, Work, refine,
        refine_hearing,
    };
    use crate::audio::AudioError;
    use crate::audio::fake::{FakeTranscriber, FakeVad};
    use crate::store::{Speaker, Store};

    const FRAME: usize = 512;

    struct Setup {
        store: Arc<Mutex<Store>>,
        hearing: i64,
        wav: PathBuf,
    }

    fn setup(name: &str) -> Setup {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let dir =
            std::env::temp_dir().join(format!("nicolaw-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Setup {
            store: Arc::new(Mutex::new(store)),
            hearing,
            wav: dir.join("hearings").join(format!("{hearing}.wav")),
        }
    }

    fn processor(setup: &Setup, transcriber: FakeTranscriber) -> Processor {
        Processor::new(
            setup.hearing,
            setup.store.clone(),
            Speaker::Other,
            Box::new(FakeVad),
            Box::new(transcriber),
            None,
            Arc::default(),
        )
    }

    fn frames(processor: &mut Processor, count: usize, level: f32) -> Vec<Event> {
        (0..count)
            .flat_map(|_| processor.push(Work::Frame(vec![level; FRAME])))
            .collect()
    }

    fn finals(events: &[Event]) -> Vec<(String, Speaker)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Final(segment) => Some((segment.text.clone(), segment.speaker)),
                _ => None,
            })
            .collect()
    }

    fn stored_segments(setup: &Setup) -> Vec<crate::store::Segment> {
        setup.store.lock().unwrap().segments(setup.hearing).unwrap()
    }

    #[test]
    fn speech_shows_a_partial_then_becomes_a_final_stored_segment() {
        let setup = setup("flow");
        let mut processor = processor(&setup, FakeTranscriber::new("Yo no estaba allí."));

        let speech = frames(&mut processor, 10, 0.5);
        let open = stored_segments(&setup);
        let silence = frames(&mut processor, 19, 0.0);

        let partial = speech.iter().find_map(|event| match event {
            Event::Partial {
                segment_id,
                speaker,
                committed,
                tentative,
                ..
            } => Some((*segment_id, *speaker, committed.clone(), tentative.clone())),
            _ => None,
        });
        assert_eq!(
            partial,
            Some((
                open[0].id,
                Speaker::Other,
                "Yo no estaba".to_owned(),
                " allí.".to_owned()
            ))
        );
        assert!(!open[0].is_final);
        assert_eq!(
            finals(&silence),
            [("Yo no estaba allí.".to_owned(), Speaker::Other)]
        );
        let stored = stored_segments(&setup);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "Yo no estaba allí.");
        assert_eq!(stored[0].t_start_ms, 0);
        assert_eq!(stored[0].t_end_ms, 320);
        assert!(stored[0].is_final);
    }

    #[test]
    fn a_speaker_marker_in_mid_sentence_ends_the_segment_at_the_marker() {
        let setup = setup("speaker");
        let mut processor = processor(&setup, FakeTranscriber::new("Sí, señor juez."));

        // No silence between the two speakers.
        let mut events = frames(&mut processor, 10, 0.5);
        events.extend(processor.push(Work::Speaker(Speaker::Accused)));
        events.extend(frames(&mut processor, 10, 0.5));
        events.extend(frames(&mut processor, 19, 0.0));

        assert_eq!(
            finals(&events),
            [
                ("Sí, señor juez.".to_owned(), Speaker::Other),
                ("Sí, señor juez.".to_owned(), Speaker::Accused),
            ]
        );
        let stored = stored_segments(&setup);
        // The second segment starts at the marker (10 frames = 320 ms), with no audio of the
        // first speaker as pre-roll.
        assert_eq!(
            stored
                .iter()
                .map(|segment| (segment.speaker, segment.t_start_ms, segment.t_end_ms))
                .collect::<Vec<_>>(),
            [(Speaker::Other, 0, 320), (Speaker::Accused, 320, 640)]
        );
    }

    #[test]
    fn a_marker_during_silence_drops_the_pre_roll_of_the_old_speaker() {
        let setup = setup("speaker-silence");
        let mut processor = processor(&setup, FakeTranscriber::new("No."));

        frames(&mut processor, 5, 0.0);
        processor.push(Work::Speaker(Speaker::Accused));
        let mut events = frames(&mut processor, 3, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));

        assert_eq!(finals(&events), [("No.".to_owned(), Speaker::Accused)]);
        assert_eq!(stored_segments(&setup)[0].t_start_ms, 5 * 32);
    }

    #[test]
    fn a_segment_without_words_is_not_kept() {
        let setup = setup("empty");
        let mut processor = processor(&setup, FakeTranscriber::new(""));

        let mut events = frames(&mut processor, 5, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));

        assert!(finals(&events).is_empty());
        assert!(stored_segments(&setup).is_empty());
    }

    #[test]
    fn a_flush_ends_the_open_segment() {
        let setup = setup("pause");
        let mut processor = processor(&setup, FakeTranscriber::new("Continúe."));
        frames(&mut processor, 5, 0.5);

        let events = processor.push(Work::Flush);

        assert_eq!(finals(&events), [("Continúe.".to_owned(), Speaker::Other)]);
    }

    #[test]
    fn a_failed_transcription_keeps_a_marker_with_its_time_range() {
        let setup = setup("failed");
        let mut transcriber = FakeTranscriber::new("Sí.");
        transcriber.failed_feeds = 1;
        let mut processor = processor(&setup, transcriber);

        let mut events = frames(&mut processor, 5, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));
        events.extend(frames(&mut processor, 5, 0.5));
        events.extend(frames(&mut processor, 19, 0.0));

        let stored = stored_segments(&setup);
        assert_eq!(stored.len(), 2);
        assert!(stored[0].failed && stored[0].is_final && stored[0].text.is_empty());
        assert_eq!((stored[0].t_start_ms, stored[0].t_end_ms), (0, 160));
        assert_eq!(stored[1].text, "Sí.");
        assert!(events.iter().any(
            |event| matches!(event, Event::Untranscribed(segment) if segment.id == stored[0].id)
        ));
        assert_eq!(finals(&events), [("Sí.".to_owned(), Speaker::Other)]);
        let errors = events
            .iter()
            .filter(|event| matches!(event, Event::Error(_)))
            .count();
        assert_eq!(errors, 1);
    }

    fn storage(setup: &Setup) -> (Storage, async_channel::Receiver<Work>, Arc<AtomicU64>) {
        let (sender, receiver) = async_channel::bounded(STT_QUEUE);
        let written = Arc::new(AtomicU64::new(0));
        let storage = Storage::create(&setup.wav, written.clone(), sender, Instant::now()).unwrap();
        (storage, receiver, written)
    }

    #[test]
    fn each_frame_gives_a_mic_level_from_0_to_1() {
        let setup = setup("level");
        let (mut storage, _receiver, _) = storage(&setup);
        let now = Instant::now();

        let silent = storage.push(Input::Frame(vec![0.0; FRAME]), now);
        let loud = storage.push(Input::Frame(vec![1.0; FRAME]), now);
        let quiet = storage.push(Input::Frame(vec![0.01; FRAME]), now);

        let level = |events: &[Event]| match events.first() {
            Some(Event::Level(level)) => *level,
            _ => panic!("no level event"),
        };
        assert_eq!(level(&silent), 0.0);
        assert_eq!(level(&loud), 1.0);
        assert!(level(&quiet) > 0.0 && level(&quiet) < 0.5);
    }

    #[test]
    fn the_wav_has_every_sample_and_the_processor_gets_every_frame_in_order() {
        let setup = setup("wav");
        let (mut storage, receiver, written) = storage(&setup);
        let now = Instant::now();
        for _ in 0..3 {
            storage.push(Input::Frame(vec![0.5; FRAME]), now);
        }
        storage.push(Input::Speaker(Speaker::Accused), now);
        storage.push(Input::Pause, now);

        let (events, pending) = storage.finish();

        assert!(events.is_empty() && pending.is_empty());
        assert_eq!(written.load(Ordering::Relaxed), 3 * FRAME as u64);
        let work: Vec<Work> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert_eq!(work.len(), 5);
        assert_eq!(work[3], Work::Speaker(Speaker::Accused));
        assert_eq!(work[4], Work::Flush);
        let reader = hound::WavReader::open(&setup.wav).unwrap();
        let spec = reader.spec();
        assert_eq!((spec.sample_rate, spec.channels), (16_000, 1));
        assert_eq!(reader.duration() as usize, 3 * FRAME);
        let first: Vec<i16> = reader
            .into_samples::<i16>()
            .take(2)
            .map(Result::unwrap)
            .collect();
        assert_eq!(first, [i16::MAX / 2, i16::MAX / 2]);
    }

    /// A processor that is slower than real time: the test pushes a long recording before the
    /// processor reads anything.
    #[test]
    fn a_slow_processor_does_not_delay_the_audio_file_and_gets_all_audio_in_order() {
        let setup = setup("backlog");
        let (mut storage, receiver, written) = storage(&setup);
        let now = Instant::now();
        let total = STT_QUEUE * 3;

        // The other person speaks, the accused answers without silence, then silence.
        for frame in 0..total {
            if frame == total / 2 {
                storage.push(Input::Speaker(Speaker::Accused), now);
            }
            let level = if frame < total - 25 { 0.5 } else { 0.0 };
            storage.push(Input::Frame(vec![level; FRAME]), now);
        }

        // Every sample is in the file, but the processor queue is full.
        assert_eq!(written.load(Ordering::Relaxed), (total * FRAME) as u64);
        assert!(storage.behind());
        assert_eq!(receiver.len(), STT_QUEUE);

        let data_offset = storage.data_offset();
        let (_, pending) = storage.finish();
        let processed = Arc::new(AtomicU64::new(0));
        let mut processor = Processor::new(
            setup.hearing,
            setup.store.clone(),
            Speaker::Other,
            Box::new(FakeVad),
            Box::new(FakeTranscriber::new("Sí.")),
            Some(WavSource::open(&setup.wav, data_offset).unwrap()),
            processed.clone(),
        );
        let work: Vec<Work> = std::iter::from_fn(|| receiver.try_recv().ok())
            .chain(pending)
            .collect();
        assert!(work.iter().any(|work| matches!(work, Work::Stored { .. })));
        let mut events: Vec<Event> = work
            .into_iter()
            .flat_map(|work| processor.push(work))
            .collect();
        events.extend(processor.finish());

        assert_eq!(processed.load(Ordering::Relaxed), (total * FRAME) as u64);
        let stored = stored_segments(&setup);
        assert_eq!(
            stored
                .iter()
                .map(|segment| (segment.speaker, segment.t_start_ms))
                .collect::<Vec<_>>(),
            [
                (Speaker::Other, 0),
                (Speaker::Accused, (total / 2 * FRAME * 1000 / 16_000) as i64)
            ]
        );
        assert_eq!(finals(&events).len(), 2);
    }

    /// A writer that fails after the WAV header.
    struct FullDisk {
        written: usize,
    }

    impl Write for FullDisk {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if self.written >= 44 {
                return Err(io::Error::other("disk full"));
            }
            self.written += data.len();
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FullDisk {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn a_write_failure_interrupts_the_recording() {
        let (sender, _receiver) = async_channel::bounded(STT_QUEUE);
        let now = Instant::now();
        let mut storage = Storage::with_writer(
            Box::new(FullDisk { written: 0 }),
            Arc::default(),
            sender,
            now,
        )
        .unwrap();

        // The WAV writer buffers nothing: the first frame reaches the disk.
        let events = storage.push(Input::Frame(vec![0.5; 20_000]), now);
        let after = storage.push(Input::Tick, now + Duration::from_secs(10));

        assert_eq!(events, [Event::Interrupted(AudioError::AudioFile)]);
        assert!(after.is_empty());
    }

    #[test]
    fn a_microphone_failure_flushes_the_segment_and_interrupts_the_recording() {
        let setup = setup("mic-failure");
        let (mut storage, receiver, _) = storage(&setup);

        let events = storage.push(Input::Failed, Instant::now());

        assert_eq!(events, [Event::Interrupted(AudioError::Microphone)]);
        assert_eq!(receiver.try_recv(), Ok(Work::Flush));
    }

    fn health_after(frames: &[(f32, u64)]) -> (Health, Instant) {
        let start = Instant::now();
        let mut meter = HealthMeter::new(start);
        let mut now = start;
        for &(level, count) in frames {
            for _ in 0..count {
                now += Duration::from_millis(32);
                meter.frame(&[level; FRAME], now);
            }
        }
        (meter.state(now), now)
    }

    #[test]
    fn exact_zero_frames_for_3_seconds_are_silent() {
        // 93 frames of 32 ms = 2.976 s; 94 frames = 3.008 s.
        assert_eq!(health_after(&[(0.0, 93)]).0, Health::Ok);
        assert_eq!(health_after(&[(0.0, 94)]).0, Health::Silent);
        assert_eq!(health_after(&[(0.0, 200), (0.2, 1)]).0, Health::Ok);
    }

    #[test]
    fn quiet_frames_that_are_not_zero_for_10_seconds_are_quiet() {
        // 313 frames of 32 ms = 10.016 s.
        assert_eq!(health_after(&[(0.0001, 312)]).0, Health::Ok);
        assert_eq!(health_after(&[(0.0001, 313)]).0, Health::Quiet);
        assert_eq!(health_after(&[(0.0001, 400), (0.05, 1)]).0, Health::Ok);
    }

    #[test]
    fn no_frames_for_3_seconds_is_reported_and_resumed_input_is_ok_again() {
        let start = Instant::now();
        let mut meter = HealthMeter::new(start);

        let waiting = meter.state(start + Duration::from_secs(1));
        let none = meter.state(start + Duration::from_secs(3));
        meter.frame(&[0.2; FRAME], start + Duration::from_secs(4));
        let resumed = meter.state(start + Duration::from_secs(4));
        let stopped = meter.state(start + Duration::from_secs(7));
        meter.restart(start + Duration::from_secs(20));
        let after_pause = meter.state(start + Duration::from_secs(21));

        assert_eq!(waiting, Health::Waiting);
        assert_eq!(none, Health::NoFrames);
        assert_eq!(resumed, Health::Ok);
        assert_eq!(stopped, Health::NoFrames);
        assert_eq!(after_pause, Health::Waiting);
    }

    #[test]
    fn the_storage_reports_each_health_change_one_time() {
        let setup = setup("health");
        let (mut storage, _receiver, _) = storage(&setup);
        let start = Instant::now();
        let mut events = Vec::new();

        events.extend(storage.push(Input::Tick, start));
        for frame in 0..100 {
            let now = start + Duration::from_millis(32 * frame);
            events.extend(storage.push(Input::Frame(vec![0.0; FRAME]), now));
        }
        events.extend(storage.push(
            Input::Frame(vec![0.3; FRAME]),
            start + Duration::from_secs(4),
        ));
        events.extend(storage.push(Input::Pause, start + Duration::from_secs(5)));
        events.extend(storage.push(Input::Tick, start + Duration::from_secs(30)));
        events.extend(storage.push(Input::Resume, start + Duration::from_secs(40)));
        events.extend(storage.push(Input::Tick, start + Duration::from_secs(44)));

        let changes: Vec<Health> = events
            .iter()
            .filter_map(|event| match event {
                Event::Health(health) => Some(*health),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes,
            [
                Health::Waiting,
                Health::Ok,
                Health::Silent,
                Health::Ok,
                Health::NoFrames
            ]
        );
    }

    #[test]
    fn refine_closes_the_session_after_a_feed_error() {
        let mut transcriber = FakeTranscriber::new("Yo no estaba allí.");
        transcriber.failed_feeds = 1;

        let failed = refine(&mut transcriber, &[0.5; FRAME]);
        let next = refine(&mut transcriber, &[0.5; FRAME]);

        assert_eq!(failed, None);
        assert_eq!(next.as_deref(), Some("Yo no estaba allí."));
    }

    #[test]
    fn refine_hearing_replaces_the_text_and_keeps_the_live_text() {
        let setup = setup("refine");
        let (mut storage, receiver, _) = storage(&setup);
        let mut processor = processor(&setup, FakeTranscriber::new("yo no estaba alli"));
        let now = Instant::now();
        for level in [0.5; 5].into_iter().chain([0.0; 19]) {
            storage.push(Input::Frame(vec![level; FRAME]), now);
        }
        storage.finish();
        for work in std::iter::from_fn(|| receiver.try_recv().ok()) {
            processor.push(work);
        }
        let id = stored_segments(&setup)[0].id;
        let mut events = Vec::new();

        let count = refine_hearing(
            &mut FakeTranscriber::new("Yo no estaba allí."),
            &setup.store,
            setup.hearing,
            &setup.wav,
            &AtomicBool::new(false),
            |event| events.push(event),
        );

        assert_eq!(count, Ok(1));
        assert_eq!(
            events,
            [Event::Refined {
                segment_id: id,
                text: "Yo no estaba allí.".into()
            }]
        );
        let store = setup.store.lock().unwrap();
        let segment = &store.segments(setup.hearing).unwrap()[0];
        assert_eq!(segment.text, "Yo no estaba allí.");
        assert_eq!(segment.revision, 1);
        assert_eq!(
            store.live_text(id).unwrap().as_deref(),
            Some("yo no estaba alli")
        );
    }

    #[test]
    fn a_cancelled_refine_stops_before_the_next_segment() {
        let setup = setup("refine-cancel");
        let (storage, _receiver, _) = storage(&setup);
        storage.finish();
        {
            let store = setup.store.lock().unwrap();
            let id = store
                .insert_segment(setup.hearing, 0, Speaker::Other)
                .unwrap();
            store.finish_segment(id, "Sí.", 0).unwrap();
        }

        let count = refine_hearing(
            &mut FakeTranscriber::new("Sí, señor."),
            &setup.store,
            setup.hearing,
            &setup.wav,
            &AtomicBool::new(true),
            |_| {},
        );

        assert_eq!(count, Ok(0));
        assert_eq!(stored_segments(&setup)[0].text, "Sí.");
    }
}
