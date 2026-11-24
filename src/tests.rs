use super::*;
use chrono::TimeZone;
use std::sync::atomic::AtomicUsize;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "qa-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn old_settings_default_to_english_and_language_roundtrips() {
    let dir = Scratch::new();
    let path = dir.0.join("settings.toml");
    fs::write(&path, "segment_minutes = 15\nretention_hours = 7").unwrap();
    let mut s = load_settings(&path).unwrap();
    assert_eq!(s.language, Language::English);
    s.language = Language::Russian;
    save_settings(&path, &s).unwrap();
    let reloaded = load_settings(&path).unwrap();
    assert_eq!(reloaded.language, Language::Russian);
    assert_eq!(reloaded.segment_minutes, 15);
    assert_eq!(reloaded.retention_hours, 7);
}

#[test]
fn recording_waits_rotates_at_midnight_and_finalizes_on_stop() {
    let dir = Scratch::new();
    let settings = Settings {
        output_directory: dir.0.to_string_lossy().into_owned(),
        ..Settings::default()
    };
    let start = Local
        .with_ymd_and_hms(2026, 11, 23, 23, 50, 0)
        .single()
        .unwrap();
    let next = start + ChronoDuration::minutes(10);
    let (send, receiver) = mpsc::channel();
    let frames = 600 * 8000;
    let signal = (0..frames)
        .map(|n| if n % 80 < 40 { 1000 } else { -1000 })
        .collect::<Vec<i16>>();
    for (time, samples) in [
        (start - ChronoDuration::seconds(1), vec![9]),
        (start, signal.clone()),
        (next, vec![11, 22]),
    ] {
        send.send(Ok(AudioChunk {
            captured_at: time,
            samples,
        }))
        .unwrap();
    }
    let running = AtomicBool::new(true);
    let mut files = Vec::new();
    let mut waited = false;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    Capture {
        device_name: "Test signal".into(),
        spec,
        receiver,
    }
    .write_segments(&settings, 3600, &running, start, |status| match status {
        Status::Waiting { boundary, .. } => {
            assert_eq!(boundary, start);
            waited = true
        }
        Status::Recording {
            file,
            next: boundary,
            ..
        } => {
            files.push(file);
            if boundary > next {
                running.store(false, Ordering::SeqCst);
            }
        }
        _ => panic!("unexpected status"),
    })
    .unwrap();
    assert!(waited);
    assert_eq!(
        files,
        vec![dir.0.join("1/23_50_00.wav"), dir.0.join("2/00_00_00.wav")]
    );
    let mut wav = hound::WavReader::open(&files[0]).unwrap();
    assert_eq!(wav.duration(), frames as u32);
    assert!(wav.samples::<i16>().map(Result::unwrap).eq(signal));
    assert!(hound::WavReader::open(&files[1])
        .unwrap()
        .samples::<i16>()
        .map(Result::unwrap)
        .eq([11, 22]));
    assert_eq!(ensure_day_folder(&dir.0, next).unwrap(), dir.0.join("2"));
}

#[test]
fn cleanup_only_expires_recordings_in_marked_day_folders() {
    let dir = Scratch::new();
    let day = ensure_day_folder(&dir.0, Local::now()).unwrap();
    let old = day.join("00_00_00.wav");
    let recent = day.join("00_10_00.wav");
    let other = dir.0.join("other.wav");
    for p in [&old, &other] {
        File::create(p)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1000000000))
            .unwrap();
    }
    File::create(&recent).unwrap();
    delete_old_recordings(&dir.0, 3600).unwrap();
    assert!(!old.exists());
    assert!(other.exists() && recent.exists());
}

#[test]
fn validates_windows_filenames_and_exact_boundaries() {
    let start = Local
        .with_ymd_and_hms(2026, 11, 24, 12, 0, 0)
        .single()
        .unwrap();
    assert_eq!(first_boundary_at_or_after(start, 10), start);
    assert_eq!(
        first_boundary_at_or_after(start + ChronoDuration::milliseconds(500), 10),
        start + ChronoDuration::minutes(10)
    );
    for bad in ["%Q", "%H:%M", "../radio", "CON", "aux.test", "...", ""] {
        assert!(filename_stem(bad, start).is_err(), "{bad}");
    }
    assert_eq!(filename_stem("%H_%M_%S", start).unwrap(), "12_00_00");
}
