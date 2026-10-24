//! Text comparison for the take check: the words a take was supposed to say
//! against what Whisper heard. The result is a word alignment (for
//! highlighting) plus a lenient verdict. It never rejects a take; at most it
//! flags it for a human listen.
//!
//! Ported from the verified research probe (`textcheck.rs`) with its required
//! fixes: explicit spelling-variant rules instead of an edit-distance
//! near-match (which accepted dead/deed, than/then, were/where), annotation
//! brackets that only count when they close nearby (an unclosed `<` or `(`
//! used to swallow the rest of the text), and a rule that flags an audio tag
//! read aloud. English contractions ("don't" / "do not") are also accepted.

/// Language rules for normalisation and thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Cs,
    Other,
}

impl Lang {
    pub fn from_code(code: &str) -> Self {
        match code {
            "en" => Lang::En,
            "cs" => Lang::Cs,
            _ => Lang::Other,
        }
    }
}

/// Verdict thresholds. They are heuristics from the research and need
/// calibration on known-good takes; they are kept together so they can be
/// tuned in one place.
#[derive(Clone, Debug, PartialEq)]
pub struct Thresholds {
    /// Lines of at most this many words are short (dialogue tags): Whisper is
    /// unreliable on clips under a second, so only clear failures flag.
    pub short_words: usize,
    /// This many missing/extra/wrong words in a row flag the take.
    pub bad_run: usize,
    /// Lines with at least this many words use `long_wer`.
    pub long_words: usize,
    /// Lines shorter than `long_words` flag above this WER with at least
    /// `short_min_errors` errors.
    pub short_wer: f32,
    pub short_min_errors: usize,
    pub long_wer: f32,
    /// WER thresholds are multiplied by this outside English, where Whisper
    /// is much weaker.
    pub non_english_factor: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            short_words: 3,
            bad_run: 3,
            long_words: 15,
            short_wer: 0.20,
            short_min_errors: 3,
            long_wer: 0.12,
            non_english_factor: 1.6,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Word,
    /// A run of digits and/or number words collapsed into one token.
    Num {
        value: Option<u64>,
        has_digits: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tok {
    /// Normalised form: lowercase, no diacritics, no apostrophes.
    pub norm: String,
    pub kind: Kind,
    /// Byte range in the ORIGINAL string (for highlighting).
    pub span: (usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// expected[i] heard as hyp[j].
    Match(usize, usize),
    /// Equivalent but not identical (spelling variant, digits vs words, ...).
    /// Not an error.
    Near(usize, usize),
    /// expected[i] and expected[i+1] heard as one word hyp[j]
    /// ("every day" / "everyday", "do not" / "don't").
    MergeRef(usize, usize),
    /// expected[i] heard as hyp[j] hyp[j+1].
    MergeHyp(usize, usize),
    /// Wrong word.
    Sub(usize, usize),
    /// expected[i] missing from the audio. `filler` = the text's own
    /// um/oh/hmm, which Whisper often leaves out: not counted.
    Del { i: usize, filler: bool },
    /// Extra word hyp[j] in the audio. `filler` = um/uh/haha..., not counted.
    Ins { j: usize, filler: bool },
}

impl Op {
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Op::Sub(..) | Op::Del { filler: false, .. } | Op::Ins { filler: false, .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// Some differences, under the flag threshold. Shown quietly.
    Minor,
    /// Worth a human listen.
    Flag(&'static str),
}

#[derive(Clone, Debug)]
pub struct Check {
    pub expected: Vec<Tok>,
    pub heard: Vec<Tok>,
    pub ops: Vec<Op>,
    pub errors: usize,
    pub wer: f32,
    pub verdict: Verdict,
}

// ---------------------------------------------------------------- tokenising

fn fold_char(c: char, out: &mut String) {
    // Lowercase first, then strip common Latin diacritics (Czech complete).
    for l in c.to_lowercase() {
        let r = match l {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ą' => "a",
            'č' | 'ç' | 'ć' => "c",
            'ď' => "d",
            'é' | 'ě' | 'è' | 'ê' | 'ë' | 'ē' | 'ę' => "e",
            'í' | 'ì' | 'î' | 'ï' => "i",
            'ň' | 'ñ' | 'ń' => "n",
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ő' => "o",
            'ř' => "r",
            'š' | 'ś' => "s",
            'ť' => "t",
            'ú' | 'ů' | 'ù' | 'û' | 'ü' | 'ű' => "u",
            'ý' | 'ÿ' => "y",
            'ž' | 'ź' | 'ż' => "z",
            'ł' => "l",
            'ß' => "ss",
            'æ' => "ae",
            'œ' => "oe",
            _ => {
                out.push(l);
                continue;
            }
        };
        out.push_str(r);
    }
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '’' | '‘' | 'ʼ' | '`' | '´')
}

/// How far (in characters) a closing bracket may follow its opener for the
/// pair to count as an annotation. An opener without a nearby closer is
/// ordinary punctuation ("A is < B").
const MAX_ANNOTATION_CHARS: usize = 40;

/// Byte ranges of bracketed annotations: an opener whose closer follows
/// within [`MAX_ANNOTATION_CHARS`] characters. `accept` sees the text between
/// them.
fn bracket_ranges(
    text: &str,
    pairs: &[(char, char)],
    accept: impl Fn(char, &str) -> bool,
) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if let Some(&(_, close)) = pairs.iter().find(|(o, _)| *o == c)
            && let Some(k) = chars[i + 1..]
                .iter()
                .take(MAX_ANNOTATION_CHARS)
                .position(|&(_, x)| x == close)
        {
            let (end, close_c) = chars[i + 1 + k];
            let inner = &text[pos + c.len_utf8()..end];
            if accept(c, inner) {
                out.push((pos, end + close_c.len_utf8()));
                i += k + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Raw word scan. Bytes inside `skip` ranges are dropped (they separate
/// words). Hyphens, dashes, quotes and all other punctuation separate words.
fn scan(text: &str, skip: &[(usize, usize)]) -> Vec<(String, (usize, usize))> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0usize;
    let flush = |cur: &mut String, start: usize, end: usize, out: &mut Vec<_>| {
        if !cur.is_empty() {
            out.push((std::mem::take(cur), (start, end)));
        }
    };
    let skipped = |pos: usize| skip.iter().any(|&(a, b)| pos >= a && pos < b);
    for i in 0..chars.len() {
        let (pos, c) = chars[i];
        if skipped(pos) {
            flush(&mut cur, start, pos, &mut out);
            continue;
        }
        let prev = (i > 0).then(|| chars[i - 1].1);
        let next = chars.get(i + 1).map(|&(_, c)| c);
        if c.is_alphanumeric() {
            if cur.is_empty() {
                start = pos;
            }
            fold_char(c, &mut cur);
        } else if is_apostrophe(c) && !cur.is_empty() && next.is_some_and(|n| n.is_alphanumeric()) {
            // don't -> dont, O’Brien -> obrien: drop the apostrophe, keep the word
        } else if matches!(c, ',' | '.' | '\u{a0}' | '\u{202f}' | '\u{2009}')
            && prev.is_some_and(|p| p.is_ascii_digit())
            && next.is_some_and(|n| n.is_ascii_digit())
        {
            // 40,000 / 40.000 / 40 000 (nbsp) are thousands; 3.5 / 3,5 decimals.
            let group = chars[i + 1..]
                .iter()
                .take_while(|&&(_, c)| c.is_ascii_digit())
                .count();
            if c != '\u{a0}' && c != '\u{202f}' && c != '\u{2009}' && group != 3 {
                cur.push('.');
            }
        } else {
            flush(&mut cur, start, pos, &mut out);
        }
    }
    flush(&mut cur, start, text.len(), &mut out);
    out
}

// ---------------------------------------------------------------- numbers

fn en_small(w: &str) -> Option<u64> {
    Some(match w {
        "zero" => 0,
        "one" | "first" => 1,
        "two" | "second" => 2,
        "three" | "third" => 3,
        "four" | "fourth" => 4,
        "five" | "fifth" => 5,
        "six" | "sixth" => 6,
        "seven" | "seventh" => 7,
        "eight" | "eighth" => 8,
        "nine" | "ninth" => 9,
        "ten" | "tenth" => 10,
        "eleven" | "eleventh" => 11,
        "twelve" | "twelfth" => 12,
        "thirteen" | "thirteenth" => 13,
        "fourteen" | "fourteenth" => 14,
        "fifteen" | "fifteenth" => 15,
        "sixteen" | "sixteenth" => 16,
        "seventeen" | "seventeenth" => 17,
        "eighteen" | "eighteenth" => 18,
        "nineteen" | "nineteenth" => 19,
        "twenty" | "twentieth" => 20,
        "thirty" | "thirtieth" => 30,
        "forty" | "fortieth" => 40,
        "fifty" | "fiftieth" => 50,
        "sixty" | "sixtieth" => 60,
        "seventy" | "seventieth" => 70,
        "eighty" | "eightieth" => 80,
        "ninety" | "ninetieth" => 90,
        _ => return None,
    })
}

fn en_scale(w: &str) -> Option<u64> {
    Some(match w {
        "hundred" | "hundredth" => 100,
        "thousand" | "thousandth" => 1_000,
        "million" | "millionth" => 1_000_000,
        "billion" | "billionth" => 1_000_000_000,
        _ => return None,
    })
}

/// Folded Czech cardinal forms (incl. common case forms). Values are not parsed.
fn cs_number_word(w: &str) -> bool {
    const W: &[&str] = &[
        "nula",
        "jedna",
        "jeden",
        "jedno",
        "jednoho",
        "dva",
        "dve",
        "dvou",
        "dvema",
        "tri",
        "trech",
        "trem",
        "tremi",
        "ctyri",
        "ctyr",
        "ctyrech",
        "ctyrem",
        "ctyrmi",
        "pet",
        "peti",
        "sest",
        "sesti",
        "sedm",
        "sedmi",
        "osm",
        "osmi",
        "devet",
        "deviti",
        "deset",
        "deseti",
        "jedenact",
        "dvanact",
        "trinact",
        "ctrnact",
        "patnact",
        "sestnact",
        "sedmnact",
        "osmnact",
        "devatenact",
        "dvacet",
        "dvaceti",
        "tricet",
        "triceti",
        "ctyricet",
        "ctyriceti",
        "padesat",
        "padesati",
        "sedesat",
        "sedesati",
        "sedmdesat",
        "sedmdesati",
        "osmdesat",
        "osmdesati",
        "devadesat",
        "devadesati",
        "sto",
        "sta",
        "set",
        "stech",
        "tisic",
        "tisice",
        "tisici",
        "tisicu",
        "milion",
        "miliony",
        "milionu",
        "miliard",
        "miliardy",
        "miliardu",
    ];
    W.contains(&w)
}

/// `Some(value)` when `w` is written with digits (value `None` for decimals).
fn digit_value(w: &str, lang: Lang) -> Option<Option<u64>> {
    let core = if lang == Lang::En {
        // 21st 2nd 3rd 4th 1990s
        ["st", "nd", "rd", "th", "s"]
            .iter()
            .find_map(|s| {
                w.strip_suffix(s)
                    .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
            })
            .unwrap_or(w)
    } else {
        w
    };
    if core.is_empty() || !core.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if core.bytes().all(|b| b.is_ascii_digit()) {
        Some(core.parse().ok())
    } else if core.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        Some(None)
    } else {
        None
    }
}

fn is_num_word(w: &str, lang: Lang) -> bool {
    match lang {
        Lang::En => en_small(w).is_some() || en_scale(w).is_some(),
        Lang::Cs => cs_number_word(w),
        Lang::Other => false,
    }
}

/// Parse a run of English number words. `None` for year/time style
/// ("nineteen ninety", "seven thirty").
fn parse_en(words: &[&str]) -> Option<u64> {
    let (mut total, mut cur) = (0u64, 0u64);
    // 0 = empty group, 1 = tens only (can take a unit), 2 = full
    let mut group = 0u8;
    for &w in words {
        if w == "and" {
            continue;
        }
        if w == "a" {
            cur = 1;
            group = 2;
            continue;
        }
        if let Some(v) = en_small(w) {
            match group {
                0 => {
                    cur += v;
                    group = if v >= 20 && v % 10 == 0 { 1 } else { 2 };
                }
                1 if v > 0 && v < 10 => {
                    cur += v;
                    group = 2;
                }
                _ => return None,
            }
        } else {
            let s = en_scale(w)?;
            if s == 100 {
                cur = cur.max(1) * 100;
            } else {
                total += cur.max(1) * s;
                cur = 0;
            }
            group = 0;
        }
    }
    Some(total + cur)
}

fn tokens(text: &str, lang: Lang, skip: &[(usize, usize)]) -> Vec<Tok> {
    let raw = scan(text, skip);
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    let numeric = |k: usize| {
        let w = raw[k].0.as_str();
        digit_value(w, lang).is_some() || is_num_word(w, lang)
    };
    // "a"/"and" only inside English number runs: "a hundred", "one hundred and five".
    let joiner = |k: usize| {
        lang == Lang::En
            && k + 1 < raw.len()
            && ((raw[k].0 == "a" && en_scale(&raw[k + 1].0).is_some())
                || (raw[k].0 == "and"
                    && k > 0
                    && is_num_word(&raw[k - 1].0, lang)
                    && is_num_word(&raw[k + 1].0, lang)))
    };
    while i < raw.len() {
        if numeric(i) || joiner(i) {
            let s = i;
            while i < raw.len() && (numeric(i) || joiner(i)) {
                i += 1;
            }
            let run: Vec<&str> = raw[s..i].iter().map(|r| r.0.as_str()).collect();
            let has_digits = run.iter().any(|w| digit_value(w, lang).is_some());
            let value = if run.len() == 1 && has_digits {
                digit_value(run[0], lang).flatten()
            } else if !has_digits && lang == Lang::En {
                parse_en(&run)
            } else {
                None
            };
            out.push(Tok {
                norm: run.join(" "),
                kind: Kind::Num { value, has_digits },
                span: (raw[s].1.0, raw[i - 1].1.1),
            });
        } else {
            out.push(Tok {
                norm: raw[i].0.clone(),
                kind: Kind::Word,
                span: raw[i].1,
            });
            i += 1;
        }
    }
    out
}

/// What is not spoken in the text sent to TTS: ElevenLabs audio tags
/// (`[whispers]`, as `crate::tags` defines them) and SSML-style
/// `<break time="1s" />`.
fn unspoken_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut r = crate::tags::find(text);
    r.extend(bracket_ranges(text, &[('<', '>')], |_, inner| {
        inner.trim_start().to_ascii_lowercase().starts_with("break")
    }));
    r
}

/// Tokens of the expected (spoken) text; tags and breaks are dropped.
pub fn expected_tokens(text: &str, lang: Lang) -> Vec<Tok> {
    tokens(text, lang, &unspoken_ranges(text))
}

/// Tokens of a Whisper transcript. Annotations `[laughs]`, `(sighs)`,
/// `*laughs*` and `♪ … ♪` are dropped when they close nearby.
pub fn heard_tokens(text: &str, lang: Lang) -> Vec<Tok> {
    let skip = bracket_ranges(
        text,
        &[('[', ']'), ('(', ')'), ('*', '*'), ('♪', '♪')],
        |_, _| true,
    );
    tokens(text, lang, &skip)
}

/// The words of each audio tag in a performance (`[long pause]` →
/// `["long", "pause"]`), for spotting a tag that was read aloud.
pub fn tag_phrases(performance: &str, lang: Lang) -> Vec<Vec<String>> {
    crate::tags::find(performance)
        .into_iter()
        .map(|(a, b)| {
            tokens(&performance[a + 1..b - 1], lang, &[])
                .into_iter()
                .map(|t| t.norm)
                .collect::<Vec<_>>()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// Normalised words of `text` joined by single spaces (for phrase lookups).
pub fn normalized(text: &str, lang: Lang) -> String {
    heard_tokens(text, lang)
        .into_iter()
        .map(|t| t.norm)
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- alignment

pub fn is_filler(w: &str) -> bool {
    matches!(
        w,
        "uh" | "um"
            | "umm"
            | "uhm"
            | "erm"
            | "hm"
            | "hmm"
            | "mm"
            | "mmm"
            | "mhm"
            | "ah"
            | "ahh"
            | "aah"
            | "ha"
            | "haha"
            | "hahaha"
            | "heh"
            | "hehe"
            | "eh"
            | "ehm"
            | "oh"
            | "ooh"
    )
}

/// Real spelling variants of one word (British/American suffixes and a small
/// pair list). Deliberately not an edit distance: dead/deed, than/then,
/// were/where and quiet/quite must stay errors.
pub fn spelling_variant(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    const PAIRS: &[(&str, &str)] = &[
        ("grey", "gray"),
        ("ok", "okay"),
        ("mr", "mister"),
        ("mrs", "missus"),
        ("dr", "doctor"),
        ("st", "saint"),
        ("till", "til"),
        ("towards", "toward"),
        ("whilst", "while"),
        ("alright", "allright"),
        ("mum", "mom"),
        ("programme", "program"),
        ("cheque", "check"),
        ("tyre", "tire"),
        ("plough", "plow"),
        ("aluminium", "aluminum"),
        ("cant", "cannot"),
    ];
    if PAIRS
        .iter()
        .any(|&(x, y)| (a == x && b == y) || (a == y && b == x))
    {
        return true;
    }
    const RULES: &[(&str, &str)] = &[
        ("our", "or"),
        ("ours", "ors"),
        ("oured", "ored"),
        ("ouring", "oring"),
        ("ise", "ize"),
        ("ised", "ized"),
        ("ising", "izing"),
        ("isation", "ization"),
        ("yse", "yze"),
        ("ysed", "yzed"),
        ("tre", "ter"),
        ("tres", "ters"),
        ("lled", "led"),
        ("lling", "ling"),
        ("ller", "ler"),
        ("ence", "ense"),
        ("logue", "log"),
        ("ogue", "og"),
    ];
    for &(x, y) in RULES {
        for (p, q) in [(a, b), (b, a)] {
            if let (Some(sp), Some(sq)) = (p.strip_suffix(x), q.strip_suffix(y))
                && sp == sq
                && sp.len() >= 2
            {
                return true;
            }
        }
    }
    a.len() >= 5 && a.replace("our", "or") == b.replace("our", "or")
}

/// English contractions as two words (apostrophes are already dropped).
fn contraction(w: &str) -> Option<(&'static str, &'static str)> {
    Some(match w {
        "dont" => ("do", "not"),
        "doesnt" => ("does", "not"),
        "didnt" => ("did", "not"),
        "cant" | "cannot" => ("can", "not"),
        "wont" => ("will", "not"),
        "isnt" => ("is", "not"),
        "arent" => ("are", "not"),
        "wasnt" => ("was", "not"),
        "werent" => ("were", "not"),
        "hasnt" => ("has", "not"),
        "havent" => ("have", "not"),
        "hadnt" => ("had", "not"),
        "couldnt" => ("could", "not"),
        "wouldnt" => ("would", "not"),
        "shouldnt" => ("should", "not"),
        "mustnt" => ("must", "not"),
        "neednt" => ("need", "not"),
        "im" => ("i", "am"),
        "youre" => ("you", "are"),
        "theyre" => ("they", "are"),
        "ive" => ("i", "have"),
        "youve" => ("you", "have"),
        "weve" => ("we", "have"),
        "theyve" => ("they", "have"),
        "youll" => ("you", "will"),
        "theyll" => ("they", "will"),
        "its" => ("it", "is"),
        "thats" => ("that", "is"),
        "theres" => ("there", "is"),
        "heres" => ("here", "is"),
        "whats" => ("what", "is"),
        "wheres" => ("where", "is"),
        "whos" => ("who", "is"),
        "hes" => ("he", "is"),
        "shes" => ("she", "is"),
        "lets" => ("let", "us"),
        _ => return None,
    })
}

/// Contractions with more than one expansion, or whose apostrophe-less form
/// is also a word ("we'll" / "well"). Only ever used to accept the exact
/// two-word expansion on the other side, so "well" vs "we will" is the only
/// thing they can excuse.
fn contraction_alt(w: &str) -> &'static [(&'static str, &'static str)] {
    match w {
        "ill" => &[("i", "will")],
        "well" => &[("we", "will")],
        "hell" => &[("he", "will")],
        "shell" => &[("she", "will")],
        "itll" => &[("it", "will")],
        "thatll" => &[("that", "will")],
        "id" => &[("i", "would"), ("i", "had")],
        "youd" => &[("you", "would"), ("you", "had")],
        "hed" => &[("he", "would"), ("he", "had")],
        "shed" => &[("she", "would"), ("she", "had")],
        "wed" => &[("we", "would"), ("we", "had")],
        "theyd" => &[("they", "would"), ("they", "had")],
        "itd" => &[("it", "would"), ("it", "had")],
        "hes" => &[("he", "has")],
        "shes" => &[("she", "has")],
        "its" => &[("it", "has")],
        "couldve" => &[("could", "have")],
        "wouldve" => &[("would", "have")],
        "shouldve" => &[("should", "have")],
        "mightve" => &[("might", "have")],
        "mustve" => &[("must", "have")],
        _ => &[],
    }
}

/// `one` said as the two words `a b`: "everyday" / "every day", and in
/// English "don't" / "do not".
fn joins(one: &Tok, a: &Tok, b: &Tok, lang: Lang) -> bool {
    if one.kind != Kind::Word || a.kind != Kind::Word || b.kind != Kind::Word {
        return false;
    }
    let (o, a, b) = (one.norm.as_str(), a.norm.as_str(), b.norm.as_str());
    (o.len() == a.len() + b.len() && o.starts_with(a) && o.ends_with(b))
        || (lang == Lang::En
            && (contraction(o) == Some((a, b)) || contraction_alt(o).contains(&(a, b))))
}

#[derive(PartialEq)]
enum Eq3 {
    Exact,
    Near,
    No,
}

fn compare(a: &Tok, b: &Tok, lang: Lang) -> Eq3 {
    if a.norm == b.norm {
        return Eq3::Exact;
    }
    match (&a.kind, &b.kind) {
        (Kind::Word, Kind::Word) => {
            if spelling_variant(&a.norm, &b.norm) {
                Eq3::Near
            } else {
                Eq3::No
            }
        }
        (
            Kind::Num {
                value: va,
                has_digits: da,
            },
            Kind::Num {
                value: vb,
                has_digits: db,
            },
        ) => match (va, vb) {
            (Some(x), Some(y)) if x == y => Eq3::Exact,
            (Some(_), Some(_)) => Eq3::No,
            // Digits on one side, unparsed words on the other: can't tell.
            _ if da != db => Eq3::Near,
            _ => Eq3::No,
        },
        // Outside English number words are not recognised, so digits against
        // a word ("25" / "fünfundzwanzig", "1." / "první") cannot be judged.
        (
            Kind::Num {
                has_digits: true, ..
            },
            Kind::Word,
        )
        | (
            Kind::Word,
            Kind::Num {
                has_digits: true, ..
            },
        ) if lang != Lang::En => Eq3::Near,
        _ => Eq3::No,
    }
}

const C_NEAR: u32 = 1;
const C_MERGE: u32 = 1;
const C_FILLER: u32 = 1;
const C_ERR: u32 = 100;
/// A wrong word costs a hair more than a missing or extra one, so that of
/// two alignments with the same number of errors the one that keeps more
/// exact matches wins ("the old door" / "the door now": "old" missing and
/// "now" extra, not two wrong words).
const C_SUB: u32 = C_ERR + 1;

/// Token-level alignment (Levenshtein with near matches, merges and cheap
/// fillers).
pub fn align(exp: &[Tok], hyp: &[Tok], lang: Lang) -> Vec<Op> {
    let (n, m) = (exp.len(), hyp.len());
    let w = m + 1;
    let mut d = vec![u32::MAX / 2; (n + 1) * w];
    // back-pointer: 0 diag, 1 del, 2 ins, 3 merge-ref, 4 merge-hyp
    let mut bp = vec![0u8; (n + 1) * w];
    let ins_cost = |j: usize| {
        if is_filler(&hyp[j].norm) {
            C_FILLER
        } else {
            C_ERR
        }
    };
    let del_cost = |i: usize| {
        if is_filler(&exp[i].norm) {
            C_FILLER
        } else {
            C_ERR
        }
    };
    d[0] = 0;
    for i in 0..=n {
        for j in 0..=m {
            if i == 0 && j == 0 {
                continue;
            }
            // (cost, tie priority, back-pointer). On equal cost an extra or
            // missing word is preferred over a match, so the backtrace (from
            // the end) attributes a repeated phrase to its second copy.
            let mut best = (u32::MAX / 2, u8::MAX, 0u8);
            let mut consider = |cost: u32, code: u8| {
                let prio = match code {
                    2 => 0,
                    1 => 1,
                    0 => 2,
                    _ => 3,
                };
                if (cost, prio) < (best.0, best.1) {
                    best = (cost, prio, code);
                }
            };
            if i > 0 && j > 0 {
                let c = match compare(&exp[i - 1], &hyp[j - 1], lang) {
                    Eq3::Exact => 0,
                    Eq3::Near => C_NEAR,
                    Eq3::No => C_SUB,
                };
                consider(d[(i - 1) * w + j - 1] + c, 0);
            }
            if i > 0 {
                consider(d[(i - 1) * w + j] + del_cost(i - 1), 1);
            }
            if j > 0 {
                consider(d[i * w + j - 1] + ins_cost(j - 1), 2);
            }
            if i > 1 && j > 0 && joins(&hyp[j - 1], &exp[i - 2], &exp[i - 1], lang) {
                consider(d[(i - 2) * w + j - 1] + C_MERGE, 3);
            }
            if i > 0 && j > 1 && joins(&exp[i - 1], &hyp[j - 2], &hyp[j - 1], lang) {
                consider(d[(i - 1) * w + j - 2] + C_MERGE, 4);
            }
            d[i * w + j] = best.0;
            bp[i * w + j] = best.2;
        }
    }
    let (mut i, mut j) = (n, m);
    let mut ops = Vec::new();
    while i > 0 || j > 0 {
        match bp[i * w + j] {
            0 if i > 0 && j > 0 => {
                ops.push(match compare(&exp[i - 1], &hyp[j - 1], lang) {
                    Eq3::Exact => Op::Match(i - 1, j - 1),
                    Eq3::Near => Op::Near(i - 1, j - 1),
                    Eq3::No => Op::Sub(i - 1, j - 1),
                });
                i -= 1;
                j -= 1;
            }
            1 if i > 0 => {
                ops.push(Op::Del {
                    i: i - 1,
                    filler: is_filler(&exp[i - 1].norm),
                });
                i -= 1;
            }
            3 => {
                ops.push(Op::MergeRef(i - 2, j - 1));
                i -= 2;
                j -= 1;
            }
            4 => {
                ops.push(Op::MergeHyp(i - 1, j - 2));
                i -= 1;
                j -= 2;
            }
            _ => {
                // 2, or the first row/column
                if j > 0 && (i == 0 || bp[i * w + j] == 2) {
                    ops.push(Op::Ins {
                        j: j - 1,
                        filler: is_filler(&hyp[j - 1].norm),
                    });
                    j -= 1;
                } else {
                    ops.push(Op::Del {
                        i: i - 1,
                        filler: is_filler(&exp[i - 1].norm),
                    });
                    i -= 1;
                }
            }
        }
    }
    ops.reverse();
    ops
}

/// A tag's words heard as extra words, in order: the tag was read aloud.
fn spoken_tag(heard: &[Tok], ops: &[Op], tags: &[Vec<String>]) -> bool {
    let inserted: Vec<bool> = {
        let mut v = vec![false; heard.len()];
        for op in ops {
            if let Op::Ins { j, filler: false } = *op {
                v[j] = true;
            }
        }
        v
    };
    tags.iter().any(|phrase| {
        let k = phrase.len();
        k > 0
            && heard.len() >= k
            && (0..=heard.len() - k)
                .any(|j| (0..k).all(|x| inserted[j + x] && heard[j + x].norm == phrase[x]))
    })
}

/// Compare the expected text with the transcript. `tags` are the audio tags'
/// words (see [`tag_phrases`]); hearing one of them as extra words flags the
/// take.
pub fn check(
    expected_text: &str,
    heard_text: &str,
    lang: Lang,
    tags: &[Vec<String>],
    t: &Thresholds,
) -> Check {
    let expected = expected_tokens(expected_text, lang);
    let heard = heard_tokens(heard_text, lang);
    let ops = align(&expected, &heard, lang);
    let errors = ops.iter().filter(|o| o.is_error()).count();
    let n = expected.len();
    let wer = errors as f32 / n.max(1) as f32;
    let (mut run, mut max_bad_run) = (0, 0);
    for op in &ops {
        match op {
            Op::Ins { filler: true, .. } => {}
            o if o.is_error() => {
                run += 1;
                max_bad_run = max_bad_run.max(run);
            }
            _ => run = 0,
        }
    }
    let matched = ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                Op::Match(..) | Op::Near(..) | Op::MergeRef(..) | Op::MergeHyp(..)
            )
        })
        .count();
    let real_heard = heard.iter().filter(|t| !is_filler(&t.norm)).count();
    // The text's own words; interjections ("Hmm.", "Oh!") are fillers.
    let real_expected = expected.iter().filter(|t| !is_filler(&t.norm)).count();
    let f = if lang == Lang::En {
        1.0
    } else {
        t.non_english_factor
    };

    let verdict = if real_expected == 0 {
        // No text, or only interjections ("Hmm.", "Ha ha!"), which Whisper
        // often leaves out; a silent take is the length check's job.
        if real_heard == 0 || errors == 0 {
            Verdict::Match
        } else {
            Verdict::Minor
        }
    } else if real_heard == 0 {
        Verdict::Flag("nothing recognisable was heard")
    } else if spoken_tag(&heard, &ops, tags) {
        Verdict::Flag("an audio tag was read aloud")
    } else if n <= t.short_words {
        // Dialogue tags: Whisper is unreliable on clips under a second.
        if matched == 0 || errors >= 2 {
            Verdict::Flag("the short line sounds different")
        } else if errors > 0 {
            Verdict::Minor
        } else {
            Verdict::Match
        }
    } else if max_bad_run >= t.bad_run {
        // A dropped, added or garbled phrase: clean TTS audio rarely gives
        // three recognition errors in a row.
        Verdict::Flag("words are missing, added or wrong in a row")
    } else if (n < t.long_words && errors >= t.short_min_errors && wer > t.short_wer * f)
        || (n >= t.long_words && wer > t.long_wer * f)
    {
        Verdict::Flag("many words differ from the text")
    } else if errors > 0 {
        Verdict::Minor
    } else {
        Verdict::Match
    };
    Check {
        expected,
        heard,
        ops,
        errors,
        wer,
        verdict,
    }
}

/// Debug rendering: `the [-quick-]{+quack+} brown {+fox+}`.
#[cfg(test)]
pub fn render(c: &Check, expected_text: &str) -> String {
    let mut s = String::new();
    for op in &c.ops {
        let e = |i: usize| &expected_text[c.expected[i].span.0..c.expected[i].span.1];
        let h = |j: usize| c.heard[j].norm.as_str();
        match *op {
            Op::Match(i, _) => s += e(i),
            Op::Near(i, j) => s += &format!("{}~{}", e(i), h(j)),
            Op::MergeRef(i, j) => s += &format!("{} {}~{}", e(i), e(i + 1), h(j)),
            Op::MergeHyp(i, j) => s += &format!("{}~{} {}", e(i), h(j), h(j + 1)),
            Op::Sub(i, j) => s += &format!("[-{}-]{{+{}+}}", e(i), h(j)),
            Op::Del { i, filler: false } => s += &format!("[-{}-]", e(i)),
            Op::Del { i, filler: true } => s += &format!("(-{}-)", e(i)),
            Op::Ins { j, filler: false } => s += &format!("{{+{}+}}", h(j)),
            Op::Ins { j, filler: true } => s += &format!("({})", h(j)),
        }
        s.push(' ');
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norms(v: &[Tok]) -> Vec<&str> {
        v.iter().map(|t| t.norm.as_str()).collect()
    }

    fn chk(exp: &str, heard: &str, lang: Lang) -> Check {
        check(exp, heard, lang, &[], &Thresholds::default())
    }

    #[test]
    fn normalisation_folds_case_quotes_dashes_and_diacritics() {
        let t = expected_tokens(
            "“Don’t,” she said—quietly… [whispers] It’s well-known.",
            Lang::En,
        );
        assert_eq!(
            norms(&t),
            ["dont", "she", "said", "quietly", "its", "well", "known"]
        );
        // Spans point back into the original text.
        let src = "Řekl: „Přijď zítra.“";
        let t = expected_tokens(src, Lang::Cs);
        assert_eq!(norms(&t), ["rekl", "prijd", "zitra"]);
        assert_eq!(&src[t[1].span.0..t[1].span.1], "Přijď");
        let h = heard_tokens(
            " Ha ha. [laughs] (sighs) *coughs* So, that's it. ♪ la la ♪",
            Lang::En,
        );
        assert_eq!(norms(&h), ["ha", "ha", "so", "thats", "it"]);
    }

    #[test]
    fn unclosed_brackets_are_punctuation() {
        // An opener without a nearby closer must not swallow the rest.
        let e = expected_tokens("A is < B, he said, and C is less than D.", Lang::En);
        assert_eq!(e.len(), 11, "{:?}", norms(&e));
        let h = heard_tokens(" A stray ( or * keeps every word after it.", Lang::En);
        assert_eq!(
            norms(&h),
            ["a", "stray", "or", "keeps", "every", "word", "after", "it"]
        );
        // Whisper's own annotations are dropped from the transcript.
        let c = chk(
            "He wrote the date.",
            " He wrote (coughs) the date.",
            Lang::En,
        );
        assert_eq!(c.errors, 0);
        // `<break .../>` is unspoken; other angle brackets are text.
        let e = expected_tokens("Wait. <break time=\"1.5s\" /> Now.", Lang::En);
        assert_eq!(norms(&e), ["wait", "now"]);
        // Footnote-like brackets are not audio tags, so they are words.
        assert_eq!(
            norms(&expected_tokens("See [1] here", Lang::En)),
            ["see", "1", "here"]
        );
    }

    #[test]
    fn numbers_as_digits_or_words() {
        let e = expected_tokens(
            "It cost forty thousand dollars in nineteen ninety-nine.",
            Lang::En,
        );
        assert_eq!(
            e[2].kind,
            Kind::Num {
                value: Some(40_000),
                has_digits: false
            }
        );
        let h = heard_tokens(" It cost $40,000 in 1999.", Lang::En);
        assert_eq!(
            h[2].kind,
            Kind::Num {
                value: Some(40_000),
                has_digits: true
            }
        );
        let exp = "It cost forty thousand dollars in nineteen ninety-nine.";
        let c = chk(exp, " It cost $40,000 in 1999.", Lang::En);
        // "dollars" is missing ($ is dropped by normalisation): the only error.
        assert_eq!(c.errors, 1, "{}", render(&c, exp));
        assert_eq!(parse_en(&["one", "hundred", "and", "five"]), Some(105));
        assert_eq!(parse_en(&["a", "hundred"]), Some(100));
        assert_eq!(parse_en(&["twenty", "first"]), Some(21));
        assert_eq!(
            parse_en(&["two", "million", "three", "hundred", "thousand"]),
            Some(2_300_000)
        );
        assert_eq!(parse_en(&["seven", "thirty"]), None);
        // A wrong number is an error.
        assert_eq!(chk("He was fourteen.", " He was 40.", Lang::En).errors, 1);
        // Decimals and thousands separators.
        assert_eq!(
            norms(&heard_tokens("3.5 a 3,5 a 40\u{a0}000 a 1,250", Lang::Cs)),
            ["3.5", "a", "3.5", "a", "40000", "a", "1250"]
        );
        // Czech: values are not parsed, but digits vs words is not flagged.
        let exp = "Stálo to čtyřicet tisíc korun.";
        let c = chk(exp, " Stálo to 40 000 korun.", Lang::Cs);
        assert_eq!(c.errors, 0, "{}", render(&c, exp));
        assert_eq!(
            chk("Měl pět jablek.", " Měl šest jablek.", Lang::Cs).errors,
            1
        );
        // Other languages: digits against an unparsed number word.
        assert_eq!(
            chk("Er war fünfundzwanzig.", " Er war 25.", Lang::Other).errors,
            0
        );
    }

    #[test]
    fn merges_spelling_and_contractions() {
        let c = chk(
            "We walk there every day.",
            " We walk there everyday.",
            Lang::En,
        );
        assert_eq!(c.errors, 0);
        assert!(c.ops.iter().any(|o| matches!(o, Op::MergeRef(..))));
        let exp = "The grey colour of the harbour.";
        let c = chk(exp, " The gray color of the harbor.", Lang::En);
        assert_eq!(c.errors, 0, "{}", render(&c, exp));
        assert_eq!(c.verdict, Verdict::Match);
        assert_eq!(
            chk("Don't go.", " Do not go.", Lang::En).verdict,
            Verdict::Match
        );
        assert_eq!(
            chk("I am not sure.", " I'm not sure.", Lang::En).verdict,
            Verdict::Match
        );
        assert_eq!(
            chk("OK, Mr. Hale.", " Okay, Mister Hale.", Lang::En).errors,
            0
        );
    }

    #[test]
    fn spelling_variants_are_explicit() {
        for (a, b) in [
            ("colour", "color"),
            ("realise", "realize"),
            ("realised", "realized"),
            ("organisation", "organization"),
            ("analyse", "analyze"),
            ("centre", "center"),
            ("travelled", "traveled"),
            ("defence", "defense"),
            ("catalogue", "catalog"),
            ("grey", "gray"),
            ("harbour", "harbor"),
            ("favourite", "favorite"),
        ] {
            assert!(spelling_variant(a, b), "{a}/{b}");
        }
        for (a, b) in [
            ("dead", "deed"),
            ("than", "then"),
            ("were", "where"),
            ("want", "went"),
            ("sing", "sang"),
            ("live", "love"),
            ("started", "startled"),
            ("quiet", "quite"),
            ("lose", "loose"),
            ("four", "for"),
            ("river", "liver"),
            ("ice", "ize"),
            ("our", "or"),
        ] {
            assert!(!spelling_variant(a, b), "{a}/{b}");
        }
    }

    #[test]
    fn tags_fillers_and_spoken_tags() {
        let exp = "[laughs] Oh, you really thought so? [sarcastic] How sweet.";
        let c = chk(
            exp,
            " Ha ha ha. Oh, you really thought so? How sweet.",
            Lang::En,
        );
        assert_eq!(c.errors, 0, "{}", render(&c, exp));
        // A tag read aloud is one error; on a short line that alone is
        // minor, so an explicit rule flags it.
        let perf = "[whispers] Come closer.";
        assert_eq!(
            chk(perf, " Whispers. Come closer.", Lang::En).verdict,
            Verdict::Minor
        );
        let tags = tag_phrases(perf, Lang::En);
        assert_eq!(tags, vec![vec!["whispers".to_string()]]);
        let c = check(
            perf,
            " Whispers. Come closer.",
            Lang::En,
            &tags,
            &Thresholds::default(),
        );
        assert_eq!(c.verdict, Verdict::Flag("an audio tag was read aloud"));
        // Multi-word tags must be heard in full.
        let perf = "It was over. [long pause] Then the sirens started again, far away.";
        let tags = tag_phrases(perf, Lang::En);
        let c = check(
            perf,
            " It was over. Long pause. Then the sirens started again, far away.",
            Lang::En,
            &tags,
            &Thresholds::default(),
        );
        assert_eq!(c.verdict, Verdict::Flag("an audio tag was read aloud"));
        let c = check(
            perf,
            " It was over. Then the long sirens started again, far away.",
            Lang::En,
            &tags,
            &Thresholds::default(),
        );
        assert_eq!(c.verdict, Verdict::Minor);
    }

    const RAIN: &str = "The rain had not stopped for three days, and the river was rising faster than anyone in the village could remember.";

    #[test]
    fn verdicts_follow_the_thresholds() {
        // 2-word dialogue tags are lenient.
        assert_eq!(
            chk("he said.", " He said.", Lang::En).verdict,
            Verdict::Match
        );
        assert_eq!(
            chk("he said.", " She said.", Lang::En).verdict,
            Verdict::Minor
        );
        assert!(matches!(
            chk("he said.", " Thank you.", Lang::En).verdict,
            Verdict::Flag(_)
        ));
        assert_eq!(
            chk("he said.", "", Lang::En).verdict,
            Verdict::Flag("nothing recognisable was heard")
        );
        assert_eq!(
            chk("he said.", " Um.", Lang::En).verdict,
            Verdict::Flag("nothing recognisable was heard")
        );
        // Punctuation-only differences are not errors.
        assert_eq!(
            chk("Well — fine; go!", " Well, fine. Go.", Lang::En).verdict,
            Verdict::Match
        );
        // A dropped clause in a long line: three missing words in a row.
        let heard = " The rain had not stopped for three days, and the river was rising faster than anyone could remember.";
        let c = chk(RAIN, heard, Lang::En);
        assert_eq!(c.errors, 3, "{}", render(&c, RAIN));
        assert_eq!(
            c.verdict,
            Verdict::Flag("words are missing, added or wrong in a row")
        );
        // A repeated phrase (a TTS stutter or loop).
        let heard = " The rain had not stopped for three days, and the river, and the river, and the river was rising faster than anyone in the village could remember.";
        assert!(matches!(
            chk(RAIN, heard, Lang::En).verdict,
            Verdict::Flag(_)
        ));
        // One misheard word in a long line: minor only.
        let heard = " The rain had not stopped for three days, and the liver was rising faster than anyone in the village could remember.";
        assert_eq!(chk(RAIN, heard, Lang::En).verdict, Verdict::Minor);
        // Scattered errors in a long line: high WER.
        let heard = " The pain had not stopped for tree days, and a river was raising faster than anyone in the valley could remember.";
        let c = chk(RAIN, heard, Lang::En);
        assert_eq!(
            c.verdict,
            Verdict::Flag("many words differ from the text"),
            "{}",
            render(&c, RAIN)
        );
        // Missing and extra words around an exact match stay apart.
        let c = chk(
            "Close the old door, please.",
            " Close the door now, please.",
            Lang::En,
        );
        assert_eq!(
            render(&c, "Close the old door, please."),
            "Close the [-old-] door {+now+} please"
        );
        // A repeated phrase or word is marked on its second copy.
        let exp = "Then we lose the signal.";
        let c = chk(
            exp,
            " Then we lose the signal. Then we lose the signal.",
            Lang::En,
        );
        assert_eq!(
            render(&c, exp),
            "Then we lose the signal {+then+} {+we+} {+lose+} {+the+} {+signal+}"
        );
        let c = chk("Go back now.", " Go back back now.", Lang::En);
        assert_eq!(render(&c, "Go back now."), "Go back {+back+} now");
        let c = chk("It was very very cold.", " It was very cold.", Lang::En);
        assert_eq!(
            render(&c, "It was very very cold."),
            "It was very [-very-] cold"
        );
        // An extra word.
        let c = chk("Close the door.", " Close the door now.", Lang::En);
        assert_eq!((c.errors, c.verdict.clone()), (1, Verdict::Minor));
        // Missing words in a medium line.
        let exp = "She opened the old wooden box and looked inside.";
        let c = chk(exp, " She opened the box and inside.", Lang::En);
        assert_eq!(c.errors, 3, "{}", render(&c, exp));
        assert!(matches!(c.verdict, Verdict::Flag(_)));
    }

    #[test]
    fn non_english_is_more_lenient() {
        // 15+ words, two errors: 0.13 > 0.12 flags English, not Czech (x1.6).
        let exp = "Ráno bylo chladné a mlha ležela nad řekou, když jsme konečně vyrazili na dlouhou cestu domů.";
        let heard = " Ráno bylo chladné a mlha ležela nad řekou, když jsme konečně vyrazili na dlouho cestu doma.";
        let c = chk(exp, heard, Lang::Cs);
        assert_eq!(c.errors, 2, "{}", render(&c, exp));
        assert_eq!(c.verdict, Verdict::Minor);
        let exp = "In the morning it was cold and the mist lay over the river when we left.";
        let heard = " In the morning it was cold and the mist lay over the liver when we laughed.";
        let c = chk(exp, heard, Lang::En);
        assert_eq!(c.errors, 2);
        assert!(matches!(c.verdict, Verdict::Flag(_)));
    }

    #[test]
    fn interjections_and_the_texts_own_fillers() {
        // Lines that are only interjections are fillers on both sides: an
        // exact transcript matches, and Whisper leaving them out is normal
        // (a silent take is the length check's job).
        for line in ["Hmm.", "Oh!", "Ha ha ha!", "Mm-hmm."] {
            assert_eq!(
                chk(line, &format!(" {line}"), Lang::En).verdict,
                Verdict::Match,
                "{line}"
            );
            assert_eq!(chk(line, "", Lang::En).verdict, Verdict::Match, "{line}");
        }
        assert_eq!(chk("Hm.", " Hm.", Lang::Cs).verdict, Verdict::Match);
        assert_eq!(chk("Hmm.", " Thank you.", Lang::En).verdict, Verdict::Minor);
        // The text's own "um" left out: not an error, but shown.
        let exp = "Um, I don't know.";
        let c = chk(exp, " I don't know.", Lang::En);
        assert_eq!((c.errors, c.verdict.clone()), (0, Verdict::Match));
        assert_eq!(render(&c, exp), "(-Um-) I don't know");
        assert_eq!(chk("Ehm, nevím.", " Nevím.", Lang::Cs).errors, 0);
        // A real word missing still counts, interjection or not.
        assert_eq!(
            chk("Oh, hello.", " Oh.", Lang::En).verdict,
            Verdict::Flag("nothing recognisable was heard")
        );
        assert_eq!(chk("Oh, hello there.", " Oh, there.", Lang::En).errors, 1);
    }

    #[test]
    fn contractions_with_several_expansions() {
        for (exp, heard) in [
            ("We'll see.", " We will see."),
            ("They'd gone.", " They had gone."),
            ("They'd go.", " They would go."),
            ("He's been here.", " He has been here."),
            ("I'd like that.", " I would like that."),
            ("You should've known.", " You should have known."),
            ("We will see.", " We'll see."),
        ] {
            assert_eq!(chk(exp, heard, Lang::En).verdict, Verdict::Match, "{exp}");
        }
        // Only the exact expansion is excused.
        assert!(matches!(
            chk("We'll see.", " We still see.", Lang::En).verdict,
            Verdict::Flag(_)
        ));
        assert!(chk("Well, see.", " We, see.", Lang::En).errors > 0);
    }

    #[test]
    fn empty_expected_text() {
        assert_eq!(chk("—", "", Lang::En).verdict, Verdict::Match);
        assert_eq!(chk("…", " Hello.", Lang::En).verdict, Verdict::Minor);
    }

    #[test]
    fn normalized_phrases() {
        assert_eq!(normalized(" Thank you.", Lang::En), "thank you");
        assert_eq!(
            normalized("Titulky vytvořil JohnyX", Lang::Cs),
            "titulky vytvoril johnyx"
        );
    }
}
