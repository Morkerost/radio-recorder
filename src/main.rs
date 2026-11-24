use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, Timelike};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
#[macro_use]
mod language;
use language::{tr, Language};
mod encoding;
use encoding::{AudioFormat, Mp3Encoder, SegmentWriter};
mod menu;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type AppResult<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    language: Language,
    input_device: String,
    output_directory: String,
    segment_minutes: u32,
    retention_hours: u64,
    filename_format: String,
    audio_format: AudioFormat,
    mp3_bitrate_kbps: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::English,
            input_device: String::new(),
            output_directory: default_recordings_directory()
                .to_string_lossy()
                .into_owned(),
            segment_minutes: 10,
            retention_hours: 1000,
            filename_format: "%H_%M_%S".to_owned(),
            audio_format: AudioFormat::Wav,
            mp3_bitrate_kbps: 128,
        }
    }
}

#[derive(Default)]
struct CliOptions {
    config: Option<PathBuf>,
    device: Option<String>,
    directory: Option<String>,
    filename_format: Option<String>,
    audio_format: Option<AudioFormat>,
    mp3_bitrate_kbps: Option<u16>,
    max_age_seconds: Option<u64>,
    list_devices: bool,
    help: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecorderState {
    year: i32,
    day_of_year: u32,
    folder_number: u64,
}

struct AudioChunk {
    samples: Vec<i16>,
    captured_at: DateTime<Local>,
}

#[derive(Clone)]
enum Status {
    Starting,
    Waiting {
        device: String,
        boundary: DateTime<Local>,
    },
    Recording {
        device: String,
        file: PathBuf,
        next: DateTime<Local>,
        peaks: Vec<f32>,
    },
}

fn main() {
    panic::set_hook(Box::new(|info| eprintln!("Application error: {info}")));
    let outcome = panic::catch_unwind(AssertUnwindSafe(run));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fail_and_exit(&error.to_string()),
        Err(_) => fail_and_exit("The program encountered an unexpected internal error."),
    }
}

fn fail_and_exit(message: &str) -> ! {
    eprintln!("{}: {message}", tr("Error", "Ошибка"));
    if io::stdin().is_terminal() {
        eprintln!(
            "{}",
            tr(
                "Press Enter to close this window.",
                "Нажмите Enter, чтобы закрыть окно."
            )
        );
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
    }
    std::process::exit(1);
}

fn run() -> AppResult<()> {
    let options = parse_args(std::env::args().skip(1))?;
    let config_path = options.config.clone().unwrap_or_else(default_config_path);
    if options.help || options.list_devices {
        if config_path.is_file() {
            if let Ok(settings) = load_settings(&config_path) {
                settings.language.activate();
            }
        }
        if options.help {
            print_help();
            return Ok(());
        }
        return list_devices(&cpal::default_host());
    }
    let host = cpal::default_host();
    let mut settings = load_settings(&config_path)?;
    settings.language.activate();
    if let Some(device) = options.device {
        settings.input_device = device;
    }
    if let Some(directory) = options.directory {
        settings.output_directory = directory;
    }
    if let Some(filename_format) = options.filename_format {
        settings.filename_format = filename_format;
    }
    if let Some(format) = options.audio_format {
        settings.audio_format = format;
    }
    if let Some(kbps) = options.mp3_bitrate_kbps {
        settings.mp3_bitrate_kbps = kbps;
    }

    if options.max_age_seconds == Some(0) {
        return Err("--max-age must be greater than zero".into());
    }

    let keep_running = Arc::new(AtomicBool::new(true));
    let signal_flag = Arc::clone(&keep_running);
    ctrlc::set_handler(move || signal_flag.store(false, Ordering::SeqCst))?;

    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return menu::run(
            &host,
            settings,
            &config_path,
            options.max_age_seconds,
            &keep_running,
        );
    }
    let age = options
        .max_age_seconds
        .unwrap_or_else(|| settings.retention_hours.saturating_mul(3600));
    let mut previous_file = PathBuf::new();
    record_once(
        &host,
        &settings,
        age,
        &keep_running,
        |status| match status {
            Status::Waiting { device, boundary } => {
                eprintln!(
                    "{}",
                    localized!(
                        "{device} • Starting at {}",
                        "{device} • Старт в {}",
                        boundary.format("%H:%M:%S")
                    )
                )
            }
            Status::Recording { file, .. } if file != previous_file => {
                eprintln!(
                    "{}",
                    localized!("Recording to {}", "Запись в {}", file.display())
                );
                previous_file = file;
            }
            _ => {}
        },
    )
}

