//! Language and presentation of the public CLI; wire records remain stable.
use crate::util::{AppError, Result};
use std::cell::Cell;
#[derive(Clone, Copy, Default)]
pub(crate) enum Language {
    #[default]
    En,
    Ru,
    Zh,
}
thread_local! {
    static LANGUAGE: Cell<Language> = const { Cell::new(Language::En) };
    static PRETTY: Cell<bool> = const { Cell::new(false) };
}
pub(crate) fn language() -> Language {
    LANGUAGE.with(Cell::get)
}
pub(crate) fn pretty() -> bool {
    PRETTY.with(Cell::get)
}
pub(crate) struct PresentationGuard(Language, bool);
pub(crate) fn install(language: Language, pretty: bool) -> PresentationGuard {
    PresentationGuard(
        LANGUAGE.with(|c| c.replace(language)),
        PRETTY.with(|c| c.replace(pretty)),
    )
}
impl Drop for PresentationGuard {
    fn drop(&mut self) {
        LANGUAGE.with(|c| c.set(self.0));
        PRETTY.with(|c| c.set(self.1));
    }
}
pub(crate) fn tr(en: &'static str, ru: &'static str, zh: &'static str) -> &'static str {
    match language() {
        Language::En => en,
        Language::Ru => ru,
        Language::Zh => zh,
    }
}
pub(crate) fn parse(args: &[String]) -> Result<Vec<String>> {
    LANGUAGE.with(|c| c.set(Language::En));
    PRETTY.with(|c| c.set(false));
    let mut rest = Vec::new();
    let mut pretty = false;
    let mut lang = None;
    let mut literal = false;
    for arg in args {
        if literal {
            rest.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => literal = true,
            "-pretty" | "--pretty" => pretty = true,
            "-en" | "-ru" | "-zh" => {
                if lang.is_some() {
                    return Err(AppError::new(
                        "Choose only one language flag: -en, -ru or -zh",
                    ));
                }
                lang = Some(match arg.as_str() {
                    "-ru" => Language::Ru,
                    "-zh" => Language::Zh,
                    _ => Language::En,
                });
                LANGUAGE.with(|c| c.set(lang.unwrap()));
            }
            _ => rest.push(arg.clone()),
        }
    }
    LANGUAGE.with(|c| c.set(lang.unwrap_or_default()));
    PRETTY.with(|c| c.set(pretty));
    if lang.is_some() && !pretty {
        return Err(AppError::new(tr(
            "Language flags require -pretty",
            "Для выбора языка нужен -pretty",
            "语言选项需要 -pretty",
        )));
    }
    Ok(rest)
}
pub(crate) fn error(message: &str) -> String {
    // Keep original technical diagnostics intact, visibly separate from UI text.
    match message {
        "Choose only one language flag: -en, -ru or -zh" => tr(
            "Choose only one language flag: -en, -ru or -zh",
            "Выберите только один язык: -en, -ru или -zh",
            "只能选择一种语言：-en、-ru 或 -zh",
        )
        .into(),
        "some configured provider checks failed or could not run" => tr(
            "Some provider checks failed or could not run",
            "Некоторые проверки завершились ошибкой или не были выполнены",
            "部分供应商检查失败或未能运行",
        )
        .into(),
        "CM is not initialized here. Run cm init in the project directory." => tr(
            "CM is not initialized here. Run cm init in the project directory.",
            "CM не инициализирован. Выполните cm init в каталоге проекта.",
            "CM 尚未初始化。请在项目目录中运行 cm init。",
        )
        .into(),
        _ => match language() {
            Language::En => message.into(),
            _ => format!(
                "{}: {message}",
                tr("Diagnostic", "Техническая диагностика", "技术诊断")
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }
    #[test]
    fn parser_resets_locale_and_localizes_conflicting_flags() {
        let e = parse(&args(&["help", "-pretty", "-ru", "-zh"])).unwrap_err();
        assert!(error(&e.msg).starts_with("Выберите"));
        parse(&args(&["help"])).unwrap();
        assert_eq!(tr("English", "Русский", "中文"), "English");
        assert!(!pretty());
        let e = parse(&args(&["-zh", "-ru", "-pretty"])).unwrap_err();
        assert!(error(&e.msg).starts_with("只能"));
        parse(&[]).unwrap();
    }
}
