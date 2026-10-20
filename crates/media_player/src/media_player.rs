mod video;

use std::{io::Cursor, path::Path, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use audio::AudioSettings;
use editor::EditorSettings;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, Image,
    ImageFormat, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ObjectFit, ParentElement, Pixels, Render, RenderImage, Styled, Task, WeakEntity, Window,
    actions, canvas, div, img, px, relative,
};
use language::{File as _, LocalFile as _};
use project::{Project, ProjectEntryId, ProjectPath};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use settings::Settings;
use symphonia::core::{
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::{MetadataOptions, MetadataRevision, StandardTagKey},
    probe::Hint,
};
use theme_settings::ThemeSettings;
use ui::{ButtonLike, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use video::{VideoContent, VideoPlayback};
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ItemEvent, ProjectItem, TabContentParams},
};
use worktree::LoadedBinaryFile;

actions!(
    media_player,
    [
        /// Play or pause the audio or video file.
        TogglePlayback,
        /// Jump back five seconds.
        SeekBackward,
        /// Jump forward five seconds.
        SeekForward,
        /// Restart playback from the beginning whenever the file ends.
        ToggleRepeat,
        /// Mute or unmute playback.
        ToggleMute,
    ]
);

/// Extensions of the formats rodio is built to decode (see the workspace's rodio features).
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "m4a", "aac"];
/// Video is decoded by ffmpeg, which reads far more formats than these; `.ts` is left out
/// because it is far more often TypeScript than an MPEG transport stream.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi"];
const SEEK_STEP: Duration = Duration::from_secs(5);
const PEAKS_PER_SECOND: u32 = 50;
const AUDIO_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const VIDEO_REFRESH_INTERVAL: Duration = Duration::from_millis(8);
const WAVEFORM_HEIGHT: f32 = 96.;
const WAVEFORM_BAR_WIDTH: f32 = 3.;
const WAVEFORM_BAR_GAP: f32 = 2.;
const DEFAULT_WAVEFORM_BAR_COUNT: usize = 120;
const COVER_SIZE: f32 = 200.;
const CONTROLS_MAX_WIDTH: f32 = 960.;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<MediaPlayer>(cx);
}

fn media_icon(name: &str) -> Icon {
    Icon::from_path(format!("{}{name}.svg", assets::ICON_RS_PREFIX))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaType {
    Audio,
    Video,
}

fn media_type(
    project: &Entity<Project>,
    path: &ProjectPath,
    cx: &App,
) -> Option<(MediaType, String)> {
    // For single-file worktrees the relative path is empty, so fall back to the worktree's own
    // path to detect media files opened directly.
    let worktree_abs_path = project
        .read(cx)
        .worktree_for_id(path.worktree_id, cx)?
        .read(cx)
        .abs_path();
    let extension = path
        .path
        .extension()
        .or_else(|| worktree_abs_path.extension()?.to_str())?
        .to_lowercase();
    if AUDIO_EXTENSIONS.contains(&extension.as_str()) {
        Some((MediaType::Audio, extension))
    } else if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
        Some((MediaType::Video, extension))
    } else {
        None
    }
}

pub struct MediaFile {
    file: Arc<project::File>,
    extension: String,
    content: MediaContent,
}

enum MediaContent {
    Audio(AudioContent),
    Video(VideoContent),
}

struct AudioContent {
    bytes: Arc<[u8]>,
    details: AudioDetails,
}

#[derive(Debug)]
struct AudioDetails {
    duration: Duration,
    sample_rate: u32,
    channels: u16,
    /// The loudest sample in each 1/PEAKS_PER_SECOND of a second, scaled so the loudest is 1.
    peaks: Vec<f32>,
    tags: Tags,
}

#[derive(Debug, Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    cover: Option<Arc<Image>>,
}

impl Tags {
    fn read(bytes: Arc<[u8]>, extension: &str) -> Result<Self> {
        let source = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(extension);
        let mut probed = symphonia::default::get_probe().format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        let mut tags = Self::default();
        // Tags can sit in front of the container (such as ID3 on MP3) or inside it.
        if let Some(metadata) = probed.metadata.get()
            && let Some(revision) = metadata.current()
        {
            tags.merge(revision);
        }
        if let Some(revision) = probed.format.metadata().current() {
            tags.merge(revision);
        }
        Ok(tags)
    }

