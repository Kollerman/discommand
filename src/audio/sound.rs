//! Sound (the `sound` feature): voice messages played with `p`, crunched to
//! 8 kHz and 8 bits for a retro feel, the alert for new messages while the
//! window isn't focused, and the startup sound. Everything plays on a thread
//! of its own, which opens the sound device only while something plays.

use std::time::Duration;

/// Whether this build can play sound.
pub const BUILT: bool = cfg!(feature = "sound");

/// Shown on `p` when the feature isn't built in.
pub const NOT_BUILT: &str = "sound isn't built in: run the install script again and answer y for \"Sound\"";

/// The retro crunch: evens out the loudness, then holds every other sample
/// (8 kHz from 16) and keeps 8 bits of each.
#[cfg_attr(not(feature = "sound"), allow(dead_code))]
fn crunch(pcm: &mut [f32]) {
    let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
    // Quiet recordings are lifted, but not so far that their noise roars.
    let gain = if peak > 0.0 { (0.8 / peak).min(8.0) } else { 1.0 };
    for pair in pcm.chunks_mut(2) {
        let s = (pair[0] * gain * 127.0).round() / 127.0;
        pair.fill(s);
    }
}

#[cfg(feature = "sound")]
mod imp {
    use std::num::NonZero;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use std::io::Cursor;

    use rodio::buffer::SamplesBuffer;
    use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

    pub enum Cmd {
        Voice(u32, Vec<f32>),
        Alert,
        /// The startup sound, at this speed (1 = as recorded).
        Startup(f32),
        Stop,
    }

    /// The device is closed this long after the last sound ends.
    const LINGER: Duration = Duration::from_secs(5);

    /// The alert is a nudge, quieter than the voice messages.
    const ALERT_VOLUME: f32 = 0.35;

    const STARTUP: &[u8] = include_bytes!("../../assets/startup.mp3");

    pub fn send(cmd: Cmd) {
        static TX: OnceLock<Mutex<Sender<Cmd>>> = OnceLock::new();
        let tx = TX.get_or_init(|| {
            let (tx, rx) = channel();
            std::thread::spawn(move || run(rx));
            Mutex::new(tx)
        });
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(cmd);
        }
    }

    fn samples(rate: u32, pcm: Vec<f32>) -> Option<SamplesBuffer> {
        Some(SamplesBuffer::new(NonZero::new(1)?, NonZero::new(rate)?, pcm))
    }

    fn open() -> Option<MixerDeviceSink> {
        // Errors are dropped: printed, they would scribble over the screen.
        let mut sink = DeviceSinkBuilder::from_default_device().ok()?.with_error_callback(|_| {}).open_sink_or_fallback().ok()?;
        sink.log_on_drop(false);
        Some(sink)
    }

    fn run(rx: Receiver<Cmd>) {
        let alert = crate::audio::decode(include_bytes!("../../assets/alert.wav")).ok();
        let mut sink: Option<MixerDeviceSink> = None;
        // Dropping the player stops the voice message.
        let mut voice: Option<Player> = None;
        let mut until = Instant::now();
        loop {
            let cmd = if sink.is_some() {
                match rx.recv_timeout(until.saturating_duration_since(Instant::now()) + LINGER) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => {
                        // Silent for a while: the device is let go.
                        drop(voice.take());
                        sink = None;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return,
                }
            };
            if matches!(cmd, Cmd::Stop) {
                drop(voice.take());
                until = Instant::now();
                continue;
            }
            if sink.is_none() {
                sink = open();
            }
            let Some(out) = sink.as_ref() else { continue };
            let length = match cmd {
                Cmd::Voice(rate, pcm) => {
                    let length = super::length(rate, pcm.len());
                    let Some(source) = samples(rate, pcm) else { continue };
                    // A new voice message replaces the one playing.
                    let player = Player::connect_new(out.mixer());
                    player.append(source);
                    drop(voice.replace(player));
                    length
                }
                // The alert and the startup sound play over whatever else is playing.
                Cmd::Alert => {
                    let Some((rate, pcm)) = &alert else { continue };
                    let Some(source) = samples(*rate, pcm.clone()) else { continue };
                    out.mixer().add(source.amplify(ALERT_VOLUME));
                    super::length(*rate, pcm.len())
                }
                Cmd::Startup(speed) => {
                    let Ok(source) = Decoder::try_from(Cursor::new(STARTUP)) else { continue };
                    let length = source.total_duration().unwrap_or(Duration::from_secs(5)).div_f32(speed);
                    out.mixer().add(source.speed(speed));
                    length
                }
                Cmd::Stop => continue,
            };
            until = until.max(Instant::now() + length);
        }
    }
}

