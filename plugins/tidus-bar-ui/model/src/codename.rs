//! Instance codenames: the second implementation of one grammar.
//!
//! SPEC (Andres, 2026-08-17). Every pane opens with a unique codename drawn
//! from a fantasy/cyberpunk/pop-culture pool; that codename IS the instance id;
//! and it prefixes the existing synced name (session title = pane label = tab
//! name) rather than replacing it:
//!
//! ```text
//! parent   Hexagon (Check tidu subagents)
//! child    Hexagon > Bulbasur (add tab to context menu)
//! ```
//!
//! The bar draws the BARE codename — no lineage prefix, no title — and reveals
//! the real name on hover, which is why the composed form has to come apart
//! again reliably.
//!
//! ## The grammar
//!
//! ```text
//! NAME  := HEAD [" (" TITLE ")"]
//! HEAD  := TOKEN (" > " TOKEN)*
//! TOKEN := [A-Z][A-Za-z0-9]{0,15}(-[0-9]+)?
//! TITLE := any non-empty text
//! ```
//!
//! The last HEAD token is the codename; the ones before it are its lineage.
//!
//! The producer is `harness-fleet`'s `cnCompose`/`cnParse`
//! (`go/codename.go`) — the same table of cases, the same doc comment. Two
//! implementations of one wire format is a real risk, so the risk is written
//! down here: if these drift, a chip ends up labelled with somebody's task
//! title and the tooltip shows their instance id, which is exactly backwards.
//!
//! ## Why the parse is conservative
//!
//! A name is read as codename-carrying ONLY when it has a `"(…)"` title AND
//! every head segment is a single token. So `"Fix the parser (again)"` parses
//! as pure title: its head is three words, not one. A bare `"Hexagon"` with no
//! title is also refused here — by shape alone it is indistinguishable from a
//! pane somebody labelled `"Refactor"` — and the collector resolves those from
//! the fleet's ledger by pane id instead, which is exact rather than clever.

const SEP: &str = " > ";
const OPEN: &str = " (";
const CLOSE: char = ')';

/// A decorated name, taken apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Ancestors, root-most first, as displayed. Empty for a root.
    pub lineage: Vec<String>,
    /// The instance id, as displayed (`"Hexagon"`). Empty when the name
    /// carries none.
    pub codename: String,
    /// The real name. The whole input when there is no codename, so a caller
    /// that only wants "what should the tooltip say" can use this field alone.
    pub title: String,
}

/// One HEAD segment: `[A-Z][A-Za-z0-9]{0,15}(-[0-9]+)?`.
///
/// Hand-written rather than regex — this crate compiles to wasm and is on the
/// per-frame path, so a regex engine would be several hundred KB and a
/// per-parse allocation for a check that is a dozen comparisons.
fn is_token(seg: &str) -> bool {
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    // The optional "-<n>" collision suffix splits the segment in two halves,
    // each with its own alphabet.
    let (head, suffix) = match seg.split_once('-') {
        Some((head, suffix)) => (head, Some(suffix)),
        None => (seg, None),
    };
    if head.len() > 16 || !head.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    match suffix {
        None => true,
        Some(s) => !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()),
    }
}

/// Split a decorated name into lineage, codename and title.
///
/// Never fails: a name that does not carry a codename comes back as pure
/// title, which is what the bar should draw for a pane the fleet has not named.
pub fn parse(name: &str) -> Parsed {
    let s = name.trim();
    let Some(open) = s.find(OPEN) else {
        return Parsed {
            title: s.to_string(),
            ..Default::default()
        };
    };
    if !s.ends_with(CLOSE) {
        return Parsed {
            title: s.to_string(),
            ..Default::default()
        };
    }
    let head = &s[..open];
    // The title runs to the LAST closing paren, so a title with parentheses of
    // its own survives intact.
    let title = s[open + OPEN.len()..s.len() - CLOSE.len_utf8()].trim();
    if head.is_empty() || title.is_empty() {
        return Parsed {
            title: s.to_string(),
            ..Default::default()
        };
    }
    let segs: Vec<&str> = head.split(SEP).collect();
    if !segs.iter().all(|seg| is_token(seg)) {
        return Parsed {
            title: s.to_string(),
            ..Default::default()
        };
    }
    let (codename, lineage) = segs.split_last().expect("split never yields empty");
    Parsed {
        lineage: lineage.iter().map(|s| s.to_string()).collect(),
        codename: codename.to_string(),
        title: title.to_string(),
    }
}

/// Render a stored (lowercase) codename the way a name carries it:
/// `"hexagon"` → `"Hexagon"`. Mirrors the fleet's `cnDisplay`.
pub fn display(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is deliberately the same one `codename_test.go` carries. Two
    /// implementations of one grammar only stay honest if both are pinned to
    /// the same cases.
    #[test]
    fn parses_the_spec_examples() {
        let p = parse("Hexagon (Check tidu subagents)");
        assert_eq!(p.codename, "Hexagon");
        assert_eq!(p.title, "Check tidu subagents");
        assert!(p.lineage.is_empty());

        let p = parse("Hexagon > Bulbasur (add tab to context menu)");
        assert_eq!(p.codename, "Bulbasur");
        assert_eq!(p.title, "add tab to context menu");
        assert_eq!(p.lineage, vec!["Hexagon".to_string()]);
    }

    /// The conservative half. An ordinary label must never be read as an
    /// instance id — that failure puts a task description on the chip and the
    /// id in the tooltip, i.e. exactly the wrong way round.
    #[test]
    fn refuses_ordinary_labels() {
        for name in [
            "Fix the parser (again)",
            "add tab to context menu",
            "claude (2.1.231)",
            "hexagon (a title)",
            "Hexagon and friends (a title)",
            "Hexagon",
            "",
            "()",
            "Hexagon ()",
        ] {
            let p = parse(name);
            assert!(
                p.codename.is_empty(),
                "{name:?} was read as codename {:?}",
                p.codename
            );
            assert_eq!(p.title, name.trim(), "{name:?} lost its title");
        }
    }

    #[test]
    fn a_title_may_contain_parentheses() {
        let p = parse("Hexagon (fix the (nested) case)");
        assert_eq!(p.codename, "Hexagon");
        assert_eq!(p.title, "fix the (nested) case");
    }

    /// The collision suffix the fleet falls back to when every word in the
    /// pool is live at once.
    #[test]
    fn a_suffixed_codename_is_still_a_codename() {
        let p = parse("Hexagon-2 (a task)");
        assert_eq!(p.codename, "Hexagon-2");
        // ...but a suffix that is not a number is not one.
        assert!(parse("Hexagon-two (a task)").codename.is_empty());
    }

    #[test]
    fn a_deep_lineage_keeps_its_order() {
        let p = parse("Alpha > Bravo > Charlie (task)");
        assert_eq!(p.lineage, vec!["Alpha".to_string(), "Bravo".to_string()]);
        assert_eq!(p.codename, "Charlie");
    }

    #[test]
    fn display_capitalises_only_the_first_letter() {
        assert_eq!(display("hexagon"), "Hexagon");
        assert_eq!(display("bulbasur"), "Bulbasur");
        assert_eq!(display("hexagon-2"), "Hexagon-2");
        assert_eq!(display(""), "");
    }

    /// Non-ASCII must not panic or half-match: slicing a decorated name is
    /// byte-indexed, so a multibyte title is the case that would panic.
    #[test]
    fn a_multibyte_title_survives() {
        let p = parse("Hexagon (café ✳ résumé)");
        assert_eq!(p.codename, "Hexagon");
        assert_eq!(p.title, "café ✳ résumé");
    }
}
