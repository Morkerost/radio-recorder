use super::*;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

mod screen;
use screen::Screen;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    Devices,
    Settings,
    Language,
    Reset,
    AudioFormat,
    Bitrate,
}
#[derive(Clone, Copy)]
enum Action {
    ChooseAudioFormat,
    ChooseBitrate,
    SaveEncoding(AudioFormat, u16),
    Languages,
    SetLanguage(Language),
    AskReset,
    RestoreDefaults,
    Toggle,
    Devices,
    Settings,
    Folder,
    AskExit,
    CancelExit,
    Exit,
    Edit(Field),
    Back,
    Refresh,
    Default,
    Pick(usize),
    Previous,
    Next,
}
#[derive(Clone, Copy)]
enum Field {
    Minutes,
    Hours,
    Directory,
    Format,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Minutes => tr("File length (min)", "Длина файла (мин)"),
            Self::Hours => tr("Keep recordings (hours)", "Срок хранения (ч)"),
            Self::Directory => tr("Recordings folder", "Папка записей"),
            Self::Format => tr("Filename pattern", "Шаблон имени файла"),
        }
    }
    fn value(self, s: &Settings) -> String {
        match self {
            Self::Minutes => s.segment_minutes.to_string(),
            Self::Hours => s.retention_hours.to_string(),
            Self::Directory => s.output_directory.clone(),
            Self::Format => s.filename_format.clone(),
        }
    }
    fn set(self, s: &mut Settings, value: &str) -> AppResult<()> {
        let value = value.trim();
        match self {
            Self::Minutes => {
                s.segment_minutes = value.parse().map_err(|_| {
                    tr(
                        "Enter a whole number of minutes",
                        "Введите целое число минут",
                    )
                })?;
                validate_minutes(s.segment_minutes)?;
            }
            Self::Hours => {
                s.retention_hours = value.parse().map_err(|_| {
                    tr("Enter a whole number of hours", "Введите целое число часов")
                })?;
                validate_hours(s.retention_hours)?;
            }
            Self::Directory => {
                if value.is_empty() {
                    return Err(tr("Choose a recordings folder", "Укажите папку записей").into());
                }
                let path = std::path::absolute(value)?;
                if path.exists() && !path.is_dir() {
                    return Err(
                        tr("This path points to a file", "По этому пути находится файл").into(),
                    );
                }
                s.output_directory = path.to_string_lossy().into_owned();
            }
            Self::Format => {
                filename_stem(value, Local::now())?;
                s.filename_format = value.to_owned();
            }
        }
        Ok(())
    }
}

struct Session {
    running: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    worker: Option<JoinHandle<Result<(), String>>>,
    folder: PathBuf,
}
impl Session {
    fn start(settings: Settings, age: u64) -> AppResult<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(Status::Starting));
        let flag = running.clone();
        let state = status.clone();
        let folder = PathBuf::from(&settings.output_directory);
        let worker = thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                record_once(&cpal::default_host(), &settings, age, &flag, |s| {
                    *state.lock().unwrap() = s
                })
                .map_err(|e| e.to_string())
            })?;
        Ok(Self {
            running,
            status,
            worker: Some(worker),
            folder,
        })
    }
    fn stop(&mut self) -> AppResult<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.worker.take() {
            thread.join().map_err(|_| {
                tr(
                    "The recording thread stopped unexpectedly",
                    "Поток записи аварийно завершился",
                )
            })??;
        }
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Choice {
    key: KeyCode,
    label: String,
    action: Action,
}
impl Choice {
    fn new(key: KeyCode, label: impl Into<String>, action: Action) -> Self {
        Self {
            key,
            label: label.into(),
            action,
        }
    }
    fn line(&self) -> String {
        let key = match self.key {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Left => "←".into(),
            KeyCode::Right => "→".into(),
            _ => unreachable!(),
        };
        format!("[{key}]  {}", self.label)
    }
}

const PAGE: usize = 9;

