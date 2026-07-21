use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Locale {
    En,
    Zh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiLanguage {
    Auto,
    En,
    Zh,
}

impl UiLanguage {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "en" => Some(Self::En),
            "zh" => Some(Self::Zh),
            _ => None,
        }
    }

    fn locale(self) -> Option<Locale> {
        match self {
            Self::Auto => None,
            Self::En => Some(Locale::En),
            Self::Zh => Some(Locale::Zh),
        }
    }
}

impl Locale {
    pub fn detect() -> Self {
        let mut env = |key: &str| std::env::var(key).ok();
        Self::system_with(&mut env)
    }

    fn from_env_value(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty() || value == "auto" || value == "c" || value == "posix" {
            return None;
        }
        if value.starts_with("zh") {
            Some(Self::Zh)
        } else {
            Some(Self::En)
        }
    }

    fn resolve_with<F>(configured: &str, mut env: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        if let Some(value) = env("MIYU_LANG") {
            if let Some(language) = UiLanguage::parse(&value) {
                return language
                    .locale()
                    .unwrap_or_else(|| Self::system_with(&mut env));
            }
            if let Some(locale) = Self::from_env_value(&value) {
                return locale;
            }
        }
        if let Some(locale) = UiLanguage::parse(configured).and_then(UiLanguage::locale) {
            return locale;
        }
        Self::system_with(&mut env)
    }

    fn system_with<F>(env: &mut F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(locale) = env(key).and_then(|value| Self::from_env_value(&value)) {
                return locale;
            }
        }
        #[cfg(windows)]
        if let Some(locale) = windows_locale().and_then(|value| Self::from_env_value(&value)) {
            return locale;
        }
        Self::En
    }
}

#[cfg(windows)]
fn windows_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

    const LOCALE_NAME_MAX_LENGTH: i32 = 85;
    let mut buffer = [0_u16; LOCALE_NAME_MAX_LENGTH as usize];
    let length = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), LOCALE_NAME_MAX_LENGTH) };
    if length <= 1 {
        return None;
    }
    String::from_utf16(&buffer[..length as usize - 1]).ok()
}

static UI_LOCALE: OnceLock<Locale> = OnceLock::new();

pub fn init(configured: &str) -> Locale {
    *UI_LOCALE.get_or_init(|| Locale::resolve_with(configured, |key| std::env::var(key).ok()))
}

pub fn locale() -> Locale {
    UI_LOCALE
        .get()
        .copied()
        .unwrap_or_else(|| Locale::resolve_with("auto", |key| std::env::var(key).ok()))
}

pub fn is_zh() -> bool {
    locale() == Locale::Zh
}

pub fn text(en: &'static str, zh: &'static str) -> &'static str {
    if is_zh() {
        zh
    } else {
        en
    }
}

pub fn agent_locale() -> Locale {
    Locale::detect()
}

pub fn agent_is_zh() -> bool {
    agent_locale() == Locale::Zh
}

pub fn agent_text(en: &'static str, zh: &'static str) -> &'static str {
    if agent_is_zh() {
        zh
    } else {
        en
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_chinese_locale_values() {
        assert_eq!(Locale::from_env_value("zh_CN.UTF-8"), Some(Locale::Zh));
        assert_eq!(Locale::from_env_value("zh_TW"), Some(Locale::Zh));
    }

    #[test]
    fn detects_english_locale_values() {
        assert_eq!(Locale::from_env_value("en_US.UTF-8"), Some(Locale::En));
        assert_eq!(Locale::from_env_value("ja_JP.UTF-8"), Some(Locale::En));
        assert_eq!(Locale::from_env_value("C"), None);
    }

    #[test]
    fn parses_ui_language_values() {
        assert_eq!(UiLanguage::parse("auto"), Some(UiLanguage::Auto));
        assert_eq!(UiLanguage::parse("en"), Some(UiLanguage::En));
        assert_eq!(UiLanguage::parse("zh"), Some(UiLanguage::Zh));
        assert_eq!(UiLanguage::parse(""), None);
        assert_eq!(UiLanguage::parse("fr"), None);
    }

    #[test]
    fn resolves_environment_then_config_then_system_locale() {
        let locale = Locale::resolve_with("zh", |key| match key {
            "MIYU_LANG" => Some("en_US.UTF-8".to_string()),
            "LANG" => Some("zh_CN.UTF-8".to_string()),
            _ => None,
        });
        assert_eq!(locale, Locale::En);

        let locale = Locale::resolve_with("zh", |key| match key {
            "LANG" => Some("en_US.UTF-8".to_string()),
            _ => None,
        });
        assert_eq!(locale, Locale::Zh);

        let locale = Locale::resolve_with("auto", |key| match key {
            "LANG" => Some("zh_CN.UTF-8".to_string()),
            _ => None,
        });
        assert_eq!(locale, Locale::Zh);

        let locale = Locale::resolve_with("zh", |key| match key {
            "MIYU_LANG" => Some("auto".to_string()),
            "LANG" => Some("en_US.UTF-8".to_string()),
            _ => None,
        });
        assert_eq!(locale, Locale::En);
    }

    #[test]
    fn system_locale_detection_ignores_ui_override() {
        let mut env = |key: &str| match key {
            "MIYU_LANG" => Some("en".to_string()),
            "LANG" => Some("zh_CN.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(Locale::system_with(&mut env), Locale::Zh);
    }
}
