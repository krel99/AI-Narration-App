//! Deterministic text handling: paragraph splitting, the quote parser, and the
//! exact-text validator that guards the LLM attribution pass.
//!
//! Invariant: the segments of a paragraph, in order, reproduce the paragraph
//! text exactly, with only whitespace allowed between them.

use crate::lang::Language;
use crate::model::{NARRATOR, SegmentKind};

/// Speaker given to dialogue the parser could not attribute. It never has a
/// voice mapped, so such segments show up as "unassigned voice".
pub const UNKNOWN_SPEAKER: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedParagraph {
    pub text: String,
    pub scene_break_before: bool,
}

/// A segment as byte range into its paragraph's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSegment {
    pub kind: SegmentKind,
    pub speaker: String,
    pub start: usize,
    pub end: usize,
}

impl DraftSegment {
    pub fn text<'a>(&self, paragraph: &'a str) -> &'a str {
        &paragraph[self.start..self.end]
    }
}

fn is_scene_break(block: &str) -> bool {
    let t = block.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_whitespace() || "*#~-_•⁂·=".contains(c))
}

/// Split chapter text into paragraphs. Blank lines separate paragraphs when
/// the text has any; otherwise every non-empty line is a paragraph. Lines made
/// only of `*`, `#`, `~`, `-` and similar are scene-break markers.
pub fn split_paragraphs(source: &str) -> Vec<ParsedParagraph> {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = source.lines().collect();
    let non_empty = |l: &&str| !l.trim().is_empty();
    let has_blank_separators = match (
        lines.iter().position(non_empty),
        lines.iter().rposition(non_empty),
    ) {
        (Some(first), Some(last)) => lines[first..last].iter().any(|l| l.trim().is_empty()),
        _ => false,
    };

    let blocks: Vec<String> = if has_blank_separators {
        let mut blocks = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        for l in &lines {
            if l.trim().is_empty() {
                if !cur.is_empty() {
                    blocks.push(cur.join(" "));
                    cur.clear();
                }
            } else if is_scene_break(l) {
                // A marker line is its own block even without blank lines around it.
                if !cur.is_empty() {
                    blocks.push(cur.join(" "));
                    cur.clear();
                }
                blocks.push(l.trim().to_string());
            } else {
                cur.push(l.trim());
            }
        }
        if !cur.is_empty() {
            blocks.push(cur.join(" "));
        }
        blocks
    } else {
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect()
    };

    let mut out = Vec::new();
    let mut pending_break = false;
    for b in blocks {
        if is_scene_break(&b) {
            pending_break = !out.is_empty();
            continue;
        }
        out.push(ParsedParagraph {
            text: b,
            scene_break_before: pending_break,
        });
        pending_break = false;
    }
    out
}

fn trim_range(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let slice = &text[start..end];
    let lead = slice.len() - slice.trim_start().len();
    let trail = slice.len() - slice.trim_end().len();
    if lead == slice.len() {
        None
    } else {
        Some((start + lead, end - trail))
    }
}

/// Split a paragraph into quoted speech and the narration between it, using
/// the language's quote styles. An unterminated quote runs to the end of the
/// paragraph (the usual convention for speech spanning several paragraphs).
pub fn quote_spans(paragraph: &str, lang: &Language) -> Vec<(SegmentKind, usize, usize)> {
    let mut out = Vec::new();
    let mut narration_start = 0;
    let mut open: Option<(usize, char)> = None;

    for (idx, c) in paragraph.char_indices() {
        match open {
            None => {
                if let Some(close) = lang.closing_for(c) {
                    if let Some((s, e)) = trim_range(paragraph, narration_start, idx) {
                        out.push((SegmentKind::Narration, s, e));
                    }
                    open = Some((idx, close));
                }
            }
            Some((start, close)) => {
                if c == close {
                    let end = idx + c.len_utf8();
                    out.push((SegmentKind::Dialogue, start, end));
                    narration_start = end;
                    open = None;
                }
            }
        }
    }
    match open {
        Some((start, _)) => {
            if let Some((s, e)) = trim_range(paragraph, start, paragraph.len()) {
                out.push((SegmentKind::Dialogue, s, e));
            }
        }
        None => {
            if let Some((s, e)) = trim_range(paragraph, narration_start, paragraph.len()) {
                out.push((SegmentKind::Narration, s, e));
            }
        }
    }
    out
}

