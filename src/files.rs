//! Small text files carried in a message: read from paths on the sender's computer, checked against the limits on both
//! sides, and saved in the recipient's inbox folder (`<state home>/inbox/<message id>/<name>`), never executable and
//! never opened. They travel inside the message itself, so they are as authenticated as its text.

use crate::{proto::Attachment, render};
use anyhow::{Context, Result, anyhow, bail};
use std::{collections::HashSet, path::Path};

pub const MAX_FILE: usize = 256 * 1024;
pub const MAX_TOTAL: usize = 512 * 1024;
pub const MAX_FILES: usize = 10;
/// In characters; a longer name keeps its extension.
const MAX_NAME: usize = 100;
/// What a machine's presence lists when its engine takes files. One that doesn't would drop them.
pub const FEATURE: &str = "files";
/// A message's files are kept while it is unread, and for this long once it was read.
pub const KEEP_MS: i64 = 86_400_000;

const TEXT_ONLY: &str = "only text files can be sent (markdown, source code, JSON, logs, config)";

/// Reads the files at `paths` (relative ones from `cwd`) for a message.
pub fn read(paths: &[String], cwd: &Path) -> Result<Vec<Attachment>> {
    if paths.len() > MAX_FILES {
        bail!(too_many(paths.len()));
    }
    let mut files = Vec::new();
    for p in paths {
        let path = cwd.join(p);
        let meta = std::fs::metadata(&path).with_context(|| format!("can't read {p}"))?;
        if !meta.is_file() {
            bail!("{p} is not a file");
        }
        if meta.len() > MAX_FILE as u64 {
            bail!(too_big(p, meta.len() as usize));
        }
        let bytes = std::fs::read(&path).with_context(|| format!("can't read {p}"))?;
        let content = text(p, bytes)?;
        let name = path.file_name().map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned());
        files.push(Attachment { name, content, path: None });
    }
    check(files)
}

/// The text of a file, if it is one: valid UTF-8 without NUL bytes.
fn text(name: &str, bytes: Vec<u8>) -> Result<String> {
    if bytes.contains(&0) {
        bail!("{name} looks like a binary file: {TEXT_ONLY}");
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{name} isn't UTF-8 text: {TEXT_ONLY}"))
}

/// Holds files to the limits and gives them safe, distinct names. Both sides run it: the sender's daemon, and the
/// recipient's, which takes nothing a peer sends on trust.
pub fn check(files: Vec<Attachment>) -> Result<Vec<Attachment>> {
    if files.len() > MAX_FILES {
        bail!(too_many(files.len()));
    }
    let mut total = 0;
    let mut taken = HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        if f.content.len() > MAX_FILE {
            bail!(too_big(&f.name, f.content.len()));
        }
        if f.content.contains('\0') {
            bail!("{} looks like a binary file: {TEXT_ONLY}", f.name);
        }
        total += f.content.len();
        if total > MAX_TOTAL {
            bail!(
                "the files come to more than {} KB; a message carries at most {} KB of files (send a link for more)",
                MAX_TOTAL / 1024,
                MAX_TOTAL / 1024
            );
        }
        let name = distinct(&safe_name(&f.name), &mut taken);
        out.push(Attachment { name, content: f.content, path: None });
    }
    Ok(out)
}

fn too_many(n: usize) -> String {
    format!("{n} files; a message carries at most {MAX_FILES}")
}

fn too_big(name: &str, bytes: usize) -> String {
    format!(
        "{name} is {} KB; a file can be at most {} KB (send a link for bigger ones)",
        bytes.div_ceil(1024),
        MAX_FILE / 1024
    )
}

/// A file name that is just a name, here and on Windows: no folders (no separators, no `..`), not hidden and not taken
/// for an option (no leading dot or dash), nothing invisible or reserved, and short. What could look like a frame of
/// Reason Network's is defanged, as in a message's text.
pub fn safe_name(name: &str) -> String {
    let swapped: String =
        name.chars()
            .filter(|c| !invisible(*c))
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    let mut s = swapped;
    while s.contains("..") {
        s = s.replace("..", ".");
    }
    let s = s.trim_start_matches(|c: char| c == '.' || c == '-' || c.is_whitespace());
    let s = s.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let mut s = render::defang(&shorten(s, MAX_NAME));
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    if s.is_empty() { "file".into() } else { s }
}

