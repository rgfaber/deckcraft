use super::*;
use crate::resolve::Ctx;

#[test]
fn new_presentation_is_valid() {
    let p = Presentation::default();
    assert_eq!(p.slides.len(), 1);
    assert_eq!(p.masters.len(), 1);
    assert_eq!(p.masters[0].layouts.len(), 11);
    assert!(p.validate().is_empty(), "{:?}", p.validate());
    let s = &p.slides[0];
    let (m, l) = p.master_for(s).unwrap();
    assert_eq!(l.unwrap().kind, LayoutType::Title);
    assert_eq!(m.theme.name, "DeckCraft");
    // Title slide has a centred title and a subtitle.
    let kinds: Vec<_> = s.shapes.iter().filter_map(|x| x.ph_type()).collect();
    assert_eq!(kinds, vec![PhType::CtrTitle, PhType::SubTitle]);
}

#[test]
fn placeholder_inherits_position_and_styles() {
    let p = Presentation::default();
    let s = &p.slides[0];
    let ctx = Ctx::for_slide(&p, s).unwrap();
    let title = &s.shapes[0];
    let x = resolve::xfrm(&ctx, title);
    assert!((x.x - 120.0).abs() < 1e-9 && (x.w - 720.0).abs() < 1e-9);
    let lvl = resolve::level_style(&ctx, title, 0);
    assert_eq!(lvl.run.size, Some(60.0)); // layout overrides the master's 44
    assert_eq!(lvl.para.align, Some(text::Align::Center));
    assert_eq!(resolve::font_family(&ctx, &lvl.run), "Inter");
    let body = resolve::body(&ctx, title);
    assert_eq!(body.anchor, Some(text::Anchor::Bottom));
}

#[test]
fn body_levels_from_master() {
    let mut p = Presentation::default();
    let lay = defaults::layout_of_kind(&p, LayoutType::TitleAndContent).unwrap();
    let s = defaults::new_slide(&mut p, lay);
    let ctx = Ctx::for_slide(&p, &s).unwrap();
    let body = &s.shapes[1];
    assert_eq!(body.ph_type(), Some(PhType::Obj));
    let l0 = resolve::level_style(&ctx, body, 0);
    let l1 = resolve::level_style(&ctx, body, 1);
    assert_eq!(l0.run.size, Some(28.0));
    assert_eq!(l1.run.size, Some(24.0));
    assert!(matches!(l0.para.bullet, Some(text::Bullet::Char { .. })));
}

#[test]
fn shape_style_fill_resolves_through_theme() {
    let p = Presentation::default();
    let s = &p.slides[0];
    let ctx = Ctx::for_slide(&p, s).unwrap();
    let sh = Shape { style: Some(ShapeStyle::accent(SchemeSlot::Accent1)), xfrm: Some(Xfrm::new(0.0, 0.0, 10.0, 10.0)), ..Default::default() };
    let (f, ph) = resolve::fill(&ctx, &sh);
    let Some(Fill::Solid { color }) = f else { panic!("expected solid") };
    assert_eq!(ctx.color(&color, ph), p.masters[0].theme.colors.get(SchemeSlot::Accent1));
    let (l, ph) = resolve::line(&ctx, &sh);
    assert!(l.width.is_some());
    assert!(ph.is_some());
}