fn record_once(
    host: &cpal::Host,
    settings: &Settings,
    age: u64,
    running: &AtomicBool,
    report: impl FnMut(Status),
) -> AppResult<()> {
    validate_settings(settings)?;
    let device = select_device(host, &settings.input_device)?;
    let device_name = device.name()?;
    let supported = device.default_input_config()?;
    let config = supported.config();
    let spec = hound::WavSpec {
        channels: config.channels,
        sample_rate: config.sample_rate.0,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if settings.audio_format == AudioFormat::Mp3 {
        Mp3Encoder::validate_input(spec)?;
    }
    let root = Path::new(&settings.output_directory);
    fs::create_dir_all(root)?;
    delete_old_recordings(root, age)?;
    let (sender, receiver) = mpsc::channel();
    let stream = build_stream(&device, &config, supported.sample_format(), sender)?;
    stream.play()?;
    let boundary = first_boundary_at_or_after(Local::now(), settings.segment_minutes);
    let capture = Capture {
        device_name,
        spec,
        receiver,
    };
    capture.write_segments(settings, age, running, boundary, report)
}

struct Capture {
    device_name: String,
    spec: hound::WavSpec,
    receiver: mpsc::Receiver<Result<AudioChunk, String>>,
}

impl Capture {
    fn write_segments(
        self,
        settings: &Settings,
        age: u64,
        running: &AtomicBool,
        mut boundary: DateTime<Local>,
        mut report: impl FnMut(Status),
    ) -> AppResult<()> {
        let Self {
            device_name,
            spec,
            receiver,
        } = self;
        let root = Path::new(&settings.output_directory);
        let interval = ChronoDuration::minutes(settings.segment_minutes.into());
        report(Status::Waiting {
            device: device_name.clone(),
            boundary,
        });
        let mut current: Option<(SegmentWriter, PathBuf)> = None;
        let mut last_report = std::time::Instant::now();
        let capture_result: AppResult<()> = loop {
            if !running.load(Ordering::SeqCst) {
                break Ok(());
            }
            let chunk = match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(chunk)) => chunk,
                Ok(Err(error)) => break Err(error.into()),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(tr(
                        "Lost connection to the audio input",
                        "Связь с аудиовходом потеряна",
                    )
                    .into())
                }
            };
            let rotate = chunk.captured_at >= boundary;
            if rotate {
                if let Some((writer, _)) = current.take() {
                    writer.finalize()?;
                    delete_old_recordings(root, age)?;
                }
                let skipped = (chunk.captured_at - boundary).num_minutes() / interval.num_minutes();
                let start = boundary + ChronoDuration::minutes(skipped * interval.num_minutes());
                let (writer, path) = start_segment(settings, root, start, spec)?;
                current = Some((writer, path));
                boundary = start + interval;
            }
            if let Some((writer, file)) = &mut current {
                writer.write_samples(&chunk.samples)?;
                if rotate || last_report.elapsed() >= Duration::from_millis(200) {
                    report(Status::Recording {
                        device: device_name.clone(),
                        file: file.clone(),
                        next: boundary,
                        peaks: calculate_peaks(&chunk.samples, spec.channels.into()),
                    });
                    last_report = std::time::Instant::now();
                }
            }
        };
        if let Some((writer, _)) = current {
            writer.finalize()?;
        }
        capture_result
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> AppResult<CliOptions> {
    let mut options = CliOptions::default();
    let args: Vec<String> = args.collect();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--help" | "-h" => options.help = true,
            "--list-devices" | "-l" => options.list_devices = true,
            "--config" => options.config = Some(PathBuf::from(next_arg(&args, &mut index, arg)?)),
            "--device" => options.device = Some(next_arg(&args, &mut index, arg)?.to_owned()),
            "--directory" | "-d" => {
                options.directory = Some(next_arg(&args, &mut index, arg)?.to_owned())
            }
            "--format" => {
                options.filename_format = Some(next_arg(&args, &mut index, arg)?.to_owned())
            }
            "--max-age" => {
                options.max_age_seconds = Some(parse_age(next_arg(&args, &mut index, arg)?)?)
            }
            "--audio-format" => {
                let format = next_arg(&args, &mut index, arg)?;
                options.audio_format = Some(match format.to_ascii_lowercase().as_str() {
                    "wav" => AudioFormat::Wav,
                    "mp3" => AudioFormat::Mp3,
                    _ => return Err("--audio-format: WAV or MP3".into()),
                });
            }
            "--mp3-bitrate" => {
                let kbps = next_arg(&args, &mut index, arg)?.parse()?;
                encoding::bitrate(kbps)?;
                options.mp3_bitrate_kbps = Some(kbps);
            }
            other => {
                return Err(format!("Unknown argument '{other}'. Use --help for options.").into())
            }
        }
        index += 1;
    }
    Ok(options)
}

