//! Finding the passages a question is about, before the question is asked.
//!
//! The project is on this disk and the search is mechanical, but the loop used to leave
//! the whole of it to the model: which file, whether to look at all, when to stop. A small
//! local model does the cheapest thing instead, which is to answer from the one file it
//! could name, and nothing noticed.
//!
//! So the search is done here. The distinctive words of the question are looked for in the
//! project, the best matches travel with it, and the model starts from what is actually
//! there rather than from what it can guess. Bounded, deterministic, and nothing is hidden:
//! what was attached is named to the person who asked.

use std::collections::BTreeMap;
use std::path::Path;

/// The most the passages may take, whatever the window.
///
/// A ceiling on top of the caller's budget, not instead of it: the passages are what the
/// question arrives with, not what the turn is for, and a model handed the whole project
/// has no room left to read the part that matters.
const MOST_CHARS: usize = 3000;

/// How many files the passages may come from. Enough to see a thing in its places, few
/// enough that a common word does not fill the window with itself.
const MOST_FILES: usize = 6;

/// How many matching lines are taken from one file.
const MOST_LINES: usize = 6;

/// The longest a line may be before it says more than it shows.
const MOST_LINE_CHARS: usize = 200;

/// What was found, and where.
pub struct Found {
    /// The passages, ready to travel with the question.
    pub text: String,
    /// The files they came from, for telling the person.
    pub files: Vec<String>,
}

/// Look for what the question is about, in the project.
///
/// `budget` is what the passages may occupy, capped at `MOST_CHARS`. It follows the
/// window: on a small one, passages sized for a large window would leave no room for the
/// reading they are meant to start.
///
/// `None` when the question has no distinctive word to look for, or when nothing in the
/// project carries one: an empty section in front of a model says the project is empty.
pub fn passages(asked: &str, root: &Path, budget: usize) -> Option<Found> {
    let budget = budget.min(MOST_CHARS);
    let terms = terms(asked);
    if terms.is_empty() {
        return None;
    }
    // Per file: which of the terms it carries, how much of it does, and the first lines
    // that show it. One pass, so that a file the budget cuts short is still known to carry
    // every term it carries: the count decides the ranking, the lines only illustrate it.
    let mut hits: BTreeMap<String, (usize, usize, Vec<String>)> = BTreeMap::new();
    for entry in crate::agent::layout::source_files(root) {
        let Ok(text) = std::fs::read_to_string(&entry.1) else {
            continue;
        };
        let mut carried = vec![false; terms.len()];
        let mut weight = 0usize;
        let mut lines = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let lowered = line.to_lowercase();
            let mut on_this_line = false;
            for (i, term) in terms.iter().enumerate() {
                if lowered.contains(term.as_str()) {
                    carried[i] = true;
                    on_this_line = true;
                }
            }
            if !on_this_line {
                continue;
            }
            weight += 1;
            if lines.len() < MOST_LINES {
                let shown: String = line.trim().chars().take(MOST_LINE_CHARS).collect();
                lines.push(format!("{:>5} | {shown}", n + 1));
            }
        }
        let matched = carried.iter().filter(|c| **c).count();
        if matched > 0 {
            hits.insert(entry.0, (matched, weight, lines));
        }
    }
    if hits.is_empty() {
        return None;
    }
    // The files that carry the most of what was asked about, first. On a question with one
    // distinctive word every file that has it ties, so the one that has it most often wins:
    // alphabetical order would answer a question about the session with the agent loop.
    let mut ranked: Vec<(String, usize, usize, Vec<String>)> = hits
        .into_iter()
        .map(|(name, (n, weight, lines))| (name, n, weight, lines))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));

    let mut text = String::new();
    let mut files = Vec::new();
    for (name, _, _, lines) in ranked.into_iter().take(MOST_FILES) {
        let block = format!("\n\n--- {name} ---\n{}", lines.join("\n"));
        if !files.is_empty() && text.len() + block.len() > budget {
            break;
        }
        text.push_str(&block);
        files.push(name);
    }
    if files.is_empty() {
        return None;
    }
    Some(Found {
        text: format!(
            "What the project holds for that question, found by searching it. Read further \
             from here rather than guessing:{text}"
        ),
        files,
    })
}

