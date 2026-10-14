//! Co-authoring a deck over the mesh: the Craft seam's shared session bound to one open
//! presentation.
//!
//! The deck travels as the seam's shared document:
//! - the slide order is the list `slides`, of slide ids;
//! - a slide is the register entry `s/<slide id>`: everything about it but its shapes;
//! - a slide's shapes, back to front, are the list `z/<slide id>`, of shape ids;
//! - a shape is the register entry `h/<shape id>`: its slide and the shape without its
//!   paragraphs;
//! - a text frame's paragraphs are the named body `<shape id>`, one paragraph per body paragraph
//!   under the paragraph's own id, so concurrent typing, Enter and Backspace merge;
//! - a paragraph's runs and formatting are the register entry `p/<paragraph id>`, laid over the
//!   merged text;
//! - a slide's comment threads, in order, are the list `c/<slide id>`, of comment ids, and a
//!   thread's replies the list `r/<comment id>`;
//! - a comment or reply is the register entry `m/<comment id>`: its slide and the comment without
//!   its replies, so two authors commenting at once keep both, and a reply, a resolve and an edit
//!   of one comment never overwrite another.
//!
//! A local edit is captured by comparing the deck with what was last shared, so every command
//! that changes slides, shapes, text or comments shares without a line of its own here. A peer's change
//! rebuilds the slides it touched from the shared document. Ids are author-scoped
//! ([`deckcraft_model::author_id`]), so two authors adding a slide, a shape or a comment at once
//! never collide. A comment made while shared carries this author's name on the shared deck.
//!
//! What cannot travel this way is refused while shared, and the deck is left as it was:
//! changes outside the slides (masters, layouts, the theme, sections, the presentation's
//! properties) and new media. Undo while shared takes back only this author's own edits, one
//! command at a time; redo is not shared.

use std::collections::BTreeSet;
use std::sync::Arc;

use craft_collab::text::{BodyParagraph, TextMerge};
use deckcraft_model::text::{Paragraph, Run, RunKind};
use deckcraft_model::{Comment, Presentation, Shape, ShapeId, Slide, SlideId};
use serde_json::{Value, json};

use crate::{EngineError, Result, Session};

/// The shared session and the presentation it is bound to.
pub struct CollabRuntime {
    shared: craft_collab::CollabSession,
    /// The bound presentation's `DocState::uid`.
    doc: u64,
    /// The deck as the shared document last had it: what a local edit is compared with.
    last: Arc<Presentation>,
}

const SLIDES: &str = "slides";

fn z_list(slide: u64) -> String {
    format!("z/{slide}")
}

fn slide_key(slide: u64) -> String {
    format!("s/{slide}")
}

fn shape_key(shape: u64) -> String {
    format!("h/{shape}")
}

fn para_key(pid: u64) -> String {
    format!("p/{pid}")
}

fn comments_list(slide: u64) -> String {
    format!("c/{slide}")
}

fn replies_list(comment: u64) -> String {
    format!("r/{comment}")
}

fn comment_key(comment: u64) -> String {
    format!("m/{comment}")
}

/// A slide as its record carries it: everything but its shapes and its comments.
fn slide_record(s: &Slide) -> String {
    let mut s = s.clone();
    s.shapes.clear();
    s.comments.clear();
    serde_json::to_string(&s).unwrap_or_default()
}

/// A comment as its record carries it: its slide, and the comment without its replies (they
/// travel as their own records).
fn comment_record(slide: SlideId, c: &Comment) -> String {
    let mut c = c.clone();
    c.replies.clear();
    json!({"slide": slide.0, "comment": c}).to_string()
}

fn ids_of(comments: &[Comment]) -> Vec<String> {
    comments.iter().map(|c| c.id.to_string()).collect()
}

/// Every comment and reply on a slide with its replies, threads first.
fn all_comments(s: &Slide) -> Vec<&Comment> {
    s.comments.iter().flat_map(|c| std::iter::once(c).chain(c.replies.iter())).collect()
}

/// A shape as its record carries it: its slide, and the shape without its paragraphs (they
/// travel in its body).
fn shape_record(slide: SlideId, sh: &Shape) -> String {
    let mut sh = sh.clone();
    if let Some(t) = sh.text.as_mut() {
        t.paragraphs.clear();
    }
    json!({"slide": slide.0, "shape": sh}).to_string()
}

/// A paragraph as its record carries it: the shape it is in, and the paragraph.
fn para_record(shape: ShapeId, p: &Paragraph) -> String {
    json!({"shape": shape.0, "para": p}).to_string()
}

/// A paragraph's text as its body carries it.
fn plain(p: &Paragraph) -> String {
    p.runs.iter().map(|r| r.text.as_str()).collect()
}

fn paragraphs_of(sh: &Shape) -> &[Paragraph] {
    sh.text.as_ref().map_or(&[], |t| t.paragraphs.as_slice())
}

/// A deck without its slides and its id counters: what must not change while shared.
fn deck_shape(p: &Presentation) -> Presentation {
    let mut q = p.clone();
    q.slides.clear();
    q.next_id = 0;
    q.author_next.clear();
    q.author_tag = 0;
    q
}

