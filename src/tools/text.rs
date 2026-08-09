//! Reading files as text, with the two refusals that keep a conversation usable.
//!
//! Both happen before the bytes are in memory, which is the point: a ceiling applied to
//! the string after the whole file has been loaded has already lost the argument.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Bytes a single file may contribute before it is refused outright.
///
/// Generous next to any source file and small next to the things that get read by
/// accident - a database, a core dump, a video the repository happens to carry.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How much of a file to inspect before deciding it is not text.
const SNIFF_BYTES: usize = 8 * 1024;

/// Whether a leading chunk looks like something other than text.
///
/// A null byte is the cheap and near-certain signal. Without this check a PNG read into
/// the conversation arrives as a wall of replacement characters that costs the whole
/// context window and tells the model nothing.
fn looks_binary(chunk: &[u8]) -> bool {
    chunk.contains(&0)
}

fn too_big(path: &Path, len: u64) -> String {
    format!(
        "{} is {len} bytes, over the {MAX_FILE_BYTES} byte ceiling; \
         read a line range or search it with grep instead",
        path.display()
    )
}

fn not_text(path: &Path) -> String {
    format!("{} is not a text file", path.display())
}

/// Read a whole file as text.
pub async fn read_text(path: &Path) -> Result<String, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(too_big(path, meta.len()));
    }

    let raw = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if looks_binary(&raw[..raw.len().min(SNIFF_BYTES)]) {
        return Err(not_text(path));
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Read a file's contents if it exists, or `None` if it does not.
///
/// The refusals still apply: a file being overwritten is one that could have been read,
/// and its previous contents go into a diff the user is asked to read.
pub async fn read_text_if_exists(path: &Path) -> Result<Option<String>, String> {
    match tokio::fs::metadata(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(_) => read_text(path).await.map(Some),
    }
}

/// Read a 1-indexed inclusive window of lines, numbered.
///
/// Streamed a line at a time so that a window stays readable in a file too large to read
/// whole - which is the case where asking for a window is the sensible thing to do.
pub async fn read_numbered(
    path: &Path,
    start: Option<usize>,
    end: Option<usize>,
) -> Result<String, String> {
    // A whole-file read is bounded by the ceiling; a window is not, because the point of
    // asking for one is that the file is bigger than you want.
    if start.is_none() && end.is_none() {
        let text = read_text(path).await?;
        return Ok(number_from(&text, 1));
    }

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let mut reader = BufReader::new(file);

    let mut head = vec![0u8; SNIFF_BYTES];
    let read = reader
        .read(&mut head)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if looks_binary(&head[..read]) {
        return Err(not_text(path));
    }

    // Start over now that the file is known to be text; the sniff consumed bytes the
    // line reader needs.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let mut lines = BufReader::new(file).lines();

    let first = start.unwrap_or(1).max(1);
    let last = end.unwrap_or(usize::MAX);
    let mut out = String::new();
    let mut n = 0usize;
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("read failed: {e}"))?
    {
        n += 1;
        if n < first {
            continue;
        }
        if n > last {
            break;
        }
        out.push_str(&format!("{n:>5} | {line}\n"));
        if out.len() as u64 > MAX_FILE_BYTES {
            out.push_str("… (window larger than the file ceiling)\n");
            break;
        }
    }
    Ok(out.trim_end_matches('\n').to_string())
}

fn number_from(text: &str, first: usize) -> String {
    text.lines()
        .enumerate()
        .map(|(i, l)| format!("{:>5} | {l}", i + first))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn write(dir: &Path, name: &str, body: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        tokio::fs::write(&p, body).await.unwrap();
        p
    }

    /// A PNG read by mistake must not become a screenful of replacement characters.
    #[tokio::test]
    async fn a_binary_file_is_refused_rather_than_mangled() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.png", b"\x89PNG\r\n\x1a\n\x00\x00rest").await;
        let err = read_text(&p).await.unwrap_err();
        assert!(err.contains("not a text file"), "unexpected message: {err}");
    }

    #[tokio::test]
    async fn a_file_over_the_ceiling_is_refused_before_it_is_read() {
        let dir = tempdir().unwrap();
        let p = write(
            dir.path(),
            "big",
            &vec![b'x'; (MAX_FILE_BYTES + 1) as usize],
        )
        .await;
        let err = read_text(&p).await.unwrap_err();
        assert!(err.contains("over the"), "unexpected message: {err}");
        assert!(
            err.contains("grep"),
            "the refusal should say what to do instead: {err}"
        );
    }

    #[tokio::test]
    async fn an_ordinary_file_reads_whole_and_numbered() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", b"one\ntwo\nthree\n").await;
        assert_eq!(read_text(&p).await.unwrap(), "one\ntwo\nthree\n");
        let numbered = read_numbered(&p, None, None).await.unwrap();
        assert_eq!(numbered, "    1 | one\n    2 | two\n    3 | three");
    }

    /// The window is what was asked for, inclusive at both ends and 1-indexed.
    #[tokio::test]
    async fn a_window_is_inclusive_and_one_indexed() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "a.txt", b"a\nb\nc\nd\ne\n").await;
        assert_eq!(
            read_numbered(&p, Some(2), Some(4)).await.unwrap(),
            "    2 | b\n    3 | c\n    4 | d"
        );
        assert_eq!(
            read_numbered(&p, Some(5), Some(9)).await.unwrap(),
            "    5 | e"
        );
        assert_eq!(read_numbered(&p, Some(9), Some(9)).await.unwrap(), "");
    }

    /// A window is the way to read a file too big to read whole, so the ceiling must not
    /// refuse it.
    #[tokio::test]
    async fn a_window_still_works_past_the_whole_file_ceiling() {
        let dir = tempdir().unwrap();
        let mut body = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        body.splice(0..0, b"first line\n".iter().copied());
        let p = write(dir.path(), "big.txt", &body).await;

        assert!(read_numbered(&p, None, None).await.is_err());
        assert_eq!(
            read_numbered(&p, Some(1), Some(1)).await.unwrap(),
            "    1 | first line"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_absence_not_failure() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nope");
        assert_eq!(read_text_if_exists(&p).await.unwrap(), None);
    }
}