pub fn has_dialogue(paragraph: &str, lang: &Language) -> bool {
    paragraph.chars().any(|c| lang.is_opening_quote(c))
}

fn is_any_quote(c: char) -> bool {
    "\"“”„«»‚‘’'".contains(c)
}

/// Fold segments with nothing speakable in them (a lone dash or comma between
/// two quotes) into the previous segment, or the next one if first.
pub fn normalize(paragraph: &str, segs: Vec<DraftSegment>) -> Vec<DraftSegment> {
    let speakable = |s: &DraftSegment| s.text(paragraph).chars().any(char::is_alphanumeric);
    let mut out: Vec<DraftSegment> = Vec::with_capacity(segs.len());
    let mut carry_start: Option<usize> = None;
    for mut s in segs {
        // Punctuation right after a closing quote (French « Bonjour », dit-il.)
        // belongs to the dialogue, not to the narration that follows.
        if let Some(prev) = out.last_mut()
            && speakable(&s)
        {
            let text = s.text(paragraph);
            let punct = text
                .char_indices()
                .find(|(_, c)| c.is_alphanumeric() || c.is_whitespace() || is_any_quote(*c))
                .map_or(text.len(), |(i, _)| i);
            if punct > 0 {
                prev.end = s.start + punct;
                let rest = &paragraph[prev.end..s.end];
                s.start = prev.end + (rest.len() - rest.trim_start().len());
            }
        }
        if !speakable(&s) {
            match out.last_mut() {
                Some(prev) => prev.end = s.end,
                None => carry_start = Some(carry_start.unwrap_or(s.start)),
            }
            continue;
        }
        if let Some(cs) = carry_start.take() {
            s.start = cs;
        }
        out.push(s);
    }
    if out.is_empty() {
        // Nothing speakable at all: keep the paragraph as one narration segment.
        if let Some((s, e)) = trim_range(paragraph, 0, paragraph.len()) {
            out.push(DraftSegment {
                kind: SegmentKind::Narration,
                speaker: NARRATOR.into(),
                start: s,
                end: e,
            });
        }
    }
    out
}

/// Parser-only segmentation, used for paragraphs without dialogue and as the
/// fallback when the LLM keeps failing validation. Dialogue gets
/// [`UNKNOWN_SPEAKER`] so it blocks generation until the user maps it.
pub fn parse_paragraph(paragraph: &str, lang: &Language) -> Vec<DraftSegment> {
    let segs = quote_spans(paragraph, lang)
        .into_iter()
        .map(|(kind, start, end)| DraftSegment {
            kind,
            speaker: match kind {
                SegmentKind::Narration => NARRATOR.into(),
                SegmentKind::Dialogue => UNKNOWN_SPEAKER.into(),
            },
            start,
            end,
        })
        .collect();
    normalize(paragraph, segs)
}

fn snippet(s: &str) -> String {
    let mut out: String = s.chars().take(40).collect();
    if s.chars().count() > 40 {
        out.push('…');
    }
    out
}

/// Locate each text in `paragraph`, in order. Only whitespace may sit between
/// consecutive texts, and together they must cover the paragraph completely.
pub fn locate(paragraph: &str, texts: &[&str]) -> Result<Vec<(usize, usize)>, String> {
    let skip_ws = |mut pos: usize| {
        while let Some(c) = paragraph[pos..].chars().next() {
            if !c.is_whitespace() {
                break;
            }
            pos += c.len_utf8();
        }
        pos
    };
    let mut pos = 0;
    let mut out = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let t = t.trim();
        if t.is_empty() {
            return Err(format!("segment {} is empty", i + 1));
        }
        pos = skip_ws(pos);
        if !paragraph[pos..].starts_with(t) {
            return Err(format!(
                "segment {} does not match the source: source continues {:?}, segment is {:?}",
                i + 1,
                snippet(&paragraph[pos..]),
                snippet(t)
            ));
        }
        out.push((pos, pos + t.len()));
        pos += t.len();
    }
    pos = skip_ws(pos);
    if pos != paragraph.len() {
        return Err(format!(
            "segments stop early; uncovered source: {:?}",
            snippet(&paragraph[pos..])
        ));
    }
    Ok(out)
}

