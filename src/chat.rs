//! Public chat entry point backed exclusively by unified thread retrieval.
use crate::{
    cli::Parsed,
    project::Project,
    util::{atomic_write, AppError, Result},
};
use std::{
    fs,
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
};

pub(crate) const INSTRUCTIONS: &str = include_str!("chat_instructions.md");
pub(crate) fn project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    for root in cwd.ancestors() {
        if crate::project::is_initialized_root(root) {
            return Ok(root.into());
        }
    }
    let exe = std::env::current_exe()?;
    if let Some(root) = exe
        .parent()
        .filter(|p| crate::project::is_initialized_root(p))
    {
        return Ok(root.into());
    }
    Err(AppError::new(
        "CM is not initialized here. Run cm init in the project directory.",
    ))
}
fn help() -> Result<()> {
    let output = format!( "CM {} — {}\n\ncm\ncm \"<message>\"\ncm init\ncm help\ncm ingest-session\ncm hooks install codex\ncm hooks uninstall codex\ncm hooks status\ncm hooks drain\ncm -test_providers\ncm feedback \"<text>\"\n\n{}\n{}\n{}\n{}", crate::build_info::BINARY_VERSION,
        crate::ui::tr("Read-only memory chat", "Чат памяти: чтение", "只读记忆聊天"),
        crate::ui::tr("Presentation: -pretty (alias --pretty). Language: -en (default), -ru, -zh; language flags require -pretty.", "Отображение: -pretty (или --pretty). Язык: -en (по умолчанию), -ru, -zh; для языка нужен -pretty.", "显示：-pretty（别名 --pretty）。语言：-en（默认）、-ru、-zh；语言选项需要 -pretty。"),
        crate::ui::tr("init initializes memory; ingest-session imports new Codex events; -test_providers tests configured model profiles.", "init создаёт память; ingest-session импортирует новые события Codex; -test_providers проверяет профили моделей.", "init 初始化记忆；ingest-session 导入新的 Codex 事件；-test_providers 测试模型配置。"),
        crate::ui::tr("Settings: memory/config.json. Statistics: memory.statistics.enabled. Continue a returned topic using cm \"@context:ID <follow-up>\". Source documents and source threads remain read-only.", "Настройки: memory/config.json. Статистика: memory.statistics.enabled. Продолжайте тему через cm \"@context:ID <уточнение>\". Исходные документы и нити доступны только для чтения.", "设置：memory/config.json。统计：memory.statistics.enabled。使用 cm \"@context:ID <追问>\" 继续主题。源文档和源线程保持只读。"),
        crate::ui::tr("ingest-session uses CODEX_THREAD_ID (or CODEX_SESSION_ID) and CODEX_HOME. JSON keys and raw technical diagnostics are not translated.", "ingest-session использует CODEX_THREAD_ID (или CODEX_SESSION_ID) и CODEX_HOME. Ключи JSON и исходная техническая диагностика не переводятся.", "ingest-session 使用 CODEX_THREAD_ID（或 CODEX_SESSION_ID）及 CODEX_HOME。JSON 键名和原始技术诊断保持不变。"));
    crate::statistics::output(&output);
    writeln!(io::stdout().lock(), "{output}")?;
    Ok(())
}
fn initialize() -> Result<()> {
    if std::env::var_os("CM_CHAT_INTERNAL").is_some() {
        return Err(AppError::new("CM workers cannot initialize memory"));
    }
    let root = std::env::current_dir()?;
    crate::mcp::preflight(&root)?;
    let p = words(vec!["init".into(), root.to_string_lossy().into()]);
    crate::output::capture(|| crate::memory_app::init(&p))?;
    let path = root.join("AGENTS.md");
    let text = fs::read_to_string(&path)?;
    let begin = "<!-- >>> climemory model instructions >>> -->";
    let end = "<!-- <<< climemory model instructions <<< -->";
    let start = text
        .find(begin)
        .ok_or_else(|| AppError::new("missing managed instruction block"))?;
    let finish = start
        + text[start..]
            .find(end)
            .ok_or_else(|| AppError::new("unclosed managed instruction block"))?
        + end.len();
    atomic_write(
        &path,
        format!(
            "{}{begin}\n{INSTRUCTIONS}{end}{}",
            &text[..start],
            &text[finish..]
        )
        .as_bytes(),
    )?;
    crate::mcp::configure(&root)?;
    println!(
        "{}",
        crate::ui::tr(
            "CM initialized. Codex MCP and AGENTS.md configured for this project. Open/restart Codex here; trust project settings if prompted. Model/provider access still requires configured credentials. CLI: cm <message>.",
            "CM инициализирован. MCP Codex и AGENTS.md настроены для проекта. Откройте/перезапустите Codex здесь; при запросе подтвердите доверие проекту. Для моделей нужны настроенные учётные данные. CLI: cm <сообщение>.",
            "CM 已初始化。已为项目配置 Codex MCP 和 AGENTS.md。请在此打开或重启 Codex，并按提示信任项目设置。模型访问仍需配置凭据。CLI：cm <消息>。"
        )
    );
    Ok(())
}
pub(crate) fn run(args: &[String]) -> Result<()> {
    if args.len() == 2 && args[0] == "feedback" {
        return crate::feedback::command(&args[1]);
    }
    if args.len() > 1 {
        return Err(AppError::new(
            "use cm \"<message>\", cm init, cm ingest-session or cm help; quote the entire message",
        ));
    }
    match args.first().map(String::as_str) {
        Some("help" | "--help" | "-h") => help(),
        Some("init") => initialize(),
        Some("ingest-session") => crate::session_ingest::run(),
        Some("-test_providers") => crate::provider_test::run(crate::ui::pretty()),
        Some(text) if text.starts_with('-') => Err(AppError::new(
            "CM accepts a quoted message, not CLI flags; use cm help",
        )),
        Some(text) => {
            let answer = chat(text)?;
            println!("{answer}");
            Ok(())
        }
        None => {
            let terminal = io::stdin().is_terminal();
            if terminal {
                println!(
                    "{}",
                    crate::ui::tr(
                        "CM memory chat. End input with EOF. Use @context:ID to continue a topic.",
                        "Чат памяти CM. EOF завершает ввод. Для продолжения темы используйте @context:ID.",
                        "CM 记忆聊天。输入 EOF 结束。使用 @context:ID 继续主题。"
                    )
                );
            }
            let stdin = io::stdin();
            let mut input = stdin.lock();
            loop {
                if terminal {
                    print!("> ");
                    io::stdout().flush()?;
                }
                let mut text = String::new();
                if input.read_line(&mut text)? == 0 {
                    break;
                }
                if text.trim().is_empty() {
                    continue;
                }
                let result = {
                    let _heartbeat = crate::heartbeat::Heartbeat::start();
                    if !crate::unified::is_details_request(text.trim()) {
                        crate::feedback::before_request();
                    }
                    chat(text.trim())
                };
                match result {
                    Ok(answer) => println!("{answer}"),
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
    }
}
fn words(positionals: Vec<String>) -> Parsed {
    let mut parsed = Parsed::default();
    parsed.positionals = positionals;
    parsed
}
fn chat(message: &str) -> Result<String> {
    crate::statistics::input(message);
    let result = chat_inner(message);
    match &result {
        Ok(answer) => crate::statistics::output(answer),
        Err(_) => crate::statistics::error(),
    }
    result
}
fn chat_inner(message: &str) -> Result<String> {
    if message.trim().is_empty() || message.chars().count() > 8000 || message.contains('\0') {
        return Err(AppError::new("message must contain 1..8000 characters"));
    }
    if std::env::var_os("CM_CHAT_INTERNAL").is_some()
        || std::env::var_os("CM_DOCS_INTERNAL").is_some()
        || std::env::var_os("CM_CONTEXT_INTERNAL").is_some()
    {
        return Err(AppError::new(
            "memory workers cannot recursively invoke the CM chat",
        ));
    }
    let project = Project::open(&project_root()?)?;
    crate::unified::chat(&project, message)
}