fn next_arg<'a>(args: &'a [String], index: &mut usize, option: &str) -> AppResult<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| format!("{option} requires a value").into())
}

fn parse_age(value: &str) -> AppResult<u64> {
    let split_at = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or("--max-age needs a unit: s, m, h, or d (example: 1000h)")?;
    let amount: u64 = value[..split_at].parse()?;
    let multiplier = match value[split_at..].to_ascii_lowercase().as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err("--max-age unit must be s, m, h, or d".into()),
    };
    Ok(amount.saturating_mul(multiplier))
}

fn print_help() {
    if language::russian() {
        println!(
            "Radio Recorder\nИспользование: radio-recorder.exe [ПАРАМЕТРЫ]\n\
            --device ИМЯ        Выбрать аудиовход по полному имени\n\
            --directory, -d ПУТЬ  Задать папку записей\n\
            --format ШАБЛОН     Шаблон имени, по умолчанию %H_%M_%S\n\
            --max-age СРОК      Срок хранения, например 1000h\n\
            --audio-format WAV|MP3  Формат записи\n\
            --mp3-bitrate КБИТ  Битрейт MP3 (по умолчанию 128 кбит/с)\n\
            --list-devices, -l  Показать аудиовходы\n\
            --config ПУТЬ       Использовать выбранный файл настроек\n\
            --help, -h          Показать справку"
        );
        return;
    }
    println!(
        "Radio Recorder\n\
         Usage: radio-recorder.exe [OPTIONS]\n\
         --device NAME       Select an exact input device name\n\
         --directory, -d DIR Set the root directory for numbered day folders\n\
         --format FORMAT     Set the filename time format (default: %H_%M_%S)\n\
         --max-age DURATION  Delete older recordings (for example 1000h, 30d)\n\
         --audio-format WAV|MP3  Choose recording format\n\
         --mp3-bitrate KBPS  Set MP3 bitrate (default 128 kbps)\n\
         --list-devices, -l  List available input devices\n\
         --config PATH       Read settings from a different TOML file\n\
         --help, -h          Show this help"
    );
}

fn load_settings(path: &Path) -> AppResult<Settings> {
    if !path.exists() {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let defaults = Settings::default();
        save_settings(path, &defaults)?;
        eprintln!("Created {} with default settings.", path.display());
        return Ok(defaults);
    }
    toml::from_str(&fs::read_to_string(path)?).map_err(|error| {
        localized!(
            "Could not read {}: {error}",
            "Не удалось прочитать {}: {error}",
            path.display()
        )
        .into()
    })
}

fn save_settings(path: &Path, settings: &Settings) -> AppResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(settings)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|_| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(tmp);
        return Err(e.into());
    }
    Ok(())
}

fn app_data_directory() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(path) = std::env::var_os("APPDATA") {
            return PathBuf::from(path);
        }
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path);
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(path);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config");
        }
    }

    std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("AppData")
}

