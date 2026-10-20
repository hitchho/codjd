//! Tokens of the set language, line by line (see `docs/set-language.md`, "Lexical rules").
//!
//! Columns are 1-based **byte** columns, which is what vim's quickfix expects.

/// An error with a position; printed as `file:line:col: message`
#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    /// 1-based
    pub line: usize,
    /// 1-based, in bytes
    pub col: usize,
    pub msg: String,
}

impl Diag {
    pub fn new(line: usize, col: usize, msg: impl Into<String>) -> Self {
        Self { line, col, msg: msg.into() }
    }

    pub fn format(&self, file: &str) -> String {
        format!("{file}:{}:{}: {}", self.line, self.col, self.msg)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    /// a number with the unit written right after it (`dB`, `s`, ...; empty if none)
    Num { value: f64, unit: String },
    Str(String),
    /// `->`
    Arrow,
    /// `..`
    DotDot,
    Dot,
    Semi,
    Colon,
    Eq,
    At,
    Slash,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    /// byte offsets into the line, `start..end`
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub struct Line<'a> {
    /// 1-based
    pub no: usize,
    pub text: &'a str,
    pub toks: Vec<Token>,
    /// the line had a lexical error (already reported); the parser skips it
    pub bad: bool,
}

pub fn lex(src: &str) -> (Vec<Line<'_>>, Vec<Diag>) {
    let mut diags = Vec::new();
    let mut lines = Vec::new();
    for (i, text) in src.lines().enumerate() {
        let before = diags.len();
        let toks = lex_line(i + 1, text, &mut diags);
        if toks.is_empty() && diags.len() == before {
            continue; // blank or comment only
        }
        lines.push(Line { no: i + 1, text, toks, bad: diags.len() > before });
    }
    (lines, diags)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn lex_line(no: usize, text: &str, diags: &mut Vec<Diag>) -> Vec<Token> {
    let b = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let digit_at = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    while i < b.len() {
        let c = b[i];
        let start = i;
        let simple = |tok: Tok, len: usize| Token { tok, start, end: start + len };
        match c {
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => break,
            b'"' => {
                let mut out = String::new();
                let mut j = i + 1;
                let mut closed = false;
                while j < b.len() {
                    let ch = text[j..].chars().next().unwrap();
                    match ch {
                        '"' => {
                            closed = true;
                            j += 1;
                            break;
                        }
                        '\\' => match b.get(j + 1) {
                            Some(b'"') => {
                                out.push('"');
                                j += 2;
                            }
                            Some(b'\\') => {
                                out.push('\\');
                                j += 2;
                            }
                            _ => {
                                let shown = text[j + 1..].chars().next().map(String::from).unwrap_or_default();
                                diags.push(Diag::new(
                                    no,
                                    j + 1,
                                    format!("unknown escape `\\{shown}` (only `\\\"` and `\\\\` exist)"),
                                ));
                                j += 1 + shown.len().max(1);
                            }
                        },
                        _ => {
                            out.push(ch);
                            j += ch.len_utf8();
                        }
                    }
                }
                if !closed {
                    diags.push(Diag::new(no, start + 1, "unterminated string"));
                }
                toks.push(Token { tok: Tok::Str(out), start, end: j });
                i = j;
            }
            b'-' if b.get(i + 1) == Some(&b'>') => {
                toks.push(simple(Tok::Arrow, 2));
                i += 2;
            }
            b'.' if b.get(i + 1) == Some(&b'.') => {
                toks.push(simple(Tok::DotDot, 2));
                i += 2;
            }
            b'.' => {
                toks.push(simple(Tok::Dot, 1));
                i += 1;
            }
            b';' => {
                toks.push(simple(Tok::Semi, 1));
                i += 1;
            }
            b':' => {
                toks.push(simple(Tok::Colon, 1));
                i += 1;
            }
            b'=' => {
                toks.push(simple(Tok::Eq, 1));
                i += 1;
            }
            b'@' => {
                toks.push(simple(Tok::At, 1));
                i += 1;
            }
            b'/' => {
                toks.push(simple(Tok::Slash, 1));
                i += 1;
            }
            _ if c.is_ascii_digit() || ((c == b'-' || c == b'+') && digit_at(i + 1)) => {
                let mut j = i + 1;
                while digit_at(j) {
                    j += 1;
                }
                // a fraction only when a digit follows the dot (`0..16` is a range)
                if b.get(j) == Some(&b'.') && digit_at(j + 1) {
                    j += 1;
                    while digit_at(j) {
                        j += 1;
                    }
                }
                let value: f64 = text[i..j].parse().unwrap_or(0.);
                let u0 = j;
                while b.get(j).is_some_and(u8::is_ascii_alphabetic) {
                    j += 1;
                }
                toks.push(Token {
                    tok: Tok::Num { value, unit: text[u0..j].to_string() },
                    start,
                    end: j,
                });
                i = j;
            }
            _ if is_ident_start(c) => {
                let mut j = i + 1;
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric()
                        || b[j] == b'_'
                        || (b[j] == b'-' && b.get(j + 1) != Some(&b'>')))
                {
                    j += 1;
                }
                // `x1.5` (a tempo scale in a grid line) would otherwise lex as `x1`, `.`, `5`
                let word = &text[i..j];
                if word.len() > 1
                    && word.starts_with('x')
                    && word[1..].bytes().all(|d| d.is_ascii_digit())
                    && b.get(j) == Some(&b'.')
                    && digit_at(j + 1)
                {
                    j += 1;
                    while digit_at(j) {
                        j += 1;
                    }
                }
                toks.push(Token { tok: Tok::Ident(text[i..j].to_string()), start, end: j });
                i = j;
            }
            _ => {
                let ch = text[i..].chars().next().unwrap();
                diags.push(Diag::new(no, i + 1, format!("unexpected character `{ch}`")));
                i += ch.len_utf8();
            }
        }
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        let (lines, d) = lex(s);
        assert!(d.is_empty(), "{d:?}");
        lines.into_iter().flat_map(|l| l.toks).map(|t| t.tok).collect()
    }

    fn id(s: &str) -> Tok {
        Tok::Ident(s.into())
    }
    fn num(v: f64) -> Tok {
        Tok::Num { value: v, unit: String::new() }
    }

    #[test]
    fn ranges_arrows_and_negatives() {
        assert_eq!(toks("bars 0..16"), [id("bars"), num(0.), Tok::DotDot, num(16.)]);
        assert_eq!(toks("a.b _->-0.6"), [id("a"), Tok::Dot, id("b"), id("_"), Tok::Arrow, num(-0.6)]);
        assert_eq!(toks("aft->smk"), [id("aft"), Tok::Arrow, id("smk")]);
        assert_eq!(toks("1->0"), [num(1.), Tok::Arrow, num(0.)]);
        assert_eq!(toks("my-id x"), [id("my-id"), id("x")]);
    }

    #[test]
    fn units_and_scales() {
        assert_eq!(
            toks("-1.5dB 0.012s"),
            [
                Tok::Num { value: -1.5, unit: "dB".into() },
                Tok::Num { value: 0.012, unit: "s".into() }
            ]
        );
        assert_eq!(toks("grid x1.5"), [id("grid"), id("x1.5")]);
        assert_eq!(toks("grid /1.5"), [id("grid"), Tok::Slash, num(1.5)]);
        // a track called x1 still has cues
        assert_eq!(toks("x1.low"), [id("x1"), Tok::Dot, id("low")]);
    }

    #[test]
    fn strings_comments_and_escapes() {
        assert_eq!(
            toks(r#"track a "Pneu \"x\" \\ # not a comment" # real comment"#),
            [id("track"), id("a"), Tok::Str(r#"Pneu "x" \ # not a comment"#.into())]
        );
        assert_eq!(toks("# only a comment\n\n   \n"), []);
    }

    #[test]
    fn lexical_errors_have_positions() {
        let (lines, d) = lex("ok\ntrack a \"abc\nx $ y\n\"a\\nb\"");
        assert_eq!(d.len(), 3);
        assert_eq!((d[0].line, d[0].col, d[0].msg.as_str()), (2, 9, "unterminated string"));
        assert_eq!((d[1].line, d[1].col), (3, 3));
        assert!(d[1].msg.contains("unexpected character `$`"));
        assert_eq!((d[2].line, d[2].col), (4, 3));
        assert!(d[2].msg.contains("unknown escape"));
        assert!(lines.iter().filter(|l| l.bad).count() == 3);
    }
}