#[cfg_attr(not(feature = "sound"), allow(dead_code))]
fn length(rate: u32, samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / rate as f64)
}

/// Plays a voice message (its file's bytes), crunched; replaces the one
/// playing. How long it lasts.
#[cfg(feature = "sound")]
pub async fn play(data: Vec<u8>) -> Result<Duration, String> {
    let (rate, pcm) = tokio::task::spawn_blocking(move || {
        let (rate, mut pcm) = crate::audio::decode(&data)?;
        crunch(&mut pcm);
        Ok::<_, String>((rate, pcm))
    })
    .await
    .map_err(|e| e.to_string())??;
    if pcm.is_empty() {
        return Err("the voice message is empty".into());
    }
    let length = length(rate, pcm.len());
    imp::send(imp::Cmd::Voice(rate, pcm));
    Ok(length)
}

#[cfg(not(feature = "sound"))]
pub async fn play(_data: Vec<u8>) -> Result<Duration, String> {
    Err(NOT_BUILT.into())
}

/// Stops the voice message playing.
pub fn stop() {
    #[cfg(feature = "sound")]
    imp::send(imp::Cmd::Stop);
}

/// The alert for a new message.
pub fn alert() {
    #[cfg(feature = "sound")]
    imp::send(imp::Cmd::Alert);
}

/// The startup sound, unless it's turned off (`:startup false`): a little
/// faster or slower each time, by up to 15 %.
pub fn startup() {
    #[cfg(feature = "sound")]
    if startup_on() {
        let mut bytes = [0u8; 4];
        let _ = getrandom::fill(&mut bytes);
        imp::send(imp::Cmd::Startup(speed(u32::from_le_bytes(bytes))));
    }
}

/// From a random number: 0.85 to 1.15.
#[cfg_attr(not(feature = "sound"), allow(dead_code))]
fn speed(random: u32) -> f32 {
    0.85 + 0.3 * (random as f64 / u32::MAX as f64) as f32
}

/// Sound settings, kept in `sound.json`.
fn settings_path() -> Option<std::path::PathBuf> {
    crate::system::secret::data_dir().map(|d| d.join("sound.json"))
}

/// Whether the startup sound plays (on unless turned off).
pub fn startup_on() -> bool {
    let settings: Option<serde_json::Value> = settings_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    settings.and_then(|s| s["startup"].as_bool()).unwrap_or(true)
}

/// `:startup true|false`.
pub fn set_startup(on: bool) -> Result<(), String> {
    let path = settings_path().ok_or("no folder for settings")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, serde_json::json!({ "startup": on }).to_string()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{crunch, speed};

    #[test]
    fn speeds_up_or_slows_down_by_at_most_15_percent() {
        assert!((speed(0) - 0.85).abs() < 1e-6);
        assert!((speed(u32::MAX) - 1.15).abs() < 1e-6);
        assert!((speed(u32::MAX / 2) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn crunches_to_8_bits_at_half_the_rate() {
        let mut pcm = [0.1, 0.05, -0.04, 0.0, 0.003];
        crunch(&mut pcm);
        // Lifted to 0.8 at the loudest, each pair holding its first sample.
        assert_eq!(pcm[0], pcm[1]);
        assert_eq!(pcm[2], pcm[3]);
        assert!((pcm[0] - 0.8).abs() < 0.01);
        assert!((pcm[2] + 0.32).abs() < 0.01);
        assert!(pcm.iter().all(|s| ((s * 127.0).round() - s * 127.0).abs() < 1e-4));
    }

    #[test]
    fn leaves_silence_alone() {
        let mut pcm = [0.0; 4];
        crunch(&mut pcm);
        assert_eq!(pcm, [0.0; 4]);
    }
}
