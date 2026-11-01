use std::{fs::File, path::Path, sync::Mutex};

use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use metacomplete::{MetaAutocompleter, prefix::meta::Cache};
use rocksdb::{DB, Options, SliceTransform};
use serde::Deserialize;
use yoke::Yoke;

const DB_FILE: &str = "dicts.db";
const INDEX_FILE: &str = "strprox";

pub struct Dictionary {
    db: DB,
    index: Yoke<MetaAutocompleter<'static>, Mmap>,
    cache: Mutex<Cache<'static>>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub word: String,
    pub dictionary: String,
    pub definition: String,
}

impl Dictionary {
    pub fn open(data_dir: &Path) -> Result<Self> {
        if !data_dir.is_dir() {
            bail!(
                "dictionary data directory does not exist: {}",
                data_dir.display()
            );
        }

        let db_path = data_dir.join(DB_FILE);
        let index_path = data_dir.join(INDEX_FILE);
        if !db_path.is_dir() {
            bail!("dictionary database does not exist: {}", db_path.display());
        }

        let mut options = Options::default();
        options.set_prefix_extractor(SliceTransform::create(
            "word-prefix",
            |key| {
                if key.len() < 4 {
                    return key;
                }
                let len = u32::from_be_bytes(key[..4].try_into().unwrap()) as usize;
                &key[..4 + len.min(key.len() - 4)]
            },
            None,
        ));

        let db = DB::open_for_read_only(&options, &db_path, false).with_context(|| {
            format!(
                "failed to open dictionary database read-only: {}",
                db_path.display()
            )
        })?;
        let index_file = File::open(&index_path).with_context(|| {
            format!("failed to open dictionary index: {}", index_path.display())
        })?;
        let index = Yoke::try_attach_to_cart(unsafe { Mmap::map(&index_file) }?, |bytes| {
            bincode::deserialize(bytes)
        })
        .context("failed to load Hoverpanel's fuzzy-search index")?;

        Ok(Self {
            db,
            index,
            cache: Mutex::new(Cache::default()),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let candidates = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("dictionary search cache lock was poisoned"))?;
            self.index
                .get()
                .threshold_top_k(query, limit, 3, &mut cache)
                .into_iter()
                .map(|candidate| candidate.string)
                .collect::<Vec<_>>()
        };

        let mut entries = Vec::new();
        for word in candidates {
            entries.extend(self.definitions_for(&word)?);
        }
        Ok(entries)
    }

    fn definitions_for(&self, word: &str) -> Result<Vec<Entry>> {
        let mut prefix = Vec::with_capacity(word.len() + 4);
        prefix.extend_from_slice(&(word.len() as u32).to_be_bytes());
        prefix.extend_from_slice(word.as_bytes());

        let mut entries = Vec::new();
        for record in self.db.prefix_iterator(&prefix) {
            let (key, value) = record.context("failed to read dictionary entry")?;
            let Some((stored_word, dictionary)) = split_db_key(&key) else {
                bail!("dictionary database contains an invalid key");
            };
            if stored_word != word.as_bytes() {
                break;
            }
            let definition: Definition = bincode::deserialize(&value)
                .with_context(|| format!("failed to decode dictionary entry for {word:?}"))?;
            if definition
                .word
                .as_deref()
                .is_some_and(|stored| stored != word)
            {
                bail!("dictionary key and stored word do not match for {word:?}");
            }
            if definition
                .dict_name
                .as_deref()
                .is_some_and(|stored| stored.as_bytes() != dictionary)
            {
                bail!("dictionary key and stored dictionary name do not match for {word:?}");
            }
            entries.push(Entry {
                word: word.to_owned(),
                dictionary: String::from_utf8_lossy(dictionary).into_owned(),
                definition: definition.summary(),
            });
        }
        Ok(entries)
    }
}

fn split_db_key(key: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_be_bytes(key.get(..4)?.try_into().ok()?) as usize;
    Some((key.get(4..4 + len)?, key.get(4 + len..)?))
}

#[derive(Deserialize)]
struct Definition {
    definitions: Option<Vec<Definition>>,
    groups: Option<Vec<Definition>>,
    etymology: Option<Vec<String>>,
    en: Option<String>,
    pronunciation: Option<StructuredText<String>>,
    title: Option<String>,
    info: Option<String>,
    #[serde(rename = "type")]
    word_type: Option<String>,
    _index: Option<u32>,
    word: Option<String>,
    cn: Option<String>,
    t1: Option<String>,
    t2: Option<String>,
    examples: Option<Vec<StructuredText<Example>>>,
    tip: Option<Vec<StructuredText<Tip>>>,
    related: Option<Vec<String>>,
    dict_name: Option<String>,
}

