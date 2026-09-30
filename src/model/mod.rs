//! Thread identity and read-only import of historical Markdown. No specification states.
use crate::util::{digest, normalize_newlines, AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadMeta {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub tags: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ThreadDoc {
    pub meta: ThreadMeta,
    pub source_revision: String,
    pub historical: String,
}

pub fn parse_tags(text: &str) -> Result<Vec<String>> {
    Ok(text
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}
fn validate(meta: &ThreadMeta) -> Result<()> {
    if meta.id.len() < 8 || meta.id.len() > 64 || !meta.id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new("invalid thread ID"));
    }
    if meta.slug.is_empty()
        || meta.slug.len() > 120
        || !meta
            .slug
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err(AppError::new(
            "thread slug must contain 1..120 lowercase ASCII letters, digits or hyphens",
        ));
    }
    for value in [&meta.title, &meta.summary, &meta.area] {
        if value.chars().count() > 1000 || value.chars().any(char::is_control) {
            return Err(AppError::new(
                "thread metadata must be one line, up to 1000 characters",
            ));
        }
    }
    if meta.title.trim().is_empty() {
        return Err(AppError::new("thread title must not be empty"));
    }
    Ok(())
}
impl ThreadDoc {
    pub fn new_memory(
        id: String,
        slug: String,
        title: String,
        summary: String,
        area: String,
        tags: Vec<String>,
    ) -> Result<Self> {
        let meta = ThreadMeta {
            id,
            slug,
            title,
            summary,
            area,
            tags,
        };
        validate(&meta)?;
        let mut doc = Self {
            meta,
            source_revision: String::new(),
            historical: String::new(),
        };
        doc.source_revision = digest(doc.render().as_bytes());
        Ok(doc)
    }
    pub fn parse(text: &str) -> Result<Self> {
        let raw = normalize_newlines(text);
        let (front, historical) = raw
            .strip_prefix("---\n")
            .and_then(|s| s.split_once("\n---\n"))
            .ok_or_else(|| AppError::new("thread requires closed frontmatter"))?;
        let mut fields = BTreeMap::new();
        for line in front.lines() {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| AppError::new("invalid thread frontmatter"))?;
            if fields.insert(key.trim(), value.trim()).is_some() {
                return Err(AppError::new("duplicate thread metadata field"));
            }
        }
        let required = |key| {
            fields
                .get(key)
                .copied()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new(format!("missing thread field '{key}'")))
        };
        if !matches!(
            required("format")?,
            "climemory-thread/1" | "climemory-memory-thread/1"
        ) {
            return Err(AppError::new("unsupported thread format"));
        }
        let meta = ThreadMeta {
            id: required("id")?.into(),
            slug: required("slug")?.into(),
            title: required("title")?.into(),
            summary: fields.get("summary").unwrap_or(&"").to_string(),
            area: fields.get("area").unwrap_or(&"").to_string(),
            tags: parse_tags(fields.get("tags").unwrap_or(&""))?,
        };
        validate(&meta)?;
        Ok(Self {
            meta,
            source_revision: digest(raw.as_bytes()),
            historical: historical.trim().into(),
        })
    }
    pub fn render(&self) -> String {
        format!("---\nformat: climemory-memory-thread/1\nid: {}\nslug: {}\ntitle: {}\nsummary: {}\narea: {}\ntags: {}\n---\n{}\n",self.meta.id,self.meta.slug,self.meta.title,self.meta.summary,self.meta.area,self.meta.tags.join(", "),self.historical)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_threads_keep_original_revision_and_render_current_format() {
        for format in ["climemory-thread/1", "climemory-memory-thread/1"] {
            let source = format!("---\nformat: {format}\nid: abcdef12\nslug: old\ntitle: Old\n---\nKeep this fact.\n");
            let doc = ThreadDoc::parse(&source).unwrap();
            assert_eq!(doc.source_revision, digest(source.as_bytes()));
            assert_eq!(doc.historical, "Keep this fact.");
            assert!(doc.render().contains("format: climemory-memory-thread/1"));
        }
    }
    #[test]
    fn historical_states_are_data_and_new_threads_have_no_states() {
        let source = "---\nformat: climemory-thread/1\nid: abcdef12\nslug: old\ntitle: Old\ncurrent_state: verified\n---\n## Current\nOld claim\n## Proposal\nAn idea\n";
        let doc = ThreadDoc::parse(source).unwrap();
        assert!(doc.historical.contains("An idea"));
        let doc = ThreadDoc::new_memory(
            "abcdef12".into(),
            "new".into(),
            "New".into(),
            "".into(),
            "".into(),
            vec![],
        )
        .unwrap();
        assert!(!doc.render().contains("current_state"));
        assert_eq!(ThreadDoc::parse(&doc.render()).unwrap().meta.slug, "new");
        assert!(ThreadDoc::parse(&source.replace("slug: old", "slug: ../../escape")).is_err());
    }
}
