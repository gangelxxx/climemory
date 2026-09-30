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
    let commands = [
        (
            "cm",
            crate::ui::tr(
                "Read questions interactively or from stdin; EOF exits",
                "Ввод вопросов интерактивно или через stdin; EOF завершает ввод",
                "交互式或从 stdin 读取问题；EOF 退出",
            ),
        ),
        (
            "cm \"<message>\"",
            crate::ui::tr(
                "Ask about project memory and exit; quote the entire message",
                "Задать вопрос по памяти проекта и выйти; всё сообщение в кавычках",
                "查询项目记忆后退出；请将完整消息放在引号中",
            ),
        ),
        (
            "cm \"@context:ID <follow-up>\"",
            crate::ui::tr(
                "Continue the returned context_session",
                "Продолжить тему по полученному context_session",
                "继续返回的 context_session 主题",
            ),
        ),
        (
            "cm \"@context:ID @details\"",
            crate::ui::tr(
                "Expand saved evidence and interpretations for the topic",
                "Показать сохранённые источники и интерпретации темы",
                "展开主题保存的证据和解释",
            ),
        ),
        (
            "cm init",
            crate::ui::tr(
                "Initialize memory, project MCP and instructions",
                "Инициализировать память, MCP и инструкции проекта",
                "初始化记忆、项目 MCP 和指令",
            ),
        ),
        (
            "cm help",
            crate::ui::tr(
                "Show this help; aliases: --help, -h",
                "Показать эту справку; также --help, -h",
                "显示此帮助；别名：--help、-h",
            ),
        ),
        (
            "cm --version",
            crate::ui::tr(
                "Print the executable version",
                "Показать версию программы",
                "显示程序版本",
            ),
        ),
        (
            "cm ingest-session",
            crate::ui::tr(
                "Import new events from the identified Codex session",
                "Импортировать новые события определённой сессии Codex",
                "导入所识别 Codex 会话的新事件",
            ),
        ),
        (
            "cm docs build [--dry-run]",
            crate::ui::tr(
                "Prepare document threads; --dry-run previews files without model calls",
                "Подготовить ветки документов; --dry-run покажет файлы без вызовов модели",
                "准备文档线程；--dry-run 预览文件，不调用模型",
            ),
        ),
        (
            "cm docs status",
            crate::ui::tr(
                "Compare documents with the last complete build",
                "Сравнить документы с последней полной сборкой",
                "将文档与上次完整构建比较",
            ),
        ),
        (
            "cm -test_providers",
            crate::ui::tr(
                "Test model profiles with real calls; may consume tokens",
                "Проверить профили моделей реальными вызовами; расходует токены",
                "通过真实调用测试模型配置；可能消耗令牌",
            ),
        ),
        (
            "cm feedback \"<text>\"",
            crate::ui::tr(
                "Save feedback without a model call",
                "Сохранить обратную связь без вызова модели",
                "保存反馈，不调用模型",
            ),
        ),
        (
            "cm hooks install codex",
            crate::ui::tr(
                "Install automatic history/context hooks",
                "Установить автоматические хуки истории и контекста",
                "安装自动历史和上下文钩子",
            ),
        ),
        (
            "cm hooks uninstall codex",
            crate::ui::tr(
                "Remove CM hooks, preserving unrelated hooks",
                "Удалить хуки CM, сохранив остальные",
                "移除 CM 钩子，保留其他钩子",
            ),
        ),
        (
            "cm hooks status",
            crate::ui::tr(
                "Show queued session count and hook runtime directory",
                "Показать число сессий в очереди и рабочий каталог хуков",
                "显示排队会话数和钩子运行目录",
            ),
        ),
        (
            "cm hooks drain",
            crate::ui::tr(
                "Process queued imports in the foreground",
                "Обработать очередь импорта в текущем процессе",
                "在前台处理导入队列",
            ),
        ),
        (
            "cm --mcp",
            crate::ui::tr(
                "Run the native stdio MCP server",
                "Запустить MCP-сервер через stdio",
                "运行原生 stdio MCP 服务器",
            ),
        ),
    ];
    let commands = commands
        .iter()
        .map(|(command, description)| format!("  {command}\n    {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = format!( "CM {} — {}\n\n{commands}\n\n{}\n{}\n{}\n{}\n{}\n{}\n\n{}\n  cm \"What requirements are unresolved?\" -pretty -en\n  cm help -pretty -ru", crate::build_info::BINARY_VERSION,
        crate::ui::tr("Read-only memory chat", "Чат памяти: чтение", "只读记忆聊天"),
        crate::ui::tr("Presentation: -pretty (alias --pretty). Language: -en (default), -ru, -zh; language flags require -pretty.", "Отображение: -pretty (или --pretty). Язык: -en (по умолчанию), -ru, -zh; для языка нужен -pretty.", "显示：-pretty（别名 --pretty）。语言：-en（默认）、-ru、-zh；语言选项需要 -pretty。"),
        crate::ui::tr("Do not add presentation flags to hooks, --mcp or --version.", "Не добавляйте флаги отображения к hooks, --mcp и --version.", "不要为 hooks、--mcp 或 --version 添加显示选项。"),
        crate::ui::tr("Settings: memory/config.json. Statistics: memory.statistics.enabled. Continue a returned topic using cm \"@context:ID <follow-up>\". Source documents and source threads remain read-only.", "Настройки: memory/config.json. Статистика: memory.statistics.enabled. Продолжайте тему через cm \"@context:ID <уточнение>\". Исходные документы и нити доступны только для чтения.", "设置：memory/config.json。统计：memory.statistics.enabled。使用 cm \"@context:ID <追问>\" 继续主题。源文档和源线程保持只读。"),
        crate::ui::tr("ingest-session uses CODEX_THREAD_ID (or CODEX_SESSION_ID) and CODEX_HOME. JSON keys and raw technical diagnostics are not translated.", "ingest-session использует CODEX_THREAD_ID (или CODEX_SESSION_ID) и CODEX_HOME. Ключи JSON и исходная техническая диагностика не переводятся.", "ingest-session 使用 CODEX_THREAD_ID（或 CODEX_SESSION_ID）及 CODEX_HOME。JSON 键名和原始技术诊断保持不变。"),
        crate::ui::tr("Each new question starts an independent topic; use @context:ID to continue one. Questions accept 1–8000 characters.", "Каждый новый вопрос начинает отдельную тему; для продолжения используйте @context:ID. Длина вопроса: 1–8000 символов.", "每个新问题开启独立主题；使用 @context:ID 继续主题。问题长度为 1–8000 个字符。"),
        crate::ui::tr("No public context, ask, reply, report or code subcommands. ask is an MCP tool.", "Подкоманд context, ask, reply, report и code нет. ask — инструмент MCP.", "没有公开的 context、ask、reply、report 或 code 子命令。ask 是 MCP 工具。"),
        crate::ui::tr("Examples (PowerShell: use .\\cm.exe; Unix: ./cm):", "Примеры (PowerShell: используйте .\\cm.exe; Unix: ./cm):", "示例（PowerShell 使用 .\\cm.exe；Unix 使用 ./cm）："));
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
    if args.len() > 1 && args.first().is_some_and(|arg| arg == "docs") {
        return crate::unified::docs::run(&args[1..]);
    }
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
                let answer = result?;
                println!("{answer}");
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
