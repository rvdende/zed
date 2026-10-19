use std::{io::Cursor, path::Path, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use audio::AudioSettings;
use editor::EditorSettings;
use file_icons::FileIcons;
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, Image, ImageFormat,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, Render, Styled, Task, WeakEntity, Window, actions, canvas, div, img, px, relative,
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
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ItemEvent, ProjectItem, TabContentParams},
};
use worktree::LoadedBinaryFile;

actions!(
    audio_player,
    [
        /// Play or pause the audio file.
        TogglePlayback,
        /// Jump back five seconds.
        SeekBackward,
        /// Jump forward five seconds.
        SeekForward,
        /// Restart playback from the beginning whenever the audio file ends.
        ToggleRepeat,
        /// Mute or unmute playback.
        ToggleMute,
    ]
);

/// Extensions of the formats rodio is built to decode (see the workspace's rodio features).
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "m4a", "aac"];
const SEEK_STEP: Duration = Duration::from_secs(5);
const PEAKS_PER_SECOND: u32 = 50;
const POSITION_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const WAVEFORM_HEIGHT: f32 = 96.;
const WAVEFORM_BAR_WIDTH: f32 = 3.;
const WAVEFORM_BAR_GAP: f32 = 2.;
const DEFAULT_WAVEFORM_BAR_COUNT: usize = 120;
const COVER_SIZE: f32 = 200.;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<AudioPlayer>(cx);
}

fn media_icon(name: &str) -> Icon {
    Icon::from_path(format!("{}{name}.svg", assets::ICON_RS_PREFIX))
}

fn audio_extension(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> Option<String> {
    // For single-file worktrees the relative path is empty, so fall back to the worktree's own
    // path to detect audio files opened directly.
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
    AUDIO_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(extension)
}

pub struct AudioFile {
    file: Arc<project::File>,
    bytes: Arc<[u8]>,
    extension: String,
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

impl project::ProjectItem for AudioFile {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let extension = audio_extension(project, path, cx)?;
        let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
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
                bytes,
                extension,
                details,
            }))
        }))
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

struct Playback {
    // Dropping the sink closes the output device, so it lives as long as the player.
    _device_sink: MixerDeviceSink,
    player: Arc<Player>,
}

pub struct AudioPlayer {
    audio_file: Entity<AudioFile>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    playback: Option<Playback>,
    is_playing: bool,
    position: Duration,
    repeat: bool,
    muted: bool,
    error: Option<SharedString>,
    waveform_bounds: Option<Bounds<Pixels>>,
    is_scrubbing: bool,
    /// The position of a seek the audio thread has not applied yet. The player still reports
    /// its old position until then, so the progress shown comes from here instead.
    pending_seek: Option<Duration>,
    refresh_task: Option<Task<()>>,
    seek_task: Option<Task<()>>,
}

impl AudioPlayer {
    fn new(audio_file: Entity<AudioFile>, project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        Self {
            audio_file,
            project,
            focus_handle: cx.focus_handle(),
            playback: None,
            is_playing: false,
            position: Duration::ZERO,
            repeat: false,
            muted: false,
            error: None,
            waveform_bounds: None,
            is_scrubbing: false,
            pending_seek: None,
            refresh_task: None,
            seek_task: None,
        }
    }

    fn duration(&self, cx: &App) -> Duration {
        self.audio_file.read(cx).details.duration
    }

    fn open_playback(&self, cx: &App) -> Result<Playback> {
        let device_id = AudioSettings::get_global(cx).output_audio_device.clone();
        let device = audio::resolve_device(device_id.as_ref(), false)?;
        let mut device_sink = DeviceSinkBuilder::from_device(device)?
            .open_stream()
            .context("opening the audio output")?;
        device_sink.log_on_drop(false);
        let player = Player::connect_new(device_sink.mixer());
        player.pause();
        Ok(Playback {
            _device_sink: device_sink,
            player: Arc::new(player),
        })
    }