fn default_config_path() -> PathBuf {
    app_data_directory()
        .join("RadioRecorder")
        .join("settings.toml")
}

fn default_recordings_directory() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("Music").join("RadioRecorder");
    }

    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Music").join("RadioRecorder");
    }

    app_data_directory()
        .join("RadioRecorder")
        .join("Recordings")
}

fn validate_minutes(minutes: u32) -> AppResult<()> {
    if minutes == 0 || minutes > 60 || 60 % minutes != 0 {
        return Err(tr(
            "Choose a length from 1 to 60 minutes that divides 60 evenly",
            "Длина сегмента должна быть от 1 до 60 минут и делить час без остатка",
        )
        .into());
    }
    Ok(())
}

fn validate_hours(hours: u64) -> AppResult<()> {
    if hours == 0 {
        return Err(tr(
            "Retention must be greater than zero",
            "Срок хранения должен быть больше нуля",
        )
        .into());
    }
    Ok(())
}

fn validate_settings(settings: &Settings) -> AppResult<()> {
    if settings.audio_format == AudioFormat::Mp3 {
        encoding::bitrate(settings.mp3_bitrate_kbps)?;
    }
    validate_minutes(settings.segment_minutes)?;
    validate_hours(settings.retention_hours)?;
    if settings.output_directory.trim().is_empty() {
        return Err(tr("Choose a recordings folder", "Укажите папку записей").into());
    }
    filename_stem(&settings.filename_format, Local::now())?;
    Ok(())
}