/// One segment as returned by the LLM for a paragraph.
#[derive(Debug, Clone)]
pub struct ProposedSegment {
    pub kind: SegmentKind,
    pub speaker: String,
    pub text: String,
}

/// Validate the LLM's segments for one paragraph. The texts must reproduce
/// the paragraph exactly; the LLM may never drop or rewrite words.
pub fn validate_proposed(
    paragraph: &str,
    proposed: &[ProposedSegment],
) -> Result<Vec<DraftSegment>, String> {
    if proposed.is_empty() {
        return Err("no segments returned".into());
    }
    let texts: Vec<&str> = proposed.iter().map(|p| p.text.as_str()).collect();
    let ranges = locate(paragraph, &texts)?;
    let segs = proposed
        .iter()
        .zip(ranges)
        .map(|(p, (start, end))| {
            let speaker = match p.kind {
                SegmentKind::Narration => NARRATOR.to_string(),
                SegmentKind::Dialogue => {
                    let s = p.speaker.trim();
                    if s.is_empty() {
                        UNKNOWN_SPEAKER.to_string()
                    } else {
                        s.to_string()
                    }
                }
            };
            DraftSegment {
                kind: p.kind,
                speaker,
                start,
                end,
            }
        })
        .collect();
    Ok(normalize(paragraph, segs))
}

/// Text of segments `first..=last` merged, taken from the paragraph so the
/// original whitespace between them is kept.
pub fn merged_text(paragraph: &str, seg_texts: &[&str], first: usize, last: usize) -> String {
    match locate(paragraph, seg_texts) {
        Ok(ranges) => paragraph[ranges[first].0..ranges[last].1].to_string(),
        Err(_) => seg_texts[first..=last].join(" "),
    }
}