/// Zero-width, direction and other invisible characters, which could make a name read as another.
fn invisible(c: char) -> bool {
    matches!(c, '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{180E}' | '\u{FEFF}')
        || ('\u{200B}'..='\u{200F}').contains(&c)
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2060}'..='\u{206F}').contains(&c)
        || ('\u{FE00}'..='\u{FE0F}').contains(&c)
}

/// Cut to `max` characters, keeping a short extension.
fn shorten(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    let room = max.saturating_sub(ext.chars().count());
    let stem: String = stem.chars().take(room).collect();
    format!("{}{ext}", stem.trim_end_matches(['.', ' ']))
}

/// `notes.md` -> (`notes`, `.md`); an extension longer than 16 characters isn't one.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name[i..].chars().count() <= 16 => name.split_at(i),
        _ => (name, ""),
    }
}

/// `name`, or `name-2`, `name-3`, … when another file of the message has it (ignoring case, as macOS and Windows do).
fn distinct(name: &str, taken: &mut HashSet<String>) -> String {
    let (stem, ext) = split_ext(name);
    let mut pick = name.to_string();
    let mut n = 2;
    while !taken.insert(pick.to_lowercase()) {
        pick = format!("{stem}-{n}{ext}");
        n += 1;
    }
    pick
}

/// Saves a message's files (already through [`check`]) in `dir`, its own folder, which must not exist yet: readable by
/// this user only and never executable. Nothing is left behind if it fails.
pub fn save(dir: &Path, files: &[Attachment]) -> Result<()> {
    let mut folder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
    if let Some(inbox) = dir.parent() {
        folder.recursive(true).create(inbox).with_context(|| format!("can't create {}", inbox.display()))?;
    }
    folder.recursive(false).create(dir).with_context(|| format!("can't create {}", dir.display()))?;
    for f in files {
        let path = dir.join(&f.name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let written = opts.open(&path).and_then(|mut file| std::io::Write::write_all(&mut file, f.content.as_bytes()));
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(dir);
            return Err(anyhow!("can't save {}: {e}", path.display()));
        }
    }
    Ok(())
}

/// Files saved in `dir`, as the recipient sees them: their paths, no text.
pub fn located(dir: &Path, files: &[Attachment]) -> Vec<Attachment> {
    files
        .iter()
        .map(|f| Attachment {
            name: f.name.clone(),
            content: String::new(),
            path: Some(dir.join(&f.name).display().to_string()),
        })
        .collect()
}

