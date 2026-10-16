//! The processing of hearing audio: WAV file, mic level, voice detection, segments, live
//! transcription, and storage of the segments.
//!
//! The [`Processor`] is synchronous, so that tests drive it frame by frame. The app runs it on a
//! background task.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavSpec, WavWriter};

use super::segmenter::{Output, Segmenter};
use super::{AudioError, SAMPLE_RATE, Transcriber, Vad};
use crate::store::{Segment, Speaker, Store};

/// The WAV header is written again after this many samples (10 s), so that the file stays
/// readable if the app stops.
const FLUSH_SAMPLES: usize = 160_000;

/// What the capture sends to the processor.
#[derive(Debug)]
pub enum Input {
    /// 16 kHz mono audio.
    Frame(Vec<f32>),
    /// The recording pauses: the open segment ends.
    Pause,
    /// The microphone stopped working.
    Failed,
}

/// What the processor reports to the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The mic level of the last frame, from 0 (silence) to 1 (full scale).
    Level(f32),
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
    /// A segment ended without text and is not kept. Its partial text is no longer valid.
    Discarded {
        segment_id: i64,
    },
    /// The refine pass replaced the text of a segment.
    Refined {
        segment_id: i64,
        text: String,
    },
    Error(AudioError),
}

/// A finished segment and its audio, for the refine pass.
#[derive(Debug)]
pub struct RefineJob {
    pub segment_id: i64,
    pub audio: Vec<f32>,
}

/// The segment that is open now.
struct Current {
    id: i64,
    speaker: Speaker,
    t_start_ms: i64,
    /// The audio of the segment, kept only for the refine pass.
    audio: Vec<f32>,
    /// True after an error: the segment gets no more audio, and it is deleted at its end.
    failed: bool,
}

pub struct Processor {
    hearing_id: i64,
    store: Arc<Mutex<Store>>,
    /// True while the switch "the accused speaks" is on.
    accused: Arc<AtomicBool>,
    vad: Box<dyn Vad>,
    transcriber: Box<dyn Transcriber>,
    refine: Option<async_channel::Sender<RefineJob>>,
    wav: WavWriter<BufWriter<File>>,
    unflushed: usize,
    segmenter: Segmenter,
    current: Option<Current>,
    /// True after a voice detection error, so that the error shows one time only.
    vad_failed: bool,
}

impl Processor {
    /// Creates the WAV file of the hearing at `wav_path`, with its folder.
    pub fn new(
        hearing_id: i64,
        wav_path: &Path,
        store: Arc<Mutex<Store>>,
        accused: Arc<AtomicBool>,
        vad: Box<dyn Vad>,
        transcriber: Box<dyn Transcriber>,
        refine: Option<async_channel::Sender<RefineJob>>,
    ) -> Result<Self, AudioError> {
        if let Some(dir) = wav_path.parent() {
            fs::create_dir_all(dir).map_err(|_| AudioError::AudioFile)?;
        }
        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE as u32,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let wav = WavWriter::create(wav_path, spec).map_err(|_| AudioError::AudioFile)?;
        Ok(Self {
            hearing_id,
            store,
            accused,
            vad,
            transcriber,
            refine,
            wav,
            unflushed: 0,
            segmenter: Segmenter::default(),
            current: None,
            vad_failed: false,
        })
    }

    pub fn push(&mut self, input: Input) -> Vec<Event> {
        let mut events = Vec::new();
        match input {
            Input::Frame(frame) => {
                if let Err(error) = self.write_wav(&frame) {
                    events.push(Event::Error(error));
                }
                events.push(Event::Level(level(&frame)));
                let speech = match self.vad.is_speech(&frame) {
                    Ok(speech) => speech,
                    Err(error) => {
                        if !std::mem::replace(&mut self.vad_failed, true) {
                            events.push(Event::Error(error));
                        }
                        false
                    }
                };
                for output in self.segmenter.push(&frame, speech) {
                    self.handle(output, &mut events);
                }
            }
            Input::Pause => self.flush(&mut events),
            Input::Failed => {
                self.flush(&mut events);
                events.push(Event::Error(AudioError::Microphone));
            }
        }
        events
    }