    fn merge(&mut self, revision: &MetadataRevision) {
        for tag in revision.tags() {
            let field = match tag.std_key {
                Some(StandardTagKey::TrackTitle) => &mut self.title,
                Some(StandardTagKey::Artist) => &mut self.artist,
                Some(StandardTagKey::Album) => &mut self.album,
                _ => continue,
            };
            let value = tag.value.to_string();
            if field.is_none() && !value.trim().is_empty() {
                *field = Some(value);
            }
        }
        if self.cover.is_none() {
            self.cover = revision.visuals().iter().find_map(|visual| {
                let format = ImageFormat::from_mime_type(&visual.media_type)?;
                Some(Arc::new(Image::from_bytes(format, visual.data.to_vec())))
            });
        }
    }
}

fn decoder(bytes: Arc<[u8]>, extension: &str) -> Result<Decoder<Cursor<Arc<[u8]>>>> {
    let byte_len = bytes.len() as u64;
    Decoder::builder()
        .with_data(Cursor::new(bytes))
        .with_byte_len(byte_len)
        .with_hint(extension)
        .with_seekable(true)
        .build()
        .context("decoding audio")
}

fn analyze(bytes: Arc<[u8]>, extension: &str) -> Result<AudioDetails> {
    let decoder = decoder(bytes.clone(), extension)?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let samples_per_peak = (sample_rate / PEAKS_PER_SECOND).max(1) as usize * channels as usize;

    let mut peaks = Vec::new();
    let mut peak: f32 = 0.;
    let mut samples_in_peak = 0;
    let mut sample_count: u64 = 0;
    for sample in decoder {
        peak = peak.max(sample.abs());
        samples_in_peak += 1;
        sample_count += 1;
        if samples_in_peak == samples_per_peak {
            peaks.push(peak);
            peak = 0.;
            samples_in_peak = 0;
        }
    }
    if samples_in_peak > 0 {
        peaks.push(peak);
    }
    anyhow::ensure!(sample_count > 0, "the audio file contains no samples");

    let loudest = peaks.iter().copied().fold(0., f32::max);
    if loudest > 0. {
        for peak in &mut peaks {
            *peak /= loudest;
        }
    }

    let tags = Tags::read(bytes, extension)
        .context("reading audio tags")
        .log_err()
        .unwrap_or_default();

    Ok(AudioDetails {
        duration: Duration::from_secs_f64(
            sample_count as f64 / (f64::from(sample_rate) * f64::from(channels)),
        ),
        sample_rate,
        channels,
        peaks,
        tags,
    })
}

impl MediaFile {
    fn duration(&self) -> Duration {
        match &self.content {
            MediaContent::Audio(audio) => audio.details.duration,
            MediaContent::Video(video) => video.details.duration,
        }
    }

    fn title(&self, cx: &App) -> String {
        let title = match &self.content {
            MediaContent::Audio(audio) => audio.details.tags.title.clone(),
            MediaContent::Video(video) => video.details.title.clone(),
        };
        title.unwrap_or_else(|| self.file.file_name(cx).to_string())
    }

    fn subtitle(&self) -> Option<String> {
        let (artist, album) = match &self.content {
            MediaContent::Audio(audio) => (
                audio.details.tags.artist.clone(),
                audio.details.tags.album.clone(),
            ),
            MediaContent::Video(video) => (video.details.artist.clone(), None),
        };
        match (artist, album) {
            (Some(artist), Some(album)) => Some(format!("{artist} — {album}")),
            (artist, album) => artist.or(album),
        }
    }

    fn describe_format(&self) -> String {
        let extension = self.extension.to_uppercase();
        match &self.content {
            MediaContent::Audio(audio) => {
                let channels = match audio.details.channels {
                    1 => "Mono".to_string(),
                    2 => "Stereo".to_string(),
                    channels => format!("{channels} channels"),
                };
                format!(
                    "{extension} · {:.1} kHz · {channels}",
                    audio.details.sample_rate as f32 / 1000.,
                )
            }
            MediaContent::Video(video) => {
                let details = &video.details;
                let mut parts = vec![extension];
                parts.extend(
                    details
                        .video_codec
                        .as_ref()
                        .map(|codec| codec.to_uppercase()),
                );
                parts.push(format!("{}×{}", details.width, details.height));
                parts.push(format_frame_rate(details.frames_per_second));
                if !details.has_audio {
                    parts.push("No audio".to_string());
                }
                parts.join(" · ")
            }
        }
    }
}

