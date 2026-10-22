use anyhow::{Context, Result};
use lazykatok::adapters::{FixtureAdapter, KakaocliAdapter, MacosAdapter, SourceAdapter};
use lazykatok::kakao::store::{store_stamp, StoreStamp};
use lazykatok::types::RawMessage;
use std::path::{Path, PathBuf};

pub(super) fn adapter_for_source(
    source: &str,
    path: Option<PathBuf>,
    data_dir: &Path,
) -> Result<Box<dyn SourceAdapter>> {
    match source {
        "fixture" => {
            let fixture_path = path.context("fixture source requires a JSONL path")?;
            Ok(Box::new(FixtureAdapter::new(fixture_path)))
        }
        "kakaocli" => Ok(Box::new(KakaocliAdapter)),
        "macos" | "kakao" => {
            let home = lazykatok::kakao::default_home().context("resolve home directory")?;
            Ok(Box::new(MacosAdapter::new(home, data_dir.to_path_buf())))
        }
        other => anyhow::bail!("unsupported source adapter: {other}"),
    }
}

/// Reads a watched source once per poll, but only when it can have changed.
///
/// Decrypting the macOS store takes seconds on a large account, so a watch loop
/// that re-read it every poll kept a core busy even while nothing was written.
/// The store's files are stamped before each read; a stamp equal to the one
/// taken before the previous successful read means that read already saw this
/// content. Sources without a stamp are read every poll, as before.
pub(super) struct WatchSourceReader {
    source: String,
    path: Option<PathBuf>,
    data_dir: PathBuf,
    last_stamp: Option<StoreStamp>,
}

impl WatchSourceReader {
    pub(super) fn new(source: &str, path: Option<PathBuf>, data_dir: &Path) -> Self {
        Self {
            source: source.to_string(),
            path,
            data_dir: data_dir.to_path_buf(),
            last_stamp: None,
        }
    }

    /// `Ok(None)` when the source is unchanged since the last successful read.
    pub(super) fn read_if_changed(&mut self) -> Result<Option<Vec<RawMessage>>> {
        let stamp = match self.source.as_str() {
            "macos" | "kakao" => Some(store_stamp(
                &lazykatok::kakao::default_home().context("resolve home directory")?,
            )),
            _ => None,
        };
        read_if_stamp_changed(&mut self.last_stamp, stamp, || {
            // A new adapter per read: the macOS adapter memoizes one read per instance.
            let adapter = adapter_for_source(&self.source, self.path.clone(), &self.data_dir)?;
            Ok(adapter.messages()?)
        })
    }
}

/// Run `read` unless `stamp` matches the stamp recorded by the last successful
/// read. A failed read records nothing, so the next poll retries it.
fn read_if_stamp_changed<S: PartialEq, T>(
    last: &mut Option<S>,
    stamp: Option<S>,
    read: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    if stamp.is_some() && *last == stamp {
        return Ok(None);
    }
    let value = read()?;
    *last = stamp;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn an_unchanged_stamp_skips_the_read() {
        let mut last = None;
        assert_eq!(
            read_if_stamp_changed(&mut last, Some(1), || Ok("first")).unwrap(),
            Some("first")
        );
        let skipped = read_if_stamp_changed(&mut last, Some(1), || -> Result<&str> {
            panic!("an unchanged source must not be read again")
        });
        assert_eq!(skipped.unwrap(), None);
        assert_eq!(
            read_if_stamp_changed(&mut last, Some(2), || Ok("second")).unwrap(),
            Some("second")
        );
    }

    #[test]
    fn a_source_without_a_stamp_is_read_every_poll() {
        let mut last: Option<u8> = None;
        for _ in 0..2 {
            assert_eq!(
                read_if_stamp_changed(&mut last, None, || Ok(())).unwrap(),
                Some(())
            );
        }
    }

    #[test]
    fn a_failed_read_is_retried_on_the_same_stamp() {
        let mut last = None;
        assert!(read_if_stamp_changed(&mut last, Some(1), || -> Result<()> {
            Err(anyhow!("synthetic read failure"))
        })
        .is_err());
        assert_eq!(
            read_if_stamp_changed(&mut last, Some(1), || Ok(())).unwrap(),
            Some(())
        );
    }
}
