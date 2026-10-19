//! Video playback through the `ffmpeg` and `ffprobe` command line tools: one `ffmpeg` process
//! decodes frames, another decodes the audio track, and the audio's position is the clock that
//! decides which frame is shown.

use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use rodio::{ChannelCount, MixerDeviceSink, Player, SampleRate, Source, nz};
use serde::Deserialize;
use util::ResultExt as _;

const MAX_FRAME_WIDTH: u32 = 1920;
const MAX_FRAME_HEIGHT: u32 = 1080;
/// Decoded frames waiting to be shown. Each is a full BGRA image, so this stays small.
const FRAME_QUEUE_LEN: usize = 4;
const AUDIO_SAMPLE_RATE: SampleRate = nz!(48000);
const AUDIO_CHANNELS: ChannelCount = nz!(2);
const AUDIO_CHUNK_FRAMES: usize = 2048;
const AUDIO_QUEUE_LEN: usize = 32;

#[derive(Debug, Clone)]
pub struct VideoDetails {
    pub duration: Duration,
    /// The upright size of the picture, before frames are shrunk for decoding.
    pub width: u32,
    pub height: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub frames_per_second: f64,
    pub video_codec: Option<String>,
    pub has_audio: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub struct VideoContent {
    pub abs_path: PathBuf,
    pub details: VideoDetails,
    pub poster: Option<Arc<RenderImage>>,
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: ProbeFormat,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    side_data_list: Vec<ProbeSideData>,
}

#[derive(Deserialize)]
struct ProbeSideData {
    rotation: Option<f64>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

// Zed avoids `std::process::Command` because spawning blocks the calling thread. These
// commands are only spawned off the main thread, and their output is read with blocking reads
// on dedicated threads, which is what std's pipes are for.
#[allow(clippy::disallowed_methods)]
fn command(program: &str) -> std::process::Command {
    let mut command = util::command::new_std_command(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[allow(clippy::disallowed_methods)]
fn spawn(mut command: std::process::Command) -> Result<ChildGuard> {
    let program = command.get_program().to_string_lossy().into_owned();
    let child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "Playing video needs `{program}`. Install FFmpeg and make sure it is on your PATH."
            )
        } else {
            anyhow::Error::new(error).context(format!("running {program}"))
        }
    })?;
    Ok(ChildGuard(child))
}

/// Runs `command` on a new thread and hands its output to `read`. The process is killed when
/// `read` returns, which it does at the end of the output or once nothing receives what it
/// reads.
fn spawn_reader(command: std::process::Command, read: impl FnOnce(ChildStdout) + Send + 'static) {
    std::thread::spawn(move || {
        spawn(command)
            .and_then(|mut process| {
                let stdout = process.0.stdout.take().context("ffmpeg has no output")?;
                read(stdout);
                Ok(())
            })
            .context("decoding video")
            .log_err();
    });
}

fn tag(tags: &HashMap<String, String>, key: &str) -> Option<String> {
    tags.iter()
        .find(|(name, value)| name.eq_ignore_ascii_case(key) && !value.trim().is_empty())
        .map(|(_, value)| value.clone())
}

fn parse_frame_rate(rate: &str) -> Option<f64> {
    let (numerator, denominator) = rate.split_once('/').unwrap_or((rate, "1"));
    let rate = numerator.parse::<f64>().ok()? / denominator.parse::<f64>().ok()?;
    (rate.is_finite() && rate > 0.).then_some(rate)
}

/// The size frames are decoded at: the displayed size, shrunk to fit MAX_FRAME_WIDTH x
/// MAX_FRAME_HEIGHT, with even sides since most pixel formats need them.
fn frame_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_FRAME_WIDTH as f64 / width as f64)
        .min(MAX_FRAME_HEIGHT as f64 / height as f64)
        .min(1.);
    let even = |side: f64| ((side.round() as u32) / 2 * 2).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