fn filename_stem(format: &str, start: DateTime<Local>) -> AppResult<String> {
    use chrono::format::{Item, StrftimeItems};
    if StrftimeItems::new(format).any(|item| matches!(item, Item::Error)) {
        return Err(tr(
            "Unknown filename format code. Try %H_%M_%S",
            "Неизвестный код в шаблоне имени. Например, %H_%M_%S",
        )
        .into());
    }
    let stem = start.format(format).to_string();
    let first = stem
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(
        first.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || first
        .strip_prefix("COM")
        .or_else(|| first.strip_prefix("LPT"))
        .is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if reserved
        || stem.trim().is_empty()
        || stem.ends_with([' ', '.'])
        || stem.encode_utf16().count() > 246
        || stem
            .chars()
            .any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
    {
        return Err(tr(
            "Choose a valid Windows filename pattern, such as %H_%M_%S",
            "Шаблон должен давать допустимое имя файла Windows. Например, %H_%M_%S",
        )
        .into());
    }
    Ok(stem)
}

fn list_devices(host: &cpal::Host) -> AppResult<()> {
    let devices = input_devices(host)?;
    for (index, (name, is_default)) in devices.iter().enumerate() {
        let marker = if *is_default {
            tr(" (default)", " (по умолчанию)")
        } else {
            ""
        };
        println!("{}: {name}{marker}", index + 1);
    }
    if devices.is_empty() {
        println!(
            "{}",
            localized!("No input devices found.", "Устройства записи не найдены.")
        );
    }
    Ok(())
}

fn input_devices(host: &cpal::Host) -> AppResult<Vec<(String, bool)>> {
    let default_name = host
        .default_input_device()
        .and_then(|device| device.name().ok());
    let mut devices = Vec::new();
    for device in host.input_devices()? {
        let name = device
            .name()
            .unwrap_or_else(|_| "Unknown input device".to_owned());
        let is_default = default_name.as_deref() == Some(name.as_str());
        devices.push((name, is_default));
    }
    Ok(devices)
}

fn open_folder(path: &Path) -> AppResult<()> {
    fs::create_dir_all(path)?;
    #[cfg(windows)]
    Command::new("explorer.exe")
        .arg(std::path::absolute(path)?)
        .spawn()?;
    #[cfg(target_os = "macos")]
    Command::new("open").arg(path).spawn()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}

fn select_device(host: &cpal::Host, wanted_name: &str) -> AppResult<cpal::Device> {
    if wanted_name.trim().is_empty() {
        return host.default_input_device().ok_or_else(|| {
            tr("No default audio input. Connect one or select a device from the menu.", "Устройство записи по умолчанию не найдено. Подключите аудиовход или выберите устройство в меню.").into()
        });
    }
    for device in host.input_devices()? {
        if device
            .name()
            .map(|name| name == wanted_name)
            .unwrap_or(false)
        {
            return Ok(device);
        }
    }
    Err(localized!(
        "Audio input “{wanted_name}” is unavailable. Choose a device from the menu.",
        "Устройство записи «{wanted_name}» не найдено. Выберите его заново в меню устройств."
    )
    .into())
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    sender: Sender<Result<AudioChunk, String>>,
) -> AppResult<Stream> {
    macro_rules! build_for {
        ($sample_type:ty) => {{
            let errors = sender.clone();
            let sender = sender.clone();
            device.build_input_stream(
                config,
                move |data: &[$sample_type], _| {
                    let samples = data
                        .iter()
                        .map(|sample| (*sample).to_sample::<i16>())
                        .collect();
                    let _ = sender.send(Ok(AudioChunk {
                        samples,
                        captured_at: Local::now(),
                    }));
                },
                move |error| {
                    let _ = errors.send(Err(localized!(
                        "Audio input error: {error}",
                        "Ошибка аудиовхода: {error}"
                    )));
                },
                None,
            )?
        }};
    }

    let stream = match sample_format {
        SampleFormat::I8 => build_for!(i8),
        SampleFormat::I16 => build_for!(i16),
        SampleFormat::I32 => build_for!(i32),
        SampleFormat::I64 => build_for!(i64),
        SampleFormat::U8 => build_for!(u8),
        SampleFormat::U16 => build_for!(u16),
        SampleFormat::U32 => build_for!(u32),
        SampleFormat::U64 => build_for!(u64),
        SampleFormat::F32 => build_for!(f32),
        SampleFormat::F64 => build_for!(f64),
        other => return Err(format!("Unsupported input sample format: {other:?}").into()),
    };
    Ok(stream)
}

fn calculate_peaks(samples: &[i16], channels: usize) -> Vec<f32> {
    let mut peaks = vec![0i16; channels.max(1)];
    for (index, sample) in samples.iter().enumerate() {
        let channel = index % peaks.len();
        let magnitude = sample.saturating_abs();
        peaks[channel] = peaks[channel].max(magnitude);
    }
    peaks
        .into_iter()
        .map(|peak| peak as f32 / i16::MAX as f32)
        .collect()
}

fn first_boundary_at_or_after(now: DateTime<Local>, interval_minutes: u32) -> DateTime<Local> {
    let minute = now.minute();
    let second = now.second();
    let truncated = now
        .with_second(0)
        .and_then(|time| time.with_nanosecond(0))
        .unwrap_or_else(|| now.clone());
    let remainder = minute % interval_minutes;
    if remainder == 0 && second == 0 && now.nanosecond() == 0 {
        truncated
    } else {
        let minutes_to_add = if remainder == 0 {
            interval_minutes
        } else {
            interval_minutes - remainder
        };
        truncated + ChronoDuration::minutes(minutes_to_add as i64)
    }
}

fn start_segment(
    settings: &Settings,
    output_root: &Path,
    boundary: DateTime<Local>,
    spec: hound::WavSpec,
) -> AppResult<(SegmentWriter, PathBuf)> {
    let mp3 = match settings.audio_format {
        AudioFormat::Wav => None,
        AudioFormat::Mp3 => Some(Mp3Encoder::new(spec, settings.mp3_bitrate_kbps)?),
    };
    let day_directory = ensure_day_folder(output_root, boundary.clone())?;
    let stem = filename_stem(&settings.filename_format, boundary)?;
    let ext = settings.audio_format.extension();

    for suffix in 0..1000u32 {
        let filename = if suffix == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}_{}.{ext}", suffix + 1)
        };
        let path = day_directory.join(filename);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let file = BufWriter::new(file);
                let writer = match mp3 {
                    Some(encoder) => SegmentWriter::Mp3(encoder, file),
                    None => SegmentWriter::Wav(hound::WavWriter::new(file, spec)?),
                };
                return Ok((writer, path));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(tr(
        "Could not find a free filename for this time slot",
        "Не удалось найти свободное имя для этого файла",
    )
    .into())
}