/// `runs` respelled to read `target`: the one contiguous change between their text and the
/// target is made inside the runs, so formatting stays on the text it was on and inserted text
/// takes the formatting of the run it lands in.
fn respell(runs: &[Run], target: &str) -> Vec<Run> {
    let current: String = runs.iter().map(|r| r.text.as_str()).collect();
    if current == target {
        return runs.to_vec();
    }
    let (at, removed, inserted) = splice(&current, target);
    let end = at + removed;
    let mut out: Vec<Run> = Vec::new();
    let mut pos = 0usize;
    let mut placed = inserted.is_empty();
    for r in runs {
        let (start, stop) = (pos, pos + r.text.len());
        pos = stop;
        let mut keep = String::new();
        if start < at {
            keep.push_str(&r.text[..at.min(stop) - start]);
        }
        let lands = !placed && at >= start && at <= stop && matches!(r.kind, RunKind::Text);
        if lands {
            keep.push_str(&inserted);
            placed = true;
        }
        if stop > end {
            keep.push_str(&r.text[end.max(start) - start..]);
        }
        if keep.is_empty() && !matches!(r.kind, RunKind::Text) && start >= at && stop <= end {
            continue;
        }
        if keep.is_empty() && matches!(r.kind, RunKind::Text) && !lands {
            continue;
        }
        out.push(Run { text: keep, ..r.clone() });
    }
    if !placed {
        let props = runs.last().map(|r| r.props.clone()).unwrap_or_default();
        out.push(Run { text: inserted, props, kind: RunKind::Text });
    }
    out
}

/// The one contiguous change from `before` to `after`: its byte index, how many bytes it
/// removes and the text it inserts, on character boundaries.
fn splice(before: &str, after: &str) -> (usize, usize, String) {
    let max = before.len().min(after.len());
    let mut pre = 0;
    while pre < max && before.as_bytes()[pre] == after.as_bytes()[pre] {
        pre += 1;
    }
    while pre > 0 && !(before.is_char_boundary(pre) && after.is_char_boundary(pre)) {
        pre -= 1;
    }
    let mut suf = 0;
    while suf < max - pre && before.as_bytes()[before.len() - 1 - suf] == after.as_bytes()[after.len() - 1 - suf] {
        suf += 1;
    }
    while suf > 0 && !(before.is_char_boundary(before.len() - suf) && after.is_char_boundary(after.len() - suf)) {
        suf -= 1;
    }
    (pre, before.len() - suf - pre, after[pre..after.len() - suf].to_string())
}

/// The body a text frame's paragraphs make.
fn body_of(sh: &Shape) -> Vec<BodyParagraph> {
    paragraphs_of(sh).iter().map(|p| BodyParagraph { para: p.pid.to_string(), text: plain(p), props: None }).collect()
}

/// Every record of a slide, its shapes and its comments: what a slide new to the shared document
/// needs.
fn slide_entries(s: &Slide) -> Vec<(String, String)> {
    let mut out = vec![(slide_key(s.id.0), slide_record(s))];
    for sh in &s.shapes {
        out.push((shape_key(sh.id.0), shape_record(s.id, sh)));
        out.extend(paragraphs_of(sh).iter().map(|p| (para_key(p.pid), para_record(sh.id, p))));
    }
    out.extend(all_comments(s).into_iter().map(|c| (comment_key(c.id), comment_record(s.id, c))));
    out
}

/// Writes that bring the shared document from `old` to `new`, with each write's update.
struct Capture<'a> {
    texts: &'a mut TextMerge,
    updates: Vec<Vec<u8>>,
}

impl Capture<'_> {
    fn push(&mut self, update: Option<Vec<u8>>) {
        self.updates.extend(update);
    }

    fn set(&mut self, key: &str, value: &str) {
        let u = self.texts.register_set(key, value);
        self.push(u);
    }

    /// A list brought to `want`: ids gone are removed, new ones inserted, the rest moved.
    fn list(&mut self, name: &str, want: &[String]) {
        for id in self.texts.list_ids(name) {
            if !want.contains(&id) {
                let u = self.texts.list_remove(name, &id);
                self.push(u);
            }
        }
        for (i, id) in want.iter().enumerate() {
            let u = if self.texts.list_ids(name).contains(id) {
                self.texts.list_move(name, id, i as u32)
            } else {
                self.texts.list_insert(name, i as u32, id)
            };
            self.push(u);
        }
    }

    /// A shape's text frame brought to `new` from what its body holds.
    fn body(&mut self, shape: ShapeId, new: &Shape) {
        let name = shape.0.to_string();
        let want = paragraphs_of(new);
        if self.texts.named_body_paragraphs(&name).is_empty() {
            let u = self.texts.seed_named_body(&name, &body_of(new));
            self.push(u);
        }
        let want_ids: Vec<String> = want.iter().map(|p| p.pid.to_string()).collect();
        // Paragraphs gone join the one before them; the first one cannot, so it stays and the
        // read-back below puts the deck's paragraphs on what the body holds.
        for p in self.texts.named_body_paragraphs(&name) {
            if !want_ids.contains(&p.para) {
                let u = self.texts.local_join(&p.para);
                self.push(u);
            }
        }
        // New paragraphs split off the paragraph before them: where its new text ends when that
        // is a prefix of what it held (Enter inside it), else at its end.
        let mut prev: Option<String> = None;
        for id in &want_ids {
            let held = self.texts.named_body_paragraphs(&name);
            if !held.iter().any(|h| &h.para == id) {
                let anchor = prev.clone().or_else(|| held.first().map(|h| h.para.clone()));
                if let Some(anchor) = anchor {
                    let anchor_text = self.texts.text(&anchor);
                    let anchor_want = want.iter().find(|q| q.pid.to_string() == anchor).map(plain).unwrap_or_default();
                    let at = if anchor_text.starts_with(&anchor_want) { anchor_want.len() } else { anchor_text.len() };
                    let u = self.texts.local_split(&anchor, at as u32, id, None);
                    self.push(u);
                }
            }
            prev = Some(id.clone());
        }
        for p in want {
            let u = self.texts.sync(&p.pid.to_string(), &plain(p));
            self.push(u);
            self.set(&para_key(p.pid), &para_record(shape, p));
        }
    }
}

