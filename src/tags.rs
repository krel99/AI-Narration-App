//! ElevenLabs audio tags (`[whispers]`, `[thoughtful]`, `[long pause]`, …).
//!
//! A segment keeps its exact source text; tags live in a separate
//! "performance" version of that text. Stripping the tags from the performance
//! must give back the source words unchanged, so tagging can never drop or
//! rewrite anything.

/// Byte ranges of the tags in `s`: square-bracketed cues of letters, spaces
/// and light punctuation. Bracketed numbers such as footnotes `[1]` are not tags.
pub fn find(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(open) = s[i..].find('[').map(|o| o + i) {
        let Some(close) = s[open + 1..].find([']', '[', '\n']).map(|c| c + open + 1) else {
            break;
        };
        if s[close..].starts_with(']') && is_cue(&s[open + 1..close]) {
            out.push((open, close + 1));
            i = close + 1;
        } else {
            i = open + 1;
        }
    }
    out
}

fn is_cue(inner: &str) -> bool {
    let n = inner.chars().count();
    (1..=48).contains(&n)
        && inner.chars().any(char::is_alphabetic)
        && inner
            .chars()
            .all(|c| c.is_alphabetic() || c == ' ' || c == ',' || c == '-' || c == '\'' || c == '’')
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn opens(c: char) -> bool {
    "\"“„«»‚‘‹›(".contains(c)
}

fn closes(c: char) -> bool {
    "\"”“»«’‘'›‹.,;:!?…)".contains(c)
}

/// `s` without its tags, whitespace collapsed. The space a removed tag leaves
/// behind is dropped next to quote marks and punctuation, so
/// `"[whispers] Get down,"` strips to `"Get down,"`.
pub fn strip(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, b) in find(s) {
        let left = &s[last..a];
        let right = &s[b..];
        let ws_left =
            left.ends_with(char::is_whitespace) || (left.is_empty() && out.ends_with(' '));
        let ws_right = right.starts_with(char::is_whitespace);
        out.push_str(left.trim_end());
        let rest = right.trim_start();
        let space = match (out.chars().last(), rest.chars().next()) {
            (Some(prev), Some(next)) => {
                (ws_left && ws_right) || ((ws_left || ws_right) && !opens(prev) && !closes(next))
            }
            _ => false,
        };
        if space {
            out.push(' ');
        }
        last = b + (right.len() - rest.len());
    }
    out.push_str(&s[last..]);
    collapse_ws(&out)
}

fn tag_names(s: &str) -> Vec<&str> {
    find(s).into_iter().map(|(a, b)| &s[a..b]).collect()
}

/// Check a tagged performance against the segment's source text. Returns the
/// cleaned performance, or `None` when it carries no tags at all.
pub fn validate(text: &str, performance: &str) -> Result<Option<String>, String> {
    let perf = collapse_ws(performance);
    if perf.is_empty() {
        return Ok(None);
    }
    let (want, got) = (strip(text), strip(&perf));
    if want != got {
        let at = want
            .char_indices()
            .zip(got.chars())
            .find(|((_, a), b)| a != b)
            .map_or(want.len().min(got.len()), |((i, _), _)| i);
        let around: String = want[want.floor_char_boundary(at.saturating_sub(12))..]
            .chars()
            .take(40)
            .collect();
        return Err(format!("the tags changed the words near {around:?}"));
    }
    // Bracketed text that was in the source (e.g. "[sic]") must survive, in order.
    let mut source_tags = tag_names(text).into_iter();
    let mut next = source_tags.next();
    for t in tag_names(&perf) {
        if Some(t) == next {
            next = source_tags.next();
        }
    }
    if let Some(missing) = next {
        return Err(format!("{missing} from the source text was removed"));
    }
    if find(&perf)
        .last()
        .is_some_and(|&(_, end)| end == perf.len())
        && !find(text)
            .last()
            .is_some_and(|&(_, e)| e == text.trim_end().len())
    {
        return Err("a tag must be followed by the text it applies to".into());
    }
    if tag_names(&perf).len() == tag_names(text).len() {
        return Ok(None);
    }
    Ok(Some(perf))
}

/// Split `s` into (is_tag, piece) runs for display.
pub fn pieces(s: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut last = 0;
    for (a, b) in find(s) {
        if a > last {
            out.push((false, &s[last..a]));
        }
        out.push((true, &s[a..b]));
        last = b;
    }
    if last < s.len() {
        out.push((false, &s[last..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NARRATION: &str = "said Lieutenant Vale. Commander Voss looked at the countdown above the forward display. Three minutes. The first enemy vessel slid out of the planet’s shadow.";
    const TAGGED: &str = "said Lieutenant Vale. [thoughtful] Commander Voss looked at the countdown above the forward display. [long pause] Three minutes. [short pause] The first enemy vessel slid out of the planet’s shadow.";

    #[test]
    fn finds_cues_not_footnotes() {
        assert_eq!(find("[whispers] hi [1] there [long pause] ok").len(), 2);
        assert_eq!(find("[whispering, fearful] go").len(), 1);
        assert!(find("no [unclosed tag").is_empty());
        assert!(find("[[odd]").len() == 1);
    }

    #[test]
    fn narration_example_validates() {
        let perf = validate(NARRATION, TAGGED).unwrap().unwrap();
        assert_eq!(perf, TAGGED);
        assert_eq!(strip(TAGGED), NARRATION);
    }

    #[test]
    fn dialogue_with_tags() {
        let t = "\"Get down,\"";
        assert_eq!(
            validate(t, "\"[whispers] Get down,\"").unwrap().as_deref(),
            Some("\"[whispers] Get down,\"")
        );
        assert_eq!(
            validate(t, "[whispers] \"Get down,\"").unwrap().as_deref(),
            Some("[whispers] \"Get down,\"")
        );
    }

    #[test]
    fn rejects_changed_words() {
        assert!(validate(NARRATION, &TAGGED.replace("Three minutes", "Two minutes")).is_err());
        assert!(validate(NARRATION, &TAGGED.replace("countdown above", "countdown")).is_err());
        assert!(
            validate("Hello there.", "[sighs] Hello there").is_err(),
            "punctuation dropped"
        );
    }

    #[test]
    fn rejects_dangling_tag_and_lost_source_brackets() {
        assert!(validate("It would not.", "It would not. [long pause]").is_err());
        assert!(validate("He wrote [sic] twice.", "[softly] He wrote twice.").is_err());
        assert!(
            validate("He wrote [sic] twice.", "[softly] He wrote [sic] twice.")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn no_tags_means_none() {
        assert_eq!(validate("Plain.", "Plain.").unwrap(), None);
        assert_eq!(validate("Plain.", "  ").unwrap(), None);
    }

    #[test]
    fn strip_spacing() {
        assert_eq!(
            strip("\"[whispers] Get down,\" he said."),
            "\"Get down,\" he said."
        );
        assert_eq!(strip("vale. [thoughtful] Commander"), "vale. Commander");
        assert_eq!(strip("[sighs] [softly] Fine."), "Fine.");
        assert_eq!(strip("Wait [short pause]... no."), "Wait... no.");
        assert_eq!(strip("one [pause] two"), "one two");
    }

    #[test]
    fn display_pieces() {
        let p = pieces("a [sighs] b");
        assert_eq!(p, vec![(false, "a "), (true, "[sighs]"), (false, " b")]);
    }
}
