//! Claude chat: read the document, propose edits, apply a proposal as one undo step.
//!
//! An assistant never edits directly: `ai.propose` stores edits beside the document without
//! changing it (the UI previews them), and only `ai.apply` — one command, so one undo step —
//! makes them. A proposal is tied to the revision it was made against; any edit in between
//! makes `ai.apply` refuse, so the user's own typing always wins.

use serde_json::{Value, json};
use wordcraft_doc::{Pos, StoryRef};

use super::{parse_pos, pos_json, sel_result};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

/// Most paragraphs one `ai.read` returns.
const MAX_READ: usize = 2_000;
/// Most edits in one proposal.
const MAX_EDITS: usize = 200;
/// Most characters of replacement text in one proposal.
const MAX_TEXT: usize = 100_000;

/// One edit of a proposal: replace `anchor..focus` (byte offsets, same story) with `text`.
#[derive(Clone, Debug, PartialEq)]
pub struct AiEdit {
    pub anchor: Pos,
    pub focus: Pos,
    /// The text it replaces, for the preview.
    pub old: String,
    pub text: String,
}

/// A stored proposal and the document state it was made against.
#[derive(Clone, Debug, PartialEq)]
pub struct AiProposal {
    pub rev: u64,
    pub document_id: u64,
    pub summary: String,
    pub edits: Vec<AiEdit>,
}

/// Where an applied edit landed, for the UI's change animation.
#[derive(Clone, Debug, PartialEq)]
pub struct AiMark {
    pub start: Pos,
    pub end: Pos,
    pub old: String,
    pub text: String,
}

/// Per-document assistant state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiState {
    pub proposal: Option<AiProposal>,
    /// The ranges the last `ai.apply` wrote; the UI takes them (`std::mem::take`).
    pub marks: Vec<AiMark>,
    /// Bumped by every apply, so the UI can tell a new set of marks from an old one.
    pub applied: u64,
}

fn read(s: &mut Session, v: &Value) -> CmdResult {
    let paths = s.doc.para_paths(StoryRef::Body);
    let from = p::u64(v, "from").map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let to = p::u64(v, "to").map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate().skip(from) {
        if i > to || out.len() >= MAX_READ {
            break;
        }
        let Some(para) = s.doc.para(StoryRef::Body, path) else { continue };
        out.push(json!({
            "index": i,
            "start": pos_json(&Pos::new(StoryRef::Body, path.clone(), 0)),
            "style": para.props.style.clone(),
            "text": para.plain_text(),
        }));
    }
    let selected = selected_text(s);
    Ok(json!({
        "rev": s.rev(),
        "count": paths.len(),
        "selection": {"anchor": pos_json(&s.sel.anchor), "focus": pos_json(&s.sel.focus), "text": selected},
        "paragraphs": out,
    }))
}

/// The selected text (empty for a caret), paragraphs joined by `\n`.
pub fn selected_text(s: &Session) -> String {
    span_text(s, &s.sel.anchor, &s.sel.focus)
}

/// The text between two positions of one story, paragraphs joined by `\n`.
fn span_text(s: &Session, a: &Pos, b: &Pos) -> String {
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    if a.story != b.story {
        return String::new();
    }
    let mut out = String::new();
    for path in s.doc.para_paths(a.story) {
        if path < a.path || path > b.path {
            continue;
        }
        let Some(para) = s.doc.para(a.story, &path) else { continue };
        let t = para.plain_text();
        let lo = if path == a.path { para.clamp(a.off) } else { 0 };
        let hi = if path == b.path { para.clamp(b.off) } else { t.len() };
        if path != a.path {
            out.push('\n');
        }
        out.push_str(t.get(lo.min(hi)..hi).unwrap_or(""));
    }
    out
}