impl Session {
    /// The bound presentation's shared session, while one is shared.
    pub fn collab_session(&self) -> Option<&craft_collab::CollabSession> {
        self.collab.as_ref().map(|c| &c.shared)
    }

    /// The same, mutable.
    pub fn collab_session_mut(&mut self) -> Option<&mut craft_collab::CollabSession> {
        self.collab.as_mut().map(|c| &mut c.shared)
    }

    /// The name a new comment carries: this author's name on the shared deck, else the
    /// author set in the preferences.
    pub fn comment_author(&self) -> String {
        match self.collab.as_ref().filter(|_| self.collab_on_active()) {
            Some(c) if !c.shared.name().is_empty() => c.shared.name().to_string(),
            _ => self.prefs.author.clone(),
        }
    }

    /// Whether a presentation is shared.
    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    /// The uid of the shared presentation.
    pub fn collab_doc(&self) -> Option<u64> {
        self.collab.as_ref().map(|c| c.doc)
    }

    /// Shares the active presentation through `shared` (a session the mesh wiring built). New ids
    /// carry this author's tag. A host gives every paragraph its id and seeds the shared document
    /// with the deck; a joiner's deck is replaced by the host's snapshot when it arrives.
    pub fn collab_install(&mut self, mut shared: craft_collab::CollabSession) -> Result<()> {
        let tag = craft_collab::author_tag(shared.author());
        let st = self.doc_mut()?;
        let doc = st.uid;
        let deck = Arc::make_mut(&mut st.doc);
        deck.set_author_tag(tag);
        if !shared.awaiting_snapshot() && !shared.awaiting_grant() {
            deck.ensure_pids();
            deck.ensure_comment_ids();
            let texts = shared.texts_mut();
            let mut updates = Vec::new();
            let mut entries = Vec::new();
            for s in &deck.slides {
                entries.extend(slide_entries(s));
            }
            updates.extend(texts.register_seed(&entries));
            let ids: Vec<String> = deck.slides.iter().map(|s| s.id.0.to_string()).collect();
            updates.extend(texts.list_seed(SLIDES, &ids));
            for s in &deck.slides {
                let z: Vec<String> = s.shapes.iter().map(|sh| sh.id.0.to_string()).collect();
                updates.extend(texts.list_seed(&z_list(s.id.0), &z));
                for sh in &s.shapes {
                    updates.extend(texts.seed_named_body(&sh.id.0.to_string(), &body_of(sh)));
                }
                updates.extend(texts.list_seed(&comments_list(s.id.0), &ids_of(&s.comments)));
                for c in &s.comments {
                    updates.extend(texts.list_seed(&replies_list(c.id), &ids_of(&c.replies)));
                }
            }
            texts.register_changed();
            texts.bodies_changed();
            texts.lists_changed();
            for u in updates {
                shared.send_text(String::new(), u, None);
            }
        }
        let last = st.doc.clone();
        self.collab = Some(CollabRuntime { shared, doc, last });
        Ok(())
    }

    /// Ends sharing (the presentation stays as it is).
    pub fn collab_leave(&mut self) {
        if let Some(i) = self.collab_bound()
            && let Some(st) = self.docs.get_mut(i)
        {
            Arc::make_mut(&mut st.doc).set_author_tag(0);
        }
        self.collab = None;
    }

    /// The realm membership status the app read for this person (the Share window shows it).
    pub fn collab_set_realm(&mut self, status: Value) {
        self.realm = Some(status);
    }

    /// The realm membership status, or a bare not-signed-in answer.
    pub fn collab_realm(&self) -> Value {
        self.realm.clone().unwrap_or_else(|| json!({"signedIn": false}))
    }

    /// The session's status for agents and the Share window.
    pub fn collab_stats(&self) -> Value {
        let Some(c) = self.collab.as_ref() else { return json!({"active": false}) };
        let mut stats = c.shared.stats();
        if let Some(obj) = stats.as_object_mut() {
            obj.insert("slides".into(), json!(c.shared.texts().list_ids(SLIDES).len()));
        }
        stats
    }

    fn collab_bound(&self) -> Option<usize> {
        let doc = self.collab.as_ref()?.doc;
        self.docs.iter().position(|d| d.uid == doc)
    }

    /// Whether the active presentation is the shared one.
    pub(crate) fn collab_on_active(&self) -> bool {
        self.collab_bound().is_some_and(|i| Some(i) == self.active)
    }