pub fn probe(abs_path: &Path) -> Result<VideoContent> {
    let mut command = command("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(abs_path);
    let mut process = spawn(command)?;
    let mut stdout = Vec::new();
    process
        .0
        .stdout
        .take()
        .context("ffprobe has no output")?
        .read_to_end(&mut stdout)
        .context("running ffprobe")?;
    let status = process.0.wait().context("running ffprobe")?;
    anyhow::ensure!(status.success(), "ffprobe could not read the file");
    let probe: Probe = serde_json::from_slice(&stdout).context("parsing ffprobe output")?;

    let video = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .context("the file has no video stream")?;
    let (Some(width), Some(height)) = (video.width, video.height) else {
        anyhow::bail!("the video stream has no size");
    };
    // ffmpeg rotates frames upright while decoding, so a quarter turn swaps the displayed sides.
    let rotation = video
        .side_data_list
        .iter()
        .find_map(|side_data| side_data.rotation)
        .or_else(|| tag(&video.tags, "rotate")?.parse().ok())
        .unwrap_or(0.);
    let (width, height) = if (rotation.abs() / 90.).round() as i64 % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    let (frame_width, frame_height) = frame_size(width, height);
    let frames_per_second = [&video.avg_frame_rate, &video.r_frame_rate]
        .into_iter()
        .find_map(|rate| parse_frame_rate(rate.as_deref()?))
        .unwrap_or(30.)
        .clamp(1., 120.);
    let duration = probe
        .format
        .duration
        .as_deref()
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration > 0.)
        .context("the video has no known duration")?;

    let details = VideoDetails {
        duration: Duration::from_secs_f64(duration),
        width,
        height,
        frame_width,
        frame_height,
        frames_per_second,
        video_codec: video.codec_name.clone(),
        has_audio: probe
            .streams
            .iter()
            .any(|stream| stream.codec_type.as_deref() == Some("audio")),
        title: tag(&probe.format.tags, "title"),
        artist: tag(&probe.format.tags, "artist"),
    };
    let poster = read_poster(abs_path, &details)
        .context("decoding the first video frame")
        .map_err(|error| log::warn!("{error:#}"))
        .ok();
    Ok(VideoContent {
        abs_path: abs_path.to_path_buf(),
        details,
        poster,
    })
}

fn video_command(
    abs_path: &Path,
    details: &VideoDetails,
    start: Duration,
) -> std::process::Command {
    let mut command = command("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-ss", &format!("{:.3}", start.as_secs_f64())])
        .arg("-i")
        .arg(abs_path)
        .args(["-map", "0:v:0", "-an", "-sn"])
        .args([
            "-vf",
            &format!("scale={}:{}", details.frame_width, details.frame_height),
        ])
        // A constant frame rate lets each frame's time follow from its index.
        .args(["-r", &format!("{}", details.frames_per_second)])
        .args(["-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"]);
    command
}

fn audio_command(abs_path: &Path, start: Duration) -> std::process::Command {
    let mut command = command("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-ss", &format!("{:.3}", start.as_secs_f64())])
        .arg("-i")
        .arg(abs_path)
        .args(["-map", "0:a:0", "-vn", "-sn"])
        .args(["-ac", &AUDIO_CHANNELS.to_string()])
        .args(["-ar", &AUDIO_SAMPLE_RATE.to_string()])
        .args(["-f", "f32le", "pipe:1"]);
    command
}

