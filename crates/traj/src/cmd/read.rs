use crate::{norm, one_store};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static EDIT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Args, Debug)]
pub struct CatArgs {
    #[arg(required = true)]
    pub paths: Vec<String>,
    /// Skip the sha check
    #[arg(long)]
    pub no_verify: bool,
}

pub fn cat(stores: &[String], a: CatArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let mut reader = st.reader();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for p in &a.paths {
        let p = norm(p);
        let Some(r) = st.stat(&p)? else {
            bail!("{p}: not in the store")
        };
        let bytes = st.read_row(&mut reader, &r, !a.no_verify)?;
        out.write_all(&bytes)?;
    }
    Ok(0)
}

#[derive(Args, Debug)]
pub struct ExtractArgs {
    /// File or directory in the store ("" or "." for everything)
    pub path: String,
    pub dst: PathBuf,
    /// Hard-link files with identical content
    #[arg(long)]
    pub hardlink_dedupe: bool,
    /// Restore recorded mtimes
    #[arg(long)]
    pub mtime: bool,
    #[arg(long)]
    pub no_verify: bool,
}

pub fn extract(stores: &[String], a: ExtractArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let p = norm(&a.path);
    let n = st.extract(&p, &a.dst, a.hardlink_dedupe, a.mtime, !a.no_verify)?;
    eprintln!("{n} entries written to {}", a.dst.display());
    Ok(0)
}

#[derive(Args, Debug)]
pub struct EditArgs {
    pub path: String,
}

fn editor_words(editor: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    let mut chars = editor.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.get_or_insert_with(String::new).push(c),
            (_, '\\') => {
                if quote == Some('"')
                    && chars
                        .peek()
                        .is_some_and(|c| !matches!(c, '$' | '`' | '"' | '\\' | '\n'))
                {
                    word.get_or_insert_with(String::new).push('\\');
                } else {
                    let next = chars.next().context("$EDITOR ends with a backslash")?;
                    if next != '\n' {
                        word.get_or_insert_with(String::new).push(next);
                    }
                }
            }
            (None, '\'' | '"') => {
                word.get_or_insert_with(String::new);
                quote = Some(c);
            }
            (None, c) if c.is_whitespace() => {
                if let Some(word) = word.take() {
                    words.push(word);
                }
            }
            (_, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        bail!("$EDITOR has an unterminated quote");
    }
    if let Some(word) = word {
        words.push(word);
    }
    if words.first().is_none_or(|word| word.is_empty()) {
        bail!("$EDITOR must name an executable");
    }
    Ok(words)
}

fn edit_copy(base: &Path, name: &str, original: &[u8]) -> Result<PathBuf> {
    let dir = loop {
        let serial = EDIT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("traj-edit-{}-{serial}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("create edit directory"),
        }
    };
    let file = dir.join(name);
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&file)?.write_all(original)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir(&dir);
        return Err(error).context("write edit copy");
    }
    Ok(file)
}

/// Extract to a temp file, run $EDITOR, print a unified diff; the store is never modified.
pub fn edit(stores: &[String], a: EditArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let p = norm(&a.path);
    let Some(r) = st.stat(&p)? else {
        bail!("{p}: not in the store")
    };
    let mut reader = st.reader();
    let original = st.read_row(&mut reader, &r, true)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let words = if Path::new(&editor).is_file() {
        vec![editor.clone()]
    } else {
        editor_words(&editor)?
    };
    let tmp = edit_copy(&std::env::temp_dir(), r.name(), &original)?;
    let status = std::process::Command::new(&words[0])
        .args(&words[1..])
        .arg(&tmp)
        .status()
        .with_context(|| format!("run {editor}; copy at {}", tmp.display()))?;
    if !status.success() {
        bail!("{editor} exited with {status}; copy at {}", tmp.display());
    }
    let edited = std::fs::read(&tmp)?;
    if edited == original {
        eprintln!("unchanged");
        std::fs::remove_file(&tmp)?;
        std::fs::remove_dir(tmp.parent().unwrap())?;
    } else {
        let a_s = String::from_utf8_lossy(&original);
        let b_s = String::from_utf8_lossy(&edited);
        let diff = similar::TextDiff::from_lines(&a_s, &b_s);
        print!(
            "{}",
            diff.unified_diff()
                .context_radius(3)
                .header(&format!("store/{p}"), &tmp.display().to_string())
        );
        eprintln!(
            "edited copy left at {} (the store is read-only)",
            tmp.display()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_editor_arguments_are_parsed_without_a_shell() {
        assert_eq!(
            editor_words(r#""editor path" --wait 'two words' "" a\ b"#).unwrap(),
            ["editor path", "--wait", "two words", "", "a b"]
        );
        assert_eq!(
            editor_words(r#"editor "\x" \$literal"#).unwrap(),
            ["editor", r"\x", "$literal"]
        );
        for invalid in ["", "   ", "''", "'unterminated", "editor \\"] {
            assert!(editor_words(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn review_edit_copies_are_private_and_never_reuse_an_earlier_copy() {
        let dir = tempfile::tempdir_in(".").unwrap();
        let first = edit_copy(dir.path(), "log", b"first").unwrap();
        let second = edit_copy(dir.path(), "log", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(first.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