    /// After a local command changed the shared deck: shares what changed since the deck was last
    /// shared, one undo step for the whole command, or refuses the change (the caller puts the
    /// deck back) when it changed what cannot travel.
    pub(crate) fn collab_capture(&mut self) -> std::result::Result<(), String> {
        let Some(i) = self.collab_bound() else { return Ok(()) };
        let Some(c) = self.collab.as_mut() else { return Ok(()) };
        let Some(st) = self.docs.get_mut(i) else { return Ok(()) };
        if Arc::ptr_eq(&st.doc, &c.last) || st.interaction.is_some() {
            return Ok(());
        }
        let old = c.last.clone();
        if deck_shape(&st.doc) != deck_shape(&old) {
            let media = st.doc.media.len() != old.media.len();
            return Err(if media {
                "pictures and media aren't shared yet".into()
            } else {
                "this change isn't shared yet: only slides, their shapes and their text travel".into()
            });
        }
        let deck = Arc::make_mut(&mut st.doc);
        deck.ensure_pids();
        deck.ensure_comment_ids();
        let new = st.doc.clone();
        let texts = c.shared.texts_mut();
        texts.begin_step();
        let mut cap = Capture { texts, updates: Vec::new() };
        let ids: Vec<String> = new.slides.iter().map(|s| s.id.0.to_string()).collect();
        for s in &new.slides {
            let before = old.slides.iter().find(|o| o.id == s.id);
            if before.is_some_and(|b| Arc::ptr_eq(b, s)) {
                continue;
            }
            if before.is_none_or(|b| slide_record(b) != slide_record(s)) {
                cap.set(&slide_key(s.id.0), &slide_record(s));
            }
            for sh in &s.shapes {
                let was = before.and_then(|b| b.shapes.iter().find(|o| o.id == sh.id));
                if was.is_none_or(|w| shape_record(s.id, w) != shape_record(s.id, sh)) {
                    cap.set(&shape_key(sh.id.0), &shape_record(s.id, sh));
                }
                if was.is_none_or(|w| paragraphs_of(w) != paragraphs_of(sh)) {
                    cap.body(sh.id, sh);
                }
            }
            let z: Vec<String> = s.shapes.iter().map(|sh| sh.id.0.to_string()).collect();
            cap.list(&z_list(s.id.0), &z);
            let was: Vec<&Comment> = before.map(|b| all_comments(b)).unwrap_or_default();
            for cm in all_comments(s) {
                if was.iter().find(|w| w.id == cm.id).is_none_or(|w| comment_record(s.id, w) != comment_record(s.id, cm)) {
                    cap.set(&comment_key(cm.id), &comment_record(s.id, cm));
                }
            }
            if before.is_none_or(|b| ids_of(&b.comments) != ids_of(&s.comments)) {
                cap.list(&comments_list(s.id.0), &ids_of(&s.comments));
            }
            for cm in &s.comments {
                let held = before.and_then(|b| b.comments.iter().find(|w| w.id == cm.id));
                if held.is_none_or(|w| ids_of(&w.replies) != ids_of(&cm.replies)) {
                    cap.list(&replies_list(cm.id), &ids_of(&cm.replies));
                }
            }
        }
        cap.list(SLIDES, &ids);
        let updates = std::mem::take(&mut cap.updates);
        c.shared.texts_mut().end_step();
        for u in updates {
            c.shared.send_text(String::new(), u, None);
        }
        // Our own writes are in the changed lists too; read the deck back from the shared
        // document only where it holds something other than the deck (a paragraph that could
        // not be placed as the deck has it).
        let texts = c.shared.texts_mut();
        texts.register_changed();
        texts.bodies_changed();
        texts.lists_changed();
        let projected = project_deck(c.shared.texts(), &new);
        if projected.slides.iter().zip(&new.slides).any(|(a, b)| a != b) || projected.slides.len() != new.slides.len() {
            st.doc = Arc::new(projected);
            st.revision += 1;
        }
        c.last = st.doc.clone();
        Ok(())
    }

    /// Applies what the peers sent and shares this author's place (slide and selection). The app
    /// calls this once per frame while shared.
    pub fn collab_tick(&mut self) {
        let Some(events) = self.collab.as_mut().map(|c| c.shared.tick()) else { return };
        for event in events {
            self.collab_apply(event);
        }
        let cursor = self.collab_bound().and_then(|i| self.docs.get(i)).and_then(|d| {
            let slide = d.doc.slides.get(d.selection.slide)?;
            let shapes: Vec<u64> = d.selection.shapes.iter().map(|s| s.0).collect();
            Some(json!({"slide": slide.id.0, "shapes": shapes}).to_string())
        });
        if let Some(c) = self.collab.as_mut() {
            c.shared.share_cursor(cursor);
        }
    }

    fn collab_apply(&mut self, event: craft_collab::Inbound) {
        match event {
            craft_collab::Inbound::Text { update, .. } => {
                if self.collab_apply_text(&update).is_ok() {
                    self.collab_follow();
                }
            }
            // Decks travel as their shared document, never as replayed commands.
            craft_collab::Inbound::Command { .. } => {
                if let Some(c) = self.collab.as_mut() {
                    c.shared.note_refused();
                }
            }
            craft_collab::Inbound::SnapshotWanted => {
                let data = self.collab_bound().and_then(|i| self.docs.get(i)).and_then(|d| deckcraft_format::save(&d.doc).ok());
                if let (Some(data), Some(c)) = (data, self.collab.as_mut()) {
                    c.shared.answer_snapshot(data);
                }
            }
            craft_collab::Inbound::Snapshot { data, crdt } => {
                let joining = self.collab_session().is_some_and(craft_collab::CollabSession::awaiting_snapshot);
                if !joining {
                    return;
                }
                let Ok(mut deck) = deckcraft_format::load(&data) else { return };
                let tag = self.collab.as_ref().map_or(0, |c| craft_collab::author_tag(c.shared.author()));
                deck.set_author_tag(tag);
                let deck = Arc::new(deck);
                if let Some(i) = self.collab_bound()
                    && let Some(st) = self.docs.get_mut(i)
                {
                    st.doc = deck.clone();
                    st.history.undo.clear();
                    st.history.redo.clear();
                    st.revision += 1;
                    st.selection = crate::Selection::default();
                }
                if let Some(c) = self.collab.as_mut() {
                    c.shared.snapshot_loaded(&crdt);
                    let texts = c.shared.texts_mut();
                    texts.register_changed();
                    texts.bodies_changed();
                    texts.lists_changed();
                    c.last = deck;
                }
            }
        }
    }