    /// Ends the open segment and completes the WAV file.
    pub fn finish(mut self) -> Vec<Event> {
        let mut events = Vec::new();
        self.flush(&mut events);
        if self.wav.finalize().is_err() {
            events.push(Event::Error(AudioError::AudioFile));
        }
        events
    }

    fn flush(&mut self, events: &mut Vec<Event>) {
        if let Some(output) = self.segmenter.flush() {
            self.handle(output, events);
        }
    }

    fn write_wav(&mut self, frame: &[f32]) -> Result<(), AudioError> {
        for &sample in frame {
            let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            self.wav
                .write_sample(sample)
                .map_err(|_| AudioError::AudioFile)?;
        }
        self.unflushed += frame.len();
        if self.unflushed >= FLUSH_SAMPLES {
            self.unflushed = 0;
            self.wav.flush().map_err(|_| AudioError::AudioFile)?;
        }
        Ok(())
    }

    fn handle(&mut self, output: Output, events: &mut Vec<Event>) {
        match output {
            Output::Start { t_ms } => self.start(t_ms, events),
            Output::Audio(audio) => self.feed(audio, events),
            Output::End { t_ms } => self.end(t_ms, events),
        }
    }

    fn start(&mut self, t_ms: i64, events: &mut Vec<Event>) {
        let speaker = if self.accused.load(Ordering::Relaxed) {
            Speaker::Accused
        } else {
            Speaker::Other
        };
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
            audio: Vec::new(),
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
        if self.refine.is_some() {
            current.audio.extend_from_slice(&audio);
        }
    }

    fn end(&mut self, t_end_ms: i64, events: &mut Vec<Event>) {
        let Some(current) = self.current.take() else {
            return;
        };
        let text = if current.failed {
            Ok(String::new())
        } else {
            self.transcriber.finish()
        };
        let text = match text {
            Ok(text) => text.trim().to_owned(),
            Err(error) => {
                events.push(Event::Error(error));
                String::new()
            }
        };
        let store = self.store.lock().unwrap();
        // A segment with no words is noise: it is not kept.
        let stored = if text.is_empty() {
            store.delete_segment(current.id)
        } else {
            store.finish_segment(current.id, &text, t_end_ms)
        };
        drop(store);
        if text.is_empty() {
            events.push(Event::Discarded {
                segment_id: current.id,
            });
        }
        if stored.is_err() {
            events.push(Event::Error(AudioError::Storage));
            return;
        }
        if text.is_empty() {
            return;
        }
        if let Some(refine) = &self.refine {
            refine
                .try_send(RefineJob {
                    segment_id: current.id,
                    audio: current.audio,
                })
                .ok();
        }
        events.push(Event::Final(Segment {
            id: current.id,
            t_start_ms: current.t_start_ms,
            t_end_ms,
            text,
            speaker: current.speaker,
            is_final: true,
            revision: 0,
            failed: false,
        }));
    }
}

/// Transcribes the segment audio again with the refine model, and stores the new text. Returns
/// `None` when the refine pass fails or gives no text: the segment keeps its live text.
pub fn refine(
    transcriber: &mut dyn Transcriber,
    store: &Mutex<Store>,
    job: RefineJob,
) -> Option<Event> {
    transcriber.begin().ok()?;
    transcriber.feed(&job.audio).ok()?;
    let text = transcriber.finish().ok()?.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    store
        .lock()
        .unwrap()
        .set_segment_text(job.segment_id, &text)
        .ok()?;
    Some(Event::Refined {
        segment_id: job.segment_id,
        text,
    })
}