impl Definition {
    fn summary(&self) -> String {
        let mut lines = Vec::new();
        if let Some(word_type) = &self.word_type {
            lines.push(word_type.clone());
        }
        if let Some(title) = &self.title {
            lines.push(title.clone());
        }
        for text in [&self.cn, &self.en, &self.t1, &self.t2, &self.info]
            .into_iter()
            .flatten()
        {
            lines.push(text.clone());
        }
        if let Some(pronunciation) = &self.pronunciation {
            lines.extend(pronunciation.texts());
        }
        if let Some(etymology) = &self.etymology {
            lines.extend(etymology.iter().cloned());
        }
        if let Some(examples) = &self.examples {
            for example in examples {
                lines.extend(example.texts());
            }
        }
        if let Some(tips) = &self.tip {
            for tip in tips {
                lines.extend(tip.texts());
            }
        }
        if let Some(related) = &self.related {
            lines.extend(related.iter().map(|word| format!("Related: {word}")));
        }
        if let Some(definitions) = &self.definitions {
            for definition in definitions {
                let text = definition.summary();
                if !text.is_empty() {
                    lines.push(text);
                }
            }
        }
        if let Some(groups) = &self.groups {
            for group in groups {
                let text = group.summary();
                if !text.is_empty() {
                    lines.push(text);
                }
            }
        }
        lines.dedup();
        lines.join("\n")
    }
}

#[derive(Deserialize)]
enum StructuredText<T> {
    Object(T),
    Vec(Vec<Option<String>>),
    Str(String),
    None,
}

impl StructuredText<String> {
    fn texts(&self) -> Vec<String> {
        match self {
            Self::Object(text) | Self::Str(text) => vec![text.clone()],
            Self::Vec(texts) => texts.iter().flatten().cloned().collect(),
            Self::None => Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct Example {
    cn: Option<String>,
    en: Option<String>,
}

#[derive(Deserialize)]
struct Tip {
    cn: Option<String>,
    en: Option<String>,
}

impl StructuredText<Example> {
    fn texts(&self) -> Vec<String> {
        match self {
            Self::Object(example) => paired_text(&example.cn, &example.en),
            Self::Vec(texts) => texts.iter().flatten().cloned().collect(),
            Self::Str(text) => vec![text.clone()],
            Self::None => Vec::new(),
        }
    }
}

impl StructuredText<Tip> {
    fn texts(&self) -> Vec<String> {
        match self {
            Self::Object(tip) => paired_text(&tip.cn, &tip.en),
            Self::Vec(texts) => texts.iter().flatten().cloned().collect(),
            Self::Str(text) => vec![text.clone()],
            Self::None => Vec::new(),
        }
    }
}

fn paired_text(left: &Option<String>, right: &Option<String>) -> Vec<String> {
    [left, right].into_iter().flatten().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_hoverpanel_keys() {
        let word = "word";
        let dictionary = "dictionary";
        let mut key = Vec::new();
        key.extend_from_slice(&(word.len() as u32).to_be_bytes());
        key.extend_from_slice(word.as_bytes());
        key.extend_from_slice(dictionary.as_bytes());

        assert_eq!(
            split_db_key(&key),
            Some((word.as_bytes(), dictionary.as_bytes()))
        );
    }

    #[test]
    fn rejects_truncated_keys() {
        assert_eq!(split_db_key(&[0, 0, 0]), None);
        assert_eq!(split_db_key(&[0, 0, 0, 5, b'w']), None);
    }

    #[test]
    fn summarizes_nested_definition_fields() {
        let definition = Definition {
            definitions: None,
            groups: None,
            etymology: Some(vec!["origin".into()]),
            en: Some("english".into()),
            pronunciation: Some(StructuredText::Str("sound".into())),
            title: None,
            info: None,
            word_type: Some("noun".into()),
            _index: None,
            word: Some("word".into()),
            cn: Some("translation".into()),
            t1: None,
            t2: None,
            examples: None,
            tip: None,
            related: None,
            dict_name: None,
        };
        assert_eq!(
            definition.summary(),
            "noun\ntranslation\nenglish\nsound\norigin"
        );
    }
}