    /// Applies a peer's update, carrying this author's caret and anchor along when they are in a
    /// text frame of the shared deck.
    fn collab_apply_text(&mut self, update: &[u8]) -> std::result::Result<(), String> {
        let at = self.collab_bound().and_then(|i| self.docs.get(i)).and_then(|d| {
            let t = d.selection.text.as_ref().filter(|t| !t.notes && t.cell.is_none())?;
            let paras = paragraphs_of(d.shape(t.shape)?).to_vec();
            let pos = |(p, ch): (usize, usize)| -> Option<(String, u32)> {
                let para = paras.get(p)?;
                let text = plain(para);
                let byte = text.char_indices().nth(ch).map_or(text.len(), |(b, _)| b);
                Some((para.pid.to_string(), byte as u32))
            };
            Some((t.shape, vec![pos(t.anchor)?, pos(t.caret)?]))
        });
        let Some(c) = self.collab.as_mut() else { return Ok(()) };
        let Some((shape, positions)) = at else {
            return c.shared.texts_mut().apply_doc_update(update);
        };
        let carried = c.shared.texts_mut().apply_update_carrying(update, &positions)?;
        // Back to (paragraph, char), against the frame as the shared document now holds it.
        let held = c.shared.texts().named_body_paragraphs(&shape.0.to_string());
        let back = |(pid, byte): &(String, u32)| -> Option<(usize, usize)> {
            let p = held.iter().position(|h| &h.para == pid)?;
            let text = &held.get(p)?.text;
            let byte = (*byte as usize).min(text.len());
            Some((p, text.get(..byte).map_or(0, |s| s.chars().count())))
        };
        if let (Some(i), Some(anchor), Some(caret)) = (self.collab_bound(), carried.first().and_then(back), carried.get(1).and_then(back))
            && let Some(st) = self.docs.get_mut(i)
            && let Some(t) = st.selection.text.as_mut()
        {
            t.anchor = anchor;
            t.caret = caret;
        }
        Ok(())
    }

    /// Brings the shared deck to what the shared document says wherever it changed (a peer's
    /// edit, an undo): the slide order, and each slide something in it changed.
    fn collab_follow(&mut self) {
        let Some(i) = self.collab_bound() else { return };
        let Some(c) = self.collab.as_mut() else { return };
        let Some(st) = self.docs.get_mut(i) else { return };
        let texts = c.shared.texts_mut();
        let keys = texts.register_changed();
        let bodies = texts.bodies_changed();
        let lists = texts.lists_changed();
        if keys.is_empty() && bodies.is_empty() && lists.is_empty() {
            return;
        }
        let texts = c.shared.texts();
        let mut dirty: BTreeSet<u64> = BTreeSet::new();
        let slide_of_shape = |shape: u64| -> Option<u64> {
            let record: Value = serde_json::from_str(&texts.register_get(&shape_key(shape))?).ok()?;
            record.get("slide")?.as_u64()
        };
        for key in &keys {
            let mut parts = key.splitn(2, '/');
            let (kind, id) = (parts.next(), parts.next().and_then(|s| s.parse::<u64>().ok()));
            match (kind, id) {
                (Some("s"), Some(id)) => {
                    dirty.insert(id);
                }
                (Some("h"), Some(id)) => {
                    dirty.extend(slide_of_shape(id));
                    // The slide it was on, if it moved.
                    dirty.extend(st.doc.slides.iter().filter(|s| s.shape(ShapeId(id)).is_some()).map(|s| s.id.0));
                }
                (Some("m"), Some(_)) => {
                    let slide = texts.register_get(key).and_then(|v| serde_json::from_str::<Value>(&v).ok()).and_then(|v| v.get("slide")?.as_u64());
                    dirty.extend(slide);
                }
                (Some("p"), Some(_)) => {
                    let shape = texts.register_get(key).and_then(|v| serde_json::from_str::<Value>(&v).ok()).and_then(|v| v.get("shape")?.as_u64());
                    dirty.extend(shape.and_then(slide_of_shape));
                }
                _ => {}
            }
        }
        dirty.extend(bodies.iter().filter_map(|b| b.parse::<u64>().ok()).filter_map(slide_of_shape));
        dirty.extend(lists.iter().filter_map(|l| l.strip_prefix("z/").or_else(|| l.strip_prefix("c/"))?.parse::<u64>().ok()));
        let slide_of_comment = |comment: u64| -> Option<u64> {
            let record: Value = serde_json::from_str(&texts.register_get(&comment_key(comment))?).ok()?;
            record.get("slide")?.as_u64()
        };
        dirty.extend(lists.iter().filter_map(|l| l.strip_prefix("r/")?.parse::<u64>().ok()).filter_map(slide_of_comment));
        let order = lists.iter().any(|l| l == SLIDES);
        let old = st.doc.clone();
        let mut deck = (*old).clone();
        let ids: Vec<u64> =
            if order { texts.list_ids(SLIDES).iter().filter_map(|s| s.parse().ok()).collect() } else { old.slides.iter().map(|s| s.id.0).collect() };
        deck.slides = ids
            .into_iter()
            .filter_map(|id| {
                let kept = old.slides.iter().find(|s| s.id.0 == id);
                match kept {
                    Some(s) if !dirty.contains(&id) => Some(s.clone()),
                    _ => project_slide(texts, id).map(Arc::new),
                }
            })
            .collect();
        st.doc = Arc::new(deck);
        st.revision += 1;
        crate::clamp_selection(st);
        c.last = st.doc.clone();
    }

    /// Undoes this author's most recent shared edit: true when there was one.
    pub(crate) fn collab_undo(&mut self) -> Result<bool> {
        let undone = self.collab.as_mut().and_then(|c| c.shared.undo_local());
        if undone.is_none() {
            return Ok(false);
        }
        self.collab_follow();
        Ok(true)
    }