/// The RMS level of the frame on a 60 dB scale: 0 is -60 dBFS or less, 1 is full scale.
fn level(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let rms = (frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32).sqrt();
    if rms <= 0.0 {
        return 0.0;
    }
    ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{Event, Input, Processor, RefineJob, refine};
    use crate::audio::fake::{FakeTranscriber, FakeVad};
    use crate::store::{Speaker, Store};

    const FRAME: usize = 512;

    struct Setup {
        store: Arc<Mutex<Store>>,
        hearing: i64,
        accused: Arc<AtomicBool>,
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
            accused: Arc::new(AtomicBool::new(false)),
            wav: dir.join("hearings").join(format!("{hearing}.wav")),
        }
    }

    fn processor(
        setup: &Setup,
        text: &str,
        refine: Option<async_channel::Sender<RefineJob>>,
    ) -> Processor {
        Processor::new(
            setup.hearing,
            &setup.wav,
            setup.store.clone(),
            setup.accused.clone(),
            Box::new(FakeVad),
            Box::new(FakeTranscriber::new(text)),
            refine,
        )
        .unwrap()
    }

    fn frames(processor: &mut Processor, count: usize, level: f32) -> Vec<Event> {
        (0..count)
            .flat_map(|_| processor.push(Input::Frame(vec![level; FRAME])))
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

    #[test]
    fn speech_shows_a_partial_then_becomes_a_final_stored_segment() {
        let setup = setup("flow");
        let mut processor = processor(&setup, "Yo no estaba allí.", None);

        let speech = frames(&mut processor, 10, 0.5);
        let open = setup.store.lock().unwrap().segments(setup.hearing).unwrap();
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
        let stored = setup.store.lock().unwrap().segments(setup.hearing).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "Yo no estaba allí.");
        assert_eq!(stored[0].t_start_ms, 0);
        assert_eq!(stored[0].t_end_ms, 320);
        assert!(stored[0].is_final);
    }

    #[test]
    fn the_speaker_switch_labels_each_new_segment_at_its_start() {
        let setup = setup("speaker");
        let mut processor = processor(&setup, "Sí, señor juez.", None);

        let mut events = frames(&mut processor, 5, 0.5);
        // A change during the segment is for the next segment.
        setup.accused.store(true, Ordering::Relaxed);
        events.extend(frames(&mut processor, 19, 0.0));
        events.extend(frames(&mut processor, 5, 0.5));
        setup.accused.store(false, Ordering::Relaxed);
        events.extend(frames(&mut processor, 19, 0.0));

        assert_eq!(
            finals(&events),
            [
                ("Sí, señor juez.".to_owned(), Speaker::Other),
                ("Sí, señor juez.".to_owned(), Speaker::Accused),
            ]
        );
        let speakers: Vec<Speaker> = setup
            .store
            .lock()
            .unwrap()
            .segments(setup.hearing)
            .unwrap()
            .iter()
            .map(|segment| segment.speaker)
            .collect();
        assert_eq!(speakers, [Speaker::Other, Speaker::Accused]);
    }

    #[test]
    fn each_frame_gives_a_mic_level_from_0_to_1() {
        let setup = setup("level");
        let mut processor = processor(&setup, "", None);

        let silent = processor.push(Input::Frame(vec![0.0; FRAME]));
        let loud = processor.push(Input::Frame(vec![1.0; FRAME]));
        let quiet = processor.push(Input::Frame(vec![0.01; FRAME]));

        let level = |events: &[Event]| match events.first() {
            Some(Event::Level(level)) => *level,
            _ => panic!("no level event"),
        };
        assert_eq!(level(&silent), 0.0);
        assert_eq!(level(&loud), 1.0);
        assert!(level(&quiet) > 0.0 && level(&quiet) < 0.5);
    }

    #[test]
    fn a_segment_without_words_is_not_kept() {
        let setup = setup("empty");
        let mut processor = processor(&setup, "", None);

        let mut events = frames(&mut processor, 5, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));

        assert!(finals(&events).is_empty());
        assert!(
            setup
                .store
                .lock()
                .unwrap()
                .segments(setup.hearing)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pause_ends_the_open_segment() {
        let setup = setup("pause");
        let mut processor = processor(&setup, "Continúe.", None);
        frames(&mut processor, 5, 0.5);

        let events = processor.push(Input::Pause);

        assert_eq!(finals(&events), [("Continúe.".to_owned(), Speaker::Other)]);
    }

    #[test]
    fn a_microphone_failure_ends_the_segment_and_reports_the_error() {
        let setup = setup("failure");
        let mut processor = processor(&setup, "Continúe.", None);
        frames(&mut processor, 5, 0.5);

        let events = processor.push(Input::Failed);

        assert_eq!(finals(&events).len(), 1);
        assert!(matches!(events.last(), Some(Event::Error(_))));
    }

    #[test]
    fn finish_ends_the_open_segment_and_the_wav_has_every_sample() {
        let setup = setup("wav");
        let mut processor = processor(&setup, "Fin.", None);
        frames(&mut processor, 7, 0.5);
        processor.push(Input::Pause);
        frames(&mut processor, 3, 0.0);
        frames(&mut processor, 2, 0.5);

        let events = processor.finish();

        assert_eq!(finals(&events), [("Fin.".to_owned(), Speaker::Other)]);
        let reader = hound::WavReader::open(&setup.wav).unwrap();
        let spec = reader.spec();
        assert_eq!((spec.sample_rate, spec.channels), (16_000, 1));
        assert_eq!(reader.duration() as usize, 12 * FRAME);
        let first: Vec<i16> = reader
            .into_samples::<i16>()
            .take(2)
            .map(Result::unwrap)
            .collect();
        assert_eq!(first, [i16::MAX / 2, i16::MAX / 2]);
    }

    #[test]
    fn after_a_feed_error_the_next_segment_can_begin() {
        let setup = setup("feed-error");
        let mut transcriber = FakeTranscriber::new("Sí.");
        transcriber.failed_feeds = 1;
        let mut processor = Processor::new(
            setup.hearing,
            &setup.wav,
            setup.store.clone(),
            setup.accused.clone(),
            Box::new(FakeVad),
            Box::new(transcriber),
            None,
        )
        .unwrap();

        let mut events = frames(&mut processor, 5, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));
        events.extend(frames(&mut processor, 5, 0.5));
        events.extend(frames(&mut processor, 19, 0.0));

        assert_eq!(finals(&events), [("Sí.".to_owned(), Speaker::Other)]);
        let errors = events
            .iter()
            .filter(|event| matches!(event, Event::Error(_)))
            .count();
        assert_eq!(errors, 1);
    }

    #[test]
    fn a_segment_with_a_partial_and_no_final_text_is_discarded() {
        let setup = setup("discarded");
        let mut transcriber = FakeTranscriber::new("Sí.");
        transcriber.failed_feeds = 1;
        let mut processor = Processor::new(
            setup.hearing,
            &setup.wav,
            setup.store.clone(),
            setup.accused.clone(),
            Box::new(FakeVad),
            Box::new(transcriber),
            None,
        )
        .unwrap();
        let mut events = frames(&mut processor, 5, 0.5);
        events.extend(frames(&mut processor, 19, 0.0));

        let id = events.iter().find_map(|event| match event {
            Event::Discarded { segment_id } => Some(*segment_id),
            _ => None,
        });
        assert!(id.is_some());
        assert!(
            setup
                .store
                .lock()
                .unwrap()
                .segments(setup.hearing)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_final_segment_goes_to_refine_and_refine_replaces_its_text() {
        let setup = setup("refine");
        let (sender, receiver) = async_channel::unbounded();
        let mut processor = processor(&setup, "yo no estaba alli", Some(sender));
        frames(&mut processor, 5, 0.5);
        frames(&mut processor, 19, 0.0);
        let job = receiver.try_recv().unwrap();

        let event = refine(
            &mut FakeTranscriber::new("Yo no estaba allí."),
            &setup.store,
            job,
        );

        let segment = setup
            .store
            .lock()
            .unwrap()
            .segments(setup.hearing)
            .unwrap()
            .remove(0);
        assert!(matches!(
            event,
            Some(Event::Refined { segment_id, ref text }) if segment_id == segment.id && text == "Yo no estaba allí."
        ));
        assert_eq!(segment.text, "Yo no estaba allí.");
    }
}