    /// Queues the audio file on the player if it finished or never started. Returns whether it
    /// had to, in which case playback starts from the beginning.
    fn ensure_queued(&self, cx: &App) -> Result<bool> {
        let Some(playback) = &self.playback else {
            return Ok(false);
        };
        if !playback.player.empty() {
            return Ok(false);
        }
        let audio_file = self.audio_file.read(cx);
        playback.player.append(decoder(
            audio_file.bytes.clone(),
            &audio_file.extension,
        )?);
        Ok(true)
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.try_play(cx) {
            log::error!("failed to play audio: {error:#}");
            self.error = Some(format!("{error:#}").into());
            self.is_playing = false;
        }
        cx.notify();
    }

    fn try_play(&mut self, cx: &mut Context<Self>) -> Result<()> {
        if self.playback.is_none() {
            let playback = self.open_playback(cx)?;
            playback.player.set_volume(if self.muted { 0. } else { 1. });
            self.playback = Some(playback);
        }
        if self.ensure_queued(cx)? {
            if self.position >= self.duration(cx) {
                self.position = Duration::ZERO;
            }
            if !self.position.is_zero() {
                self.seek_to(self.position, cx);
            }
        }
        if let Some(playback) = &self.playback {
            playback.player.play();
        }
        self.error = None;
        self.is_playing = true;
        self.start_refreshing(cx);
        Ok(())
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        if let Some(playback) = &self.playback {
            playback.player.pause();
            if self.pending_seek.is_none() && !playback.player.empty() {
                self.position = playback.player.get_pos();
            }
        }
        self.is_playing = false;
        self.refresh_task = None;
        cx.notify();
    }

    fn start_refreshing(&mut self, cx: &mut Context<Self>) {
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(POSITION_REFRESH_INTERVAL)
                    .await;
                let keep_refreshing = this.update(cx, |this, cx| this.refresh_position(cx));
                if !matches!(keep_refreshing, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn refresh_position(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(playback) = &self.playback else {
            return false;
        };
        if playback.player.empty() && self.pending_seek.is_none() {
            self.position = Duration::ZERO;
            if self.repeat
                && let Err(error) = self.ensure_queued(cx)
            {
                log::error!("failed to repeat audio: {error:#}");
                self.error = Some(format!("{error:#}").into());
                self.is_playing = false;
            } else if !self.repeat {
                self.is_playing = false;
            }
        } else if self.pending_seek.is_none() && !self.is_scrubbing {
            self.position = playback.player.get_pos();
        }
        cx.notify();
        self.is_playing
    }

    fn seek_to(&mut self, position: Duration, cx: &mut Context<Self>) {
        let position = position.min(self.duration(cx));
        self.position = position;
        cx.notify();

        let Some(playback) = &self.playback else {
            return;
        };
        if playback.player.empty() {
            // Nothing is queued: play() queues the file and seeks to `self.position`.
            return;
        }
        let player = playback.player.clone();
        self.pending_seek = Some(position);
        // `try_seek` blocks until the audio thread applies the seek, so it runs off the main
        // thread.
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
        if let Some(playback) = &self.playback {
            playback.player.set_volume(if self.muted { 0. } else { 1. });
        }
        cx.notify();
    }

    fn position_at(&self, x: Pixels, cx: &App) -> Option<Duration> {
        let bounds = self.waveform_bounds?;
        if bounds.size.width <= px(0.) {
            return None;
        }
        let fraction = ((x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        Some(self.duration(cx).mul_f32(fraction))
    }

    fn handle_waveform_mouse_down(
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

    fn handle_waveform_mouse_move(
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

    fn render_cover(&self, cx: &App) -> impl IntoElement {
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
            .map(|this| match self.audio_file.read(cx).details.tags.cover.clone() {
                Some(cover) => this.child(img(cover).size_full().object_fit(gpui::ObjectFit::Cover)),
                None => this.child(
                    media_icon("music")
                        .size(IconSize::Custom(rems(5.)))
                        .color(Color::Muted),
                ),
            })
    }

    fn render_waveform(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let played_color = colors.text_accent;
        let unplayed_color = colors.text_muted.opacity(0.4);
        let duration = self.duration(cx);
        let progress = progress_fraction(self.position, duration);

        let bar_count = self
            .waveform_bounds
            .map(|bounds| {
                let width: f32 = bounds.size.width.into();
                (width / (WAVEFORM_BAR_WIDTH + WAVEFORM_BAR_GAP)).floor() as usize
            })
            .unwrap_or(DEFAULT_WAVEFORM_BAR_COUNT)
            .max(1);
        let bars = waveform_bars(&self.audio_file.read(cx).details.peaks, bar_count);
        let entity = cx.entity().downgrade();

        div()
            .id("waveform")
            .relative()
            .w_full()
            .h(px(WAVEFORM_HEIGHT))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::handle_waveform_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_waveform_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrubbing(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_scrubbing(cx)),
            )
            .child(record_bounds(entity))
            .child(
                h_flex()
                    .size_full()
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
                                v_flex().flex_1().justify_end().child(
                                    div()
                                        .w_full()
                                        .h(relative(height))
                                        .rounded_t_sm()
                                        .bg(color),
                                ),
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
                    })),
            )
    }

    fn render_progress(&self, cx: &App) -> impl IntoElement {
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
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toggle_mute(&ToggleMute, window, cx)
                })),
            )
    }
}

