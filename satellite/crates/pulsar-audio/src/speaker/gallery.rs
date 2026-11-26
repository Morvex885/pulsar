//! Записанные голоса: центроиды отпечатков, сравнение и хранение в файле.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use log::warn;

/// "embedding-dim": 512 в конфиге модели.
pub const EMBEDDING_DIM: usize = 512;
// bio_lingware/config.json: guest_threshold и confidence_diff_threshold прошивки.
const MATCH_THRESHOLD: f32 = 0.66;
const MATCH_MARGIN: f32 = 0.02;
pub const MAX_NAME_CHARS: usize = 32;
// Имя для «не узнан» в сущности HA: так назвать голос нельзя.
const RESERVED_NAME: &str = "unknown";

/// Имя голоса: одна строка без управляющих символов и разделителя файла.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control)
        && name.trim() == name
        && !name.eq_ignore_ascii_case(RESERVED_NAME)
}

/// Вектор единичной длины; None для нулевого или с не-числами.
pub(super) fn normalized(mut v: Vec<f32>) -> Option<Vec<f32>> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return None;
    }
    for x in &mut v {
        *x /= norm;
    }
    Some(v)
}

pub(super) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub name: String,
    pub score: f32,
    /// Отрыв от второго голоса.
    pub lead: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gallery {
    voices: Vec<(String, Vec<f32>)>,
}

impl Gallery {
    /// Нет файла — пустая галерея; битые строки пропускаются с предупреждением.
    #[must_use]
    pub fn load(path: &Path) -> Gallery {
        let mut g = Gallery::default();
        let Ok(text) = fs::read_to_string(path) else {
            return g;
        };
        for line in text.lines() {
            let parsed = line.split_once('\t').and_then(|(name, values)| {
                let v: Option<Vec<f32>> = values.split(',').map(|x| x.parse().ok()).collect();
                let v = v.filter(|v| v.len() == EMBEDDING_DIM)?;
                valid_name(name).then(|| (name.to_owned(), normalized(v)))
            });
            match parsed {
                Some((name, Some(v))) => g.voices.push((name, v)),
                _ => warn!("speaker: bad line in {}", path.display()),
            }
        }
        g
    }

    /// Права 0600: это биометрия.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = String::new();
        for (name, v) in &self.voices {
            text.push_str(name);
            let mut sep = '\t';
            for x in v {
                let _ = write!(text, "{sep}{x}");
                sep = ',';
            }
            text.push('\n');
        }
        let tmp = path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().map_err(|e| format!("{}: {e}", path.display()))
    }

    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.voices.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Заменяет голос `name` центроидом `samples`.
    pub fn add(&mut self, name: &str, samples: &[Vec<f32>]) {
        let mut sum = vec![0.0f32; EMBEDDING_DIM];
        for s in samples {
            for (a, b) in sum.iter_mut().zip(s) {
                *a += b;
            }
        }
        let Some(v) = normalized(sum) else {
            return;
        };
        self.remove(name);
        self.voices.push((name.to_owned(), v));
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.voices.len();
        self.voices.retain(|(n, _)| n != name);
        self.voices.len() != before
    }

    /// Лучший голос, если он выше порога и заметно лучше второго, и описание оценок для настройки.
    #[must_use]
    pub fn identify(&self, embedding: &[f32]) -> (Option<Match>, String) {
        let mut scores: Vec<(&str, f32)> = self
            .voices
            .iter()
            .map(|(n, v)| (n.as_str(), cosine(v, embedding)))
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1));
        let detail = scores
            .iter()
            .map(|(n, s)| format!("{n} {s:.2}"))
            .collect::<Vec<_>>()
            .join(", ");
        let found = scores.first().and_then(|&(name, score)| {
            let second = scores.get(1).map_or(f32::MIN, |s| s.1);
            (score >= MATCH_THRESHOLD && score - second >= MATCH_MARGIN).then(|| Match {
                name: name.to_owned(),
                score,
                lead: score - second,
            })
        });
        (found, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Единичный вектор в плоскости двух первых осей под углом с косинусом `cos` к первой оси.
    fn voice(cos: f32) -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIM];
        v[0] = cos;
        v[1] = (1.0 - cos * cos).sqrt();
        v
    }

    fn other_axis() -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIM];
        v[2] = 1.0;
        v
    }

    #[test]
    fn names_are_validated() {
        assert!(valid_name("Иван"));
        assert!(valid_name("Mum 2"));
        assert!(!valid_name(""));
        assert!(!valid_name(" a"));
        assert!(!valid_name("a\tb"));
        assert!(!valid_name("a\nb"));
        assert!(!valid_name("Unknown"));
        assert!(!valid_name(&"я".repeat(33)));
    }

    #[test]
    fn identifies_above_threshold_only() {
        let mut g = Gallery::default();
        g.add("a", &[voice(1.0)]);
        assert_eq!(g.identify(&voice(0.8)).0.unwrap().name, "a");
        assert!(g.identify(&voice(0.6)).0.is_none());
        assert!(Gallery::default().identify(&voice(1.0)).0.is_none());
    }

    #[test]
    fn close_second_voice_means_unknown() {
        let mut g = Gallery::default();
        g.add("a", &[voice(1.0)]);
        let mut b = voice(1.0);
        b[1] = 0.01;
        g.add("b", &[normalized(b).unwrap()]);
        assert!(g.identify(&voice(1.0)).0.is_none());
        g.add("b", &[other_axis()]);
        assert_eq!(g.identify(&voice(1.0)).0.unwrap().name, "a");
    }

    #[test]
    fn add_replaces_and_averages() {
        let mut g = Gallery::default();
        g.add("a", &[other_axis()]);
        g.add("a", &[voice(1.0), voice(1.0)]);
        assert_eq!(g.names(), ["a"]);
        assert!(g.identify(&voice(1.0)).1.starts_with("a 1.00"));
        assert!(g.remove("a"));
        assert!(!g.remove("a"));
    }

    #[test]
    fn file_round_trip_skips_bad_lines_and_is_private() {
        let dir = std::env::temp_dir().join(format!("pulsar-gallery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("speakers.conf");
        let mut g = Gallery::default();
        g.add("Иван", &[voice(1.0)]);
        g.save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("broken\tline\nno-tab\n");
        fs::write(&path, text).unwrap();
        assert_eq!(Gallery::load(&path).names(), ["Иван"]);
        fs::remove_dir_all(&dir).unwrap();
        assert!(Gallery::load(&path).names().is_empty());
    }

    #[test]
    fn zero_vector_is_rejected() {
        assert!(normalized(vec![0.0; 4]).is_none());
        assert!(normalized(vec![f32::NAN; 4]).is_none());
    }
}