/// Split `original` at the `|` markers the user inserted into `marked`.
/// Removing the markers must give back `original` unchanged.
pub fn split_at_markers(original: &str, marked: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = marked.split('|').map(str::trim).collect();
    if parts.len() < 2 {
        return Err("Insert at least one | where the segment should be split.".into());
    }
    if parts.iter().any(|p| p.is_empty()) {
        return Err("A split would produce an empty segment.".into());
    }
    let ranges = locate(original.trim(), &parts)
        .map_err(|_| "Only insert | markers; the text itself must not change.".to_string())?;
    Ok(ranges
        .into_iter()
        .map(|(s, e)| original.trim()[s..e].to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::by_code;

    fn texts(par: &str, segs: &[DraftSegment]) -> Vec<String> {
        segs.iter().map(|s| s.text(par).to_string()).collect()
    }

    #[test]
    fn design_doc_example() {
        let par = "\"It is,\" started Vortram, \"truly magnificent!\"";
        let segs = parse_paragraph(par, by_code("en"));
        assert_eq!(
            texts(par, &segs),
            vec!["\"It is,\"", "started Vortram,", "\"truly magnificent!\""]
        );
        assert_eq!(segs[0].kind, SegmentKind::Dialogue);
        assert_eq!(segs[1].kind, SegmentKind::Narration);
        assert_eq!(segs[1].speaker, NARRATOR);
        assert_eq!(segs[2].speaker, UNKNOWN_SPEAKER);
    }

    #[test]
    fn curly_and_language_specific_quotes() {
        let par = "„Ano,“ řekl. »Ne!«";
        let segs = parse_paragraph(par, by_code("cs"));
        assert_eq!(texts(par, &segs), vec!["„Ano,“", "řekl.", "»Ne!«"]);

        let par = "« Bonjour », dit-il.";
        let segs = parse_paragraph(par, by_code("fr"));
        assert_eq!(texts(par, &segs), vec!["« Bonjour »,", "dit-il."]);
    }

    #[test]
    fn unterminated_quote_runs_to_end() {
        let par = "He said: “This continues into the next paragraph";
        let segs = parse_paragraph(par, by_code("en"));
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].kind, SegmentKind::Dialogue);
        assert_eq!(segs[1].text(par), "“This continues into the next paragraph");
    }

    #[test]
    fn punctuation_only_segment_is_folded() {
        let par = "“Yes” — “No”";
        let segs = parse_paragraph(par, by_code("en"));
        assert_eq!(texts(par, &segs), vec!["“Yes” —", "“No”"]);
    }

    #[test]
    fn narration_only_paragraph() {
        let par = "  The wind rose. ";
        let segs = parse_paragraph(par, by_code("en"));
        assert_eq!(texts(par, &segs), vec!["The wind rose."]);
        assert!(!has_dialogue(par, by_code("en")));
    }

    #[test]
    fn split_paragraphs_blank_lines_and_scene_breaks() {
        let src = "Chapter text line one\ncontinues here.\n\n\"Hi,\" she said.\n\n* * *\n\nLater.";
        let ps = split_paragraphs(src);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].text, "Chapter text line one continues here.");
        assert!(!ps[1].scene_break_before);
        assert!(ps[2].scene_break_before);
        assert_eq!(ps[2].text, "Later.");
    }

    #[test]
    fn split_paragraphs_one_per_line() {
        let ps = split_paragraphs("One.\nTwo.\n###\nThree.\n");
        assert_eq!(ps.len(), 3);
        assert!(ps[2].scene_break_before);
    }

    #[test]
    fn locate_accepts_whitespace_gaps_only() {
        let par = "\"It is,\" started Vortram, \"truly magnificent!\"";
        let ok = locate(
            par,
            &["\"It is,\"", "started Vortram,", "\"truly magnificent!\""],
        );
        assert!(ok.is_ok());
        // A dropped word is rejected.
        assert!(locate(par, &["\"It is,\"", "started,", "\"truly magnificent!\""]).is_err());
        // A rewritten quote style is rejected.
        assert!(
            locate(
                par,
                &["“It is,”", "started Vortram,", "\"truly magnificent!\""]
            )
            .is_err()
        );
        // Missing tail is rejected.
        assert!(locate(par, &["\"It is,\"", "started Vortram,"]).is_err());
        // Empty segment is rejected.
        assert!(locate(par, &["", "\"It is,\""]).is_err());
    }

    #[test]
    fn validate_forces_narrator_on_narration() {
        let par = "“Go,” said Ann.";
        let proposed = vec![
            ProposedSegment {
                kind: SegmentKind::Dialogue,
                speaker: "ann".into(),
                text: "“Go,”".into(),
            },
            ProposedSegment {
                kind: SegmentKind::Narration,
                speaker: "ann".into(),
                text: "said Ann.".into(),
            },
        ];
        let segs = validate_proposed(par, &proposed).unwrap();
        assert_eq!(segs[0].speaker, "ann");
        assert_eq!(segs[1].speaker, NARRATOR);
    }

    #[test]
    fn merge_keeps_original_spacing() {
        let par = "“Go,”  said Ann.";
        assert_eq!(
            merged_text(par, &["“Go,”", "said Ann."], 0, 1),
            "“Go,”  said Ann."
        );
    }

    #[test]
    fn split_markers() {
        let parts = split_at_markers("said Ann. Then left.", "said Ann. | Then left.").unwrap();
        assert_eq!(parts, vec!["said Ann.", "Then left."]);
        assert!(split_at_markers("abc", "abd|").is_err());
        assert!(split_at_markers("abc", "abc").is_err());
        assert!(split_at_markers("abc", "|abc").is_err());
    }
}
