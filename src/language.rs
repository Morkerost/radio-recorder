use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Default, PartialEq, Debug, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ru")]
    Russian,
}

// The console and the audio worker share one application language.
static RUSSIAN: AtomicBool = AtomicBool::new(false);
impl Language {
    pub fn activate(self) {
        RUSSIAN.store(self == Self::Russian, Ordering::Relaxed);
    }
}
pub fn russian() -> bool {
    RUSSIAN.load(Ordering::Relaxed)
}
pub fn tr<'a>(en: &'a str, ru: &'a str) -> &'a str {
    if russian() {
        ru
    } else {
        en
    }
}

macro_rules! localized {
    ($en:literal, $ru:literal $(,$arg:expr)* $(,)?) => {
        if crate::language::russian() { format!($ru $(,$arg)*) } else { format!($en $(,$arg)*) }
    };
}