#[test]
fn serde_roundtrip() {
    let p = Presentation::default();
    let j = serde_json::to_string(&p).unwrap();
    let q: Presentation = serde_json::from_str(&j).unwrap();
    assert_eq!(p, q);
    // Unknown and missing fields are tolerated.
    let q: Presentation = serde_json::from_str(r#"{"slides":[],"zzz":1}"#).unwrap();
    assert!(q.slides.is_empty());
}

#[test]
fn ids_are_unique_and_fix_next_id() {
    let mut p = Presentation::default();
    let a = p.alloc_id();
    let b = p.alloc_id();
    assert!(b > a);
    p.next_id = 0;
    p.fix_next_id();
    assert!(u64::from(p.next_id) >= b.min(u64::from(p.next_id)));
    let c = p.alloc_id();
    let mut all = vec![];
    for s in &p.slides {
        all.push(s.id.0);
        walk(&s.shapes, &mut |sh, _| all.push(sh.id.0));
    }
    assert!(!all.contains(&c));
}

/// Every id in a deck: slides, shapes, masters, layouts and media.
fn every_id(p: &Presentation) -> Vec<u64> {
    let mut ids = vec![];
    for s in &p.slides {
        ids.push(s.id.0);
        walk(&s.shapes, &mut |sh, _| ids.push(sh.id.0));
    }
    for m in &p.masters {
        ids.push(m.id.0);
        walk(&m.shapes, &mut |sh, _| ids.push(sh.id.0));
        for l in &m.layouts {
            ids.push(l.id.0);
            walk(&l.shapes, &mut |sh, _| ids.push(sh.id.0));
        }
    }
    ids.extend(p.media.iter().map(|m| m.id.0));
    ids
}

/// Two authors add slides, placeholders, layouts and media to their own copy of one deck.
fn author_edits(p: &mut Presentation, tag: u32) {
    p.set_author_tag(tag);
    let layout = p.masters[0].layouts[1].id;
    for _ in 0..3 {
        let s = defaults::new_slide(p, layout);
        p.slides.push(Arc::new(s));
    }
    let id = LayoutId(p.alloc_id());
    Arc::make_mut(&mut p.masters[0]).layouts.push(Layout { id, name: format!("Mine {tag}"), ..Default::default() });
    p.add_media(&format!("{tag}.png"), "image/png", vec![tag as u8; 8]);
}

#[test]
fn two_authors_never_mint_the_same_id() {
    let base = Presentation::default();
    let before = every_id(&base);
    let (mut a, mut b) = (base.clone(), base.clone());
    author_edits(&mut a, 1);
    author_edits(&mut b, 2);
    let mut minted: Vec<u64> = every_id(&a).into_iter().chain(every_id(&b)).filter(|i| !before.contains(i)).collect();
    let n = minted.len();
    assert_eq!(n, 2 * (3 * 3 + 1 + 1), "three slides of three ids, a layout and a media item each");
    minted.sort_unstable();
    minted.dedup();
    assert_eq!(minted.len(), n, "an id minted by both authors");
    assert!(minted.iter().all(|&i| i < 1 << 53), "ids stay exact as JSON numbers");
    assert!(minted.iter().all(|&i| matches!(split_id(i), (1 | 2, _))));
}

#[test]
fn the_solo_author_mints_as_before() {
    let mut p = Presentation::default();
    let last = p.next_id;
    assert_eq!(p.alloc_id(), u64::from(last) + 1);
    assert!(p.author_next.is_empty());
}

#[test]
fn a_counter_is_never_reused_after_a_delete() {
    let mut p = Presentation::default();
    p.set_author_tag(7);
    let first = p.alloc_id();
    let id = SlideId(p.alloc_id());
    p.slides.push(Arc::new(Slide { id, ..Default::default() }));
    p.slides.retain(|s| s.id != id);
    p.fix_next_id();
    let next = p.alloc_id();
    assert_eq!(split_id(first), (7, 1));
    assert_eq!(split_id(next), (7, 3), "the deleted slide's id is not minted again");
}

#[test]
fn fix_next_id_counts_each_tag_on_its_own() {
    let mut p = Presentation::default();
    let solo = p.next_id;
    p.slides.push(Arc::new(Slide { id: SlideId(author_id(5, 40)), ..Default::default() }));
    p.fix_next_id();
    assert_eq!(p.next_id, solo, "an author's ids do not move the solo counter");
    assert_eq!(p.author_next.get(&5), Some(&40));
    p.set_author_tag(5);
    assert_eq!(p.alloc_id(), author_id(5, 41));
}

#[test]
fn a_tag_wider_than_21_bits_is_masked() {
    let mut p = Presentation::default();
    p.set_author_tag(u32::MAX);
    assert!(p.alloc_id() < 1 << 53);
}

#[test]
fn group_find_and_walk() {
    let child = Shape { id: ShapeId(900), ..Default::default() };
    let g = Shape { id: ShapeId(901), kind: ShapeKind::Group { children: vec![child], child: Xfrm::new(0.0, 0.0, 1.0, 1.0) }, ..Default::default() };
    let mut slide = Slide { shapes: vec![g], ..Default::default() };
    assert!(slide.shape(ShapeId(900)).is_some());
    assert!(slide.shape_mut(ShapeId(900)).is_some());
    let mut n = 0;
    walk(&slide.shapes, &mut |_, _| n += 1);
    assert_eq!(n, 2);
}

#[test]
fn text_body_helpers() {
    let mut t = TextBody::from_text("Hello\nWorld");
    assert_eq!(t.text(), "Hello\nWorld");
    assert_eq!(t.char_len(), 11);
    t.paragraphs[0].runs.push(Run::new(" there"));
    t.paragraphs[0].normalize();
    assert_eq!(t.paragraphs[0].runs.len(), 1);
    assert_eq!(t.paragraphs[0].text(), "Hello there");
}

#[test]
fn builtin_themes_have_distinct_names() {
    let t = theme::builtin_themes();
    let mut names: Vec<_> = t.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), t.len());
    assert!(t.len() >= 8);
}

