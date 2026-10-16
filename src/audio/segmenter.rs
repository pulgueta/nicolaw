//! Cuts a stream of audio frames into speech segments, with a voice activity decision per frame.
//!
//! The segmenter is a pure state machine: it does not know where the decisions come from, so
//! tests can drive it with a fake voice detector.

use std::collections::VecDeque;

use super::SAMPLE_RATE;

/// A new segment starts with at least this much audio from before the speech (300 ms), so that
/// the first word is not cut.
const PRE_ROLL: usize = 4_800;
/// This much silence ends a segment (600 ms).
const END_SILENCE: usize = 9_600;
/// A segment ends at this length (30 s), even if the speech continues.
const MAX_LENGTH: usize = 480_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// A segment starts. `t_ms` is the position of its first sample in the hearing audio.
    Start { t_ms: i64 },
    /// Audio of the open segment.
    Audio(Vec<f32>),
    /// The segment ends. `t_ms` is the end of its last speech frame.
    End { t_ms: i64 },
}

#[derive(Default)]
pub struct Segmenter {
    /// Samples pushed so far.
    position: usize,
    /// While no segment is open: the last frames, for the pre-roll of the next segment.
    pre_roll: VecDeque<Vec<f32>>,
    pre_roll_len: usize,
    open: Option<Open>,
}

struct Open {
    length: usize,
    /// Samples of silence since the last speech frame.
    silence: usize,
    /// The position after the last speech frame.
    speech_end: usize,
}

impl Segmenter {
    /// Adds one frame of audio. `speech` is the voice activity decision for the frame.
    pub fn push(&mut self, frame: &[f32], speech: bool) -> Vec<Output> {
        self.position += frame.len();
        let mut outputs = Vec::new();
        let open = match self.open.as_mut() {
            Some(open) => {
                outputs.push(Output::Audio(frame.to_vec()));
                open.length += frame.len();
                if speech {
                    open.silence = 0;
                    open.speech_end = self.position;
                } else {
                    open.silence += frame.len();
                }
                open
            }
            None if speech => {
                let start = self.position - frame.len() - self.pre_roll_len;
                outputs.push(Output::Start { t_ms: ms(start) });
                outputs.extend(self.pre_roll.drain(..).map(Output::Audio));
                outputs.push(Output::Audio(frame.to_vec()));
                let open = self.open.insert(Open {
                    length: self.pre_roll_len + frame.len(),
                    silence: 0,
                    speech_end: self.position,
                });
                self.pre_roll_len = 0;
                open
            }
            None => {
                self.pre_roll.push_back(frame.to_vec());
                self.pre_roll_len += frame.len();
                while let Some(oldest) = self.pre_roll.front()
                    && self.pre_roll_len - oldest.len() >= PRE_ROLL
                {
                    self.pre_roll_len -= oldest.len();
                    self.pre_roll.pop_front();
                }
                return outputs;
            }
        };
        if open.silence >= END_SILENCE || open.length >= MAX_LENGTH {
            outputs.push(Output::End {
                t_ms: ms(open.speech_end),
            });
            self.open = None;
        }
        outputs
    }

    /// Ends the open segment, if there is one. Use it when the recording pauses or stops. The
    /// audio from before the pause is not pre-roll for the next segment.
    pub fn flush(&mut self) -> Option<Output> {
        self.pre_roll.clear();
        self.pre_roll_len = 0;
        self.open.take().map(|open| Output::End {
            t_ms: ms(open.speech_end),
        })
    }
}

/// The time of a sample position, in milliseconds.
fn ms(samples: usize) -> i64 {
    (samples as u64 * 1000 / SAMPLE_RATE) as i64
}

#[cfg(test)]
mod tests {
    use super::{Output, Segmenter};

    /// 512 samples: 32 ms at 16 kHz, the frame of the Silero VAD.
    const FRAME: usize = 512;

    /// Pushes `count` frames with the same decision. The first sample of each frame is the
    /// frame number, so that tests can see which frames a segment has.
    fn push(
        segmenter: &mut Segmenter,
        next: &mut usize,
        count: usize,
        speech: bool,
    ) -> Vec<Output> {
        let mut outputs = Vec::new();
        for _ in 0..count {
            let mut frame = vec![0.0; FRAME];
            frame[0] = *next as f32;
            *next += 1;
            outputs.extend(segmenter.push(&frame, speech));
        }
        outputs
    }

