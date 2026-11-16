//! SFZ text → a list of headers with their opcodes, in file order.
//!
//! Two passes: the preprocessor expands `#include` and `#define` (in order: a
//! `$NAME` means whatever it was last defined as at that point, and Salamander
//! redefines `$VEL` per velocity layer between includes) and strips comments; the
//! tokenizer then splits the text into `<header>`s and `name=value` opcodes. A
//! value runs until the next opcode, header or end of line, so it can contain
//! spaces (`sample=Grand Piano C4.wav`).

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

/// One header and the opcodes after it, up to the next header.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// Lower-case, without the brackets: `region`, `group`, `control`…
    pub header: String,
    pub opcodes: Vec<(String, String)>,
}

/// Includes nested deeper than this are taken to be a cycle.
const MAX_INCLUDE_DEPTH: usize = 32;

/// Parses an SFZ file. `#include` paths are relative to `root` (normally the
/// file's own folder).
pub fn parse_file(path: &Path, root: &Path) -> Result<Vec<Section>> {
    let text = read_text(path)?;
    parse_str(&text, Some(root))
}

/// Parses SFZ text. Without a `root`, an `#include` is an error.
pub fn parse_str(text: &str, root: Option<&Path>) -> Result<Vec<Section>> {
    let mut pre = Preprocessor { root, defines: HashMap::new(), out: String::new() };
    pre.expand(text, 0)?;
    Ok(tokenize(&pre.out))
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    // SFZ files in the wild aren't always UTF-8 (Latin-1 comments, labels).
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Preprocessor<'a> {
    root: Option<&'a Path>,
    defines: HashMap<String, String>,
    out: String,
}

impl Preprocessor<'_> {
    fn expand(&mut self, text: &str, depth: usize) -> Result<()> {
        let text = strip_block_comments(text);
        for line in text.lines() {
            let line = line.find("//").map_or(line, |i| &line[..i]);
            let mut rest = line;
            while let Some(i) = rest.find('#') {
                let after = &rest[i + 1..];
                if let Some(args) = directive(after, "define") {
                    self.emit(&rest[..i]);
                    let (name, args) = next_token(args);
                    let (value, args) = next_token(args);
                    if !name.starts_with('$') || name.len() < 2 {
                        bail!("#define needs a $NAME, got {name:?}");
                    }
                    self.defines.insert(name.to_string(), value.to_string());
                    rest = args;
                } else if let Some(args) = directive(after, "include") {
                    self.emit(&rest[..i]);
                    let args = args.trim_start();
                    let path = args
                        .strip_prefix('"')
                        .and_then(|a| a.find('"').map(|end| &a[..end]))
                        .context("#include needs a quoted path")?;
                    rest = &args[path.len() + 2..];
                    self.include(&self.substitute(path), depth)?;
                } else {
                    // A '#' in a value (`sample=D#1.wav`) is just text.
                    self.emit(&rest[..=i]);
                    rest = after;
                }
            }
            self.emit(rest);
            self.out.push('\n');
        }
        Ok(())
    }

    fn include(&mut self, path: &str, depth: usize) -> Result<()> {
        if depth >= MAX_INCLUDE_DEPTH {
            bail!("#include nested more than {MAX_INCLUDE_DEPTH} deep (a cycle?)");
        }
        let root = self.root.context("#include without a file to resolve it against")?;
        // Relative to the root file's folder, also from included files (that's how
        // sfizz resolves them, and Salamander relies on it).
        let full = root.join(path.replace('\\', "/"));
        let text = read_text(&full)?;
        self.expand(&text, depth + 1)
            .with_context(|| format!("in {}", full.display()))
    }

    /// Appends text with `$NAME`s replaced.
    fn emit(&mut self, text: &str) {
        let text = self.substitute(text);
        self.out.push_str(&text);
    }

    fn substitute(&self, text: &str) -> String {
        if !text.contains('$') {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let name_len = rest[i + 1..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len() - i - 1);
            let name = &rest[i..i + 1 + name_len];
            match self.defines.get(name) {
                Some(value) => out.push_str(value),
                None => out.push_str(name),
            }
            rest = &rest[i + 1 + name_len..];
        }
        out.push_str(rest);
        out
    }
}

/// `text` after a `#`: the arguments if it's the directive `name`.
fn directive<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    let args = text.strip_prefix(name)?;
    args.starts_with(|c: char| c.is_whitespace() || c == '"').then_some(args)
}

/// The next whitespace-separated token and what follows it.
fn next_token(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    (&text[..end], &text[end..])
}