#[test]
fn four_by_three_master_scales() {
    let p = defaults::blank_presentation(defaults::STANDARD, Theme::default(), true);
    let m = &p.masters[0];
    let title = m.shapes.iter().find(|s| s.ph_type() == Some(PhType::Title)).unwrap();
    let x = title.xfrm.unwrap();
    assert!(x.x + x.w <= 720.0 + 1e-6);
}

fn pids(p: &Presentation) -> Vec<u64> {
    let mut out = vec![];
    for s in &p.slides {
        walk(&s.shapes, &mut |sh, _| out.extend(sh.text.iter().flat_map(|t| t.paragraphs.iter().map(|q| q.pid))));
    }
    out
}

#[test]
fn ensure_pids_gives_every_paragraph_its_own_id() {
    let mut p = Presentation::default();
    p.set_author_tag(4);
    let body = TextBody { paragraphs: vec![text::Paragraph::new("one"), text::Paragraph::new("two")], ..Default::default() };
    let group = Shape {
        kind: ShapeKind::Group { children: vec![Shape { text: Some(body.clone()), ..Default::default() }], child: Xfrm::new(0.0, 0.0, 1.0, 1.0) },
        ..Default::default()
    };
    Arc::make_mut(&mut p.slides[0]).shapes.extend([Shape { text: Some(body), ..Default::default() }, group]);
    p.ensure_pids();
    let ids = pids(&p);
    assert!(ids.len() >= 4 && ids.iter().all(|&i| i != 0));
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
    assert!(ids.iter().all(|&i| split_id(i).0 == 4), "paragraph ids are author-scoped");
    let before = ids.clone();
    p.ensure_pids();
    assert_eq!(pids(&p), before, "ids that are set stay");
}

#[test]
fn ensure_pids_renumbers_a_copied_paragraph_and_keeps_the_first() {
    let mut p = Presentation::default();
    Arc::make_mut(&mut p.slides[0]).shapes[0].text = Some(TextBody::from_text("title"));
    p.ensure_pids();
    let slide = Arc::make_mut(&mut p.slides[0]);
    let t = slide.shapes[0].text.as_mut().unwrap();
    t.paragraphs.push(t.paragraphs[0].clone());
    let first = t.paragraphs[0].pid;
    p.ensure_pids();
    let t = p.slides[0].shapes[0].text.as_ref().unwrap();
    assert_eq!(t.paragraphs[0].pid, first);
    assert_ne!(t.paragraphs[1].pid, first);
}

#[test]
fn ensure_pids_leaves_a_finished_slide_shared() {
    let mut p = Presentation::default();
    let layout = p.masters[0].layouts[1].id;
    let mut s = defaults::new_slide(&mut p, layout);
    s.shapes[0].text = Some(TextBody::from_text("second"));
    p.slides.push(Arc::new(s));
    p.ensure_pids();
    let snapshot = p.slides.clone();
    Arc::make_mut(&mut p.slides[1]).shapes[0].text.as_mut().unwrap().paragraphs[0].pid = 0;
    p.ensure_pids();
    assert!(Arc::ptr_eq(&p.slides[0], &snapshot[0]), "an untouched slide is not copied");
    assert!(!Arc::ptr_eq(&p.slides[1], &snapshot[1]));
}