/// The words worth looking for in a question.
///
/// The ones a project would use: long enough to be a name rather than grammar, and not one
/// of the words every question is made of. Lowercased, because a search that misses
/// `Session` when asked about `session` is a search that finds nothing.
fn terms(asked: &str) -> Vec<String> {
    // Long enough to be a name, and not one of the words every question is made of. The
    // list is longer than a stop list for prose because a word like "work" or "here"
    // appears in every file of every project, and finds nothing by finding everything.
    const GRAMMAR: [&str; 61] = [
        "the", "and", "for", "with", "what", "does", "this", "that", "from", "are", "you", "can",
        "how", "why", "when", "where", "who", "was", "were", "have", "has", "had", "not", "but",
        "all", "any", "its", "based", "sources", "source", "code", "project", "repo", "crate",
        "codebase", "features", "feature", "please", "tell", "work", "works", "here", "there",
        "make", "made", "need", "want", "like", "just", "also", "some", "more", "most", "then",
        "than", "into", "over", "only", "very", "much", "about",
    ];
    // Words that turn what follows them into something to leave out. A question that
    // names a file in order to exclude it - "from the sources but not the README" - was
    // read as a question about that file, and the one thing the person asked to be left
    // alone came back ranked first, with an instruction to read further from it.
    //
    // Narrow on purpose: only the words immediately after one of these, and only until
    // the next comma or the end of the clause. Anything cleverer would be guessing at the
    // shape of a sentence, and guessing wrong here drops what the question was about.
    const EXCEPT: [&str; 6] = ["not", "without", "except", "excluding", "besides", "ignore"];
    let mut excluded: Vec<String> = Vec::new();
    let mut dropping = false;
    for token in asked.split_whitespace() {
        let bare: String = token
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .to_lowercase();
        if EXCEPT.contains(&bare.as_str()) {
            dropping = true;
            continue;
        }
        if dropping {
            if !bare.is_empty() && !GRAMMAR.contains(&bare.as_str()) {
                excluded.push(bare);
            }
            // A comma ends the aside; so does the end of what was said.
            if token.ends_with(',') || token.ends_with(';') {
                dropping = false;
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    for word in asked.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let word = word.to_lowercase();
        if word.len() < 4
            || GRAMMAR.contains(&word.as_str())
            || excluded.contains(&word)
            || out.contains(&word)
        {
            continue;
        }
        out.push(word);
        if out.len() == 4 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/session.rs"),
            "pub struct Session {}\n// compaction lives here\nfn compact() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/other.rs"), "fn unrelated() {}\n").unwrap();
        dir
    }

    #[test]
    fn the_words_looked_for_are_the_ones_a_project_would_use() {
        assert_eq!(
            terms("based on the sources, how does compaction work"),
            vec!["compaction"]
        );
        assert_eq!(
            terms("where is the approval scope decided"),
            vec!["approval", "scope", "decided"]
        );
        // A question made only of grammar has nothing to look for.
        assert!(terms("what does this do").is_empty());
    }

    /// A file named in order to be left out is not what the question is about.
    ///
    /// Asked "from the sources but not the README", the search took README for the
    /// subject, ranked it first, and put it in front of the model with an instruction to
    /// read further from it - the one file the person had asked it to leave alone.
    #[test]
    fn a_word_named_to_be_excluded_is_not_searched_for() {
        assert_eq!(
            terms("using the sources but not the readme, what are the verve features"),
            vec!["using", "verve"]
        );
        assert!(!terms("describe compaction without the digest").contains(&"digest".into()));
        // The aside ends at the comma: what comes after it is the question again.
        assert!(terms("not the readme, how does compaction work").contains(&"compaction".into()));
    }

    /// The passages come from the files that carry what was asked about, and say where.
    #[test]
    fn what_the_question_is_about_travels_with_it() {
        let dir = project();
        let found =
            passages("how does compaction work here", dir.path(), MOST_CHARS).expect("found");
        assert_eq!(found.files, vec!["src/session.rs"]);
        assert!(
            found.text.contains("compaction lives here"),
            "{}",
            found.text
        );
        assert!(found.text.contains("src/session.rs"), "{}", found.text);
        assert!(!found.text.contains("unrelated"), "{}", found.text);
    }

    /// An empty section in front of a model says the project is empty.
    #[test]
    fn a_question_that_matches_nothing_attaches_nothing() {
        let dir = project();
        assert!(passages("how does telemetry work here", dir.path(), MOST_CHARS).is_none());
        assert!(passages("what does this do", dir.path(), MOST_CHARS).is_none());
    }

    /// Carrying more of the question beats carrying one word of it often, and among files
    /// that carry the same words the one that carries them most often comes first.
    #[test]
    fn the_files_that_carry_the_question_come_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("both.rs"), "compaction\nsession\n").unwrap();
        std::fs::write(dir.path().join("dense.rs"), "compaction\n".repeat(50)).unwrap();
        std::fs::write(dir.path().join("thin.rs"), "compaction\n").unwrap();
        let found = passages(
            "how does compaction of a session work",
            dir.path(),
            MOST_CHARS,
        )
        .expect("found");
        assert_eq!(found.files, vec!["both.rs", "dense.rs", "thin.rs"]);
    }

    /// The budget follows the window, and one file always comes back: passages sized for
    /// a large window would leave a small one no room for the reading they start.
    #[test]
    fn a_small_window_gets_smaller_passages() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..4 {
            std::fs::write(
                dir.path().join(format!("f{i}.rs")),
                "compaction\n".repeat(20),
            )
            .unwrap();
        }
        let wide = passages("compaction", dir.path(), MOST_CHARS).expect("found");
        let narrow = passages("compaction", dir.path(), 1).expect("found");
        assert_eq!(wide.files.len(), 4);
        // Never nothing: a budget too small for one file still returns that file, because
        // a question about the project answered from no passage at all is the defect this
        // exists to fix.
        assert_eq!(narrow.files.len(), 1);
    }

    #[test]
    fn the_passages_stay_within_their_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            std::fs::write(
                dir.path().join(format!("f{i}.rs")),
                "compaction\n".repeat(200),
            )
            .unwrap();
        }
        let found = passages("compaction", dir.path(), MOST_CHARS).expect("found");
        assert!(found.text.len() <= MOST_CHARS + 200, "{}", found.text.len());
        assert!(found.files.len() <= MOST_FILES);
    }
}