    /// The peers to draw on the shared deck: name, colour, and the slide (by index) and shapes
    /// they have selected.
    pub fn collab_peers(&self) -> Vec<(String, [u8; 3], Option<(usize, Vec<ShapeId>)>)> {
        let (Some(c), Some(d)) = (self.collab.as_ref(), self.collab_bound().and_then(|i| self.docs.get(i))) else { return Vec::new() };
        c.shared
            .peers()
            .map(|p| {
                let at = p.position.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| {
                    let slide = v.get("slide")?.as_u64()?;
                    let index = d.doc.slides.iter().position(|s| s.id.0 == slide)?;
                    let shapes = v.get("shapes")?.as_array()?.iter().filter_map(|x| x.as_u64().map(ShapeId)).collect();
                    Some((index, shapes))
                });
                (p.name.clone(), p.color, at)
            })
            .collect()
    }

    /// Refuses `id` on the shared deck when it has no shared form (redo), or runs undo as this
    /// author's own: `Some` when it was handled here.
    pub(crate) fn collab_intercept(&mut self, id: &str) -> Option<Result<Value>> {
        if !self.collab_on_active() {
            return None;
        }
        match id {
            "edit.undo" => Some(self.collab_undo().map(|done| json!({"undone": done}))),
            "edit.redo" => Some(Err(EngineError::Disabled(id.to_string(), "redo isn't shared".into()))),
            _ => None,
        }
    }

    /// After a command on the shared deck: shares it, or puts the deck back as it was last shared
    /// and says why.
    pub(crate) fn collab_after(&mut self, id: &str) -> Result<()> {
        if !self.collab_on_active() {
            return Ok(());
        }
        let Err(why) = self.collab_capture() else { return Ok(()) };
        if let (Some(i), Some(last)) = (self.collab_bound(), self.collab.as_ref().map(|c| c.last.clone()))
            && let Some(st) = self.docs.get_mut(i)
        {
            st.doc = last;
            st.revision += 1;
            st.history.undo.pop();
            crate::clamp_selection(st);
        }
        Err(EngineError::Disabled(id.to_string(), why))
    }
}

/// A slide as the shared document holds it: its record, and its shapes in their order.
fn project_slide(texts: &TextMerge, id: u64) -> Option<Slide> {
    let mut slide: Slide = serde_json::from_str(&texts.register_get(&slide_key(id))?).ok()?;
    slide.shapes = texts.list_ids(&z_list(id)).iter().filter_map(|s| s.parse::<u64>().ok()).filter_map(|sh| project_shape(texts, sh)).collect();
    slide.comments = texts
        .list_ids(&comments_list(id))
        .iter()
        .filter_map(|c| c.parse::<u64>().ok())
        .filter_map(|c| {
            let mut thread = project_comment(texts, c)?;
            thread.replies =
                texts.list_ids(&replies_list(c)).iter().filter_map(|r| r.parse::<u64>().ok()).filter_map(|r| project_comment(texts, r)).collect();
            Some(thread)
        })
        .collect();
    Some(slide)
}

/// A comment as the shared document holds it, without its replies.
fn project_comment(texts: &TextMerge, id: u64) -> Option<Comment> {
    let record: Value = serde_json::from_str(&texts.register_get(&comment_key(id))?).ok()?;
    serde_json::from_value(record.get("comment")?.clone()).ok()
}

/// A shape as the shared document holds it: its record, with its text frame's paragraphs from
/// its body, each laid over its own record.
fn project_shape(texts: &TextMerge, id: u64) -> Option<Shape> {
    let record: Value = serde_json::from_str(&texts.register_get(&shape_key(id))?).ok()?;
    let mut shape: Shape = serde_json::from_value(record.get("shape")?.clone()).ok()?;
    if let Some(t) = shape.text.as_mut() {
        t.paragraphs = texts
            .named_body_paragraphs(&id.to_string())
            .into_iter()
            .filter_map(|bp| {
                let pid = bp.para.parse::<u64>().ok()?;
                let mut p: Paragraph = texts
                    .register_get(&para_key(pid))
                    .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                    .and_then(|v| serde_json::from_value(v.get("para")?.clone()).ok())
                    .unwrap_or_default();
                p.runs = respell(&p.runs, &bp.text);
                p.pid = pid;
                Some(p)
            })
            .collect();
    }
    Some(shape)
}

/// The deck as the shared document holds it, around `base`'s deck-wide parts.
fn project_deck(texts: &TextMerge, base: &Presentation) -> Presentation {
    let mut deck = base.clone();
    deck.slides =
        texts.list_ids(SLIDES).iter().filter_map(|s| s.parse::<u64>().ok()).filter_map(|id| project_slide(texts, id).map(Arc::new)).collect();
    deck
}

#[cfg(test)]
mod tests {
    use craft_collab::CollabSession;
    use craft_collab::mem::MemoryTransport;
    use serde_json::json;

    use super::*;

    const A: &str = "aaaaaaaa11111111";
    const B: &str = "bbbbbbbb22222222";

    /// A shares a new deck, B joins with a blank one and catches up.
    fn two_authors() -> (Session, Session) {
        let (ta, tb) = MemoryTransport::pair(A, B);
        let mut a = Session::with_new();
        a.collab_install(CollabSession::host("deck", "Ann", Box::new(ta), &[])).unwrap();
        let mut b = Session::with_new();
        b.collab_install(CollabSession::join("deck", "Bob", Box::new(tb))).unwrap();
        settle(&mut a, &mut b);
        (a, b)
    }

    fn settle(a: &mut Session, b: &mut Session) {
        for _ in 0..4 {
            b.collab_tick();
            a.collab_tick();
        }
    }

    fn deck(s: &Session) -> Arc<Presentation> {
        s.doc().unwrap().doc.clone()
    }

    /// The deck as both authors must agree on it: slide ids, and per slide its shapes' ids and
    /// text.
    fn reading(s: &Session) -> Vec<(u64, Vec<(u64, String)>)> {
        deck(s)
            .slides
            .iter()
            .map(|sl| (sl.id.0, sl.shapes.iter().map(|sh| (sh.id.0, paragraphs_of(sh).iter().map(plain).collect::<Vec<_>>().join("\n"))).collect()))
            .collect()
    }

