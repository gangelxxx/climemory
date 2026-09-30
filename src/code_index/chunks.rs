use super::*;

pub(super) struct Chunk {
    pub(super) key: String,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) is_test: bool,
    pub(super) symbols: String,
    pub(super) body: String,
}

pub(super) fn chunks_for(path: &str, source: &str, definitions: &[code::Def]) -> Vec<Chunk> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }
    let stride = CHUNK_LINES - CHUNK_OVERLAP;
    let mut chunks = Vec::new();
    let mut ordered_definitions = definitions.iter().collect::<Vec<_>>();
    ordered_definitions.sort_by_key(|definition| definition.line);
    let file_is_test = code::path_is_test(path);
    let mut definition_start = 0usize;
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + CHUNK_LINES).min(lines.len());
        let start_line = start + 1;
        while definition_start < ordered_definitions.len()
            && ordered_definitions[definition_start].line < start_line
        {
            definition_start += 1;
        }
        let mut definition_end = definition_start;
        while definition_end < ordered_definitions.len()
            && ordered_definitions[definition_end].line <= end
        {
            definition_end += 1;
        }
        let chunk_definitions = &ordered_definitions[definition_start..definition_end];
        let symbols = chunk_definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let is_test = file_is_test
            || chunk_definitions
                .iter()
                .any(|definition| definition.is_test);
        let body = lines[start..end].join("\n");
        chunks.push(Chunk {
            key: digest(format!("{path}\0{start_line}\0{end}\0{body}")),
            start_line,
            end_line: end,
            is_test,
            symbols,
            body,
        });
        if end == lines.len() {
            break;
        }
        start += stride;
    }
    chunks
}