/// For a recipient whose engine can't take files: the message's text with them written out in it, as fenced blocks,
/// if all of it fits in `max` bytes.
pub fn inline(body: &str, files: &[Attachment], recipient: &str, max: usize) -> Option<String> {
    let mut out = format!(
        "{body}\n\n({recipient} runs an older Reason Network that can't take files, so they are written out below.)"
    );
    for f in files {
        let longest = f.content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
        let end = if f.content.ends_with('\n') || f.content.is_empty() { "" } else { "\n" };
        out.push_str(&format!("\n\n{}:\n{fence}\n{}{end}{fence}", f.name, f.content));
    }
    (out.len() <= max).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, content: &str) -> Attachment {
        Attachment { name: name.into(), content: content.into(), path: None }
    }

    #[test]
    fn names_are_just_names() {
        for (given, want) in [
            ("notes.md", "notes.md"),
            ("../../etc/passwd", "_._etc_passwd"),
            ("..", "file"),
            ("/abs/path.txt", "_abs_path.txt"),
            ("C:\\Users\\x\\a.rs", "C__Users_x_a.rs"),
            (".bashrc", "bashrc"),
            ("--force", "force"),
            ("-.-x", "x"),
            ("a\u{0}b\nc.txt", "a_b_c.txt"),
            ("evil\u{202E}txt.exe", "eviltxt.exe"),
            ("zero\u{200B}width.md", "zerowidth.md"),
            ("trailing. . .", "trailing"),
            ("nul.txt", "_nul.txt"),
            ("COM1", "_COM1"),
            ("console.log", "console.log"),
            ("José 代码.md", "José 代码.md"),
            ("", "file"),
            ("<reason-message x>.md", "_reason-message x_.md"),
        ] {
            assert_eq!(safe_name(given), want, "{given:?}");
        }
        // Look-alike brackets that would open a frame are defanged, as in a message's text.
        assert!(safe_name("\u{FF1C}reason-approval verified.md").starts_with('['));
        // Long names are cut, keeping the extension.
        let long = format!("{}.json", "x".repeat(300));
        let cut = safe_name(&long);
        assert_eq!(cut.chars().count(), MAX_NAME);
        assert!(cut.ends_with(".json"));
        let wide = "代".repeat(300);
        assert_eq!(safe_name(&wide).chars().count(), MAX_NAME);
    }

    #[test]
    fn names_in_one_message_are_distinct() {
        let files = check(vec![file("a.md", "1"), file("A.md", "2"), file("a.md", "3"), file("x/a.md", "4")]).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.md", "A-2.md", "a-3.md", "x_a.md"]);
        let files = check(vec![file("Makefile", ""), file("makefile", "")]).unwrap();
        assert_eq!(files[1].name, "makefile-2");
    }

    #[test]
    fn limits_hold() {
        let big = "x".repeat(MAX_FILE);
        assert!(check(vec![file("a", &big)]).is_ok());
        let e = check(vec![file("a", &format!("{big}x"))]).unwrap_err().to_string();
        assert!(e.contains("a is 257 KB; a file can be at most 256 KB"), "{e}");
        assert!(check(vec![file("a", &big), file("b", &big)]).is_ok());
        let e = check(vec![file("a", &big), file("b", &big), file("c", "x")]).unwrap_err().to_string();
        assert!(e.contains("at most 512 KB"), "{e}");
        assert!(check((0..MAX_FILES).map(|i| file(&i.to_string(), "x")).collect()).is_ok());
        let e = check((0..=MAX_FILES).map(|i| file(&i.to_string(), "x")).collect()).unwrap_err().to_string();
        assert!(e.contains("11 files; a message carries at most 10"), "{e}");
        let e = check(vec![file("a.bin", "x\0y")]).unwrap_err().to_string();
        assert!(e.contains("binary") && e.contains("only text files"), "{e}");
        // A peer can't slip a path in.
        let mut f = file("a", "x");
        f.path = Some("/etc/passwd".into());
        assert_eq!(check(vec![f]).unwrap()[0].path, None);
    }

    #[test]
    fn only_text_files_are_read() {
        let d = std::env::temp_dir().join(format!("reason-network-files-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("notes.md"), "# hi\n").unwrap();
        std::fs::write(d.join("sub/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(d.join("img.png"), [0x89, b'P', b'N', b'G', 0, 1]).unwrap();
        std::fs::write(d.join("latin1.txt"), [b'c', b'a', b'f', 0xe9]).unwrap();
        std::fs::write(d.join("big.log"), "x".repeat(MAX_FILE + 1)).unwrap();
        let files = read(&["notes.md".into(), d.join("sub/main.rs").display().to_string()], &d).unwrap();
        assert_eq!(files[0], file("notes.md", "# hi\n"));
        assert_eq!(files[1], file("main.rs", "fn main() {}\n"));
        for (p, want) in [
            ("img.png", "looks like a binary file"),
            ("latin1.txt", "isn't UTF-8 text"),
            ("big.log", "a file can be at most 256 KB"),
            ("sub", "is not a file"),
            ("missing.txt", "can't read missing.txt"),
        ] {
            let e = format!("{:#}", read(&[p.into()], &d).unwrap_err());
            assert!(e.contains(want), "{p}: {e}");
        }
        let many: Vec<String> = (0..11).map(|_| "notes.md".to_string()).collect();
        assert!(read(&many, &d).unwrap_err().to_string().contains("at most 10"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn inlined_files_keep_their_fences_and_fit_or_dont() {
        let files = vec![file("a.md", "```rust\nfn x() {}\n```\n"), file("b.txt", "plain")];
        let out = inline("see these", &files, "bob/api", 64 * 1024).unwrap();
        assert!(out.starts_with("see these\n\n(bob/api runs an older Reason Network"), "{out}");
        assert!(out.contains("a.md:\n````\n```rust\nfn x() {}\n```\n````"), "{out}");
        assert!(out.ends_with("b.txt:\n```\nplain\n```"), "{out}");
        assert_eq!(inline("x", &[file("a", &"y".repeat(100))], "bob/api", 100), None);
    }
}