    /// The frame numbers in the audio outputs.
    fn frames(outputs: &[Output]) -> Vec<usize> {
        outputs
            .iter()
            .filter_map(|output| match output {
                Output::Audio(audio) => Some(audio[0] as usize),
                _ => None,
            })
            .collect()
    }

    fn starts_and_ends(outputs: &[Output]) -> Vec<Output> {
        outputs
            .iter()
            .filter(|output| !matches!(output, Output::Audio(_)))
            .cloned()
            .collect()
    }

    #[test]
    fn silence_gives_no_output() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;

        assert!(push(&mut segmenter, &mut next, 100, false).is_empty());
    }

    #[test]
    fn speech_starts_a_segment_with_at_least_300_ms_of_audio_before_it() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        push(&mut segmenter, &mut next, 20, false);

        let outputs = push(&mut segmenter, &mut next, 1, true);

        // 10 frames of 32 ms = 320 ms of pre-roll: frames 10 to 19, then the speech frame 20.
        assert_eq!(outputs[0], Output::Start { t_ms: 320 });
        assert_eq!(frames(&outputs), (10..=20).collect::<Vec<_>>());
    }

    #[test]
    fn speech_at_the_start_has_only_the_audio_before_it() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        push(&mut segmenter, &mut next, 3, false);

        let outputs = push(&mut segmenter, &mut next, 1, true);

        assert_eq!(outputs[0], Output::Start { t_ms: 0 });
        assert_eq!(frames(&outputs), [0, 1, 2, 3]);
    }

    #[test]
    fn a_short_pause_does_not_end_the_segment() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        push(&mut segmenter, &mut next, 5, true);

        // 18 frames = 576 ms of silence, less than 600 ms.
        let pause = push(&mut segmenter, &mut next, 18, false);
        let more = push(&mut segmenter, &mut next, 5, true);

        assert_eq!(frames(&pause).len(), 18);
        assert!(starts_and_ends(&pause).is_empty());
        assert!(starts_and_ends(&more).is_empty());
    }

    #[test]
    fn six_hundred_ms_of_silence_ends_the_segment_at_the_last_speech() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        let start = push(&mut segmenter, &mut next, 10, true);

        let silence = push(&mut segmenter, &mut next, 19, false);
        let after = push(&mut segmenter, &mut next, 5, false);

        assert_eq!(start[0], Output::Start { t_ms: 0 });
        // The 19th silent frame makes 608 ms. The speech ended after frame 9, at 320 ms.
        assert_eq!(silence.last(), Some(&Output::End { t_ms: 320 }));
        assert_eq!(frames(&silence), (10..29).collect::<Vec<_>>());
        assert!(after.is_empty());
    }

    #[test]
    fn a_new_segment_after_the_end_has_its_own_pre_roll() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        push(&mut segmenter, &mut next, 10, true);
        push(&mut segmenter, &mut next, 19, false);
        push(&mut segmenter, &mut next, 2, false);

        let outputs = push(&mut segmenter, &mut next, 1, true);

        // Only the 2 frames after the end are pre-roll; the trailing silence went to the
        // previous segment.
        assert_eq!(outputs[0], Output::Start { t_ms: 29 * 32 });
        assert_eq!(frames(&outputs), [29, 30, 31]);
    }

    #[test]
    fn a_segment_ends_at_30_seconds_and_speech_continues_in_a_new_one() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;

        // 938 frames of 32 ms = 30 016 ms, the first frame count at 30 s or more.
        let first = push(&mut segmenter, &mut next, 938, true);
        let second = push(&mut segmenter, &mut next, 1, true);

        assert_eq!(
            starts_and_ends(&first),
            [Output::Start { t_ms: 0 }, Output::End { t_ms: 30_016 }]
        );
        assert_eq!(frames(&first).len(), 938);
        assert_eq!(second[0], Output::Start { t_ms: 30_016 });
        assert_eq!(frames(&second), [938]);
    }

    #[test]
    fn flush_ends_an_open_segment_and_clears_the_pre_roll() {
        let mut segmenter = Segmenter::default();
        let mut next = 0;
        push(&mut segmenter, &mut next, 4, true);
        push(&mut segmenter, &mut next, 3, false);

        let flushed = segmenter.flush();
        let again = segmenter.flush();
        push(&mut segmenter, &mut next, 2, false);
        segmenter.flush();
        let after = push(&mut segmenter, &mut next, 1, true);

        assert_eq!(flushed, Some(Output::End { t_ms: 128 }));
        assert_eq!(again, None);
        // A pause in the recording: audio from before it is not pre-roll.
        assert_eq!(frames(&after), [9]);
    }
}