impl project::ProjectItem for MediaFile {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let (media_type, extension) = media_type(project, path, cx)?;
        let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
        match media_type {
            MediaType::Audio => {
                let load_file =
                    worktree.update(cx, |worktree, cx| worktree.load_binary_file(&path.path, cx));
                Some(cx.spawn(async move |cx| {
                    let LoadedBinaryFile { file, content } = load_file.await?;
                    let bytes: Arc<[u8]> = content.into();
                    let details = cx
                        .background_spawn({
                            let bytes = bytes.clone();
                            let extension = extension.clone();
                            async move { analyze(bytes, &extension) }
                        })
                        .await?;
                    Ok(cx.new(|_| Self {
                        file,
                        extension,
                        content: MediaContent::Audio(AudioContent { bytes, details }),
                    }))
                }))
            }
            MediaType::Video => {
                // Video is streamed from disk by ffmpeg rather than loaded into memory, which
                // needs the file to be on this machine.
                let opened = (|| {
                    anyhow::ensure!(
                        project.read(cx).is_local(),
                        "Video files can only be played in local projects"
                    );
                    let entry = project
                        .read(cx)
                        .entry_for_path(path, cx)
                        .cloned()
                        .context("the video file is not in the project")?;
                    let abs_path = project
                        .read(cx)
                        .absolute_path(path, cx)
                        .context("the video file has no path on disk")?;
                    Ok((project::File::for_entry(entry, worktree), abs_path))
                })();
                let (file, abs_path) = match opened {
                    Ok(opened) => opened,
                    Err(error) => return Some(Task::ready(Err(error))),
                };
                Some(cx.spawn(async move |cx| {
                    let content = cx
                        .background_spawn(async move { video::probe(&abs_path) })
                        .await?;
                    Ok(cx.new(|_| Self {
                        file,
                        extension,
                        content: MediaContent::Video(content),
                    }))
                }))
            }
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        })
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

struct AudioPlayback {
    // Dropping the sink closes the output device, so it lives as long as the player.
    _device_sink: MixerDeviceSink,
    player: Arc<Player>,
}

enum Playback {
    Audio(AudioPlayback),
    Video(VideoPlayback),
}

pub struct MediaPlayer {
    media_file: Entity<MediaFile>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    playback: Option<Playback>,
    is_playing: bool,
    position: Duration,
    repeat: bool,
    muted: bool,
    error: Option<SharedString>,
    /// The bounds of the area you click or drag in to seek (the waveform and the slider).
    seek_area_bounds: Option<Bounds<Pixels>>,
    is_scrubbing: bool,
    /// The position of an audio seek the audio thread has not applied yet. The player still
    /// reports its old position until then, so the progress shown comes from here instead.
    pending_seek: Option<Duration>,
    /// Whether to show the next decoded frame as soon as it arrives, after seeking a paused
    /// video.
    awaiting_frame: bool,
    video_frame: Option<Arc<RenderImage>>,
    refresh_task: Option<Task<()>>,
    seek_task: Option<Task<()>>,
}