#[cfg(test)]
#[test]
fn keys_select_the_displayed_device_on_each_page() {
    let mut menu = Menu {
        view: View::Devices,
        parent: View::Home,
        page: 0,
        devices: (1..=20).map(|n| (format!("Input #{n}."), false)).collect(),
        device_error: None,
        message: String::new(),
        session: None,
        editor: None,
        confirming_exit: false,
    };
    let s = Settings::default();
    for page in 0..3 {
        menu.page = page;
        let choices = menu.choices(&s);
        let mut picked = 0;
        for c in &choices {
            assert_eq!(choices.iter().filter(|x| x.key == c.key).count(), 1);
            if let Action::Pick(i) = c.action {
                assert_eq!(i, page * PAGE + picked);
                assert_eq!(c.key, KeyCode::Char(char::from(b'1' + picked as u8)));
                assert!(c.line().contains(&menu.devices[i].0));
                picked += 1;
            }
        }
        assert_eq!(picked, (20 - page * PAGE).min(PAGE));
    }
}
struct Menu {
    view: View,
    parent: View,
    page: usize,
    devices: Vec<(String, bool)>,
    device_error: Option<String>,
    message: String,
    session: Option<Session>,
    editor: Option<Editor>,
    confirming_exit: bool,
}

struct Editor {
    field: Field,
    value: String,
    cursor: usize,
    replace: bool,
}
impl Editor {
    fn new(field: Field, settings: &Settings) -> Self {
        let value = field.value(settings);
        Self {
            field,
            cursor: value.len(),
            value,
            replace: true,
        }
    }
    fn type_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(c) if !c.is_control() => {
                if self.replace {
                    self.value.clear();
                    self.cursor = 0;
                }
                self.value.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace | KeyCode::Delete if self.replace => {
                self.value.clear();
                self.cursor = 0;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let i = self.value[..self.cursor]
                    .char_indices()
                    .next_back()
                    .unwrap()
                    .0;
                self.value.drain(i..self.cursor);
                self.cursor = i;
            }
            KeyCode::Delete if self.cursor < self.value.len() => {
                self.value.remove(self.cursor);
            }
            KeyCode::Left => {
                self.cursor = self.value[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i)
            }
            KeyCode::Right => {
                self.cursor += self.value[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            _ => return,
        }
        self.replace = false;
    }
}

impl Menu {
    fn refresh(&mut self, host: &cpal::Host) {
        match input_devices(host) {
            Ok(devices) => {
                self.devices = devices;
                self.device_error = None;
            }
            Err(e) => {
                self.devices.clear();
                self.device_error = Some(e.to_string());
            }
        }
        self.page = self.page.min(self.devices.len().saturating_sub(1) / PAGE);
    }
    fn choices(&self, s: &Settings) -> Vec<Choice> {
        use Action::*;
        use KeyCode::Char;
        let c = Choice::new;
        let mut items = Vec::new();
        let mut add = |key, label: &str, action| items.push(c(key, label.to_owned(), action));
        if self.confirming_exit {
            add(Char('1'), tr("Yes, exit", "Да, выйти"), Exit);
            add(Char('0'), tr("Cancel", "Отмена"), CancelExit);
            return items;
        }
        match self.view {
            View::Home => {
                add(
                    Char('1'),
                    if self.session.is_some() {
                        tr("Stop recording", "Остановить запись")
                    } else {
                        tr("Start recording", "Начать запись")
                    },
                    Toggle,
                );
                add(Char('2'), tr("Audio devices", "Аудиоустройства"), Devices);
                add(Char('3'), tr("Settings", "Настройки"), Settings);
                add(
                    Char('4'),
                    tr("Open recordings folder", "Открыть папку записей"),
                    Folder,
                );
                add(Char('5'), tr("Exit", "Выход"), AskExit);
            }
            View::Devices => {
                add(
                    Char('0'),
                    tr(
                        "Use the default Windows input",
                        "Использовать вход Windows по умолчанию",
                    ),
                    Default,
                );
                for (i, (name, default)) in self
                    .devices
                    .iter()
                    .enumerate()
                    .skip(self.page * PAGE)
                    .take(PAGE)
                {
                    add(
                        Char(char::from(b'1' + (i % PAGE) as u8)),
                        &format!(
                            "{name}{}",
                            if *default {
                                tr(" (default)", " (по умолчанию)")
                            } else {
                                ""
                            }
                        ),
                        Pick(i),
                    );
                }
                if self.page > 0 {
                    add(
                        KeyCode::Left,
                        tr("Previous devices", "Предыдущие устройства"),
                        Previous,
                    );
                }
                if (self.page + 1) * PAGE < self.devices.len() {
                    add(
                        KeyCode::Right,
                        tr("Next devices", "Следующие устройства"),
                        Next,
                    );
                }
                add(Char('r'), tr("Refresh list", "Обновить список"), Refresh);
                add(KeyCode::Esc, tr("Back", "Назад"), Back);
            }
            View::Settings => {
                add(
                    Char('1'),
                    &localized!("Audio device · {}", "Аудиоустройство · {}", source(s)),
                    Devices,
                );
                for (key, f) in [
                    ('2', Field::Minutes),
                    ('3', Field::Hours),
                    ('4', Field::Directory),
                    ('5', Field::Format),
                ] {
                    add(
                        Char(key),
                        &format!("{} · {}", f.label(), f.value(s)),
                        Edit(f),
                    );
                }
                add(Char('6'), "Language / Язык", Languages);
                add(
                    Char('7'),
                    tr("Restore default settings", "Сбросить настройки"),
                    AskReset,
                );
                add(
                    Char('8'),
                    &localized!(
                        "Recording format · {}",
                        "Формат записи · {}",
                        s.audio_format.extension().to_ascii_uppercase()
                    ),
                    ChooseAudioFormat,
                );
                add(
                    Char('9'),
                    &localized!(
                        "MP3 bitrate · {} kbps",
                        "Битрейт MP3 · {} кбит/с",
                        s.mp3_bitrate_kbps
                    ),
                    ChooseBitrate,
                );
                add(Char('0'), tr("Back", "Назад"), Back);
            }
            View::AudioFormat => {
                add(
                    Char('1'),
                    "WAV (PCM 16-bit)",
                    SaveEncoding(AudioFormat::Wav, s.mp3_bitrate_kbps),
                );
                add(
                    Char('2'),
                    "MP3 (LAME)",
                    SaveEncoding(AudioFormat::Mp3, s.mp3_bitrate_kbps),
                );
                add(Char('0'), tr("Back", "Назад"), Back);
            }
            View::Bitrate => {
                for (key, rate) in ('1'..='9').zip(encoding::BITRATES.iter().copied()) {
                    let kbps = rate as u16;
                    add(
                        Char(key),
                        &localized!("{kbps} kbps", "{kbps} кбит/с"),
                        SaveEncoding(s.audio_format, kbps),
                    );
                }
                add(Char('0'), tr("Back", "Назад"), Back);
            }
            View::Language => {
                add(Char('1'), "English", SetLanguage(Language::English));
                add(Char('2'), "Русский", SetLanguage(Language::Russian));
                add(Char('0'), tr("Back", "Назад"), Back);
            }
            View::Reset => {
                add(
                    Char('1'),
                    tr("Restore defaults", "Сбросить"),
                    RestoreDefaults,
                );
                add(Char('0'), tr("Cancel", "Отмена"), Back);
            }
        }
        items
    }

