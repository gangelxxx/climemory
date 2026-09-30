use std::collections::BTreeSet;

pub fn search_terms(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    normalize_search_text(text)
        .split_whitespace()
        .filter(|term| seen.insert((*term).to_string()))
        .map(str::to_string)
        .collect()
}

/// Ubiquitous function words excluded from the plain-link relevance gate
/// (Proposal automatic-linked-context): they appear in nearly every text,
/// so a boolean any-term gate containing them would never filter anything.
const LINK_GATE_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "being", "by", "can", "could", "did", "do",
    "does", "done", "each", "every", "for", "from", "has", "have", "how", "in", "into", "is", "it",
    "its", "may", "might", "must", "no", "not", "of", "on", "or", "over", "per", "shall", "should",
    "that", "the", "these", "this", "those", "to", "under", "via", "vs", "was", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would", "yes",
];

/// Selects the discriminating query terms for the plain-link relevance gate
/// (Proposal automatic-linked-context): soft-stemmed like lexical search, at
/// least 3 chars after stemming, and not a stopword. An empty result means
/// the query cannot judge relevance and plain links stay ungated.
pub fn link_gate_terms(query_terms: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    query_terms
        .iter()
        .filter(|term| !LINK_GATE_STOPWORDS.contains(&term.as_str()))
        .map(|term| soft_stem(term))
        .filter(|stem| stem.chars().count() >= 3 && seen.insert(stem.clone()))
        .collect()
}

/// True when any link-gate term appears in the text's normalized,
/// soft-stemmed terms — the same tokenization as lexical search, so the gate
/// agrees with what the search index considers a term match.
#[cfg(test)]
pub fn link_gate_overlap(gate_terms: &[String], text: &str) -> bool {
    if gate_terms.is_empty() {
        return true;
    }
    normalize_search_text(text)
        .split_whitespace()
        .any(|token| gate_terms.contains(&soft_stem(token)))
}

/// The same document tokens as `link_gate_overlap`, reusable across query terms.
pub(crate) fn link_gate_tokens(text: &str) -> BTreeSet<String> {
    normalize_search_text(text)
        .split_whitespace()
        .map(soft_stem)
        .collect()
}

fn is_thread_id_literal(text: &str) -> bool {
    text.len() >= 8 && text.chars().all(|character| character.is_ascii_hexdigit())
}

pub fn normalize_search_text(text: &str) -> String {
    let mut tokens = Vec::new();
    for identifier in identifier_groups(text) {
        tokens.extend(normalize_identifier(&identifier).terms);
    }
    tokens.join(" ")
}

pub(super) fn soft_stem(term: &str) -> String {
    let length = term.chars().count();
    if length < 5 || is_thread_id_literal(term) {
        return term.to_string();
    }
    if term.is_ascii() {
        if let Some(stem) = term.strip_suffix("ies").filter(|stem| stem.len() >= 3) {
            return format!("{stem}y");
        }
        for suffix in ["ingly", "edly", "ing", "ed", "es", "s"] {
            if let Some(stem) = term.strip_suffix(suffix).filter(|stem| stem.len() >= 3) {
                return stem.to_string();
            }
        }
        return term.to_string();
    }
    for suffix in [
        "иями", "ьями", "ого", "ему", "ыми", "ими", "иях", "ией", "ций", "ции", "ция", "ями",
        "ами", "ому", "ее", "ие", "ые", "ое", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "их",
        "ых", "ую", "юю", "ая", "яя", "ою", "ею", "ах", "ях", "ам", "ям", "ы", "и", "а", "я", "у",
        "ю", "е", "о",
    ] {
        if let Some(stem) = term.strip_suffix(suffix) {
            if stem.chars().count() >= 3 {
                return stem.to_string();
            }
        }
    }
    term.to_string()
}

fn identifier_groups(text: &str) -> Vec<String> {
    let mut groups = Vec::new();
    let mut identifier = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '-') {
            identifier.push(character);
        } else if !identifier.is_empty() {
            groups.push(std::mem::take(&mut identifier));
        }
    }
    if !identifier.is_empty() {
        groups.push(identifier);
    }
    groups
}

struct NormalizedIdentifier {
    terms: Vec<String>,
}

fn normalize_identifier(identifier: &str) -> NormalizedIdentifier {
    if is_thread_id_literal(identifier) {
        return NormalizedIdentifier {
            terms: vec![identifier.to_ascii_lowercase()],
        };
    }

    let mut pieces = Vec::new();
    for component in identifier.split(['_', '-']).filter(|part| !part.is_empty()) {
        append_camel_tokens(component, &mut pieces);
    }
    if pieces.is_empty() {
        return NormalizedIdentifier { terms: Vec::new() };
    }

    let compact = identifier
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .map(|character| if character == 'ё' { 'е' } else { character })
        .collect::<String>();
    let has_alias = pieces.len() > 1 && !pieces.iter().any(|piece| piece == &compact);
    let joined_singletons = pieces
        .windows(2)
        .filter(|pair| pair[0].chars().count() == 1)
        .map(|pair| format!("{}{}", pair[0], pair[1]))
        .filter(|alias| alias != &compact && !pieces.contains(alias))
        .collect::<Vec<_>>();
    if has_alias {
        pieces.retain(|piece| piece.chars().count() > 1);
    }
    let mut terms = pieces;
    terms.extend(joined_singletons);
    if has_alias {
        terms.push(compact.clone());
    }
    NormalizedIdentifier { terms }
}

fn append_camel_tokens(component: &str, tokens: &mut Vec<String>) {
    let characters = component.chars().collect::<Vec<_>>();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|at| characters.get(at))
            .copied();
        let next = characters.get(index + 1).copied();
        let camel_boundary = !current.is_empty()
            && character.is_uppercase()
            && (previous.is_some_and(|value| value.is_lowercase() || value.is_numeric())
                || (previous.is_some_and(char::is_uppercase)
                    && next.is_some_and(char::is_lowercase)));
        if camel_boundary {
            tokens.push(std::mem::take(&mut current));
        }
        for lowered in character.to_lowercase() {
            current.push(if lowered == 'ё' { 'е' } else { lowered });
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
}