impl MediaPlayer {
    fn new(
        media_file: Entity<MediaFile>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|this, cx| {
            if let Some(frame) = this.video_frame.take() {
                cx.drop_image(frame, None);
            }
        })
        .detach();
        Self {
            media_file,
            project,
            focus_handle: cx.focus_handle(),
            playback: None,
            is_playing: false,
            position: Duration::ZERO,
            repeat: false,
            muted: false,
            error: None,
            seek_area_bounds: None,
            is_scrubbing: false,
            pending_seek: None,
            awaiting_frame: false,
            video_frame: None,
            refresh_task: None,
            seek_task: None,
        }
    }

    fn duration(&self, cx: &App) -> Duration {
        self.media_file.read(cx).duration()
    }

    fn volume(&self) -> f32 {
        if self.muted { 0. } else { 1. }
    }

    fn open_device_sink(cx: &App) -> Result<MixerDeviceSink> {
        let device_id = AudioSettings::get_global(cx).output_audio_device.clone();
        let device = audio::resolve_device(device_id.as_ref(), false)?;
        let mut device_sink = DeviceSinkBuilder::from_device(device)?
            .open_stream()
            .context("opening the audio output")?;
        device_sink.log_on_drop(false);
        Ok(device_sink)
    }

    fn ensure_playback(&mut self, cx: &App) -> Result<()> {
        if self.playback.is_some() {
            return Ok(());
        }
        let playback = match &self.media_file.read(cx).content {
            MediaContent::Audio(_) => {
                let device_sink = Self::open_device_sink(cx)?;
                let player = Player::connect_new(device_sink.mixer());
                player.pause();
                player.set_volume(self.volume());
                Playback::Audio(AudioPlayback {
                    _device_sink: device_sink,
                    player: Arc::new(player),
                })
            }
            MediaContent::Video(video) => {
                let device_sink = if video.details.has_audio {
                    Some(Self::open_device_sink(cx)?)
                } else {
                    None
                };
                let mut playback =
                    VideoPlayback::new(video.abs_path.clone(), video.details.clone(), device_sink);
                playback.set_volume(self.volume());
                Playback::Video(playback)
            }
        };
        self.playback = Some(playback);
        Ok(())
    }

    /// Queues the audio file on the player if it finished or never started. Returns whether it
    /// had to, in which case playback starts from the beginning.
    fn ensure_audio_queued(&self, cx: &App) -> Result<bool> {
        let Some(Playback::Audio(playback)) = &self.playback else {
            return Ok(false);
        };
        if !playback.player.empty() {
            return Ok(false);
        }
        let media_file = self.media_file.read(cx);
        let MediaContent::Audio(audio) = &media_file.content else {
            return Ok(false);
        };
        playback
            .player
            .append(decoder(audio.bytes.clone(), &media_file.extension)?);
        Ok(true)
    }

    fn show_error(&mut self, action: &str, error: anyhow::Error) {
        log::error!("failed to {action}: {error:#}");
        self.error = Some(format!("{error:#}").into());
        self.is_playing = false;
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.try_play(cx) {
            self.show_error("play media", error);
        }
        cx.notify();
    }

    fn try_play(&mut self, cx: &mut Context<Self>) -> Result<()> {
        self.ensure_playback(cx)?;
        if self.position >= self.duration(cx) {
            self.position = Duration::ZERO;
        }
        match &mut self.playback {
            Some(Playback::Audio(_)) => {
                if self.ensure_audio_queued(cx)? && !self.position.is_zero() {
                    self.seek_to(self.position, cx);
                }
                if let Some(Playback::Audio(playback)) = &self.playback {
                    playback.player.play();
                }
            }
            Some(Playback::Video(playback)) => {
                if playback.has_started() && !playback.is_finished() {
                    playback.play();
                } else {
                    playback.start(self.position, true);
                }
            }
            None => {}
        }
        self.error = None;
        self.is_playing = true;
        self.start_refreshing(cx);
        Ok(())
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        match &mut self.playback {
            Some(Playback::Audio(playback)) => {
                playback.player.pause();
                if self.pending_seek.is_none() && !playback.player.empty() {
                    self.position = playback.player.get_pos();
                }
            }
            Some(Playback::Video(playback)) => {
                playback.pause();
                if let Some(position) = playback.position() {
                    self.position = position;
                }
            }
            None => {}
        }
        self.is_playing = false;
        self.refresh_task = None;
        cx.notify();
    }

    fn start_refreshing(&mut self, cx: &mut Context<Self>) {
        let interval = match self.playback {
            Some(Playback::Video(_)) => VIDEO_REFRESH_INTERVAL,
            _ => AUDIO_REFRESH_INTERVAL,
        };
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let keep_refreshing = this.update(cx, |this, cx| this.refresh(cx));
                if !matches!(keep_refreshing, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Catches up with playback. Returns whether it needs to be called again.
    fn refresh(&mut self, cx: &mut Context<Self>) -> bool {
        match &mut self.playback {
            Some(Playback::Audio(playback)) => {
                if playback.player.empty() && self.pending_seek.is_none() {
                    self.position = Duration::ZERO;
                    if !self.repeat {
                        self.is_playing = false;
                    } else if let Err(error) = self.ensure_audio_queued(cx) {
                        self.show_error("repeat audio", error);
                    }
                } else if self.pending_seek.is_none() && !self.is_scrubbing {
                    self.position = playback.player.get_pos();
                }
            }
            Some(Playback::Video(playback)) => {
                if let Some(frame) = playback.take_due_frame(self.awaiting_frame) {
                    self.awaiting_frame = false;
                    if let Some(previous) = self.video_frame.replace(frame) {
                        cx.drop_image(previous, None);
                    }
                }
                if self.is_playing {
                    if playback.is_finished() {
                        if self.repeat {
                            self.position = Duration::ZERO;
                            playback.start(Duration::ZERO, true);
                        } else {
                            // Leaves the last frame up; playing again starts over.
                            self.position = self.duration(cx);
                            self.is_playing = false;
                        }
                    } else if !self.is_scrubbing
                        && let Some(position) = playback.position()
                    {
                        self.position = position;
                    }
                }
            }
            None => return false,
        }
        cx.notify();
        self.is_playing || self.awaiting_frame
    }

    fn seek_to(&mut self, position: Duration, cx: &mut Context<Self>) {
        let position = position.min(self.duration(cx));
        self.position = position;
        cx.notify();

        if matches!(self.media_file.read(cx).content, MediaContent::Video(_)) {
            // A paused video still shows the frame it was seeked to, which needs a decoder.
            if let Err(error) = self.ensure_playback(cx) {
                self.show_error("seek video", error);
                return;
            }
        }
        match &mut self.playback {
            Some(Playback::Audio(playback)) => {
                if playback.player.empty() {
                    // Nothing is queued: play() queues the file and seeks to `self.position`.
                    return;
                }
                let player = playback.player.clone();
                self.pending_seek = Some(position);
                // `try_seek` blocks until the audio thread applies the seek, so it runs off the
                // main thread.
                self.seek_task = Some(cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_spawn(async move { player.try_seek(position) })
                        .await;
                    this.update(cx, |this, cx| {
                        if let Err(error) = result {
                            log::error!("failed to seek audio: {error}");
                        }
                        if this.pending_seek == Some(position) {
                            this.pending_seek = None;
                        }
                        cx.notify();
                    })
                    .log_err();
                }));
            }
            Some(Playback::Video(playback)) => {
                playback.start(position, self.is_playing);
                self.awaiting_frame = !self.is_playing;
                self.start_refreshing(cx);
            }
            None => {}
        }
    }

    fn toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_playing {
            self.pause(cx);
        } else {
            self.play(cx);
        }
    }

    fn seek_backward(&mut self, _: &SeekBackward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_to(self.position.saturating_sub(SEEK_STEP), cx);
    }

    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_to(self.position + SEEK_STEP, cx);
    }

    fn toggle_repeat(&mut self, _: &ToggleRepeat, _: &mut Window, cx: &mut Context<Self>) {
        self.repeat = !self.repeat;
        cx.notify();
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        let volume = self.volume();
        match &mut self.playback {
            Some(Playback::Audio(playback)) => playback.player.set_volume(volume),
            Some(Playback::Video(playback)) => playback.set_volume(volume),
            None => {}
        }
        cx.notify();
    }

    fn position_at(&self, x: Pixels, cx: &App) -> Option<Duration> {
        let bounds = self.seek_area_bounds?;
        if bounds.size.width <= px(0.) {
            return None;
        }
        let fraction = ((x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        Some(self.duration(cx).mul_f32(fraction))
    }

    fn handle_seek_area_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(position) = self.position_at(event.position.x, cx) {
            self.is_scrubbing = true;
            self.position = position;
            cx.notify();
        }
    }

    fn handle_seek_area_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_scrubbing {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_scrubbing(cx);
        } else if let Some(position) = self.position_at(event.position.x, cx) {
            self.position = position;
            cx.notify();
        }
    }

    fn finish_scrubbing(&mut self, cx: &mut Context<Self>) {
        if self.is_scrubbing {
            self.is_scrubbing = false;
            self.seek_to(self.position, cx);
        }
    }

    /// The waveform (for audio) and the slider, as one area you click or drag in to seek.
    fn render_seek_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let waveform = match &self.media_file.read(cx).content {
            MediaContent::Audio(audio) => Some(self.render_waveform(&audio.details.peaks, cx)),
            MediaContent::Video(_) => None,
        };
        v_flex()
            .id("seek-area")
            .relative()
            .w_full()
            .gap_3()
            // Room above and below the slider makes it easier to grab.
            .py_1p5()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::handle_seek_area_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_seek_area_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrubbing(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrubbing(cx)),
            )
            .child(record_bounds(cx.entity().downgrade()))
            .children(waveform)
            .child(self.render_slider(cx))
    }

    fn render_waveform(&self, peaks: &[f32], cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let played_color = colors.text_accent;
        let unplayed_color = colors.text_muted.opacity(0.4);
        let progress = progress_fraction(self.position, self.duration(cx));

        let bar_count = self
            .seek_area_bounds
            .map(|bounds| {
                let width: f32 = bounds.size.width.into();
                (width / (WAVEFORM_BAR_WIDTH + WAVEFORM_BAR_GAP)).floor() as usize
            })
            .unwrap_or(DEFAULT_WAVEFORM_BAR_COUNT)
            .max(1);
        let bars = waveform_bars(peaks, bar_count);

        h_flex()
            .w_full()
            .h(px(WAVEFORM_HEIGHT))
            .justify_between()
            .children(bars.into_iter().enumerate().map(|(index, peak)| {
                let is_played = (index as f32 + 0.5) / bar_count as f32 <= progress;
                let color = if is_played {
                    played_color
                } else {
                    unplayed_color
                };
                // A floor keeps silent stretches visible as a thin line.
                let height = peak.max(0.04);
                v_flex()
                    .w(px(WAVEFORM_BAR_WIDTH))
                    .h_full()
                    .child(
                        v_flex()
                            .flex_1()
                            .justify_end()
                            .child(div().w_full().h(relative(height)).rounded_t_sm().bg(color)),
                    )
                    .child(
                        v_flex().flex_1().pt_px().child(
                            div()
                                .w_full()
                                .h(relative(height * 0.6))
                                .rounded_b_sm()
                                .bg(color.opacity(0.45)),
                        ),
                    )
            }))
    }

    fn render_slider(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let progress = progress_fraction(self.position, self.duration(cx));
        div()
            .relative()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .bg(colors.border_variant)
            .child(
                div()
                    .h_full()
                    .w(relative(progress))
                    .rounded_full()
                    .bg(colors.text_accent),
            )
            .child(
                div()
                    .absolute()
                    .top(px(-4.))
                    .left(relative(progress))
                    .ml(px(-6.))
                    .size(px(12.))
                    .rounded_full()
                    .bg(colors.text_accent),
            )
    }

    fn render_times(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .justify_between()
            .font_family(ThemeSettings::get_global(cx).buffer_font.family.clone())
            .child(Label::new(format_duration(self.position)))
            .child(Label::new(format_duration(self.duration(cx))))
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let small_button = |id: &'static str, icon: &str| {
            ButtonLike::new(id)
                .size(ButtonSize::Large)
                .child(media_icon(icon).size(IconSize::Medium))
        };
        let (play_icon, play_title) = if self.is_playing {
            ("pause", "Pause")
        } else {
            ("play", "Play")
        };

        h_flex()
            .w_full()
            .justify_center()
            .gap_6()
            .child(
                small_button("repeat", "repeat")
                    .toggle_state(self.repeat)
                    .tooltip(Tooltip::for_action_title("Repeat", &ToggleRepeat))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_repeat(&ToggleRepeat, window, cx)
                    })),
            )
            .child(
                small_button("seek-backward", "skip-back")
                    .tooltip(Tooltip::for_action_title("Back 5 Seconds", &SeekBackward))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.seek_backward(&SeekBackward, window, cx)
                    })),
            )
            .child(
                div()
                    .id("toggle-playback")
                    .size(px(56.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_2()
                    .border_color(colors.text)
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .active(|style| style.bg(colors.element_active))
                    .tooltip(Tooltip::for_action_title(play_title, &TogglePlayback))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_playback(&TogglePlayback, window, cx)
                    }))
                    .child(media_icon(play_icon).size(IconSize::XLarge)),
            )
            .child(
                small_button("seek-forward", "skip-forward")
                    .tooltip(Tooltip::for_action_title("Forward 5 Seconds", &SeekForward))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.seek_forward(&SeekForward, window, cx)
                    })),
            )
            .child(
                small_button(
                    "toggle-mute",
                    if self.muted { "volume-mute" } else { "volume" },
                )
                .toggle_state(self.muted)
                .tooltip(Tooltip::for_action_title(
                    if self.muted { "Unmute" } else { "Mute" },
                    &ToggleMute,
                ))
                .on_click(
                    cx.listener(|this, _, window, cx| this.toggle_mute(&ToggleMute, window, cx)),
                ),
            )
    }

    fn render_footer(&self, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_1()
            .items_center()
            .child(
                Label::new(self.media_file.read(cx).describe_format())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }

    fn render_cover(&self, cover: Option<Arc<Image>>, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .flex_none()
            .size(px(COVER_SIZE))
            .rounded_lg()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.element_background)
            .border_1()
            .border_color(colors.border_variant)
            .map(|this| match cover {
                Some(cover) => this.child(img(cover).size_full().object_fit(ObjectFit::Cover)),
                None => this.child(
                    media_icon("music")
                        .size(IconSize::Custom(rems(5.)))
                        .color(Color::Muted),
                ),
            })
    }

    fn render_audio(&self, cover: Option<Arc<Image>>, cx: &mut Context<Self>) -> AnyElement {
        let media_file = self.media_file.read(cx);
        let title = media_file.title(cx);
        let subtitle = media_file.subtitle();
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                h_flex()
                    .w_full()
                    .max_w(px(CONTROLS_MAX_WIDTH))
                    .gap_10()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_none()
                            .w(px(COVER_SIZE))
                            .gap_3()
                            .items_center()
                            .child(self.render_cover(cover, cx))
                            .child(
                                Label::new(title)
                                    .size(LabelSize::Large)
                                    .weight(gpui::FontWeight::BOLD)
                                    .truncate(),
                            )
                            .when_some(subtitle, |this, subtitle| {
                                this.child(Label::new(subtitle).color(Color::Muted).truncate())
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(self.render_seek_area(cx))
                            .child(self.render_times(cx))
                            .child(self.render_controls(cx))
                            .child(self.render_footer(cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_video(&self, poster: Option<Arc<RenderImage>>, cx: &mut Context<Self>) -> AnyElement {
        let frame = self.video_frame.clone().or(poster);
        let media_file = self.media_file.read(cx);
        let title = media_file.title(cx);
        let subtitle = media_file.subtitle();
        v_flex()
            .size_full()
            .child(
                div()
                    .id("video")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .bg(gpui::black())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_playback(&TogglePlayback, window, cx)
                    }))
                    .map(|this| match frame {
                        Some(frame) => {
                            this.child(img(frame).size_full().object_fit(ObjectFit::Contain))
                        }
                        None => this.child(Label::new("No preview").color(Color::Muted)),
                    }),
            )
            .child(
                h_flex().w_full().justify_center().px_8().py_4().child(
                    v_flex()
                        .w_full()
                        .max_w(px(CONTROLS_MAX_WIDTH))
                        .gap_3()
                        .child(
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .child(Label::new(title).weight(gpui::FontWeight::BOLD).truncate())
                                .when_some(subtitle, |this, subtitle| {
                                    this.child(Label::new(subtitle).color(Color::Muted).truncate())
                                }),
                        )
                        .child(self.render_seek_area(cx))
                        .child(self.render_times(cx))
                        .child(self.render_controls(cx))
                        .child(self.render_footer(cx)),
                ),
            )
            .into_any_element()
    }
}

/// Draws nothing; it records the seek area's bounds so clicks can be mapped to a position.
fn record_bounds(entity: WeakEntity<MediaPlayer>) -> impl IntoElement {
    canvas(
        move |bounds, _, cx| {
            entity
                .update(cx, |this, _| this.seek_area_bounds = Some(bounds))
                .log_err();
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

fn progress_fraction(position: Duration, duration: Duration) -> f32 {
    if duration.is_zero() {
        0.
    } else {
        (position.as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
    }
}

/// Reduces the peaks to `bar_count` bars, each as loud as the loudest peak it covers.
fn waveform_bars(peaks: &[f32], bar_count: usize) -> Vec<f32> {
    if peaks.is_empty() || bar_count == 0 {
        return vec![0.; bar_count];
    }
    (0..bar_count)
        .map(|bar| {
            let start = bar * peaks.len() / bar_count;
            let end = ((bar + 1) * peaks.len() / bar_count).max(start + 1);
            peaks
                .get(start..end.min(peaks.len()))
                .unwrap_or_default()
                .iter()
                .copied()
                .fold(0., f32::max)
        })
        .collect()
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn format_frame_rate(frames_per_second: f64) -> String {
    if (frames_per_second - frames_per_second.round()).abs() < 0.01 {
        format!("{} fps", frames_per_second.round())
    } else {
        format!("{frames_per_second:.2} fps")
    }
}

impl Render for MediaPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.media_file.read(cx).content {
            MediaContent::Audio(audio) => {
                let cover = audio.details.tags.cover.clone();
                self.render_audio(cover, cx)
            }
            MediaContent::Video(video) => {
                let poster = video.poster.clone();
                self.render_video(poster, cx)
            }
        };
        div()
            .track_focus(&self.focus_handle)
            .key_context("MediaPlayer")
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::toggle_repeat))
            .on_action(cx.listener(Self::toggle_mute))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

impl Focusable for MediaPlayer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum MediaPlayerEvent {}

impl EventEmitter<MediaPlayerEvent> for MediaPlayer {}

impl Item for MediaPlayer {
    type Event = MediaPlayerEvent;

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.media_file.entity_id(), self.media_file.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.media_file.read(cx).file.abs_path(cx);
        Some(abs_path.compact().to_string_lossy().into_owned().into())
    }

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.media_file
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.media_file.read(cx).file.abs_path(cx);
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if EditorSettings::get_global(cx).toolbar.breadcrumbs {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let project = self.project.read(cx);
        let file = &self.media_file.read(cx).file;
        let mut path = file.path().to_rel_path_buf();
        if project.visible_worktrees(cx).count() > 1
            && let Some(worktree) = project.worktree_for_id(file.worktree_id(cx), cx)
        {
            path = worktree.read(cx).root_name().join(&path);
        }
        Some((
            vec![HighlightedText {
                text: path.display(project.path_style(cx)).to_string().into(),
                highlights: vec![],
            }],
            Some(ThemeSettings::get_global(cx).buffer_font.clone()),
        ))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.media_file.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for MediaPlayer {
    type Item = MediaFile;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit PCM WAV file holding `samples`, interleaved over `channels`.
    fn wav_bytes(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn analyzes_wav() {
        // Two seconds of stereo: quiet for the first second, loud for the second.
        let sample_rate = 8000;
        let samples: Vec<i16> = (0..sample_rate * 2 * 2)
            .map(|index| if index < sample_rate * 2 { 1000 } else { 16000 })
            .map(|amplitude: u32| amplitude as i16)
            .collect();
        let details = analyze(wav_bytes(sample_rate, 2, &samples).into(), "wav").unwrap();

        assert_eq!(details.sample_rate, sample_rate);
        assert_eq!(details.channels, 2);
        assert_eq!(details.duration, Duration::from_secs(2));
        assert_eq!(details.peaks.len(), 2 * PEAKS_PER_SECOND as usize);
        let bars = waveform_bars(&details.peaks, 2);
        assert!(bars[0] < 0.1, "quiet half should be quiet: {bars:?}");
        assert_eq!(bars[1], 1.);
        assert!(details.tags.title.is_none());
    }

    #[test]
    fn decodes_compressed_audio_extensions() {
        use symphonia::core::codecs::{
            CODEC_TYPE_AAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_VORBIS,
        };

        let codecs = symphonia::default::get_codecs();
        for (name, codec) in [
            ("mp3", CODEC_TYPE_MP3),
            ("flac", CODEC_TYPE_FLAC),
            ("vorbis", CODEC_TYPE_VORBIS),
            ("aac", CODEC_TYPE_AAC),
        ] {
            assert!(codecs.get_codec(codec).is_some(), "no {name} decoder");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(analyze(vec![0u8; 64].into(), "mp3").is_err());
    }

    #[test]
    fn waveform_bars_cover_all_peaks() {
        let peaks = [0.1, 0.5, 0.2, 0.9, 0.3];
        assert_eq!(waveform_bars(&peaks, 2), vec![0.5, 0.9]);
        assert_eq!(waveform_bars(&peaks, 1), vec![0.9]);
        // More bars than peaks repeats peaks rather than leaving gaps.
        assert_eq!(waveform_bars(&[0.4, 0.8], 4), vec![0.4, 0.4, 0.8, 0.8]);
        assert_eq!(waveform_bars(&[], 3), vec![0., 0., 0.]);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(format_duration(Duration::from_secs(145)), "2:25");
        assert_eq!(format_duration(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn formats_frame_rates() {
        assert_eq!(format_frame_rate(30.), "30 fps");
        assert_eq!(format_frame_rate(30000. / 1001.), "29.97 fps");
    }
}