fn strip_block_comments(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("/*") {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => {
                // Keep line breaks, so line comments and values stay on their lines.
                let comment = &rest[start..start + 2 + end + 2];
                out.extend(comment.chars().filter(|&c| c == '\n'));
                rest = &rest[start + 2 + end + 2..];
            }
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.into()
}

fn tokenize(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines() {
        // The opcode whose value is being read: its name and where its value starts.
        let mut open: Option<(String, usize)> = None;
        let bytes = line.as_bytes();
        let mut i = 0;
        let finish = |open: &mut Option<(String, usize)>, end: usize, sections: &mut Vec<Section>| {
            if let Some((name, start)) = open.take() {
                let value = line[start..end].trim().to_string();
                match sections.last_mut() {
                    Some(section) => section.opcodes.push((name, value)),
                    None => log::warn!("sfz: opcode {name} before any header, ignored"),
                }
            }
        };
        while i < bytes.len() {
            if bytes[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            let token_start = i;
            if bytes[i] == b'<'
                && let Some(len) = line[i..].find('>')
            {
                finish(&mut open, i, &mut sections);
                let header = line[i + 1..i + len].trim().to_ascii_lowercase();
                sections.push(Section { header, opcodes: Vec::new() });
                i += len + 1;
                continue;
            }
            // An opcode starts with an identifier and '=' at a token boundary ('$':
            // an undefined variable, kept so the opcode is reported as unknown).
            let ident_len = line[i..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(line.len() - i);
            if ident_len > 0 && bytes.get(i + ident_len) == Some(&b'=') {
                finish(&mut open, token_start, &mut sections);
                open = Some((line[i..i + ident_len].to_string(), i + ident_len + 1));
                i += ident_len + 1;
                continue;
            }
            // Part of a value (or stray text): skip to the next whitespace or header.
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && (i == token_start || bytes[i] != b'<') {
                i += 1;
            }
        }
        finish(&mut open, line.len(), &mut sections);
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(section: &Section) -> Vec<(&str, &str)> {
        section.opcodes.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect()
    }

    #[test]
    fn values_with_spaces_and_several_opcodes_per_line() {
        let s = parse_str("<region> sample=Grand Piano C4.wav lokey=60 hikey=62 // comment\n<group>key=5", None).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].header, "region");
        assert_eq!(ops(&s[0]), [("sample", "Grand Piano C4.wav"), ("lokey", "60"), ("hikey", "62")]);
        assert_eq!(ops(&s[1]), [("key", "5")]);
    }

    #[test]
    fn values_end_at_line_end_and_headers() {
        let s = parse_str("<region>sample=a.wav\nkey=1<region>sample=b c.wav<group>", None).unwrap();
        assert_eq!(ops(&s[0]), [("sample", "a.wav"), ("key", "1")]);
        assert_eq!(ops(&s[1]), [("sample", "b c.wav")]);
        assert_eq!(s[2].header, "group");
    }

    #[test]
    fn defines_apply_in_order_and_hash_in_values_is_text() {
        let text = "#define $VEL v1\n#define $EXT flac\n<region> sample=D#1$VEL.$EXT\n#define $VEL v2\n<region> sample=D#1$VEL.$EXT locc$UNDEFINED=1";
        let s = parse_str(text, None).unwrap();
        assert_eq!(ops(&s[0]), [("sample", "D#1v1.flac")]);
        assert_eq!(ops(&s[1]), [("sample", "D#1v2.flac"), ("locc$UNDEFINED", "1")]);
    }

    #[test]
    fn block_comments_keep_lines() {
        let s = parse_str("<region> key=1 /* a\nb */ lovel=2\nhivel=3", None).unwrap();
        assert_eq!(ops(&s[0]), [("key", "1"), ("lovel", "2"), ("hivel", "3")]);
    }

    #[test]
    fn inline_includes_relative_to_the_root() {
        let dir = std::env::temp_dir().join(format!("live-keys-sfz-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        std::fs::write(dir.join("Data/vel.txt"), "#define $VEL v7\n").unwrap();
        std::fs::write(dir.join("Data/region.txt"), "<region> sample=A0$VEL.flac\n").unwrap();
        std::fs::write(dir.join("Data/notes.txt"), "<group> #include \"Data/vel.txt\" lovel=1 #include \"Data/region.txt\"\n").unwrap();
        std::fs::write(dir.join("main.sfz"), "<master>\n#include \"Data/notes.txt\"\n").unwrap();
        let s = parse_file(&dir.join("main.sfz"), &dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let headers: Vec<_> = s.iter().map(|s| s.header.as_str()).collect();
        assert_eq!(headers, ["master", "group", "region"]);
        assert_eq!(ops(&s[1]), [("lovel", "1")]);
        assert_eq!(ops(&s[2]), [("sample", "A0v7.flac")]);
    }
}