/// One edit from its params: `{anchor, focus, text}` or `{find, text, occurrence?}` (the
/// `occurrence`-th match, 1-based, of `find` within one body paragraph).
fn parse_edit(s: &Session, e: &Value) -> Result<AiEdit, CmdError> {
    let text = p::str(e, "text").ok_or_else(|| CmdError::Params("every edit needs `text`".into()))?.to_string();
    let (anchor, focus) = if let Some(find) = p::str(e, "find") {
        if find.is_empty() {
            return Err(CmdError::Params("`find` is empty".into()));
        }
        let want = p::u64(e, "occurrence").unwrap_or(1).max(1);
        let mut seen = 0u64;
        let mut hit = None;
        'outer: for path in s.doc.para_paths(StoryRef::Body) {
            let Some(para) = s.doc.para(StoryRef::Body, &path) else { continue };
            let t = para.plain_text();
            for (off, _) in t.match_indices(find) {
                seen = seen.saturating_add(1);
                if seen == want {
                    let end = off.saturating_add(find.len());
                    hit = Some((Pos::new(StoryRef::Body, path.clone(), off), Pos::new(StoryRef::Body, path.clone(), end)));
                    break 'outer;
                }
            }
        }
        hit.ok_or_else(|| CmdError::Failed(format!("`{find}` is not in the document")))?
    } else {
        let a = e.get("anchor").and_then(parse_pos).ok_or_else(|| CmdError::Params("an edit needs `find` or `anchor` and `focus`".into()))?;
        let b = e.get("focus").and_then(parse_pos).ok_or_else(|| CmdError::Params("an edit needs `find` or `anchor` and `focus`".into()))?;
        let (a, b) = (s.doc.clamp(&a), s.doc.clamp(&b));
        if a.story != b.story {
            return Err(CmdError::Params("`anchor` and `focus` must be in the same story".into()));
        }
        if a <= b { (a, b) } else { (b, a) }
    };
    let old = span_text(s, &anchor, &focus);
    Ok(AiEdit { anchor, focus, old, text })
}

fn propose(s: &mut Session, v: &Value) -> CmdResult {
    let edits = v.get("edits").and_then(Value::as_array).ok_or_else(|| CmdError::Params("`edits` (a list) is required".into()))?;
    if edits.is_empty() || edits.len() > MAX_EDITS {
        return Err(CmdError::Params(format!("`edits` needs 1 to {MAX_EDITS} entries")));
    }
    let mut out: Vec<AiEdit> = edits.iter().map(|e| parse_edit(s, e)).collect::<Result<_, _>>()?;
    let total: usize = out.iter().map(|e| e.text.chars().count()).fold(0, usize::saturating_add);
    if total > MAX_TEXT {
        return Err(CmdError::Params(format!("the proposal's text is over {MAX_TEXT} characters")));
    }
    out.sort_by(|a, b| a.anchor.cmp(&b.anchor));
    if out.windows(2).any(|w| matches!(w, [a, b] if b.anchor < a.focus)) {
        return Err(CmdError::Params("edits overlap".into()));
    }
    let summary = p::str(v, "summary").unwrap_or("").chars().take(500).collect::<String>();
    let preview: Vec<Value> =
        out.iter().map(|e| json!({"anchor": pos_json(&e.anchor), "focus": pos_json(&e.focus), "old": e.old, "text": e.text})).collect();
    s.doc_ai.proposal = Some(AiProposal { rev: s.rev(), document_id: s.document_id(), summary, edits: out });
    Ok(json!({"proposed": preview.len(), "edits": preview, "note": "Shown to the user; it is applied only when they choose Apply."}))
}

fn apply(s: &mut Session, _: &Value) -> CmdResult {
    let prop = s.doc_ai.proposal.take().ok_or_else(|| CmdError::Failed("there's no proposal to apply".into()))?;
    if prop.rev != s.rev() || prop.document_id != s.document_id() {
        return Err(CmdError::Failed("the document changed since the proposal; propose again".into()));
    }
    let insert = s.registry.get("text.insert").map(|c| c.run).ok_or_else(|| CmdError::Unknown("text.insert".into()))?;
    let mut marks = Vec::new();
    // Back to front, so earlier positions stay valid.
    for e in prop.edits.iter().rev() {
        s.sel = Selection { anchor: e.anchor.clone(), focus: e.focus.clone() };
        if e.text.is_empty() {
            super::delete_selection(s)?;
        } else {
            insert(s, &json!({"text": e.text, "raw": true, "paragraphs": e.text.contains('\n')}))?;
        }
        marks.push(AiMark { start: e.anchor.clone(), end: s.sel.focus.clone(), old: e.old.clone(), text: e.text.clone() });
    }
    marks.reverse();
    // Later marks moved by nothing (we edited back to front), earlier ones are exact.
    s.doc_ai.marks = marks;
    s.doc_ai.applied = s.doc_ai.applied.wrapping_add(1);
    sel_result(s)
}