    fn lines(
        &self,
        s: &Settings,
        config: &Path,
        choices: &[Choice],
        age_override: Option<u64>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let status = self
            .session
            .as_ref()
            .map(|session| session.status.lock().unwrap().clone());
        let detail = self.view == View::Home && self.editor.is_none() && !self.confirming_exit;
        match status {
            None => lines.push(tr("Recording is off", "Запись выключена").into()),
            Some(Status::Starting) => {
                lines.push(tr("Connecting to audio input…", "Подключение к аудиовходу…").into())
            }
            Some(Status::Waiting { device, boundary }) => {
                lines.push(localized!(
                    "Waiting • starting at {}",
                    "Ожидание • начало в {}",
                    boundary.format("%H:%M:%S")
                ));
                if detail {
                    lines.push(localized!("Source · {device}", "Источник · {device}"));
                }
            }
            Some(Status::Recording {
                device,
                file,
                next,
                peaks,
            }) => {
                lines.push(localized!(
                    "● Recording • next file at {}",
                    "● Идёт запись • следующий файл в {}",
                    next.format("%H:%M:%S")
                ));
                if detail {
                    lines.push(localized!("Source · {device}", "Источник · {device}"));
                    lines.push(localized!("File · {}", "Файл · {}", file.display()));
                    for (i, p) in peaks.into_iter().enumerate() {
                        let db = 20.0 * p.max(0.00003).log10();
                        let n = ((db + 60.).clamp(0., 60.) / 3.) as usize;
                        lines.push(localized!(
                            "Channel {}  {}{}  {:.1} dB",
                            "Канал {}  {}{}  {:.1} dB",
                            i + 1,
                            "█".repeat(n),
                            "·".repeat(20 - n),
                            db
                        ));
                    }
                }
            }
        }
        if self.confirming_exit {
            lines.push(
                if self.session.is_some() {
                    tr(
                        "Stop recording and exit Radio Recorder?",
                        "Остановить запись и выйти из Radio Recorder?",
                    )
                } else {
                    tr("Exit Radio Recorder?", "Выйти из Radio Recorder?")
                }
                .into(),
            );
            lines.push(String::new());
            lines.extend(choices.iter().map(Choice::line));
        } else if let Some(editor) = &self.editor {
            let field = editor.field;
            lines.push(field.label().into());
            if editor.replace {
                lines.push(format!("› {}", editor.value));
                lines.push(
                    tr(
                        "Type to replace the value or use the arrows to edit",
                        "Введите новое значение или передвиньте курсор стрелкой",
                    )
                    .into(),
                );
            } else {
                lines.push(format!(
                    "› {}▏{}",
                    &editor.value[..editor.cursor],
                    &editor.value[editor.cursor..]
                ));
            }
            lines.push(
                tr(
                    "[Enter] Save    [Esc] Cancel",
                    "[Enter] Сохранить    [Esc] Отмена",
                )
                .into(),
            );
        } else {
            match self.view {
                View::Home => {
                    if self.session.is_none() {
                        lines.push(localized!("Source · {}", "Источник · {}", source(s)));
                        if self.devices.is_empty() {
                            lines.push(
                                tr(
                                    "No inputs available. Press [2] for audio devices",
                                    "Входы недоступны. Откройте [2] Аудиоустройства",
                                )
                                .into(),
                            );
                        }
                    }
                }
                View::Devices => {
                    lines.push(localized!("Selected · {}", "Выбран · {}", source(s)));
                    if let Some(e) = &self.device_error {
                        lines.push(localized!(
                            "Could not list audio devices · {e}",
                            "Ошибка списка устройств · {e}"
                        ));
                    } else if self.devices.is_empty() {
                        lines.push(
                            tr(
                                "Windows found no audio inputs. Connect one and press R",
                                "Windows не обнаружила аудиовход. Подключите его и нажмите R",
                            )
                            .into(),
                        );
                    } else if self.devices.len() > PAGE {
                        lines.push(localized!(
                            "Page {} of {}",
                            "Страница {} из {}",
                            self.page + 1,
                            self.devices.len().div_ceil(PAGE)
                        ));
                    }
                }
                View::Settings => {
                    lines.push(localized!(
                        "Settings file · {}",
                        "Файл настроек · {}",
                        config.display()
                    ));
                    if let Some(age) = age_override {
                        lines.push(localized!(
                            "The --max-age override applies until exit ({age} s)",
                            "Параметр --max-age действует до выхода ({age} с)"
                        ));
                    }
                }
                View::Language => {
                    lines.push(tr("Current language · English", "Текущий язык · Русский").into())
                }
                View::AudioFormat => lines.push(localized!(
                    "Selected · {}",
                    "Выбран · {}",
                    s.audio_format.extension().to_ascii_uppercase()
                )),
                View::Bitrate => lines.push(localized!(
                    "Selected · {} kbps",
                    "Выбран · {} кбит/с",
                    s.mp3_bitrate_kbps
                )),
                View::Reset => {
                    lines.push(tr(
                        "Restore all settings, including the language, audio input and recordings folder?",
                        "Сбросить все настройки, включая язык, аудиовход и папку записей?",
                    ).into());
                    if self.session.is_some() {
                        lines.push(
                            tr(
                                "The current recording will continue with its existing settings.",
                                "Текущая запись продолжится с прежними настройками.",
                            )
                            .into(),
                        );
                    }
                }
            }
            if matches!(
                self.view,
                View::Devices | View::Settings | View::AudioFormat | View::Bitrate
            ) && self.session.is_some()
            {
                lines.push(
                    tr(
                        "Recording settings apply when you next start recording",
                        "Изменения вступят в силу при следующем старте записи",
                    )
                    .into(),
                );
            }
            lines.push(String::new());
            lines.extend(choices.iter().map(Choice::line));
        }
        if !self.message.is_empty() {
            lines.push(String::new());
            lines.push(self.message.clone());
        }
        lines
    }
    fn apply(
        &mut self,
        action: Action,
        host: &cpal::Host,
        s: &mut Settings,
        config: &Path,
        age: Option<u64>,
    ) -> AppResult<bool> {
        self.message.clear();
        match action {
            Action::ChooseAudioFormat => self.view = View::AudioFormat,
            Action::ChooseBitrate => self.view = View::Bitrate,
            Action::SaveEncoding(format, kbps) => {
                let mut next = s.clone();
                next.audio_format = format;
                next.mp3_bitrate_kbps = kbps;
                save_settings(config, &next)?;
                *s = next;
                self.view = View::Settings;
                self.message = tr("Settings saved", "Настройки сохранены").into();
            }
            Action::AskReset => self.view = View::Reset,
            Action::RestoreDefaults => {
                let defaults = Settings::default();
                save_settings(config, &defaults)?;
                *s = defaults;
                s.language.activate();
                self.view = View::Settings;
                self.message = tr("Default settings restored", "Настройки сброшены").into();
            }
            Action::Languages => self.view = View::Language,
            Action::SetLanguage(language) => {
                let mut next = s.clone();
                next.language = language;
                save_settings(config, &next)?;
                *s = next;
                language.activate();
                self.view = View::Settings;
                self.message = tr("Language saved", "Язык сохранён").into();
            }
            Action::AskExit => self.confirming_exit = true,
            Action::CancelExit => self.confirming_exit = false,
            Action::Exit => {
                self.confirming_exit = false;
                if let Some(mut session) = self.session.take() {
                    session.stop()?;
                }
                return Ok(false);
            }
            Action::Toggle => {
                if let Some(mut session) = self.session.take() {
                    session.stop()?;
                    self.message = tr("Recording stopped", "Запись остановлена").into();
                } else {
                    self.session = Some(Session::start(
                        s.clone(),
                        age.unwrap_or_else(|| s.retention_hours.saturating_mul(3600)),
                    )?);
                }
            }
            Action::Folder => {
                let path = self
                    .session
                    .as_ref()
                    .map(|x| x.folder.as_path())
                    .unwrap_or(Path::new(&s.output_directory));
                open_folder(path)?;
                self.message = localized!("Opened folder {}", "Открыта папка {}", path.display());
            }
            Action::Devices => {
                self.parent = self.view;
                self.view = View::Devices;
                self.page = 0;
                self.refresh(host);
            }
            Action::Settings => self.view = View::Settings,
            Action::Back => {
                self.view = if matches!(
                    self.view,
                    View::Language | View::Reset | View::AudioFormat | View::Bitrate
                ) {
                    View::Settings
                } else if self.view == View::Devices {
                    self.parent
                } else {
                    View::Home
                }
            }
            Action::Previous => self.page = self.page.saturating_sub(1),
            Action::Next => self.page += 1,
            Action::Refresh => {
                self.refresh(host);
                self.message = tr("Device list refreshed", "Список обновлён").into();
            }
            Action::Default | Action::Pick(_) => {
                let mut candidate = s.clone();
                candidate.input_device = if let Action::Pick(i) = action {
                    self.devices[i].0.clone()
                } else {
                    String::new()
                };
                save_settings(config, &candidate)?;
                *s = candidate;
                self.view = self.parent;
                self.message = localized!("Selected input · {}", "Выбран вход · {}", source(s));
            }
            Action::Edit(field) => self.editor = Some(Editor::new(field, s)),
        }
        Ok(true)
    }
}

fn source(s: &Settings) -> &str {
    if s.input_device.is_empty() {
        tr("Windows default", "Windows по умолчанию")
    } else {
        &s.input_device
    }
}

pub(super) fn run(
    host: &cpal::Host,
    mut settings: Settings,
    config: &Path,
    age: Option<u64>,
    running: &AtomicBool,
) -> AppResult<()> {
    let mut screen = Screen::enter()?;
    let mut menu = Menu {
        view: View::Home,
        parent: View::Home,
        page: 0,
        devices: vec![],
        device_error: None,
        message: validate_settings(&settings)
            .err()
            .map(|e| localized!("Check settings · {e}", "Исправьте настройки · {e}"))
            .unwrap_or_default(),
        session: None,
        editor: None,
        confirming_exit: false,
    };
    menu.refresh(host);
    let mut dirty = true;
    loop {
        // In an interactive console Ctrl+C may arrive as an OS signal or a key event.
        if !running.swap(true, Ordering::SeqCst) {
            menu.confirming_exit = true;
            screen.top();
            dirty = true;
        }
        if menu
            .session
            .as_ref()
            .and_then(|s| s.worker.as_ref())
            .is_some_and(|w| w.is_finished())
        {
            if let Some(mut finished) = menu.session.take() {
                menu.message = finished.stop().err().map_or_else(
                    || tr("Recording stopped", "Запись остановлена").into(),
                    |e| localized!("Recording error · {e}", "Ошибка записи · {e}"),
                );
                dirty = true;
            }
        }
        let choices = menu.choices(&settings);
        if dirty || menu.session.is_some() {
            let title = if menu.confirming_exit {
                tr("Confirm exit", "Подтверждение выхода")
            } else {
                match menu.view {
                    View::Home => tr("Main menu", "Главное меню"),
                    View::Devices => tr("Audio devices", "Аудиоустройства"),
                    View::Settings => tr("Settings", "Настройки"),
                    View::Language => "Language / Язык",
                    View::Reset => tr("Restore default settings", "Сброс настроек"),
                    View::AudioFormat => tr("Recording format", "Формат записи"),
                    View::Bitrate => tr("MP3 bitrate", "Битрейт MP3"),
                }
            };
            screen.draw(title, &menu.lines(&settings, config, &choices, age))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let editing = menu.editor.is_some() && !menu.confirming_exit;
        let key = match event::read()? {
            Event::Resize(..) => {
                screen.resize();
                dirty = true;
                continue;
            }
            // Some terminals deliver ESC as a control character or only on key-up.
            // Consume cancellation here so it cannot also navigate the parent menu.
            Event::Key(key) if editing && is_escape(&key) => {
                menu.editor = None;
                menu.message = tr("Edit cancelled", "Изменения отменены").into();
                screen.top();
                dirty = true;
                continue;
            }
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    || (key.kind == KeyEventKind::Repeat && editing) =>
            {
                key
            }
            _ => continue,
        };
        dirty = true;
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
        {
            menu.confirming_exit = true;
            screen.top();
            continue;
        }
        if screen.scroll(key.code) {
            continue;
        }
        if let Some(ed) = menu.editor.as_mut().filter(|_| editing) {
            match key.code {
                KeyCode::Enter => {
                    let mut candidate = settings.clone();
                    match ed
                        .field
                        .set(&mut candidate, &ed.value)
                        .and_then(|_| save_settings(config, &candidate))
                    {
                        Ok(()) => {
                            settings = candidate;
                            menu.editor = None;
                            menu.message = tr("Settings saved", "Настройки сохранены").into();
                        }
                        Err(e) => {
                            menu.message = e.to_string();
                            ed.replace = true;
                        }
                    }
                }
                key => {
                    ed.type_key(key);
                    menu.message.clear();
                }
            }
            continue;
        }
        let code = shortcut(key);
        let action = choices
            .iter()
            .find(|c| c.key == code)
            .map(|c| c.action)
            .or_else(|| match (menu.view, code) {
                _ if menu.confirming_exit => None,
                (View::Home, KeyCode::Char('q' | 's')) if menu.session.is_some() => {
                    Some(Action::Toggle)
                }
                (View::Home, KeyCode::Char('q')) => Some(Action::AskExit),
                (_, KeyCode::Char('b' | 'q')) => Some(Action::Back),
                _ => None,
            });
        if let Some(action) = action {
            screen.top();
            match menu.apply(action, host, &mut settings, config, age) {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(e) => menu.message = e.to_string(),
            }
        }
    }
}

fn is_escape(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('\u{1b}'))
        || (key.code == KeyCode::Char('[') && key.modifiers == KeyModifiers::CONTROL)
}

fn shortcut(key: KeyEvent) -> KeyCode {
    if is_escape(&key) {
        return KeyCode::Esc;
    }
    match key.code {
        KeyCode::Char(c) => KeyCode::Char(match c.to_ascii_lowercase() {
            'к' | 'К' => 'r',
            'и' | 'И' => 'b',
            'й' | 'Й' => 'q',
            'ы' | 'Ы' => 's',
            c => c,
        }),
        code => code,
    }
}