fn date_state(at: &DateTime<Local>, folder_number: u64) -> RecorderState {
    RecorderState {
        year: at.year(),
        day_of_year: at.ordinal(),
        folder_number,
    }
}

fn ensure_day_folder(output_root: &Path, at: DateTime<Local>) -> AppResult<PathBuf> {
    let requested = date_state(&at, 0);
    let state_path = output_root.join(".recorder_state");
    let stored_state = fs::read_to_string(&state_path)
        .ok()
        .and_then(|contents| toml::from_str::<RecorderState>(&contents).ok());

    if let Some(state) = stored_state.as_ref() {
        if state.year == requested.year
            && state.day_of_year == requested.day_of_year
            && output_root.join(state.folder_number.to_string()).is_dir()
        {
            let directory = output_root.join(state.folder_number.to_string());
            write_day_marker(&directory, state)?;
            return Ok(directory);
        }
    }

    let mut matching_folder: Option<(u64, PathBuf)> = None;
    let mut largest_folder = stored_state
        .as_ref()
        .map(|state| state.folder_number)
        .unwrap_or(0);
    for entry in fs::read_dir(output_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(number) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        largest_folder = largest_folder.max(number);
        let marker = entry.path().join(".day_state");
        if let Ok(contents) = fs::read_to_string(marker) {
            if let Ok(state) = toml::from_str::<RecorderState>(&contents) {
                if state.folder_number == number
                    && state.year == requested.year
                    && state.day_of_year == requested.day_of_year
                {
                    if matching_folder
                        .as_ref()
                        .map(|(existing, _)| number < *existing)
                        .unwrap_or(true)
                    {
                        matching_folder = Some((number, entry.path()));
                    }
                }
            }
        }
    }

    if let Some((number, directory)) = matching_folder {
        let state = date_state(&at, number);
        write_day_marker(&directory, &state)?;
        write_root_state(&state_path, &state)?;
        return Ok(directory);
    }

    let folder_number = largest_folder.saturating_add(1).max(1);
    let directory = output_root.join(folder_number.to_string());
    let temporary_directory = output_root.join(format!(
        ".day-{}-{}-{}-{}.tmp",
        requested.year,
        requested.day_of_year,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if temporary_directory.exists() {
        return Err(format!(
            "Temporary folder already exists: {}",
            temporary_directory.display()
        )
        .into());
    }
    fs::create_dir(&temporary_directory)?;
    let state = date_state(&at, folder_number);
    write_day_marker(&temporary_directory, &state)?;
    fs::rename(&temporary_directory, &directory)?;
    write_root_state(&state_path, &state)?;
    Ok(directory)
}

fn write_day_marker(directory: &Path, state: &RecorderState) -> AppResult<()> {
    fs::write(directory.join(".day_state"), toml::to_string(state)?)?;
    Ok(())
}

fn write_root_state(path: &Path, state: &RecorderState) -> AppResult<()> {
    let temporary = path.with_extension("state.tmp");
    fs::write(&temporary, toml::to_string(state)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn delete_old_recordings(output_root: &Path, retention_seconds: u64) -> AppResult<()> {
    let age_limit = Duration::from_secs(retention_seconds);
    let now = SystemTime::now();
    for e in fs::read_dir(output_root)? {
        let day = e?;
        if !day.file_type()?.is_dir() {
            continue;
        }
        let Some(number) = day.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let marker = day.path().join(".day_state");
        if !fs::read_to_string(marker)
            .ok()
            .and_then(|s| toml::from_str::<RecorderState>(&s).ok())
            .is_some_and(|s| s.folder_number == number)
        {
            continue;
        }
        delete_old_day(&day.path(), age_limit, now)?;
    }
    Ok(())
}

fn delete_old_day(directory: &Path, age_limit: Duration, now: SystemTime) -> AppResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let is_recording = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| {
                extension.eq_ignore_ascii_case("wav") || extension.eq_ignore_ascii_case("mp3")
            })
            .unwrap_or(false);
        if !is_recording {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if now.duration_since(modified).unwrap_or_default() > age_limit {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