fn frame_from_bytes(details: &VideoDetails, bytes: Vec<u8>) -> Option<Arc<RenderImage>> {
    // gpui keeps render images in BGRA, which is what ffmpeg writes, so the bytes go in as is.
    let buffer = image::RgbaImage::from_raw(details.frame_width, details.frame_height, bytes)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

fn frame_len(details: &VideoDetails) -> usize {
    details.frame_width as usize * details.frame_height as usize * 4
}

fn read_poster(abs_path: &Path, details: &VideoDetails) -> Result<Arc<RenderImage>> {
    let mut command = video_command(abs_path, details, Duration::ZERO);
    command.args(["-frames:v", "1"]);
    let mut process = spawn(command)?;
    let mut stdout = process.0.stdout.take().context("ffmpeg has no output")?;
    let mut bytes = vec![0; frame_len(details)];
    stdout.read_exact(&mut bytes)?;
    frame_from_bytes(details, bytes).context("ffmpeg wrote a frame of the wrong size")
}

/// Kills the process when dropped, which also ends the thread reading its output.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // The process may already have exited, so failing to kill it is expected.
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

pub struct VideoFrame {
    pub time: Duration,
    pub image: Arc<RenderImage>,
}

/// The audio track as a rodio source, fed by a thread reading ffmpeg's output.
struct AudioStream {
    chunks: Receiver<Vec<f32>>,
    chunk: Vec<f32>,
    index: usize,
    /// Frames of silence played because ffmpeg had not decoded the audio yet. The player counts
    /// them in its position, so the clock subtracts them to stay in step with the audio.
    underrun_frames: Arc<AtomicU64>,
}

impl Iterator for AudioStream {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.chunk.len() {
            self.index = 0;
            match self.chunks.try_recv() {
                Ok(chunk) => self.chunk = chunk,
                Err(TryRecvError::Empty) => {
                    // Fill a whole frame, so the channels stay in order. This runs on the audio
                    // thread, so it must not wait for ffmpeg.
                    self.chunk.clear();
                    self.chunk.resize(AUDIO_CHANNELS.get() as usize, 0.);
                    self.underrun_frames.fetch_add(1, Ordering::Relaxed);
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
        let sample = self.chunk.get(self.index).copied();
        self.index += 1;
        sample
    }
}

impl Source for AudioStream {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        AUDIO_CHANNELS
    }

    fn sample_rate(&self) -> SampleRate {
        AUDIO_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn read_audio(mut stdout: impl std::io::Read, chunks: SyncSender<Vec<f32>>) {
    let mut bytes = vec![0u8; AUDIO_CHUNK_FRAMES * AUDIO_CHANNELS.get() as usize * 4];
    // A short read at the very end drops less than one chunk of audio.
    while stdout.read_exact(&mut bytes).is_ok() {
        let chunk = bytes
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]))
            .collect();
        if chunks.send(chunk).is_err() {
            break;
        }
    }
}

fn read_frames(
    mut stdout: impl std::io::Read,
    details: VideoDetails,
    start: Duration,
    frames: SyncSender<VideoFrame>,
) {
    let mut index: u64 = 0;
    loop {
        let mut bytes = vec![0; frame_len(&details)];
        if stdout.read_exact(&mut bytes).is_err() {
            break;
        }
        let Some(image) = frame_from_bytes(&details, bytes) else {
            break;
        };
        let time = start + Duration::from_secs_f64(index as f64 / details.frames_per_second);
        if frames.send(VideoFrame { time, image }).is_err() {
            break;
        }
        index += 1;
    }
}

/// The processes and player for playing from one start position. Seeking replaces it.
struct Pipeline {
    start: Duration,
    frames: Receiver<VideoFrame>,
    next_frame: Option<VideoFrame>,
    video_ended: bool,
    player: Option<Player>,
    underrun_frames: Arc<AtomicU64>,
    /// Without an audio track, time is measured on the wall clock while playing.
    played_before: Duration,
    playing_since: Option<Instant>,
}

pub struct VideoPlayback {
    abs_path: PathBuf,
    details: VideoDetails,
    device_sink: Option<MixerDeviceSink>,
    volume: f32,
    pipeline: Option<Pipeline>,
}

impl VideoPlayback {
    /// `device_sink` plays the audio track and is only needed when the video has one.
    pub fn new(
        abs_path: PathBuf,
        details: VideoDetails,
        device_sink: Option<MixerDeviceSink>,
    ) -> Self {
        Self {
            abs_path,
            details,
            device_sink,
            volume: 1.,
            pipeline: None,
        }
    }

    /// Starts decoding from `start`, replacing whatever was playing. The decoders start on
    /// their own threads, and frames and audio arrive once they are running.
    pub fn start(&mut self, start: Duration, playing: bool) {
        // Dropping the previous pipeline's channels stops its decoders.
        self.pipeline = None;

        let (frame_sender, frames) = mpsc::sync_channel(FRAME_QUEUE_LEN);
        let details = self.details.clone();
        spawn_reader(
            video_command(&self.abs_path, &self.details, start),
            move |stdout| read_frames(stdout, details, start, frame_sender),
        );

        let underrun_frames = Arc::new(AtomicU64::new(0));
        let player = self.device_sink.as_ref().map(|device_sink| {
            let (chunk_sender, chunks) = mpsc::sync_channel(AUDIO_QUEUE_LEN);
            spawn_reader(audio_command(&self.abs_path, start), move |stdout| {
                read_audio(stdout, chunk_sender)
            });

            let player = Player::connect_new(device_sink.mixer());
            player.set_volume(self.volume);
            if !playing {
                player.pause();
            }
            player.append(AudioStream {
                chunks,
                chunk: Vec::new(),
                index: 0,
                underrun_frames: underrun_frames.clone(),
            });
            player
        });

        self.pipeline = Some(Pipeline {
            start,
            frames,
            next_frame: None,
            video_ended: false,
            player,
            underrun_frames,
            played_before: Duration::ZERO,
            playing_since: playing.then(Instant::now),
        });
    }

    pub fn has_started(&self) -> bool {
        self.pipeline.is_some()
    }

    pub fn play(&mut self) {
        let Some(pipeline) = &mut self.pipeline else {
            return;
        };
        if let Some(player) = &pipeline.player {
            player.play();
        }
        pipeline.playing_since.get_or_insert_with(Instant::now);
    }

    pub fn pause(&mut self) {
        let Some(pipeline) = &mut self.pipeline else {
            return;
        };
        if let Some(player) = &pipeline.player {
            player.pause();
        }
        if let Some(playing_since) = pipeline.playing_since.take() {
            pipeline.played_before += playing_since.elapsed();
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        if let Some(player) = self
            .pipeline
            .as_ref()
            .and_then(|pipeline| pipeline.player.as_ref())
        {
            player.set_volume(volume);
        }
    }

    pub fn position(&self) -> Option<Duration> {
        let pipeline = self.pipeline.as_ref()?;
        let played = match &pipeline.player {
            Some(player) => {
                let underrun = Duration::from_secs_f64(
                    pipeline.underrun_frames.load(Ordering::Relaxed) as f64
                        / f64::from(AUDIO_SAMPLE_RATE.get()),
                );
                player.get_pos().saturating_sub(underrun)
            }
            None => {
                pipeline.played_before
                    + pipeline
                        .playing_since
                        .map_or(Duration::ZERO, |since| since.elapsed())
            }
        };
        Some((pipeline.start + played).min(self.details.duration))
    }

    /// The newest decoded frame that is due at the current position, or with `first`, the
    /// next decoded frame whatever its time (to show where a paused video was seeked to).
    pub fn take_due_frame(&mut self, first: bool) -> Option<Arc<RenderImage>> {
        let position = self.position()?;
        let pipeline = self.pipeline.as_mut()?;
        let mut due = None;
        loop {
            if pipeline.next_frame.is_none() {
                match pipeline.frames.try_recv() {
                    Ok(frame) => pipeline.next_frame = Some(frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        pipeline.video_ended = true;
                        break;
                    }
                }
            }
            match pipeline.next_frame.take() {
                Some(frame) if first || frame.time <= position => {
                    due = Some(frame.image);
                    if first {
                        break;
                    }
                }
                frame => {
                    pipeline.next_frame = frame;
                    break;
                }
            }
        }
        due
    }

    pub fn is_finished(&self) -> bool {
        let Some(pipeline) = &self.pipeline else {
            return false;
        };
        // With an audio track, the audio is the clock: once it ends the clock stops, and any
        // frames still queued would never come due.
        let finished = match &pipeline.player {
            Some(player) => player.empty(),
            None => pipeline.video_ended && pipeline.next_frame.is_none(),
        };
        finished
            || self
                .position()
                .is_some_and(|position| position >= self.details.duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_fit_and_stay_even() {
        assert_eq!(frame_size(1280, 720), (1280, 720));
        assert_eq!(frame_size(3840, 2160), (1920, 1080));
        assert_eq!(frame_size(1080, 1920), (608, 1080));
        assert_eq!(frame_size(641, 361), (640, 360));
        assert_eq!(frame_size(1, 1), (2, 2));
    }

    #[allow(clippy::disallowed_methods)]
    fn ffmpeg_installed() -> bool {
        ["ffmpeg", "ffprobe"].iter().all(|program| {
            util::command::new_std_command(program)
                .arg("-version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn plays_a_video_without_audio() {
        if !ffmpeg_installed() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clip.mp4");
        let status = util::command::new_std_command("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=25:duration=2")
            .args(["-metadata", "title=Test pattern", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let content = probe(&path).unwrap();
        let details = &content.details;
        assert_eq!((details.width, details.height), (320, 180));
        assert_eq!((details.frame_width, details.frame_height), (320, 180));
        assert_eq!(details.frames_per_second, 25.);
        assert!(!details.has_audio);
        assert_eq!(details.title.as_deref(), Some("Test pattern"));
        assert!((details.duration.as_secs_f64() - 2.).abs() < 0.1);
        let poster = content.poster.expect("the first frame is decoded");
        assert_eq!(
            (poster.size(0).width.0, poster.size(0).height.0),
            (320, 180)
        );

        // Seeked and paused, the first decoded frame is shown at the seeked position.
        let mut playback = VideoPlayback::new(path, details.clone(), None);
        playback.start(Duration::from_secs(1), false);
        let wait_for_frame = |playback: &mut VideoPlayback, first| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(frame) = playback.take_due_frame(first) {
                    return frame;
                }
                assert!(Instant::now() < deadline, "no frame was decoded");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for_frame(&mut playback, true);
        assert_eq!(playback.position(), Some(Duration::from_secs(1)));
        assert!(!playback.is_finished());

        playback.play();
        std::thread::sleep(Duration::from_millis(200));
        wait_for_frame(&mut playback, false);
        let position = playback.position().unwrap();
        assert!(position > Duration::from_secs(1), "{position:?}");

        // Playing to the end finishes.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !playback.is_finished() {
            playback.take_due_frame(false);
            assert!(Instant::now() < deadline, "the video never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn parses_frame_rates() {
        assert_eq!(parse_frame_rate("30/1"), Some(30.));
        assert!((parse_frame_rate("30000/1001").unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parse_frame_rate("25"), Some(25.));
        assert_eq!(parse_frame_rate("0/0"), None);
    }
}