fn discard(s: &mut Session, _: &Value) -> CmdResult {
    let had = s.doc_ai.proposal.take().is_some();
    Ok(json!({"discarded": had}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("ai.read", "Read for Assistant", "Assistant", read)
            .params(r#"{"from"?: number, "to"?: number} → {rev, count, selection{anchor, focus, text}, paragraphs[{index, start, style, text}]} (body paragraphs, at most 2000)"#)
            .pure(),
        CommandSpec::new("ai.propose", "Propose Edits", "Assistant", propose)
            .params(r#"{"edits": [{"find": string, "occurrence"?: number, "text": string} | {"anchor": Pos, "focus": Pos, "text": string}], "summary"?: string} (stores a proposal; the document is unchanged until ai.apply; at most 200 edits, 100000 characters)"#)
            .pure(),
        CommandSpec::new("ai.apply", "Apply Proposal", "Assistant", apply)
            .when(|s| if s.doc_ai.proposal.is_none() { Some("there's no proposal") } else { None }),
        CommandSpec::new("ai.discard", "Discard Proposal", "Assistant", discard).pure(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> Session {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("text.insert", &json!({"text": text, "paragraphs": true})).unwrap();
        s
    }

    fn body(s: &Session) -> String {
        s.doc.plain_text(StoryRef::Body)
    }

    #[test]
    fn read_lists_paragraphs_and_selection() {
        let mut s = session("Eins\nZwei");
        s.run("select.range", &json!({"anchor": {"block": 1, "off": 0}, "focus": {"block": 1, "off": 4}})).unwrap();
        let r = s.run("ai.read", &json!({})).unwrap();
        assert_eq!(r["paragraphs"][1]["text"], "Zwei");
        assert_eq!(r["selection"]["text"], "Zwei");
    }

    #[test]
    fn propose_does_not_change_document() {
        let mut s = session("Der Hund schläft.");
        let depth = s.undo_depth();
        s.run("ai.propose", &json!({"edits": [{"find": "Hund", "text": "Kater"}]})).unwrap();
        assert_eq!(body(&s), "Der Hund schläft.");
        assert_eq!(s.undo_depth(), depth);
        assert!(s.doc_ai.proposal.is_some());
    }

    #[test]
    fn apply_is_one_undo_step() {
        let mut s = session("Der Hund schläft.\nDie Katze wacht.");
        let before = body(&s);
        s.run("ai.propose", &json!({"edits": [{"find": "Hund", "text": "Kater"}, {"find": "wacht", "text": "träumt\nund schnurrt"}]})).unwrap();
        s.run("ai.apply", &json!({})).unwrap();
        assert_eq!(body(&s), "Der Kater schläft.\nDie Katze träumt\nund schnurrt.");
        assert_eq!(s.doc_ai.marks.len(), 2);
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(body(&s), before);
    }

    #[test]
    fn empty_text_deletes() {
        let mut s = session("sehr sehr gut");
        s.run("ai.propose", &json!({"edits": [{"find": "sehr ", "text": ""}]})).unwrap();
        s.run("ai.apply", &json!({})).unwrap();
        assert_eq!(body(&s), "sehr gut");
    }

    #[test]
    fn apply_refuses_after_user_edit() {
        let mut s = session("Der Hund schläft.");
        s.run("ai.propose", &json!({"edits": [{"find": "Hund", "text": "Kater"}]})).unwrap();
        s.run("text.insert", &json!({"text": "!"})).unwrap();
        let after = body(&s);
        assert!(s.run("ai.apply", &json!({})).is_err());
        assert_eq!(body(&s), after);
    }

    #[test]
    fn apply_without_proposal_errors() {
        let mut s = session("x");
        assert!(s.run("ai.apply", &json!({})).is_err());
    }

    #[test]
    fn discard_drops_the_proposal() {
        let mut s = session("Der Hund");
        s.run("ai.propose", &json!({"edits": [{"find": "Hund", "text": "Kater"}]})).unwrap();
        s.run("ai.discard", &json!({})).unwrap();
        assert!(s.doc_ai.proposal.is_none());
    }

    #[test]
    fn overlapping_and_missing_edits_are_refused() {
        let mut s = session("abcdef");
        assert!(s.run("ai.propose", &json!({"edits": [{"find": "abcd", "text": "x"}, {"find": "cdef", "text": "y"}]})).is_err());
        assert!(s.run("ai.propose", &json!({"edits": [{"find": "zzz", "text": "x"}]})).is_err());
        assert!(s.run("ai.propose", &json!({"edits": []})).is_err());
    }
}
