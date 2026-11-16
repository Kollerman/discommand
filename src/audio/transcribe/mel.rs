//! What Whisper hears: audio cut into pieces of at most 30 seconds, each
//! turned into a log-mel spectrogram the way OpenAI's Whisper does it.

use std::ops::Range;

/// Samples per second.
pub const RATE: usize = 16_000;
/// The longest piece Whisper takes in: 30 seconds.
const PIECE: usize = 30 * RATE;
/// Spectrogram frames of a piece (one every 10 ms).
pub const FRAMES: usize = 3000;
const HOP: usize = 160;
const FFT: usize = 400;
/// Frequencies per frame.
const BINS: usize = FFT / 2 + 1;

/// Cuts the audio into pieces of at most 30 seconds. A piece that has to end
/// before the audio does ends in a pause (the quietest moment of its last
/// ten seconds; the latest one, if there are several), so that no word is
/// cut in two.
pub fn pieces(pcm: &[f32]) -> Vec<Range<usize>> {
    // Loudness is measured in steps of 20 ms, a pause over 200 ms.
    const STEP: usize = RATE / 50;
    const PAUSE: usize = 10;
    let mut out = Vec::new();
    let mut start = 0;
    while pcm.len() - start > PIECE {
        let from = start + PIECE - 10 * RATE;
        let energy: Vec<f32> = pcm[from..start + PIECE].chunks_exact(STEP).map(|c| c.iter().map(|s| s * s).sum()).collect();
        let pauses: Vec<f32> = energy.windows(PAUSE).map(|w| w.iter().sum()).collect();
        let quietest = pauses.iter().copied().fold(f32::INFINITY, f32::min);
        // As quiet as the quietest (within 3 dB), as late as possible.
        let at = pauses.iter().rposition(|&e| e <= quietest * 2.0 + f32::MIN_POSITIVE).unwrap_or(pauses.len() - 1);
        let cut = from + (at + PAUSE / 2) * STEP;
        out.push(start..cut);
        start = cut;
    }
    if start < pcm.len() {
        out.push(start..pcm.len());
    }
    out
}

/// The log-mel spectrogram of a piece (at most 30 seconds, padded with
/// silence to 30): `mels` rows of `FRAMES` values. `filters` is the mel
/// filter bank, `mels` rows of 201 weights.
pub fn spectrogram(pcm: &[f32], filters: &[f32], mels: usize) -> Vec<f32> {
    let pcm = &pcm[..pcm.len().min(PIECE)];
    // Each frame is centered on its moment, the audio mirrored before its
    // start (as in the training of Whisper), silence after its end.
    let sample = |n: isize| -> f32 {
        let i = n.unsigned_abs();
        if i < pcm.len() { pcm[i] } else { 0.0 }
    };
    let window: Vec<f32> = (0..FFT).map(|j| 0.5 * (1.0 - (std::f32::consts::TAU * j as f32 / FFT as f32).cos())).collect();
    let (cos, sin): (Vec<f32>, Vec<f32>) = (0..FFT).map(|j| (std::f32::consts::TAU * j as f32 / FFT as f32).sin_cos()).map(|(s, c)| (c, s)).unzip();
    // Frames that reach into the audio; the rest is silence.
    let used = (pcm.len() + FFT / 2).div_ceil(HOP).min(FRAMES);
    let silence = 1e-10f32.log10();
    let mut mel = vec![silence; mels * FRAMES];
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(used.max(1));
    let per_thread = used.div_ceil(threads).max(1);
    // Each thread computes a run of frames: (first frame, its columns).
    let columns: Vec<(usize, Vec<f32>)> = std::thread::scope(|scope| {
        let (sample, window, cos, sin) = (&sample, &window, &cos, &sin);
        let jobs: Vec<_> = (0..used)
            .step_by(per_thread)
            .map(|first| {
                scope.spawn(move || {
                    let last = (first + per_thread).min(used);
                    let mut out = Vec::with_capacity((last - first) * mels);
                    let mut frame = [0f32; FFT];
                    let mut power = [0f32; BINS];
                    for i in first..last {
                        let begin = (i * HOP) as isize - (FFT / 2) as isize;
                        for (j, f) in frame.iter_mut().enumerate() {
                            *f = window[j] * sample(begin + j as isize);
                        }
                        for (k, p) in power.iter_mut().enumerate() {
                            let (mut re, mut im) = (0f32, 0f32);
                            let mut phase = 0;
                            for f in &frame {
                                re += f * cos[phase];
                                im -= f * sin[phase];
                                phase += k;
                                if phase >= FFT {
                                    phase -= FFT;
                                }
                            }
                            *p = re * re + im * im;
                        }
                        for m in 0..mels {
                            let sum: f32 = filters[m * BINS..(m + 1) * BINS].iter().zip(&power).map(|(f, p)| f * p).sum();
                            out.push(sum.max(1e-10).log10());
                        }
                    }
                    (first, out)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().expect("spectrogram thread")).collect()
    });
    for (first, values) in columns {
        for (n, column) in values.chunks_exact(mels).enumerate() {
            for (m, v) in column.iter().enumerate() {
                mel[m * FRAMES + first + n] = *v;
            }
        }
    }
    // At most 8 below the loudest (in log10 units), then scaled to about -1..1.
    let floor = mel.iter().copied().fold(f32::NEG_INFINITY, f32::max) - 8.0;
    for v in &mut mel {
        *v = (v.max(floor) + 4.0) / 4.0;
    }
    mel
}

#[cfg(test)]
mod tests {
    use super::{PIECE, RATE, pieces};

    /// A tone with a pause (silence) from `pause` on, 0.3 s long.
    fn speech(seconds: usize, pauses: &[f32]) -> Vec<f32> {
        (0..seconds * RATE)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                if pauses.iter().any(|p| (*p..*p + 0.3).contains(&t)) { 0.0 } else { (t * 440.0).sin() }
            })
            .collect()
    }

    #[test]
    fn short_audio_is_one_piece() {
        assert_eq!(pieces(&vec![0.1; 12 * RATE]), [0..12 * RATE]);
        assert_eq!(pieces(&vec![0.1; PIECE]), [0..PIECE]);
        assert!(pieces(&[]).is_empty());
    }

    #[test]
    fn cuts_in_the_last_pause() {
        // Pauses at 12 s (too early), 23 s and 27.5 s: cut in the last one.
        let pcm = speech(40, &[12.0, 23.0, 27.5]);
        let p = pieces(&pcm);
        assert_eq!(p.len(), 2);
        let cut = p[0].end as f32 / RATE as f32;
        assert!((27.5..27.8).contains(&cut), "{cut}");
        assert_eq!((p[0].start, p[1].start, p[1].end), (0, p[0].end, pcm.len()));
    }

    #[test]
    fn pieces_are_never_too_long() {
        // No pause at all: still cut, within the last ten seconds.
        let pcm = speech(95, &[]);
        let p = pieces(&pcm);
        assert!(p.iter().all(|r| r.len() <= PIECE && r.len() >= 20 * RATE || r.end == pcm.len()));
        assert_eq!(p.iter().map(|r| r.len()).sum::<usize>(), pcm.len());
    }
}