    fn type_into(s: &mut Session, slide: usize, shape: usize, text: &str) {
        s.execute("slide.go", &json!({"index": slide})).unwrap();
        let id = s.doc().unwrap().doc.slides[slide].shapes[shape].id.0;
        s.execute("text.edit", &json!({"id": id, "end": true})).unwrap();
        s.execute("text.insert", &json!({"text": text})).unwrap();
    }

    #[test]
    fn a_joiner_receives_the_deck_and_both_edit_it() {
        let (mut a, mut b) = two_authors();
        assert_eq!(reading(&a), reading(&b), "the snapshot arrived");
        type_into(&mut a, 0, 0, "Quarterly review");
        b.execute("slide.new", &json!({})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        assert_eq!(deck(&b).slides.len(), 2);
        assert!(reading(&b)[0].1[0].1.contains("Quarterly review"));
    }

    #[test]
    fn two_authors_typing_in_one_frame_keep_both_texts() {
        let (mut a, mut b) = two_authors();
        type_into(&mut a, 0, 0, "Ann ");
        settle(&mut a, &mut b);
        type_into(&mut a, 0, 0, "first");
        type_into(&mut b, 0, 0, "Bob");
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        let title = &reading(&a)[0].1[0].1;
        assert!(title.contains("first") && title.contains("Bob"), "{title}");
    }

    #[test]
    fn enter_in_a_frame_travels_as_paragraphs() {
        let (mut a, mut b) = two_authors();
        type_into(&mut a, 0, 1, "one\ntwo\nthree");
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        assert_eq!(reading(&b)[0].1[1].1, "one\ntwo\nthree");
    }

    #[test]
    fn slides_added_at_once_get_distinct_ids_and_one_order() {
        let (mut a, mut b) = two_authors();
        a.execute("slide.new", &json!({})).unwrap();
        b.execute("slide.new", &json!({})).unwrap();
        b.execute("slide.new", &json!({})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        let ids: BTreeSet<u64> = deck(&a).slides.iter().map(|s| s.id.0).collect();
        assert_eq!(ids.len(), 4, "every slide kept, none collided");
    }

    #[test]
    fn a_reorder_and_a_delete_converge() {
        let (mut a, mut b) = two_authors();
        a.execute("slide.new", &json!({})).unwrap();
        a.execute("slide.new", &json!({})).unwrap();
        settle(&mut a, &mut b);
        a.execute("slide.move", &json!({"from": 2, "to": 0})).unwrap();
        b.execute("slide.delete", &json!({"index": 1})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        assert_eq!(deck(&a).slides.len(), 2);
    }

    #[test]
    fn a_shape_inserted_moved_and_deleted_travels() {
        let (mut a, mut b) = two_authors();
        a.execute("shape.insert", &json!({"preset": "rect", "rect": [100, 100, 200, 100]})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        let id = *a.doc().unwrap().selection.shapes.first().unwrap();
        assert!(deckcraft_model::split_id(id.0).0 != 0, "the new shape's id is author-scoped");
        a.execute("shape.move", &json!({"dx": 50, "dy": 0})).unwrap();
        settle(&mut a, &mut b);
        let x = |s: &Session| s.doc().unwrap().shape(id).and_then(|sh| sh.xfrm).map(|x| x.x);
        assert_eq!(x(&a), x(&b));
        b.execute("edit.delete", &json!({"ids": [id.0]})).unwrap();
        settle(&mut a, &mut b);
        assert!(a.doc().unwrap().shape(id).is_none(), "the delete travelled");
        assert_eq!(reading(&a), reading(&b));
    }

    #[test]
    fn bring_to_front_travels_as_z_order() {
        let (mut a, mut b) = two_authors();
        a.execute("slide.new", &json!({"layout": "blank"})).unwrap();
        a.execute("shape.insert", &json!({"preset": "rect", "rect": [0, 0, 100, 100]})).unwrap();
        a.execute("shape.insert", &json!({"preset": "ellipse", "rect": [50, 50, 100, 100]})).unwrap();
        settle(&mut a, &mut b);
        let first = deck(&a).slides[1].shapes[0].id.0;
        a.execute("arrange.bringToFront", &json!({"ids": [first]})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
        assert_eq!(deck(&b).slides[1].shapes.last().map(|s| s.id.0), Some(first));
    }

    #[test]
    fn undo_takes_back_only_this_authors_command_as_one_step() {
        let (mut a, mut b) = two_authors();
        b.execute("slide.new", &json!({})).unwrap();
        a.execute("shape.insert", &json!({"preset": "rect", "rect": [0, 0, 100, 100]})).unwrap();
        settle(&mut a, &mut b);
        let before = deck(&a).slides[0].shapes.len();
        a.execute("edit.undo", &json!({})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(deck(&a).slides[0].shapes.len(), before - 1, "one undo took the inserted shape back");
        assert_eq!(deck(&b).slides.len(), 2, "B's slide stays");
        assert_eq!(reading(&a), reading(&b));
        assert!(a.execute("edit.redo", &json!({})).is_err(), "redo isn't shared");
    }

    #[test]
    fn a_change_outside_the_slides_is_refused_and_leaves_the_deck() {
        let (mut a, mut b) = two_authors();
        let was = deck(&a);
        let r = a.execute("section.add", &json!({"name": "Part one"}));
        if r.is_ok() {
            panic!("a section change went through");
        }
        assert_eq!(deck(&a).sections, was.sections);
        settle(&mut a, &mut b);
        assert_eq!(reading(&a), reading(&b));
    }

    #[test]
    fn a_peer_sees_this_authors_slide_and_selection() {
        let (mut a, mut b) = two_authors();
        a.execute("slide.new", &json!({})).unwrap();
        let id = deck(&a).slides[1].shapes[0].id;
        a.execute("edit.select", &json!({"ids": [id.0]})).unwrap();
        settle(&mut a, &mut b);
        let peers = b.collab_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].2.as_ref().map(|(slide, _)| *slide), Some(1));
    }

    /// Every comment thread on a slide as both authors must agree on it: id, author, text,
    /// resolved, and its replies the same way.
    fn threads(s: &Session, slide: usize) -> Vec<(u64, String, String, bool, Vec<(u64, String, String)>)> {
        deck(s).slides[slide]
            .comments
            .iter()
            .map(|c| {
                (c.id, c.author.clone(), c.text.clone(), c.resolved, c.replies.iter().map(|r| (r.id, r.author.clone(), r.text.clone())).collect())
            })
            .collect()
    }

    #[test]
    fn two_comments_at_once_on_one_slide_are_both_kept_with_their_authors() {
        let (mut a, mut b) = two_authors();
        a.execute("comment.add", &json!({"text": "from Ann"})).unwrap();
        b.execute("comment.add", &json!({"text": "from Bob"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(threads(&a, 0), threads(&b, 0));
        let t = threads(&a, 0);
        assert_eq!(t.len(), 2, "neither comment was lost: {t:?}");
        assert_ne!(t[0].0, t[1].0, "distinct ids");
        let mut by: Vec<(String, String)> = t.iter().map(|c| (c.1.clone(), c.2.clone())).collect();
        by.sort();
        assert_eq!(by, vec![("Ann".into(), "from Ann".into()), ("Bob".into(), "from Bob".into())], "each comment carries its author's name");
    }

    #[test]
    fn a_reply_and_a_resolve_at_once_on_one_thread_both_land() {
        let (mut a, mut b) = two_authors();
        let shape = deck(&a).slides[0].shapes[0].id.0;
        a.execute("comment.add", &json!({"text": "Is this right?", "id": shape})).unwrap();
        settle(&mut a, &mut b);
        b.execute("comment.reply", &json!({"index": 0, "text": "Yes"})).unwrap();
        a.execute("comment.resolve", &json!({"index": 0})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(threads(&a, 0), threads(&b, 0));
        let t = &threads(&b, 0)[0];
        assert!(t.3, "the resolve travelled");
        assert_eq!(
            t.4.iter().map(|r| (r.1.as_str(), r.2.as_str())).collect::<Vec<_>>(),
            vec![("Bob", "Yes")],
            "the reply travelled under its author"
        );
        assert_eq!(deck(&b).slides[0].comments[0].shape, Some(ShapeId(shape)), "still on its shape");
    }

    #[test]
    fn two_replies_at_once_are_both_kept() {
        let (mut a, mut b) = two_authors();
        a.execute("comment.add", &json!({"text": "Thoughts?"})).unwrap();
        settle(&mut a, &mut b);
        a.execute("comment.reply", &json!({"index": 0, "text": "Ann adds"})).unwrap();
        b.execute("comment.reply", &json!({"index": 0, "text": "Bob adds"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(threads(&a, 0), threads(&b, 0));
        assert_eq!(threads(&a, 0)[0].4.len(), 2, "{:?}", threads(&a, 0));
    }

    #[test]
    fn undo_takes_back_only_this_authors_comment() {
        let (mut a, mut b) = two_authors();
        b.execute("comment.add", &json!({"text": "Bob's"})).unwrap();
        settle(&mut a, &mut b);
        a.execute("comment.add", &json!({"text": "Ann's"})).unwrap();
        settle(&mut a, &mut b);
        a.execute("edit.undo", &json!({})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(threads(&a, 0), threads(&b, 0));
        assert_eq!(threads(&b, 0).iter().map(|c| c.2.as_str()).collect::<Vec<_>>(), vec!["Bob's"]);
    }

    #[test]
    fn a_comment_from_before_sharing_reaches_the_joiner() {
        let (ta, tb) = MemoryTransport::pair(A, B);
        let mut a = Session::with_new();
        a.execute("comment.add", &json!({"text": "written alone"})).unwrap();
        a.collab_install(CollabSession::host("deck", "Ann", Box::new(ta), &[])).unwrap();
        let mut b = Session::with_new();
        b.collab_install(CollabSession::join("deck", "Bob", Box::new(tb))).unwrap();
        settle(&mut a, &mut b);
        b.execute("comment.reply", &json!({"index": 0, "text": "seen"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(threads(&a, 0), threads(&b, 0));
        assert_eq!(threads(&a, 0)[0].4.len(), 1);
    }

    #[test]
    fn respell_keeps_formatting_on_its_text() {
        let bold = deckcraft_model::text::RunProps { bold: Some(true), ..Default::default() };
        let runs = vec![Run::new("Hello "), Run { text: "world".into(), props: bold.clone(), kind: RunKind::Text }];
        let out = respell(&runs, "Hello big world");
        assert_eq!(out.iter().map(|r| r.text.as_str()).collect::<String>(), "Hello big world");
        assert_eq!((out[0].text.as_str(), out[1].text.as_str()), ("Hello big ", "world"), "the insert went into the run it landed in");
        assert_eq!(out[1].props, bold, "the bold run is still bold");
        let out = respell(&runs, "Hello wonderful world");
        assert_eq!(out.iter().map(|r| r.text.as_str()).collect::<String>(), "Hello wonderful world");
        assert_eq!(respell(&runs, "").iter().map(|r| r.text.as_str()).collect::<String>(), "");
        assert_eq!(respell(&[], "new").iter().map(|r| r.text.as_str()).collect::<String>(), "new");
    }
}