/// Draws nothing; it records the waveform's bounds so clicks can be mapped to a position.
fn record_bounds(entity: WeakEntity<AudioPlayer>) -> impl IntoElement {
    canvas(
        move |bounds, _, cx| {
            entity
                .update(cx, |this, _| this.waveform_bounds = Some(bounds))
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

fn describe_format(audio_file: &AudioFile) -> String {
    let details = &audio_file.details;
    let channels = match details.channels {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        channels => format!("{channels} channels"),
    };
    format!(
        "{} · {:.1} kHz · {channels}",
        audio_file.extension.to_uppercase(),
        details.sample_rate as f32 / 1000.,
    )
}

impl Render for AudioPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let audio_file = self.audio_file.read(cx);
        let title = audio_file
            .details
            .tags
            .title
            .clone()
            .unwrap_or_else(|| audio_file.file.file_name(cx).to_string());
        let artist = match (
            audio_file.details.tags.artist.clone(),
            audio_file.details.tags.album.clone(),
        ) {
            (Some(artist), Some(album)) => Some(format!("{artist} — {album}")),
            (artist, album) => artist.or(album),
        };
        let format_description = describe_format(audio_file);
        let duration = audio_file.details.duration;
        let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();

        div()
            .track_focus(&self.focus_handle)
            .key_context("AudioPlayer")
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::seek_backward))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::toggle_repeat))
            .on_action(cx.listener(Self::toggle_mute))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .w_full()
                    .max_w(px(960.))
                    .gap_10()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_none()
                            .w(px(COVER_SIZE))
                            .gap_3()
                            .items_center()
                            .child(self.render_cover(cx))
                            .child(
                                Label::new(title)
                                    .size(LabelSize::Large)
                                    .weight(gpui::FontWeight::BOLD)
                                    .truncate(),
                            )
                            .when_some(artist, |this, artist| {
                                this.child(Label::new(artist).color(Color::Muted).truncate())
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(self.render_waveform(cx))
                            .child(self.render_progress(cx))
                            .child(
                                h_flex()
                                    .justify_between()
                                    .font_family(buffer_font)
                                    .child(Label::new(format_duration(self.position)))
                                    .child(Label::new(format_duration(duration))),
                            )
                            .child(self.render_controls(cx))
                            .child(
                                h_flex().justify_center().child(
                                    Label::new(format_description)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    h_flex().justify_center().child(
                                        Label::new(error)
                                            .size(LabelSize::Small)
                                            .color(Color::Error),
                                    ),
                                )
                            }),
                    ),
            )
    }
}

impl Focusable for AudioPlayer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum AudioPlayerEvent {}

impl EventEmitter<AudioPlayerEvent> for AudioPlayer {}

impl Item for AudioPlayer {
    type Event = AudioPlayerEvent;

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.audio_file.entity_id(), self.audio_file.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.audio_file.read(cx).file.abs_path(cx);
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
        self.audio_file
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.audio_file.read(cx).file.abs_path(cx);
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
        let file = &self.audio_file.read(cx).file;
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
        self.audio_file.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for AudioPlayer {
    type Item = AudioFile;

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
}
