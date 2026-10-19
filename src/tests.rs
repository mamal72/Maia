//! Unit tests for the core (non-UI) logic.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::commands::{self, Cmd, Keymap};
use crate::config::{SortKey, ViewSettings};
use crate::fuzzy;
use crate::ops::{self, Conflict, OpKind};
use crate::panel::{Node, Side};
use crate::rename::{Outcome, Rename};
use crate::tab::{self, Tab, ViewCtx};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("maia-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn touch(p: &Path, content: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

#[test]
fn fuzzy_ranks_prefix_and_boundaries() {
    let q = fuzzy::Query::new("dl");
    assert!(fuzzy::score(&q, "downloads").is_some());
    assert!(fuzzy::score(&q, "xyz").is_none());
    let pre = fuzzy::score(&fuzzy::Query::new("doc"), "documents").unwrap();
    let mid = fuzzy::score(&fuzzy::Query::new("doc"), "my_old_doc_backup").unwrap();
    assert!(pre > mid, "prefix should outrank middle: {pre} vs {mid}");
    let (_, pos) = fuzzy::positions(&fuzzy::Query::new("rmd"), "README.md").unwrap();
    assert_eq!(pos.len(), 3);
    assert_eq!(pos[0], 0);
}

#[test]
fn natural_sort_orders_numbers() {
    let mut v = vec!["file10", "file2", "file1", "File3", "file02"];
    v.sort_by(|a, b| tab::natural_cmp(&a.to_lowercase(), &b.to_lowercase()));
    assert_eq!(v, vec!["file1", "file2", "file02", "File3", "file10"]);
}

#[test]
fn rename_selects_stem_and_mirrors_edits() {
    let items = vec![(0, PathBuf::from("/x/01 alpha.txt"), "01 alpha.txt".to_string()), (1, PathBuf::from("/x/02 beta.txt"), "02 beta.txt".to_string())];
    let mut r = Rename::new(items, false);
    assert_eq!(r.fields[0].selection(), (0, 8)); // stem only
    // Home, then shift+right twice selects "01"/"02"; delete removes them everywhere.
    use egui::{Event, Key, Modifiers};
    let key = |k: Key, m: Modifiers| Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: m };
    let mut clip = None;
    let ev = vec![key(Key::Home, Modifiers::NONE), key(Key::ArrowRight, Modifiers::SHIFT), key(Key::ArrowRight, Modifiers::SHIFT), key(Key::Delete, Modifiers::NONE)];
    assert_eq!(r.handle(&ev, &mut clip), Outcome::Continue);
    assert_eq!(r.fields[0].text_string(), " alpha.txt");
    assert_eq!(r.fields[1].text_string(), " beta.txt");
    r.insert_incremental_id();
    assert_eq!(r.fields[0].text_string(), "1 alpha.txt");
    assert_eq!(r.fields[1].text_string(), "2 beta.txt");
    // Multi-line paste distributes lines.
    r.select_all();
    r.paste("a.txt\nb.txt");
    assert_eq!(r.fields[0].text_string(), "a.txt");
    assert_eq!(r.fields[1].text_string(), "b.txt");
    let ev = vec![key(Key::Enter, Modifiers::NONE)];
    assert_eq!(r.handle(&ev, &mut clip), Outcome::Commit);
}

#[test]
fn rename_detects_conflicts_and_swaps_safely() {
    let d = tmpdir("rename");
    touch(&d.join("a.txt"), "A");
    touch(&d.join("b.txt"), "B");
    touch(&d.join("c.txt"), "C");
    // Swap a <-> b (allowed: both are being renamed).
    let items = vec![(0, d.join("a.txt"), "a.txt".into()), (1, d.join("b.txt"), "b.txt".into())];
    let mut r = Rename::new(items, false);
    r.select_all();
    r.paste("b.txt\na.txt");
    assert!(r.conflicts().is_empty());
    let errs = crate::rename::apply(&r.plan());
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "B");
    assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "A");
    // Renaming onto an existing, unrelated file is a conflict.
    let mut r = Rename::new(vec![(0, d.join("a.txt"), "a.txt".into())], false);
    r.select_all();
    r.insert_all("c.txt");
    assert!(!r.conflicts().is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hotkeys_parse_and_lookup() {
    let b = commands::parse_binding("Ctrl+Shift+P").unwrap();
    assert_eq!(commands::binding_to_string(&b), "Ctrl+Shift+P");
    let seq = commands::parse_binding("G, H").unwrap();
    assert_eq!(seq.len(), 2);
    let km = Keymap::build(&Default::default());
    let (hit, _) = km.lookup(&commands::parse_binding("Ctrl+Shift+P").unwrap());
    assert_eq!(hit, Some(Cmd::Palette));
    let (hit, _) = km.lookup(&commands::parse_binding("Ctrl+E").unwrap());
    assert_eq!(hit, Some(Cmd::ToggleFlatten));
    // Every default binding parses, and ids are unique.
    let mut ids = HashSet::new();
    for &c in commands::ALL {
        assert!(ids.insert(c.info().id), "duplicate id {}", c.info().id);
        for k in c.info().keys {
            assert!(commands::parse_binding(k).is_some(), "bad default key {k} for {}", c.info().id);
        }
    }
    // Overrides with a sequence.
    let mut ov = std::collections::BTreeMap::new();
    ov.insert("home".to_string(), vec!["G, H".to_string()]);
    let km = Keymap::build(&ov);
    let g = commands::parse_binding("G").unwrap();
    let (exact, longer) = km.lookup(&g);
    assert!(exact.is_none() && longer);
}

#[test]
fn default_hotkeys_have_no_conflicts() {
    let km = Keymap::build(&Default::default());
    let mut seen: HashMap<String, Cmd> = HashMap::new();
    for &c in commands::ALL {
        for b in km.bindings(c) {
            let s = commands::binding_to_string(b);
            if let Some(o) = seen.insert(s.clone(), c) {
                panic!("{s} bound to both {:?} and {:?}", o, c);
            }
        }
    }
}

#[test]
fn split_tree_operations() {
    let mut t = Node::Leaf(1);
    assert!(t.split(1, 2, Side::Right));
    assert!(t.split(2, 3, Side::Bottom));
    let mut leaves = vec![];
    t.leaves(&mut leaves);
    assert_eq!(leaves, vec![1, 2, 3]);
    assert!(t.remove(2));
    leaves.clear();
    t.leaves(&mut leaves);
    assert_eq!(leaves, vec![1, 3]);
    assert!(t.remove(1));
    assert!(matches!(t, Node::Leaf(3)));
}

#[test]
fn uri_roundtrip() {
    let p = PathBuf::from("/home/u/My Files/naïve #1.txt");
    let u = crate::clipboard::path_to_uri(&p);
    assert!(u.starts_with("file:///home/u/My%20Files/"));
    assert_eq!(crate::clipboard::uri_to_path(&u).unwrap(), p);
    assert_eq!(crate::clipboard::quote_if_needed("/a/b"), "/a/b");
    assert_eq!(crate::clipboard::quote_if_needed("/a b/it's"), "'/a b/it'\\''s'");
}

#[test]
fn unique_names() {
    let d = tmpdir("unique");
    touch(&d.join("x.txt"), "");
    assert_eq!(crate::fsx::unique_dest(&d, "x.txt"), d.join("x (1).txt"));
    assert_eq!(crate::fsx::unique_dest(&d, "y.txt"), d.join("y.txt"));
    std::fs::create_dir(d.join("New folder")).unwrap();
    assert_eq!(crate::fsx::unique_dest(&d, "New folder"), d.join("New folder (1)"));
    let _ = std::fs::remove_dir_all(&d);
}

fn wait_job(j: &ops::Job) {
    let start = Instant::now();
    while !j.progress.lock().unwrap().finished {
        assert!(start.elapsed() < Duration::from_secs(10), "job timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn copy_move_merge_and_conflicts() {
    let ctx = egui::Context::default();
    let d = tmpdir("ops");
    let src = d.join("src");
    let dst = d.join("dst");
    touch(&src.join("a.txt"), "new a");
    touch(&src.join("sub/b.txt"), "b");
    touch(&dst.join("a.txt"), "old a");
    touch(&dst.join("sub/keep.txt"), "keep");
    // Keep both
    let j = ops::spawn(1, OpKind::Copy { srcs: vec![src.join("a.txt")], dest: dst.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert_eq!(std::fs::read_to_string(dst.join("a (1).txt")).unwrap(), "new a");
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "old a");
    // Overwrite + merge folders
    let mut decide = HashMap::new();
    decide.insert(src.join("a.txt"), Conflict::Overwrite);
    decide.insert(src.join("sub"), Conflict::Merge);
    let j = ops::spawn(2, OpKind::Copy { srcs: vec![src.join("a.txt"), src.join("sub")], dest: dst.clone(), decide }, ctx.clone());
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.is_empty(), "{:?}", j.progress.lock().unwrap().errors);
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "new a");
    assert!(dst.join("sub/keep.txt").exists() && dst.join("sub/b.txt").exists());
    // Skip
    let mut decide = HashMap::new();
    decide.insert(src.join("a.txt"), Conflict::Skip);
    touch(&src.join("a.txt"), "newer a");
    let j = ops::spawn(3, OpKind::Copy { srcs: vec![src.join("a.txt")], dest: dst.clone(), decide }, ctx.clone());
    wait_job(&j);
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "new a");
    // Move into a fresh folder
    let other = d.join("other");
    std::fs::create_dir(&other).unwrap();
    let j = ops::spawn(4, OpKind::Move { srcs: vec![src.join("sub")], dest: other.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(other.join("sub/b.txt").exists() && !src.join("sub").exists());
    assert_eq!(j.progress.lock().unwrap().produced.len(), 1);
    // Copying a folder into itself is refused.
    let j = ops::spawn(5, OpKind::Copy { srcs: vec![other.clone()], dest: other.join("sub"), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(!j.progress.lock().unwrap().errors.is_empty());
    // Permanent delete
    let j = ops::spawn(6, OpKind::Delete { paths: vec![other.clone()] }, ctx);
    wait_job(&j);
    assert!(!other.exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// A copy keeps what the file carries besides its bytes: tags (extended attributes),
/// read-only files' tags too, folder dates, and big files whole.
#[test]
fn copy_keeps_tags_dates_and_bytes() {
    let ctx = egui::Context::default();
    let d = tmpdir("xattr");
    let src = d.join("src");
    touch(&src.join("sub/a.txt"), "a");
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    if crate::tags::write(&src.join("sub/a.txt"), &["red".into(), "work".into()]).is_err() {
        return; // this file system keeps no tags
    }
    crate::tags::write(&src.join("sub"), &["blue".into()]).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(src.join("sub/a.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    std::fs::File::open(src.join("sub")).unwrap().set_modified(old).unwrap();
    let out = d.join("out");
    std::fs::create_dir(&out).unwrap();
    let j = ops::spawn(1, OpKind::Copy { srcs: vec![src.join("sub"), src.join("big.bin")], dest: out.clone(), decide: HashMap::new() }, ctx);
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.is_empty(), "{:?}", j.progress.lock().unwrap().errors);
    assert_eq!(crate::tags::read(&out.join("sub/a.txt")), vec!["red".to_string(), "work".to_string()]);
    assert_eq!(crate::tags::read(&out.join("sub")), vec!["blue".to_string()]);
    assert_eq!(std::fs::metadata(out.join("sub")).unwrap().modified().unwrap(), old);
    assert_eq!(std::fs::read(out.join("big.bin")).unwrap(), big);
    assert_eq!(j.progress.lock().unwrap().done_bytes, big.len() as u64 + 1);
    std::fs::set_permissions(out.join("sub/a.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(src.join("sub/a.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// A finished move takes what it moved out of every listing that showed it, whichever
/// tab it is in: a plain listing and a tree are read again, a flattened one drops the
/// rows at once, the destination shows them, and a listing it did not touch stays put.
/// A tab whose own folder was moved away moves up.
#[test]
fn finished_move_updates_every_listing() {
    let ctx = egui::Context::default();
    let d = tmpdir("moved-away");
    let (from, to) = (d.join("from"), d.join("to"));
    for f in ["a.txt", "b.txt", "sub/c.txt", "sub/d.txt", "deep/e.txt"] {
        touch(&from.join(f), f);
    }
    touch(&d.join("elsewhere/x.txt"), "x");
    std::fs::create_dir_all(&to).unwrap();
    let stats = HashMap::new();
    let tab = |p: &Path| Tab::new(1, p.to_path_buf(), ViewSettings::default(), ctx.clone());
    let (mut plain, mut flat, mut tree, mut dest, mut other, mut deep) = (tab(&from), tab(&from), tab(&from), tab(&to), tab(&d.join("elsewhere")), tab(&from.join("deep")));
    flat.set_flatten(true);
    tree.settings.zoom = 0.1;
    for t in [&mut plain, &mut flat, &mut tree, &mut dest, &mut other, &mut deep] {
        pump(t, &stats);
    }
    tree.toggle_expand(&from.join("sub"));
    pump(&mut tree, &stats);
    let shows = |t: &Tab, p: &Path| t.view.iter().any(|&i| t.entries[i as usize].path == p);
    assert!(shows(&plain, &from.join("a.txt")) && shows(&flat, &from.join("sub/c.txt")) && shows(&tree, &from.join("sub/c.txt")));
    // Cut a.txt, sub/c.txt and the folder deep, pasted into `to`.
    let srcs = vec![from.join("a.txt"), from.join("sub/c.txt"), from.join("deep")];
    let j = ops::spawn(1, OpKind::Move { srcs: srcs.clone(), dest: to.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    let c = ops::Changed::of(&j.kind, &j.progress.lock().unwrap());
    assert_eq!(c, ops::Changed { arrived: vec![to.clone()], left: vec![from.clone(), from.join("sub")], gone: srcs.clone() });
    for t in [&mut plain, &mut flat, &mut tree, &mut dest, &mut other, &mut deep] {
        t.changed(&c);
    }
    assert!(!flat.is_loading() && !other.is_loading(), "nothing arrived under them: no walk, no read");
    assert!(plain.is_loading() && dest.is_loading(), "the folders it left and reached are read again");
    let recents = HashSet::new();
    let vc = ViewCtx { show_hidden: false, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
    flat.rebuild(&vc);
    assert!(!shows(&flat, &from.join("a.txt")) && !shows(&flat, &from.join("sub/c.txt")) && !shows(&flat, &from.join("deep/e.txt")), "a flattened listing drops what went at once");
    assert!(shows(&flat, &from.join("b.txt")) && shows(&flat, &from.join("sub/d.txt")));
    for t in [&mut plain, &mut tree, &mut dest, &mut deep] {
        pump(t, &stats);
    }
    assert!(!shows(&plain, &from.join("a.txt")) && !shows(&plain, &from.join("deep")) && shows(&plain, &from.join("b.txt")));
    assert!(!shows(&tree, &from.join("sub/c.txt")) && shows(&tree, &from.join("sub/d.txt")), "the unfolded folder is read again");
    assert!(shows(&dest, &to.join("a.txt")) && shows(&dest, &to.join("c.txt")) && shows(&dest, &to.join("deep")));
    assert!(deep.gone, "the tab inside the folder that moved finds it gone");
    // Moving them back (Undo) lands them in a flattened listing by walking it again.
    let c = ops::Changed::moved(&[(to.join("a.txt"), from.join("a.txt"))]);
    std::fs::rename(to.join("a.txt"), from.join("a.txt")).unwrap();
    flat.changed(&c);
    assert!(flat.is_loading(), "something arrived under it");
    pump(&mut flat, &stats);
    assert!(shows(&flat, &from.join("a.txt")) && !shows(&flat, &from.join("sub/c.txt")));
    let _ = std::fs::remove_dir_all(&d);
}

fn pump(t: &mut Tab, stats: &HashMap<PathBuf, crate::stats::DirStats>) {
    let recents = HashSet::new();
    let ctx = ViewCtx { show_hidden: false, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats };
    let start = Instant::now();
    while t.is_loading() {
        t.poll();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    t.poll();
    t.rebuild(&ctx);
}

#[test]
fn tab_listing_sort_filter_flatten() {
    let d = tmpdir("tab");
    touch(&d.join("b10.txt"), "1234567890");
    touch(&d.join("b2.txt"), "1");
    touch(&d.join(".hidden"), "");
    touch(&d.join("zdir/deep/needle.rs"), "");
    touch(&d.join("adir/x.png"), "");
    let ctx = egui::Context::default();
    let stats = HashMap::new();
    let mut t = Tab::new(1, d.clone(), ViewSettings::default(), ctx);
    pump(&mut t, &stats);
    let names: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).collect();
    assert_eq!(names, vec!["adir", "zdir", "b2.txt", "b10.txt"], "folders first, natural order, hidden excluded");
    assert_eq!(t.hidden_count, 1);
    assert_eq!(t.cursor, Some(t.view[0]), "cursor rests on the first item");
    assert!(t.selected.is_empty(), "no selection on entry");
    assert_eq!(t.counts, (2, 2, 2, 3), "totals include the hidden file");
    // Size sort defaults to descending.
    t.set_sort(SortKey::Size);
    assert!(t.settings.desc);
    pump(&mut t, &stats);
    let files: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).filter(|n| n.ends_with(".txt")).collect();
    assert_eq!(files, vec!["b10.txt", "b2.txt"]);
    // Filter
    t.filter = "b1".into();
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(t.view.len(), 1);
    // Flatten finds nested files, filter matches relative path.
    t.filter = "needle".into();
    t.set_flatten(true);
    pump(&mut t, &stats);
    let found: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].rel().map(|r| r.into_owned()).unwrap_or_default()).collect();
    assert_eq!(found, vec!["zdir/deep/needle.rs".to_string()]);
    // Flattened lists are files only.
    t.filter.clear();
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(t.view.iter().all(|&i| !t.entries[i as usize].is_dir), "flatten shows files only");
    // Type filter: only the checked types are shown.
    t.set_flatten(false);
    let txt = crate::mime::mime_db().label("a.txt", "txt", false, false);
    t.settings.only_types = vec![txt.clone()];
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(!t.view.is_empty());
    assert!(t.view.iter().all(|&i| t.entries[i as usize].ext() == "txt"));
    t.settings.only_types.clear();
    // Footer toggles hide folders / files.
    t.settings.show_folders = false;
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(t.view.iter().all(|&i| !t.entries[i as usize].is_dir));
    t.settings.show_folders = true;
    // Details view has no tree; Tree view (10% zoom) unfolds folders inline, indented.
    assert!(!t.tree_active());
    t.settings.zoom = 0.1;
    assert!(t.tree_active());
    t.dirty = true;
    pump(&mut t, &stats);
    t.toggle_expand(&d.join("zdir"));
    pump(&mut t, &stats);
    let names: Vec<(String, u8)> = t.view.iter().map(|&i| (t.entries[i as usize].name.clone(), t.entries[i as usize].depth)).collect();
    let zpos = names.iter().position(|(n, _)| n == "zdir").unwrap();
    assert_eq!(names[zpos + 1], ("deep".to_string(), 1), "child right below its parent: {names:?}");
    t.toggle_expand(&d.join("zdir/deep"));
    pump(&mut t, &stats);
    assert!(t.view.iter().any(|&i| t.entries[i as usize].name == "needle.rs" && t.entries[i as usize].depth == 2));
    // The status bar counts the folder's own items: unfolded rows are not shown "of" it.
    let (vf, tf, vfi, tfi) = t.counts;
    assert!(vf <= tf && vfi <= tfi, "shown {vf}/{tf} folders, {vfi}/{tfi} files");
    // Zooming out of the tree (which does not mark the tab dirty) asks for new rows, and
    // they leave the unfolded folders' children out.
    t.settings.zoom = 0.0;
    assert!(t.built_tree != t.tree_active(), "the frame loop rebuilds when the tree comes or goes");
    pump(&mut t, &stats);
    assert!(!t.view.iter().any(|&i| t.entries[i as usize].depth > 0), "Details shows no tree rows");
    t.settings.zoom = 0.1;
    assert!(t.built_tree != t.tree_active());
    pump(&mut t, &stats);
    assert!(t.view.iter().any(|&i| t.entries[i as usize].depth == 2), "back in the tree, the folders are still open");
    // Filtering in Tree view keeps the hierarchy: the match stays under its folders,
    // which are kept as dimmed context because they don't match themselves.
    t.filter = "needle".into();
    t.dirty = true;
    pump(&mut t, &stats);
    let names: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    assert_eq!(names, vec!["zdir", "deep", "needle.rs"], "{names:?}");
    let dim: Vec<String> = t.dimmed.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    assert!(dim.contains(&"zdir".to_string()) && dim.contains(&"deep".to_string()) && !dim.contains(&"needle.rs".to_string()));
    t.filter.clear();
    t.dirty = true;
    pump(&mut t, &stats);
    t.toggle_expand(&d.join("zdir"));
    pump(&mut t, &stats);
    assert!(!t.view.iter().any(|&i| t.entries[i as usize].depth > 0), "collapsing hides descendants");
    // Ctrl+E in Tree view unfolds every folder recursively (and the title gets the […] mark).
    t.toggle_expand_all();
    for _ in 0..50 {
        pump(&mut t, &stats);
        if !t.is_loading() {
            break;
        }
    }
    assert!(t.view.iter().any(|&i| t.entries[i as usize].name == "needle.rs"), "expand all reaches deep files");
    assert!(t.title().ends_with("[…]"));
    t.toggle_expand_all();
    pump(&mut t, &stats);
    assert!(!t.view.iter().any(|&i| t.entries[i as usize].depth > 0));
    t.settings.zoom = 0.0;
    // Navigation + history + up selects the child.
    t.navigate(d.join("zdir"), None);
    pump(&mut t, &stats);
    assert!(t.can_back());
    t.up(|_| None);
    pump(&mut t, &stats);
    assert_eq!(t.cursor_path(), Some(d.join("zdir")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn group_by_name_ranges() {
    let d = tmpdir("group");
    for n in ["apple", "kiwi", "zebra", "_x"] {
        touch(&d.join(n), "");
    }
    let mut v = ViewSettings::default();
    v.group = crate::config::GroupBy::Name;
    let mut t = Tab::new(1, d.clone(), v, egui::Context::default());
    pump(&mut t, &HashMap::new());
    let headers: Vec<String> = t.header_labels();
    assert_eq!(headers, vec!["A – H", "I – P", "Q – Z", "Other"]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn view_settings_inheritance() {
    use crate::config::{SaveScope, SavedView, UserData};
    let mut ud = UserData::default();
    let mut v = ViewSettings::default();
    v.zoom = 0.6;
    ud.views.insert(PathBuf::from("/a"), SavedView { scope: SaveScope::Subfolders, view: v.clone() });
    assert_eq!(ud.view_for(Path::new("/a/b/c")).0.zoom, 0.6);
    ud.views.insert(PathBuf::from("/x"), SavedView { scope: SaveScope::ThisFolder, view: v });
    assert_eq!(ud.view_for(Path::new("/x/y")).0.zoom, 0.0);
}

#[test]
fn mime_lookup() {
    let db = crate::mime::mime_db();
    assert_eq!(db.for_name("x.png", false, false), "image/png");
    assert_eq!(db.for_name("dir", true, false), "inode/directory");
    assert_eq!(db.for_name("a.tar.gz", false, false), "application/x-compressed-tar");
}

#[test]
fn image_decoders_and_downscale() {
    let d = tmpdir("img");
    // PNG via the png encoder.
    let png_path = d.join("a.png");
    {
        let f = std::fs::File::create(&png_path).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), 40, 20);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().unwrap();
        let data: Vec<u8> = (0..40 * 20).flat_map(|i| [(i % 40 * 6) as u8, 100, 200]).collect();
        w.write_image_data(&data).unwrap();
    }
    let (img, dims) = crate::imgdec::load(&png_path, 10).unwrap();
    assert_eq!(dims, [40, 20]);
    assert_eq!((img.w, img.h), (10, 5));
    assert_eq!(img.px.len(), 10 * 5 * 4);
    assert_eq!(img.px[3], 255);
    // 24-bit bottom-up BMP, 2x2: red, green / blue, white (rows stored bottom first).
    let mut bmp = vec![];
    let row = |a: [u8; 3], b: [u8; 3]| [a[2], a[1], a[0], b[2], b[1], b[0], 0, 0];
    let pixels = [row([0, 0, 255], [255, 255, 255]), row([255, 0, 0], [0, 255, 0])].concat();
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&2i32.to_le_bytes());
    bmp.extend_from_slice(&2i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    bmp.extend_from_slice(&pixels);
    let bmp_path = d.join("b.bmp");
    std::fs::write(&bmp_path, &bmp).unwrap();
    let (img, _) = crate::imgdec::load(&bmp_path, 64).unwrap();
    assert_eq!(&img.px[0..4], &[255, 0, 0, 255], "top-left red");
    assert_eq!(&img.px[12..16], &[255, 255, 255, 255], "bottom-right white");
    assert!(crate::imgdec::load(&d.join("missing.png"), 64).is_none());
    std::fs::write(d.join("junk.png"), b"not an image").unwrap();
    assert!(crate::imgdec::load(&d.join("junk.png"), 64).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn trash_roundtrip() {
    let d = tmpdir("trash");
    let f = d.join("doomed file.txt");
    touch(&f, "bye");
    crate::trashcan::delete(&f).unwrap();
    assert!(!f.exists());
    let items: Vec<_> = crate::trashcan::list().into_iter().filter(|i| i.original == f).collect();
    assert_eq!(items.len(), 1, "listed with its original path");
    assert!(items[0].file.exists() && items[0].info.exists());
    assert!(!items[0].deleted.is_empty());
    crate::trashcan::restore(&items[0]).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "bye");
    // Trash again and purge.
    crate::trashcan::delete(&f).unwrap();
    let it = crate::trashcan::list().into_iter().find(|i| i.original == f).unwrap();
    crate::trashcan::purge(&it).unwrap();
    assert!(!it.file.exists() && !it.info.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn inotify_reports_changes() {
    let d = tmpdir("watch");
    let mut w = crate::watch::Watch::new(egui::Context::default());
    w.sync(&[d.clone()].into_iter().collect());
    touch(&d.join("new.txt"), "x");
    let start = Instant::now();
    let mut got = vec![];
    while got.is_empty() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(30));
        got = w.poll();
    }
    assert_eq!(got, vec![d.clone()]);
    assert!(w.changed_files.contains(&d.join("new.txt")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn time_formatting() {
    let t = std::time::UNIX_EPOCH + Duration::from_secs(86_400 * 400);
    assert_eq!(crate::timefmt::format(t, "%Y"), "1971");
    let now = std::time::SystemTime::now();
    assert_eq!(crate::timefmt::local_day(now) - crate::timefmt::local_day(now - Duration::from_secs(86_400)), 1);
}

#[test]
fn desktop_exec_expansion() {
    let p = vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")];
    assert_eq!(crate::mime::expand_exec("gedit %U", &p, "Gedit"), vec!["gedit", "/tmp/a b.txt", "/tmp/c.txt"]);
    assert_eq!(crate::mime::expand_exec("vlc --started-from-file %f", &p[..1], "VLC"), vec!["vlc", "--started-from-file", "/tmp/a b.txt"]);
    assert_eq!(crate::mime::expand_exec("\"/opt/My App/run\" --x", &p[..1], "X"), vec!["/opt/My App/run", "--x", "/tmp/a b.txt"]);
    assert_eq!(crate::mime::expand_exec("app --name=%c %%", &[], "Zed"), vec!["app", "--name=Zed", "%"]);
}

#[test]
fn trash_metadata_columns_and_groups() {
    // A fake per-volume trash: files/ plus info/*.trashinfo.
    let d = tmpdir("trashmeta");
    let files = d.join(".Trash-1000/files");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::create_dir_all(d.join(".Trash-1000/info")).unwrap();
    touch(&files.join("old.txt"), "x");
    touch(&files.join("new.txt"), "y");
    let now = crate::timefmt::format(std::time::SystemTime::now(), "%Y-%m-%dT%H:%M:%S");
    touch(&d.join(".Trash-1000/info/old.txt.trashinfo"), "[Trash Info]\nPath=/srv/a/old.txt\nDeletionDate=2001-02-03T04:05:06\n");
    touch(&d.join(".Trash-1000/info/new.txt.trashinfo"), &format!("[Trash Info]\nPath=/srv/b/new.txt\nDeletionDate={now}\n"));
    assert!(crate::trashcan::is_trash_files(&files));
    assert!(!crate::trashcan::is_trash_files(&d));
    let m = crate::trashcan::info_for(&files);
    assert_eq!(m.get(&files.join("old.txt")).map(|x| x.0.clone()), Some(PathBuf::from("/srv/a/old.txt")));
    let t0 = crate::timefmt::parse_local("2001-02-03T04:05:06").unwrap();
    assert_eq!(crate::timefmt::format(t0, "%Y-%m-%dT%H:%M:%S"), "2001-02-03T04:05:06");
    // A tab on the trash groups by deletion date and by original folder.
    let stats = HashMap::new();
    let mut t = Tab::new(7, files.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    assert_eq!(t.trash_meta.len(), 2);
    t.settings.group = crate::config::GroupBy::Deleted;
    t.dirty = true;
    pump(&mut t, &stats);
    let heads: Vec<String> = t.header_labels();
    assert_eq!(heads, vec!["Today", "Older"]);
    t.settings.group = crate::config::GroupBy::Original;
    t.dirty = true;
    pump(&mut t, &stats);
    let heads: Vec<String> = t.header_labels();
    assert_eq!(heads, vec!["/srv/a", "/srv/b"]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Renders the SVGs listed in $MAIA_SVG_SAMPLES (one path per line) into $MAIA_SVG_OUT
/// as PNGs, for eyeballing the renderer. Run with --ignored.
#[test]
#[ignore]
fn svg_render_samples() {
    let (Ok(list), Ok(out)) = (std::env::var("MAIA_SVG_SAMPLES"), std::env::var("MAIA_SVG_OUT")) else { return };
    std::fs::create_dir_all(&out).unwrap();
    for (i, p) in std::fs::read_to_string(list).unwrap().lines().enumerate() {
        let data = std::fs::read(p).unwrap();
        for px in [24u32, 64, 128] {
            let t = Instant::now();
            let Some((w, h, mut rgba)) = crate::svg::render(&data, px) else { eprintln!("FAILED {p}"); continue };
            // Unpremultiply for the PNG file.
            for c in rgba.chunks_exact_mut(4) {
                if c[3] > 0 && c[3] < 255 {
                    for k in 0..3 {
                        c[k] = ((c[k] as u32 * 255 + c[3] as u32 / 2) / c[3] as u32).min(255) as u8;
                    }
                }
            }
            eprintln!("{i:02} {px} {:?} {p}", t.elapsed());
            let f = std::fs::File::create(format!("{out}/{i:02}-{px}.png")).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
    }
}

#[test]
fn zoom_steps_through_views_then_icon_sizes() {
    use crate::listing::zoom_step;
    // Up: one named view per step, then 5.5% icon steps up to 100%.
    let mut z = 0.0;
    let mut seen = vec![];
    for _ in 0..20 {
        z = zoom_step(z, 1);
        seen.push((z * 1000.0).round() as i32);
    }
    assert_eq!(&seen[..6], &[100, 200, 300, 355, 410, 465]);
    assert_eq!(*seen.last().unwrap(), 1000);
    // Down retraces through the dense views without skipping any.
    let mut z = 0.355;
    let mut down = vec![];
    for _ in 0..5 {
        z = zoom_step(z, -1);
        down.push((z * 1000.0).round() as i32);
    }
    assert_eq!(down, vec![300, 200, 100, 0, 0]);
    // From a slider position between stops, a step lands on the neighbouring stop.
    assert_eq!((zoom_step(0.15, 1) * 1000.0).round() as i32, 200);
    assert_eq!((zoom_step(0.15, -1) * 1000.0).round() as i32, 100);
}

#[test]
fn decoders_reject_huge_or_corrupt_headers_without_allocating() {
    let t = Instant::now();
    // GIF claiming a 65535x65535 screen (4.3 gigapixels) with one 1x1 frame.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x80, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff]);
    gif.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x02, 0x02, 0x44, 0x01, 0, 0x3b]);
    assert!(crate::imgdec::decode_bytes(&gif).is_none());
    // BMP claiming 32000x32000 pixels.
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0; 8]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&32000i32.to_le_bytes());
    bmp.extend_from_slice(&32000i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    assert!(crate::imgdec::decode_bytes(&bmp).is_none());
    // 8-bit BMP with a palette count of 4 billion must not try to build that palette.
    let mut pal = b"BM".to_vec();
    pal.extend_from_slice(&[0; 8]);
    pal.extend_from_slice(&58u32.to_le_bytes());
    pal.extend_from_slice(&40u32.to_le_bytes());
    pal.extend_from_slice(&1i32.to_le_bytes());
    pal.extend_from_slice(&1i32.to_le_bytes());
    pal.extend_from_slice(&1u16.to_le_bytes());
    pal.extend_from_slice(&8u16.to_le_bytes());
    pal.extend_from_slice(&0u32.to_le_bytes());
    pal.extend_from_slice(&[0; 12]);
    pal.extend_from_slice(&u32::MAX.to_le_bytes());
    pal.extend_from_slice(&[0; 4 + 4 + 4]);
    let _ = crate::imgdec::decode_bytes(&pal);
    assert!(t.elapsed() < Duration::from_secs(1), "rejected quickly");
}

#[test]
fn copy_skips_special_files_and_refuses_self_nesting_through_symlinks() {
    let ctx = egui::Context::default();
    let d = tmpdir("ops_edge");
    let src = d.join("src");
    touch(&src.join("ok.txt"), "ok");
    // A FIFO inside a copied folder: must be skipped, not opened (that would block forever).
    let fifo = std::ffi::CString::new(src.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let out = d.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let t = Instant::now();
    let j = ops::spawn(10, OpKind::Copy { srcs: vec![src.clone()], dest: out.clone(), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(t.elapsed() < Duration::from_secs(5), "copy finished despite the FIFO");
    assert_eq!(std::fs::read_to_string(out.join("src/ok.txt")).unwrap(), "ok");
    assert!(!out.join("src/pipe").exists());
    assert!(j.progress.lock().unwrap().errors.iter().any(|e| e.contains("special file")));
    // Copying `src` into a symlink that points inside `src` is copying it into itself.
    std::os::unix::fs::symlink(&src, d.join("alias")).unwrap();
    std::fs::create_dir_all(src.join("inner")).unwrap();
    let j = ops::spawn(11, OpKind::Copy { srcs: vec![src.clone()], dest: d.join("alias/inner"), decide: HashMap::new() }, ctx.clone());
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.iter().any(|e| e.contains("into itself")), "{:?}", j.progress.lock().unwrap().errors);
    assert!(!src.join("inner/src").exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn deleted_folder_is_flagged_gone_but_unreadable_is_not() {
    let stats = HashMap::new();
    let d = tmpdir("gone");
    let sub = d.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mut t = Tab::new(21, sub.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    assert!(!t.gone);
    std::fs::remove_dir(&sub).unwrap();
    t.reload();
    pump(&mut t, &stats);
    assert!(t.gone, "missing folder reported as gone: {:?}", t.error);
    // A folder we can't read (permission denied) stays put with an error instead.
    let locked = d.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut t = Tab::new(22, locked.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let readable = std::fs::read_dir(&locked).is_ok(); // true when running as root
    if !readable {
        assert!(!t.gone && t.error.is_some());
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn special_files_are_labelled_and_never_executable() {
    let d = tmpdir("special");
    let f = d.join("p");
    let c = std::ffi::CString::new(f.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o755) }, 0);
    let e = crate::fsx::Entry::from_path(&f).unwrap();
    assert!(e.special && !e.is_executable());
    assert_eq!(e.special_kind(), Some("Pipe"));
    let n = crate::fsx::Entry::from_path(&d).unwrap();
    assert_eq!(n.special_kind(), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn entry_derives_lowercase_name_extension_and_relative_path() {
    let d = tmpdir("derive");
    std::fs::create_dir_all(d.join("Sub.D/inner")).unwrap();
    for n in ["Photo.JPG", ".bashrc", "archive.tar.gz", "file.", "plain", "..odd", "Sub.D/inner/x.Rs"] {
        std::fs::write(d.join(n), b"x").unwrap();
    }
    let e = |n: &str| crate::fsx::Entry::from_path(&d.join(n)).unwrap();
    let (p, b, a, f, pl, o, dir) = (e("Photo.JPG"), e(".bashrc"), e("archive.tar.gz"), e("file."), e("plain"), e("..odd"), e("Sub.D"));
    assert_eq!((p.name.as_str(), p.lname(), p.ext()), ("Photo.JPG", "photo.jpg", "jpg"));
    assert_eq!((b.ext(), b.hidden), ("", true));
    assert_eq!((a.lname(), a.ext()), ("archive.tar.gz", "gz"));
    assert_eq!((f.ext(), pl.ext(), o.ext(), dir.ext()), ("", "", "odd", ""));
    assert_eq!(dir.lname(), "sub.d");
    assert!(p.rel().is_none());
    // Renaming a volume entry keeps the derived fields in sync.
    let mut v = e("plain");
    v.set_name("My Disk".into());
    assert_eq!((v.lname(), v.ext()), ("my disk", ""));
    // Results of a recursive walk carry their path relative to the root.
    let w = crate::fsx::walk_async(d.clone(), true, egui::Context::default());
    while !w.done.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut rels: Vec<String> = w.rx.try_iter().flatten().filter_map(|e| e.rel().map(|r| r.into_owned())).collect();
    rels.sort();
    assert!(rels.contains(&"Sub.D/inner/x.Rs".to_string()) && rels.contains(&"plain".to_string()), "{rels:?}");
    assert!(std::mem::size_of::<crate::fsx::Entry>() <= 144, "Entry grew to {}", std::mem::size_of::<crate::fsx::Entry>());
    let _ = std::fs::remove_dir_all(&d);
}

/// The previous (char-based, allocating) natural order, kept as an oracle.
fn natural_cmp_reference(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[test]
fn natural_cmp_matches_reference_on_random_names() {
    let alphabet: Vec<char> = "ab0123456789Z._- éß日😀".chars().collect();
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut names = vec![];
    for _ in 0..3000 {
        let n = (next() % 9) as usize;
        names.push((0..n).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect::<String>());
    }
    names.extend(["file2", "file10", "file02", "file002", "a1b2", "a1b10", "", "0", "00", "007", "7"].map(String::from));
    for a in &names {
        for b in names.iter().step_by(7) {
            assert_eq!(crate::tab::natural_cmp(a, b), natural_cmp_reference(a, b), "{a:?} vs {b:?}");
        }
    }
}

#[test]
fn fast_name_sort_orders_exactly_like_natural_order() {
    let d = tmpdir("namesort");
    let parts = ["a", "A", "b", "file", "File", "file1", "file10", "file2", "img_0001", "img_10", "z", "é", "Éa", "日本", "abcdefgh", "abcdefgi", "abcdefgh1", "abcdefgh10", "x.y", "00", "0", "007"];
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut made = std::collections::HashSet::new();
    for k in 0..400 {
        let n = 1 + (next() % 3) as usize;
        let name: String = (0..n).map(|_| parts[(next() % parts.len() as u64) as usize]).collect::<Vec<_>>().join("");
        if !made.insert(name.to_lowercase()) {
            continue;
        }
        if k % 5 == 0 {
            std::fs::create_dir_all(d.join(&name)).unwrap();
        } else {
            std::fs::write(d.join(&name), b"x").unwrap();
        }
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    for (folders_first, desc, filter) in [(true, false, ""), (true, true, ""), (false, false, ""), (false, true, ""), (true, false, "file")] {
        let mut t = Tab::new(31, d.clone(), ViewSettings::default(), egui::Context::default());
        pump(&mut t, &stats);
        t.settings.desc = desc;
        t.filter = filter.to_string();
        let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first, recents_on_top: false, recents: &recents, stats: &stats };
        t.rebuild(&ctx);
        assert!(t.view.len() > if filter.is_empty() { 120 } else { 10 }, "{} items", t.view.len());
        let q = crate::fuzzy::Query::new(filter);
        let tier = |e: &crate::fsx::Entry| crate::fuzzy::score(&q, e.lname()).map(|s| (s + 40) / 25).unwrap_or(0);
        for w in t.view.windows(2) {
            let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
            if !filter.is_empty() && tier(a) != tier(b) {
                assert!(tier(a) > tier(b), "better matches first");
                continue;
            }
            if a.is_dir != b.is_dir {
                assert_eq!(a.is_dir, folders_first, "{:?} before {:?}", a.name, b.name);
                continue;
            }
            let o = crate::tab::natural_cmp(a.lname(), b.lname());
            let o = if desc { o.reverse() } else { o };
            assert_ne!(o, std::cmp::Ordering::Greater, "{:?} before {:?} (desc {desc}, folders first {folders_first})", a.name, b.name);
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn parallel_sort_is_stable_and_matches_sort_by() {
    let mut x: u64 = 88172645463325252;
    for n in [0usize, 1, 39_999, 40_000, 40_001, 123_457, 200_000] {
        let v: Vec<(u32, u32)> = (0..n as u32)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x % 1000) as u32, i) // many equal keys; .1 records the original order
            })
            .collect();
        let mut a = v.clone();
        let mut b = v.clone();
        a.sort_by(|p, q| p.0.cmp(&q.0));
        crate::tab::par_sort_by(&mut b, &|p: &(u32, u32), q: &(u32, u32)| p.0.cmp(&q.0));
        assert_eq!(a, b, "n = {n}");
    }
}

#[test]
fn parallel_filter_path_keeps_every_match_in_order() {
    let d = tmpdir("parfilter");
    std::fs::write(d.join("seed.txt"), b"x").unwrap();
    std::fs::create_dir_all(d.join(".hid")).unwrap();
    let base = crate::fsx::Entry::from_path(&d.join("seed.txt")).unwrap();
    let words = ["alpha", "beta", "gamma", "delta", "lib", "Liberty", "x11", "file10", "file2", ".dot"];
    let mut entries = vec![];
    for i in 0..45_000u32 {
        let mut e = base.clone();
        let dir = if i % 97 == 0 { ".hid/" } else { "sub/" };
        let name = format!("{}{}.txt", words[(i as usize * 7) % words.len()], i % 1000);
        e.path = d.join(format!("{dir}{name}"));
        e.set_name(name);
        e.rel_at = (d.as_os_str().len() + 1) as u16;
        entries.push(e);
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    let mut t = Tab::new(41, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    t.flatten = true;
    t.set_entries(entries.clone());
    t.filter = "lib".into();
    let ctx = ViewCtx { show_hidden: false, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
    t.rebuild(&ctx);
    let q = crate::fuzzy::Query::new("lib");
    let expect: usize = entries.iter().filter(|e| !e.hidden && !e.rel_hidden() && (crate::fuzzy::score(&q, e.lname()).is_some() || e.rel().is_some_and(|r| crate::fuzzy::score_ci(&q, &r).is_some()))).count();
    assert_eq!(t.view.len(), expect, "every visible match listed once");
    assert!(expect > 1000);
    let tier = |e: &crate::fsx::Entry| {
        let n = crate::fuzzy::score(&q, e.lname()).map(|s| s + 40);
        let p = e.rel().and_then(|r| crate::fuzzy::score_ci(&q, &r));
        n.max(p).unwrap() / 25
    };
    for w in t.view.windows(2) {
        let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
        assert!(tier(a) > tier(b) || (tier(a) == tier(b) && crate::tab::natural_cmp(a.lname(), b.lname()) != std::cmp::Ordering::Greater), "{} before {}", a.name, b.name);
    }
    let hidden = entries.iter().filter(|e| e.rel_hidden()).count();
    assert_eq!(t.hidden_count, hidden);
    assert_eq!(t.counts.3, 45_000, "total files counted");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn compact_key_sorts_match_full_comparison_for_size_and_dates() {
    use crate::config::SortKey;
    let d = tmpdir("keysort");
    let base = std::time::SystemTime::UNIX_EPOCH;
    for i in 0..160u64 {
        let name = format!("{}{}", ["f", "F", "img", "a1b"][(i % 4) as usize], i % 37);
        let p = d.join(&name);
        if p.exists() {
            continue;
        }
        std::fs::write(&p, vec![b'x'; (i % 9) as usize * 100]).unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        // Some timestamps before 1970, many equal ones.
        let t = if i % 11 == 0 { base - std::time::Duration::from_secs(1000 + i) } else { base + std::time::Duration::from_secs(1_700_000_000 + (i % 7) * 3600) };
        f.set_modified(t).unwrap();
    }
    let stats = HashMap::new();
    let recents = HashSet::new();
    for sort in [SortKey::Size, SortKey::Modified] {
        for desc in [false, true] {
            let mut t = Tab::new(51, d.clone(), ViewSettings::default(), egui::Context::default());
            pump(&mut t, &stats);
            t.settings.sort = sort;
            t.settings.desc = desc;
            let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
            t.rebuild(&ctx);
            assert!(t.view.len() > 100);
            for w in t.view.windows(2) {
                let (a, b) = (&t.entries[w[0] as usize], &t.entries[w[1] as usize]);
                let o = match sort {
                    SortKey::Size => a.size.cmp(&b.size),
                    _ => a.modified.unwrap().cmp(&b.modified.unwrap()),
                }
                .then_with(|| crate::tab::natural_cmp(a.lname(), b.lname()));
                let o = if desc { o.reverse() } else { o };
                assert_ne!(o, std::cmp::Ordering::Greater, "{sort:?} desc {desc}: {:?} before {:?}", a.name, b.name);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn recents_on_top_sorts_recents_first_then_folders_then_names() {
    let d = tmpdir("recentsort");
    for n in ["b.txt", "a.txt", "c10.txt", "c2.txt", "z.txt"] {
        std::fs::write(d.join(n), b"x").unwrap();
    }
    for n in ["dir1", "dir2"] {
        std::fs::create_dir_all(d.join(n)).unwrap();
    }
    let stats = HashMap::new();
    let recents: HashSet<PathBuf> = [d.join("z.txt"), d.join("c10.txt"), d.join("dir2")].into_iter().collect();
    let mut t = Tab::new(61, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let ctx = ViewCtx { show_hidden: true, show_system: true, folders_first: true, recents_on_top: true, recents: &recents, stats: &stats };
    t.rebuild(&ctx);
    let names: Vec<&str> = t.view.iter().map(|&i| t.entries[i as usize].name.as_str()).collect();
    assert_eq!(names, ["dir2", "c10.txt", "z.txt", "dir1", "a.txt", "b.txt", "c2.txt"]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn md5_matches_rfc1321_and_the_thumbnail_spec_example() {
    use crate::thumbcache::md5_hex;
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(md5_hex(b"The quick brown fox jumps over the lazy dog"), "9e107d9d372bb6826bd81d3542a419d6");
    assert_eq!(md5_hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
    // freedesktop.org Thumbnail Managing Standard, "Thumbnail Saving" example.
    assert_eq!(md5_hex(b"file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697");
}

#[test]
fn thumbnail_uris_encode_like_glib_and_round_trip() {
    use crate::thumbcache::{path_from_uri, uri_for};
    assert_eq!(uri_for(Path::new("/home/jens/photos/me.png")), "file:///home/jens/photos/me.png");
    assert_eq!(uri_for(Path::new("/tmp/a b/é#%.png")), "file:///tmp/a%20b/%C3%A9%23%25.png");
    for p in ["/tmp/a b/é#%.png", "/home/x/日本/file (1).txt", "/plain/path"] {
        assert_eq!(path_from_uri(&uri_for(Path::new(p))).as_deref(), Some(Path::new(p)));
    }
    assert_eq!(path_from_uri("file://localhost/etc/hosts").as_deref(), Some(Path::new("/etc/hosts")));
    assert_eq!(path_from_uri("https://example.com/x"), None);
}

#[test]
fn thumbnail_cache_round_trip_and_staleness() {
    use crate::thumbcache::{flavor, lookup_in, store_in};
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir("thumbcache");
    let root = d.join("thumbnails");
    let file = d.join("photo one.png");
    std::fs::write(&file, b"x").unwrap();
    let mtime = std::fs::metadata(&file).unwrap().modified().ok();
    let img = crate::imgdec::Rgba { w: 3, h: 2, px: (0..24).map(|i| i as u8 * 10).collect() };
    store_in(&root, &file, mtime, 128, &img, 1, Some([4000, 3000]));
    let (got, dims) = lookup_in(&root, &file, mtime, 100).expect("cached");
    assert_eq!((got.w, got.h, got.px), (3, 2, img.px.clone()));
    assert_eq!(dims, Some([4000, 3000]));
    // Stored where the spec says, private.
    let name = format!("{}.png", crate::thumbcache::md5_hex(crate::thumbcache::uri_for(&file).as_bytes()));
    let meta = std::fs::metadata(root.join("normal").join(&name)).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Other flavours miss; a changed file is stale.
    assert!(lookup_in(&root, &file, mtime, 512).is_none());
    let later = mtime.map(|t| t + std::time::Duration::from_secs(5));
    assert!(lookup_in(&root, &file, later, 100).is_none(), "stale after the file changed");
    assert!(lookup_in(&root, &d.join("other.png"), mtime, 100).is_none());
    assert_eq!(flavor(64), ("normal", 128));
    assert_eq!(flavor(200), ("large", 256));
    assert_eq!(flavor(512), ("x-large", 512));
    assert_eq!(flavor(900), ("xx-large", 1024));
    let _ = std::fs::remove_dir_all(&d);
}

/// Read-only check against a real thumbnail cache made by other apps (run by hand:
/// `cargo test real_cache -- --ignored`): Maia's URI encoding and hashing must find
/// exactly the files they wrote.
#[test]
#[ignore]
fn real_thumbnail_cache_entries_match_our_encoding() {
    let root = crate::thumbcache::cache_root();
    let (mut checked, mut bad) = (0, vec![]);
    for flavor in ["normal", "large", "x-large", "xx-large"] {
        let Ok(rd) = std::fs::read_dir(root.join(flavor)) else { continue };
        for e in rd.flatten().take(400) {
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let Some((_, text)) = crate::imgdec::png_with_text(&bytes) else { continue };
            let Some(uri) = text.iter().find(|(k, _)| k == "Thumb::URI").map(|(_, v)| v.clone()) else { continue };
            let Some(p) = crate::thumbcache::path_from_uri(&uri) else { continue };
            checked += 1;
            let ours = crate::thumbcache::uri_for(&p);
            let stem = e.path().file_stem().unwrap().to_string_lossy().into_owned();
            if ours != uri || crate::thumbcache::md5_hex(ours.as_bytes()) != stem {
                bad.push(format!("{uri} -> {ours}"));
            }
        }
    }
    eprintln!("real cache: {checked} thumbnails checked, {} mismatches", bad.len());
    assert!(bad.is_empty(), "{:?}", &bad[..bad.len().min(5)]);
}

#[test]
fn syntax_highlighting_tokens() {
    use crate::syntax::{highlight, lang_for, Kind};
    let spans = |name: &str, src: &[&str]| {
        let lines: Vec<String> = src.iter().map(|s| s.to_string()).collect();
        let hl = highlight(lang_for(name).expect(name), &lines, 1000);
        hl.into_iter().enumerate().flat_map(|(n, v)| v.into_iter().map(move |(a, b, k)| (n, src_owned(src, n, a, b), k))).collect::<Vec<_>>()
    };
    fn src_owned(src: &[&str], n: usize, a: usize, b: usize) -> String {
        src[n][a..b].to_string()
    }
    let rs = spans("main.rs", &["fn main() { let s = \"a \\\" b\"; // done", "/* multi", " line */ let x: u32 = 42;"]);
    assert!(rs.contains(&(0, "fn".into(), Kind::Keyword)));
    assert!(rs.contains(&(0, "\"a \\\" b\"".into(), Kind::Str)), "escaped quote stays inside the string: {rs:?}");
    assert!(rs.contains(&(0, "// done".into(), Kind::Comment)));
    assert!(rs.contains(&(1, "/* multi".into(), Kind::Comment)));
    assert!(rs.contains(&(2, " line */".into(), Kind::Comment)), "block comment carried to the next line");
    assert!(rs.contains(&(2, "u32".into(), Kind::Type)));
    assert!(rs.contains(&(2, "42".into(), Kind::Number)));
    let py = spans("x.py", &["def f():", "    s = \"\"\"doc", "    more\"\"\" # tail", "a#b = 1"]);
    assert!(py.contains(&(0, "def".into(), Kind::Keyword)));
    assert!(py.contains(&(1, "\"\"\"doc".into(), Kind::Str)));
    assert!(py.contains(&(2, "    more\"\"\"".into(), Kind::Str)), "triple-quoted string spans lines: {py:?}");
    assert!(py.contains(&(2, "# tail".into(), Kind::Comment)));
    assert!(!py.iter().any(|(n, _, k)| *n == 3 && *k == Kind::Comment), "a#b is not a comment");
    let js = spans("app.ts", &["const x = `tpl", "end` // c"]);
    assert!(js.contains(&(1, "end`".into(), Kind::Str)), "template literal spans lines");
    let html = spans("index.html", &["<div class=\"a\"><!-- note --></div>"]);
    assert!(html.contains(&(0, "<div".into(), Kind::Keyword)));
    assert!(html.contains(&(0, "\"a\"".into(), Kind::Str)));
    assert!(html.contains(&(0, "<!-- note -->".into(), Kind::Comment)));
    let md = spans("README.md", &["# Title", "text `code` more"]);
    assert!(md.contains(&(0, "# Title".into(), Kind::Keyword)));
    assert!(md.contains(&(1, "`code`".into(), Kind::Str)));
    assert!(lang_for("Makefile").is_some() && lang_for("photo.png").is_none() && lang_for("notes.txt").is_none());
}

#[test]
fn syntax_highlighting_never_panics_and_spans_stay_on_char_boundaries() {
    use crate::syntax::{highlight, lang_for};
    let pieces = ["fn", " ", "\"", "'", "`", "\\", "/*", "*/", "//", "#", "<!--", "-->", "\"\"\"", "日本", "é", "😀", "<a", "42", "x", "\t", "--", "--[[", "]]"];
    let mut x: u64 = 0xdead_beef;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let lines: Vec<String> = (0..3000).map(|_| (0..(next() % 14)).map(|_| pieces[(next() % pieces.len() as u64) as usize]).collect()).collect();
    for name in ["a.rs", "a.c", "a.ts", "a.py", "a.sh", "a.go", "a.java", "a.lua", "a.rb", "a.php", "a.sql", "a.zig", "a.json", "a.toml", "a.yaml", "a.css", "a.html", "a.md", "Makefile"] {
        let hl = highlight(lang_for(name).unwrap(), &lines, 10_000);
        for (line, spans) in lines.iter().zip(&hl) {
            for &(a, b, _) in spans {
                assert!(a < b && b <= line.len() && line.is_char_boundary(a) && line.is_char_boundary(b), "{name}: bad span {a}..{b} in {line:?}");
            }
        }
    }
}

#[test]
fn ffprobe_output_is_read_like_a_player_needs() {
    let flat = b"streams.stream.0.codec_name=\"mp3\"\nstreams.stream.0.codec_type=\"audio\"\nstreams.stream.0.disposition.attached_pic=0\nstreams.stream.1.codec_name=\"png\"\nstreams.stream.1.codec_type=\"video\"\nstreams.stream.1.width=200\nstreams.stream.1.height=200\nstreams.stream.1.disposition.attached_pic=1\nformat.duration=\"2.035000\"\nformat.tags.title=\"Test \\\"Tone\\\"\"\nformat.tags.ARTIST=\"Maia\"\n";
    let p = crate::media::parse_probe(flat).unwrap();
    assert!(p.video.is_none(), "cover art is not a video stream");
    assert!(p.audio && (p.duration - 2.035).abs() < 1e-6);
    assert_eq!(p.title.as_deref(), Some("Test \"Tone\""), "escaped quotes unescaped");
    assert_eq!(p.artist.as_deref(), Some("Maia"), "tag keys in any case");
    let v = b"streams.stream.0.codec_name=\"h264\"\nstreams.stream.0.codec_type=\"video\"\nstreams.stream.0.width=1920\nstreams.stream.0.height=1080\nstreams.stream.0.disposition.attached_pic=0\nformat.duration=\"61.5\"\n";
    let p = crate::media::parse_probe(v).unwrap();
    assert_eq!(p.video, Some((1920, 1080)));
    assert!(!p.audio);
    assert!(crate::media::parse_probe(b"format.duration=\"1\"\n").is_none());
    assert_eq!(crate::media::fmt_time(61.5), "1:01");
    assert_eq!(crate::media::fmt_time(3725.0), "1:02:05");
}

#[test]
fn lsblk_pairs_parse_into_drives_with_safe_removability() {
    use crate::drives::parse_lsblk;
    // A real machine: system disk (reports HOTPLUG=1) and a Windows NVMe, plus a USB stick.
    let text = r#"PATH="/dev/sda" PKNAME="" LABEL="" FSTYPE="" SIZE="500107862016" RM="0" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sda1" PKNAME="sda" LABEL="" FSTYPE="vfat" SIZE="4294967296" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/boot"
PATH="/dev/sda2" PKNAME="sda" LABEL="" FSTYPE="btrfs" SIZE="495810760704" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/var/tmp\x0a/home\x0a/"
PATH="/dev/zram0" PKNAME="" LABEL="zram0" FSTYPE="swap" SIZE="33488371712" RM="0" HOTPLUG="0" TYPE="disk" MOUNTPOINTS="[SWAP]"
PATH="/dev/nvme0n1p1" PKNAME="nvme0n1" LABEL="" FSTYPE="vfat" SIZE="104857600" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p2" PKNAME="nvme0n1" LABEL="" FSTYPE="" SIZE="16777216" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p3" PKNAME="nvme0n1" LABEL="" FSTYPE="ntfs" SIZE="680705130496" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p5" PKNAME="nvme0n1" LABEL="New Volume" FSTYPE="ntfs" SIZE="336961994752" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/sdb" PKNAME="" LABEL="" FSTYPE="" SIZE="31004295168" RM="1" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sdb1" PKNAME="sdb" LABEL="My \x22USB\x22 日本" FSTYPE="exfat" SIZE="31003246592" RM="1" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/run/media/me/My \x22USB\x22 日本""#;
    let d = parse_lsblk(text);
    let get = |dev: &str| d.iter().find(|x| x.dev == dev).unwrap_or_else(|| panic!("{dev}"));
    let root = get("/dev/sda2");
    assert_eq!(root.mounts, vec![PathBuf::from("/var/tmp"), PathBuf::from("/home"), PathBuf::from("/")]);
    assert!(!root.removable && !get("/dev/sda1").removable, "the system disk is never ejectable, HOTPLUG or not");
    let usb = get("/dev/sdb1");
    assert!(usb.removable && usb.mounted());
    assert_eq!(usb.label, "My \"USB\" 日本");
    assert_eq!(usb.disk, "/dev/sdb");
    assert_eq!(usb.mounts, vec![PathBuf::from("/run/media/me/My \"USB\" 日本")]);
    // What gets offered for mounting: the big Windows partitions, not EFI/MSR/swap.
    let offered: Vec<&str> = d.iter().filter(|x| x.offer_mount()).map(|x| x.dev.as_str()).collect();
    assert_eq!(offered, ["/dev/nvme0n1p3", "/dev/nvme0n1p5"]);
    assert_eq!(get("/dev/nvme0n1p5").title(), "New Volume");
    assert!(get("/dev/nvme0n1p3").title().ends_with(" Volume"));
    // Garbage and partial lines don't panic.
    assert!(parse_lsblk("PATH=\"/dev/x\\x\" LABEL=\"\\xZZ é \\x\"\nnonsense\nPATH=").len() <= 1);
}

#[test]
fn stick_mounted_inside_home_stays_ejectable() {
    use crate::drives::parse_lsblk;
    let text = "PATH=\"/dev/sdc\" PKNAME=\"\" LABEL=\"\" FSTYPE=\"\" SIZE=\"8000000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"disk\" MOUNTPOINTS=\"\"\n\
PATH=\"/dev/sdc1\" PKNAME=\"sdc\" LABEL=\"S\" FSTYPE=\"vfat\" SIZE=\"7999000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"part\" MOUNTPOINTS=\"/home/me/usb\"\n\
PATH=\"/dev/sdd\" PKNAME=\"\" LABEL=\"\" FSTYPE=\"\" SIZE=\"8000000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"disk\" MOUNTPOINTS=\"\"\n\
PATH=\"/dev/sdd1\" PKNAME=\"sdd\" LABEL=\"\" FSTYPE=\"ext4\" SIZE=\"7999000000\" RM=\"1\" HOTPLUG=\"1\" TYPE=\"part\" MOUNTPOINTS=\"/home\"\n";
    let d = parse_lsblk(text);
    assert!(d.iter().find(|x| x.dev == "/dev/sdc1").unwrap().removable, "a folder inside /home is just a mount point");
    assert!(!d.iter().find(|x| x.dev == "/dev/sdd1").unwrap().removable, "but a disk holding /home itself is the system's");
    assert!(crate::drives::can_unmount(d.iter().find(|x| x.dev == "/dev/sdc1").unwrap()));
    assert!(!crate::drives::can_unmount(d.iter().find(|x| x.dev == "/dev/sdd1").unwrap()));
}

/// Encrypted volumes as lsblk lists them: a locked LUKS partition is offered (as
/// "Encrypted …" with no label), an unlocked one is not, the filesystem inside it is,
/// on the disk under it and lockable; a system disk unlocked at boot offers nothing.
#[test]
fn lsblk_encrypted_volumes_locked_and_unlocked() {
    use crate::drives::{Crypt, can_lock, parse_lsblk};
    let text = r#"PATH="/dev/nvme0n1" PKNAME="" LABEL="" FSTYPE="" SIZE="1024209543168" RM="0" HOTPLUG="0" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/nvme0n1p1" PKNAME="nvme0n1" LABEL="" FSTYPE="vfat" SIZE="629145600" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS="/boot/efi"
PATH="/dev/nvme0n1p2" PKNAME="nvme0n1" LABEL="" FSTYPE="crypto_LUKS" SIZE="807453851648" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/nvme0n1p3" PKNAME="nvme0n1" LABEL="" FSTYPE="crypto_LUKS" SIZE="214748364800" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS=""
PATH="/dev/mapper/luks-8c2b7d51-4c1e-4f0a-9d63-0f6e2a7b9c11" PKNAME="nvme0n1p2" LABEL="" FSTYPE="btrfs" SIZE="807437074432" RM="0" HOTPLUG="0" TYPE="crypt" MOUNTPOINTS="/home\x0a/"
PATH="/dev/sdv" PKNAME="" LABEL="" FSTYPE="" SIZE="536870912000" RM="0" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sdv1" PKNAME="sdv" LABEL="" FSTYPE="crypto_LUKS" SIZE="536869863424" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS=""
PATH="/dev/mapper/luks-3f6c0e5a-8d2b-4c1e-9a7f-2b5d8e1c4a90" PKNAME="sdv1" LABEL="Vault" FSTYPE="ext4" SIZE="536853086208" RM="0" HOTPLUG="0" TYPE="crypt" MOUNTPOINTS="/run/media/me/Vault"
PATH="/dev/sdw" PKNAME="" LABEL="" FSTYPE="" SIZE="64023257088" RM="1" HOTPLUG="1" TYPE="disk" MOUNTPOINTS=""
PATH="/dev/sdw1" PKNAME="sdw" LABEL="Diary" FSTYPE="crypto_LUKS" SIZE="64022208512" RM="1" HOTPLUG="1" TYPE="part" MOUNTPOINTS="""#;
    let d = parse_lsblk(text);
    let get = |dev: &str| d.iter().find(|x| x.dev == dev).unwrap_or_else(|| panic!("{dev}"));
    let offered: Vec<&str> = d.iter().filter(|x| x.offer_mount()).map(|x| x.dev.as_str()).collect();
    assert_eq!(offered, ["/dev/nvme0n1p3", "/dev/sdw1"], "locked volumes; not the open ones, nor what is mounted");
    assert_eq!(get("/dev/nvme0n1p3").crypt, Crypt::Locked);
    assert_eq!(get("/dev/nvme0n1p3").title(), "Encrypted 200 GB");
    assert_eq!(get("/dev/sdw1").title(), "Diary", "a LUKS2 label is its name");
    assert_eq!(get("/dev/sdv1").crypt, Crypt::Plain, "unlocked: what's inside stands for it");
    let vault = get("/dev/mapper/luks-3f6c0e5a-8d2b-4c1e-9a7f-2b5d8e1c4a90");
    assert_eq!(vault.crypt, Crypt::Open("/dev/sdv1".into()));
    assert_eq!(vault.disk, "/dev/sdv", "the disk under the partition it opens, to power off");
    assert!(vault.removable && can_lock(vault) && vault.title() == "Vault");
    let root = get("/dev/mapper/luks-8c2b7d51-4c1e-4f0a-9d63-0f6e2a7b9c11");
    assert_eq!(root.disk, "/dev/nvme0n1");
    assert!(!root.removable && !can_lock(root), "the system, unlocked at boot, stays as it is");
}

/// Unlocking through stand-ins for lsblk and udisksctl: the passphrase goes in on
/// udisksctl's input as a key file, exactly as typed, never on a command line; a wrong
/// one comes back as such and changes nothing; the right one unlocks and mounts what is
/// inside. Unmount, Lock and Eject close the volume again, after unmounting and before
/// the power goes.
#[test]
fn unlock_an_encrypted_drive_with_its_passphrase() {
    use crate::drives::{Crypt, Op, Passphrase, WRONG_PASSPHRASE, list, perform, unlock, wrong_passphrase};
    let d = tmpdir("unlock");
    let rows = [
        r#"PATH="/dev/sdv" PKNAME="" LABEL="" FSTYPE="" SIZE="536870912000" RM="0" HOTPLUG="1" TYPE="disk" MOUNTPOINTS="""#,
        r#"PATH="/dev/sdv1" PKNAME="sdv" LABEL="" FSTYPE="crypto_LUKS" SIZE="536869863424" RM="0" HOTPLUG="1" TYPE="part" MOUNTPOINTS="""#,
    ];
    std::fs::write(d.join("rows"), rows.join("\n") + "\n").unwrap();
    let inside = r#"PATH="/dev/mapper/luks-3f6c" PKNAME="sdv1" LABEL="Vault" FSTYPE="ext4" SIZE="536853086208" RM="0" HOTPLUG="0" TYPE="crypt" MOUNTPOINTS="""#;
    std::fs::write(d.join("inside"), format!("{inside}\n")).unwrap();
    fake_tool(&d, "lsblk", "cat \"$D/rows\"");
    let udisks = r#"case "$1" in
  unlock)
    if [ "$(cat; echo .)" = "correct horse." ]; then cat "$D/inside" >> "$D/rows"; echo "Unlocked $3 as /dev/dm-0."
    else echo "Error unlocking $3: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: Error unlocking $3: Failed to activate device: Operation not permitted" >&2; exit 1; fi ;;
  mount) echo "Mounted $3 at /run/media/me/Vault." ;;
esac"#;
    fake_tool(&d, "udisksctl", udisks);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let locked = list(&t).into_iter().find(|x| x.dev == "/dev/sdv1").unwrap();
    assert_eq!(locked.crypt, Crypt::Locked);
    let pass = |s: &str| Passphrase(s.to_string());
    assert_eq!(unlock(&t, &locked, &pass("correct horse\n")).err().as_deref(), Some(WRONG_PASSPHRASE), "exactly as typed");
    assert_eq!(unlock(&t, &locked, &pass("battery staple")).err().as_deref(), Some(WRONG_PASSPHRASE));
    assert_eq!(list(&t).len(), 2, "still locked");
    assert_eq!(unlock(&t, &locked, &pass("correct horse")).unwrap(), Some(PathBuf::from("/run/media/me/Vault")));
    let calls = fake_calls(&d);
    assert!(calls.iter().filter(|c| c[0] == "udisksctl").all(|c| !c.iter().any(|a| a.contains("horse") || a.contains("staple"))), "{calls:?}");
    let progs: Vec<&[String]> = calls.iter().filter(|c| c[0] == "udisksctl").map(|c| &c[1..]).collect();
    assert_eq!(progs.len(), 4);
    assert_eq!(progs[2], ["unlock", "-b", "/dev/sdv1", "--key-file", "/dev/stdin"]);
    assert_eq!(progs[3], ["mount", "-b", "/dev/mapper/luks-3f6c"]);
    // Closing it again: whatever is mounted comes off first, the power goes last.
    let all = list(&t);
    let mut vault = all.iter().find(|x| x.dev == "/dev/mapper/luks-3f6c").unwrap().clone();
    assert_eq!((vault.crypt.clone(), vault.disk.as_str()), (Crypt::Open("/dev/sdv1".into()), "/dev/sdv"));
    let _ = std::fs::remove_file(d.join("calls.log"));
    perform(&t, Op::Lock, &vault, &all).unwrap();
    vault.mounts = vec![PathBuf::from("/run/media/me/Vault")];
    perform(&t, Op::Unmount, &vault, &all).unwrap();
    let all: Vec<_> = all.into_iter().map(|x| if x.dev == vault.dev { vault.clone() } else { x }).collect();
    perform(&t, Op::Eject, &vault, &all).unwrap();
    let calls: Vec<String> = fake_calls(&d).iter().map(|c| c[1..].join(" ")).collect();
    assert_eq!(calls, ["lock -b /dev/sdv1", "unmount -b /dev/mapper/luks-3f6c", "lock -b /dev/sdv1", "unmount -b /dev/mapper/luks-3f6c", "lock -b /dev/sdv1", "power-off -b /dev/sdv"]);
    // cryptsetup's words for a wrong passphrase, old and new.
    assert!(wrong_passphrase("Failed to activate device: Incorrect passphrase") && wrong_passphrase("No key available with this passphrase"));
    assert!(!wrong_passphrase("Not authorized to perform operation"));
}

#[test]
fn uri_lists_round_trip_any_file_name() {
    use crate::clipboard::{parse_uri_list, path_to_uri, uri_list};
    let paths: Vec<PathBuf> = ["/home/me/a b.txt", "/tmp/résumé – v3 (final).pdf", "/x/100% #1?.png", "/日本/ファイル", "/q/'quote\"s'", "/p/a+b=c&d"].iter().map(PathBuf::from).collect();
    let body = uri_list(&paths);
    assert!(body.lines().all(|l| l.starts_with("file:///") && !l.contains(' ')), "{body}");
    assert!(body.ends_with("\r\n"));
    assert_eq!(parse_uri_list(&body), paths);
    assert_eq!(path_to_uri(std::path::Path::new("/a b")), "file:///a%20b");
    // What other apps send: comments, LF or CRLF, localhost, other schemes.
    let foreign = "# copied by an app\nfile:///home/me/x.txt\r\nfile://localhost/home/me/y%20z.png\nhttps://example.com/pic.jpg\n\n  file:///tmp/t  \n";
    assert_eq!(parse_uri_list(foreign), ["/home/me/x.txt", "/home/me/y z.png", "/tmp/t"].iter().map(PathBuf::from).collect::<Vec<_>>());
    // Not valid UTF-8 on disk: still the same bytes back.
    use std::os::unix::ffi::OsStrExt;
    let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/odd/\xff\xfe.bin"));
    assert_eq!(parse_uri_list(&uri_list(std::slice::from_ref(&odd))), vec![odd]);
    assert!(parse_uri_list("file:///bad%zz%").len() == 1, "malformed escapes are kept, not panicked on");
}

#[test]
fn copied_files_read_as_other_file_managers_write_them() {
    use crate::clipboard::{file_items, parse_gnome_files, uri_list};
    let paths: Vec<PathBuf> = ["/home/me/a b.txt", "/tmp/résumé.pdf"].iter().map(PathBuf::from).collect();
    let text = |items: &[(String, Vec<u8>)], m: &str| items.iter().find(|i| i.0 == m).map(|i| String::from_utf8_lossy(&i.1).into_owned());
    let copied = file_items(&paths, false);
    assert_eq!(text(&copied, "text/uri-list").unwrap(), uri_list(&paths));
    assert_eq!(text(&copied, "x-special/gnome-copied-files").unwrap(), "copy\nfile:///home/me/a%20b.txt\nfile:///tmp/r%C3%A9sum%C3%A9.pdf");
    assert_eq!(text(&copied, "UTF8_STRING").unwrap(), "/home/me/a b.txt\n/tmp/résumé.pdf");
    assert!(text(&copied, "application/x-kde-cutselection").is_none());
    let cut = file_items(&paths, true);
    assert!(text(&cut, "x-special/gnome-copied-files").unwrap().starts_with("cut\n"));
    assert_eq!(text(&cut, "application/x-kde-cutselection").unwrap(), "1");
    for items in [&copied, &cut] {
        let g = text(items, "x-special/gnome-copied-files").unwrap();
        assert_eq!(parse_gnome_files(&g), (paths.clone(), items == &cut));
    }
    // As Nautilus and Caja write it: a trailing newline, localhost.
    assert_eq!(parse_gnome_files("cut\nfile:///a%20b\nfile://localhost/c\n"), (vec![PathBuf::from("/a b"), PathBuf::from("/c")], true));
    assert_eq!(parse_gnome_files(""), (vec![], false));
}

/// The X11 clipboard and drag out against a real server: two clipboards of ours trade
/// file lists (a large one in pieces), and a minimal XDND target takes a drop. It takes
/// the clipboard and moves the pointer, so it runs only when asked, on a throwaway
/// server: MAIA_TEST_X11=1 DISPLAY=:N.
#[test]
fn x11_clipboard_and_drag_out() {
    use crate::clipboard::{file_items, parse_gnome_files, uri_list};
    use crate::xclip::XClip;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;
    if std::env::var_os("MAIA_TEST_X11").is_none() {
        return;
    }
    let (a, b) = (XClip::spawn(None, 0).unwrap(), XClip::spawn(None, 0).unwrap());
    // Asked again until the other clipboard has taken over.
    let load = |c: &XClip, prefer: &[&str], want: &str| -> Vec<u8> {
        let t0 = Instant::now();
        loop {
            match c.load(prefer) {
                Ok((m, bytes)) if m == want => return bytes,
                r if t0.elapsed() > Duration::from_secs(5) => panic!("{want}: {r:?}"),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    };
    let paths: Vec<PathBuf> = ["/home/me/a b.txt", "/tmp/résumé.pdf"].iter().map(PathBuf::from).collect();
    a.store(file_items(&paths, true));
    let g = load(&b, &["x-special/gnome-copied-files", "text/uri-list"], "x-special/gnome-copied-files");
    assert_eq!(parse_gnome_files(&String::from_utf8_lossy(&g)), (paths.clone(), true));
    assert_eq!(load(&b, &["application/x-kde-cutselection"], "application/x-kde-cutselection"), b"1");
    assert_eq!(load(&a, &["text/uri-list"], "text/uri-list"), uri_list(&paths).into_bytes(), "our own, without the server");
    // Text under X11's own names: UTF8_STRING for plain text, STRING as Latin-1.
    b.store(vec![("UTF8_STRING".into(), "héllo ✓".into())]);
    assert_eq!(load(&a, &["text/plain;charset=utf-8", "text/plain"], "UTF8_STRING"), "héllo ✓".as_bytes());
    assert_eq!(load(&a, &["STRING"], "STRING"), "héllo ?".as_bytes());
    // Lists too large for one request go in pieces, served and read.
    let many: Vec<PathBuf> = (0..5000).map(|i| PathBuf::from(format!("/home/me/Pictures/Trip to the coast/IMG_{i:05} copy.jpg"))).collect();
    assert!(uri_list(&many).len() > crate::xclip::CHUNK);
    a.store(file_items(&many, false));
    assert_eq!(load(&b, &["text/uri-list"], "text/uri-list"), uri_list(&many).into_bytes());

    // A drop target: a window that is XDND-aware, under the pointer.
    let (conn, screen) = x11rb::xcb_ffi::XCBConnection::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |n: &str| conn.intern_atom(false, n.as_bytes()).unwrap().reply().unwrap().atom;
    let win = conn.generate_id().unwrap();
    conn.create_window(0, win, root, 100, 100, 200, 200, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new().override_redirect(1)).unwrap();
    conn.change_property32(PropMode::REPLACE, win, atom("XdndAware"), AtomEnum::ATOM, &[5]).unwrap();
    conn.map_window(win).unwrap();
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 150, 150).unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
    // No button is held, so the drag drops at once: entered, positioned, dropped, read.
    a.start_drag(paths.clone(), 1.0);
    let (copy, uris) = (atom("XdndActionCopy"), atom("text/uri-list"));
    let mut seen = vec![];
    let t0 = Instant::now();
    let got = loop {
        assert!(t0.elapsed() < Duration::from_secs(5), "drop not finished: {seen:?}");
        let Some(ev) = conn.poll_for_event().unwrap() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        match ev {
            Event::ClientMessage(e) => {
                let l = e.data.as_data32();
                let name = String::from_utf8(conn.get_atom_name(e.type_).unwrap().reply().unwrap().name).unwrap();
                let reply = |kind: &str, data: [u32; 5]| conn.send_event(false, l[0], EventMask::NO_EVENT, ClientMessageEvent::new(32, l[0], atom(kind), data)).unwrap();
                match name.as_str() {
                    "XdndEnter" => {
                        assert_eq!((l[1] >> 24, l[1] & 1), (5, 1), "version 5, more than three types");
                        let list = conn.get_property(false, l[0], atom("XdndTypeList"), AtomEnum::ATOM, 0, 16).unwrap().reply().unwrap();
                        assert!(list.value32().unwrap().any(|t| t == uris));
                    }
                    "XdndPosition" => {
                        assert_eq!((l[2] >> 16, l[2] & 0xffff, l[4]), (150, 150, copy), "at the pointer, copying");
                        reply("XdndStatus", [win, 1, 0, 0, copy]);
                    }
                    "XdndDrop" => {
                        conn.convert_selection(win, atom("XdndSelection"), uris, atom("MAIA_TEST"), l[2]).unwrap();
                    }
                    _ => {}
                }
                seen.push(name);
            }
            Event::SelectionNotify(e) => {
                let r = conn.get_property(true, win, e.property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
                let source = conn.get_selection_owner(atom("XdndSelection")).unwrap().reply().unwrap().owner;
                conn.send_event(false, source, EventMask::NO_EVENT, ClientMessageEvent::new(32, source, atom("XdndFinished"), [win, 1, copy, 0, 0])).unwrap();
                break r.value;
            }
            _ => {}
        }
        conn.flush().unwrap();
    };
    conn.flush().unwrap();
    assert_eq!(seen, ["XdndEnter", "XdndPosition", "XdndDrop"]);
    assert_eq!(got, uri_list(&paths).into_bytes());
    // Over Maia's own window the drag is Maia's again, in its logical pixels.
    let c = XClip::spawn(None, win).unwrap();
    crate::wlclip::take_events();
    c.start_drag(paths.clone(), 2.0);
    let t0 = Instant::now();
    let mut events = vec![];
    while !events.iter().any(|e| matches!(e, crate::wlclip::Dnd::Dropped { .. })) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
        events.extend(crate::wlclip::take_events());
    }
    assert_eq!(events.last(), Some(&crate::wlclip::Dnd::Dropped { x: 25.0, y: 25.0, paths, own: true }), "{events:?}");
    assert!(conn.poll_for_event().unwrap().is_none(), "no XDND to Maia itself");
}

#[test]
fn drag_picture_is_a_clean_premultiplied_card() {
    let ctx = egui::Context::default();
    let cfg = crate::config::Config::default();
    crate::theme::install_fonts(&ctx, &cfg.font_family, &cfg.inspector_font);
    let mut raw = egui::RawInput::default();
    raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(1.6);
    let _ = ctx.run(raw, |_| {});
    let pal = crate::theme::palette(crate::theme::ThemeName::default());
    let dir = std::env::temp_dir();
    let long = PathBuf::from("/x/a very long file name that keeps going and going well past what fits.txt");
    let cases: Vec<(&str, Vec<PathBuf>)> = vec![("one", vec![PathBuf::from("/x/photo from chat.png")]), ("many", vec![PathBuf::from("/x/a"), PathBuf::from("/x/b"), PathBuf::from("/x/c")]), ("folder", vec![dir.clone()]), ("long", vec![long])];
    for (name, paths) in cases {
        let pic = crate::ui::drag_picture(&ctx, &paths, &pal);
        let (w, h, px) = (pic.w, pic.h, &pic.px);
        assert_eq!(pic.scale, 2, "1.6 rounds up to a 2x picture");
        assert_eq!(h, 60);
        assert_eq!(px.len(), w * h * 4);
        assert!(px.chunks(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]), "premultiplied");
        let at = |x: usize, y: usize| &px[(y * w + x) * 4..(y * w + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "rounded corner is transparent");
        assert!(at(w / 2, h - 4)[3] > 230, "card is nearly opaque");
        let text = (70..w).flat_map(|x| (0..h).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[1] > 150).count();
        assert!(text > 60, "{name}: the label is drawn ({text} bright pixels)");
        assert!(w <= ((10.0 + 16.0 + 8.0 + 241.0 + 12.0) * 2.0) as usize + 2, "{name}: long names are cut ({w})");
        if let Some(d) = std::env::var_os("MAIA_ICON_DUMP") {
            let mut rgba = px.clone();
            for p in rgba.chunks_mut(4) {
                p.swap(0, 2);
            }
            let f = std::fs::File::create(std::path::Path::new(&d).join(format!("drag-{name}.png"))).unwrap();
            let mut e = png::Encoder::new(f, w as u32, h as u32);
            e.set_color(png::ColorType::Rgba);
            e.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
    }
}

#[test]
fn window_icon_is_the_launcher_icon() {
    let (px, w, h) = crate::install::window_icon();
    assert_eq!((w, h), (64, 64));
    let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
    assert_eq!(at(0, 0)[3], 0, "rounded corner");
    assert!(at(32, 60)[3] == 255 && at(32, 60)[0] < 40, "dark tile");
    // The star's centre (82, 46 of 128) is white, and its glow is blue.
    let c = at(41, 23);
    assert!(c[0] > 240 && c[1] > 240 && c[2] > 240, "star {c:?}");
    let g = at(41, 31);
    assert!(g[2] > g[0], "blue glow under the star {g:?}");
    if let Some(d) = std::env::var_os("MAIA_ICON_DUMP") {
        let f = std::fs::File::create(std::path::Path::new(&d).join("window-icon.png")).unwrap();
        let mut e = png::Encoder::new(f, w, h);
        e.set_color(png::ColorType::Rgba);
        e.write_header().unwrap().write_image_data(&px).unwrap();
    }
}

#[test]
fn finished_jobs_read_in_the_past_tense() {
    use crate::ui::finished_label;
    assert_eq!(finished_label("Copying 1 item"), "Copied 1 item");
    assert_eq!(finished_label("Moving 3 items"), "Moved 3 items");
    assert_eq!(finished_label("Moving 2 items to the trash"), "Moved 2 items to the trash");
    assert_eq!(crate::ui::job_undo_label("Moving notes.txt to the trash"), "Move notes.txt to the trash");
    assert_eq!(crate::ui::job_undo_label("Copying 3 items"), "Copy 3 items");
    assert_eq!(finished_label("Deleting 1 item"), "Deleted 1 item");
    assert_eq!(crate::ui::failed_label("Copying 1 item"), "Couldn't copy 1 item");
    assert_eq!(crate::ui::failed_label("Moving 2 items to the trash"), "Couldn't move 2 items to the trash");
    assert_eq!(crate::ui::failed_label("Deleting 3 items as administrator"), "Couldn't delete 3 items as administrator");
}

/// Run a tab's contents search to the end and rebuild its view.
fn pump_contents(t: &mut Tab, hidden: bool) -> Vec<String> {
    let stats = HashMap::new();
    let recents = HashSet::new();
    let ctx = ViewCtx { show_hidden: hidden, show_system: false, folders_first: true, recents_on_top: false, recents: &recents, stats: &stats };
    let start = Instant::now();
    loop {
        crate::contentsearch::pump(t, hidden);
        t.poll();
        if !t.is_loading() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "contents search finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    crate::contentsearch::pump(t, hidden);
    t.rebuild(&ctx);
    let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
    v.sort();
    v
}

#[test]
fn contents_search_finds_text_and_skips_binary_big_and_hidden() {
    let d = tmpdir("contents");
    touch(&d.join("a.txt"), "Hello World\n\tsecond   line has Needle here\nthird needle\n");
    touch(&d.join("b.md"), "nothing to see");
    touch(&d.join("needle-in-name.txt"), "the name matches, the text does not");
    std::fs::write(d.join("c.bin"), b"\x7fELF\x00\x00 needle").unwrap();
    touch(&d.join(".hidden.txt"), "a hidden needle");
    touch(&d.join("sub/deep.txt"), "NEEDLE at depth");
    touch(&d.join("long.txt"), &format!("{}needle{}\n", "x".repeat(500), "y".repeat(500)));
    touch(&d.join("grüße.txt"), "Viel ÄRGER heute\n");
    // A match across the 64 KB read boundary is still found.
    let mut seam = vec![b'.'; (64 << 10) - 3];
    seam.extend_from_slice(b"needle\n");
    std::fs::write(d.join("seam.txt"), seam).unwrap();
    // Over the size cap (sparse, so it costs no disk): skipped although it matches.
    let big = std::fs::File::create(d.join("big.log")).unwrap();
    std::io::Write::write_all(&mut &big, b"needle\n").unwrap();
    big.set_len(crate::contentsearch::MAX_BYTES + 1).unwrap();
    drop(big);

    let mut t = Tab::new(40, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &HashMap::new());
    t.content.on = true;
    t.filter = "needle".into();
    assert_eq!(pump_contents(&mut t, false), vec!["a.txt", "long.txt", "seam.txt"]);
    assert_eq!(t.content.snippet(&d.join("a.txt")), Some("second line has Needle here"));
    let long = t.content.snippet(&d.join("long.txt")).unwrap_or_default().to_string();
    assert!(long.starts_with('…') && long.contains("needle") && long.chars().count() < 140, "{long}");
    // Hidden files once they are shown; the subfolder only when flattened.
    assert_eq!(pump_contents(&mut t, true), vec![".hidden.txt", "a.txt", "long.txt", "seam.txt"]);
    t.set_flatten(true);
    assert_eq!(pump_contents(&mut t, false), vec!["a.txt", "deep.txt", "long.txt", "seam.txt"]);
    t.set_flatten(false);
    pump(&mut t, &HashMap::new());
    // Case-insensitive outside ASCII too.
    t.filter = "ärger".into();
    assert_eq!(pump_contents(&mut t, false), vec!["grüße.txt"]);
    assert_eq!(t.content.snippet(&d.join("grüße.txt")), Some("Viel ÄRGER heute"));
    // Back to names: the same query matches the file name, not the text.
    t.filter = "needle".into();
    t.content.on = false;
    assert_eq!(pump_contents(&mut t, false), vec!["needle-in-name.txt"]);
    assert!(t.content.take().is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn contents_search_never_opens_a_fifo_and_stops_when_cancelled() {
    let d = tmpdir("contents_fifo");
    let fifo = std::ffi::CString::new(d.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    touch(&d.join("x.txt"), "needle");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut buf = vec![0u8; 64 << 10];
    let t = Instant::now();
    assert_eq!(crate::contentsearch::find(&d.join("pipe"), "needle", &mut buf, &stop), None);
    assert!(t.elapsed() < Duration::from_secs(1), "the FIFO was not waited on");
    assert_eq!(crate::contentsearch::find(&d.join("x.txt"), "needle", &mut buf, &stop).as_deref(), Some("needle"));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(crate::contentsearch::find(&d.join("x.txt"), "needle", &mut buf, &stop), None, "a cancelled search reads nothing");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn second_launch_hands_its_folders_to_the_first() {
    use crate::single::{Request, claim_at, forward_to, parse, serve};
    let d = tmpdir("single");
    let sock = d.join("maia.sock");
    assert!(!forward_to(&sock, &[PathBuf::from("/x")]), "nobody listening: start normally");
    let l = claim_at(&sock).expect("first launch listens");
    assert!(claim_at(&sock).is_none(), "a second one does not take over");
    let (tx, rx) = std::sync::mpsc::channel();
    serve(l, tx, || {});
    let odd = PathBuf::from("/tmp/two\nlines – é/日本 #1");
    assert!(forward_to(&sock, &[PathBuf::from("/home"), odd.clone()]), "handed over");
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), Request::Open(vec![PathBuf::from("/home"), odd]));
    assert!(forward_to(&sock, &[]));
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), Request::Focus);
    // A socket left by a crash (nobody answering) is replaced.
    let stale = d.join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(stale.exists() && claim_at(&stale).is_some(), "stale socket replaced");
    // Garbage never panics.
    assert_eq!(parse(b"open"), None);
    assert_eq!(parse(b"\0\0\0"), None);
    assert_eq!(parse(b"what\0ever\0"), None);
}

#[test]
fn default_file_manager_is_read_from_mimeapps() {
    use crate::install::default_in;
    let t = "[Added Associations]\ninode/directory=other.desktop;\n\n[Default Applications]\ntext/plain=gedit.desktop\ninode/directory=io.github.yappologistic.Maia.desktop;org.gnome.Nautilus.desktop;\n";
    assert_eq!(default_in(t).as_deref(), Some("io.github.yappologistic.Maia.desktop"));
    assert_eq!(default_in("[Default Applications]\ninode/directory= ;thunar.desktop\n").as_deref(), Some("thunar.desktop"));
    assert_eq!(default_in("[Added Associations]\ninode/directory=x.desktop\n"), None, "only Default Applications counts");
    assert_eq!(default_in(""), None);
}

#[test]
fn archive_listings_parse_like_bsdtar_prints_them() {
    use crate::archive::parse_list;
    let now = std::time::SystemTime::now();
    // Captured from bsdtar 3.8: a zip (entries stored as "./name"), a tar.gz with a
    // symlink, and a 7z that lists a folder after its file.
    let zip = "drwxr-xr-x  0 1000   1000        0 Oct 14 22:57 ./\n-rw-r--r--  0 1000   1000        0 Mar  5  2020 ./old.txt\nlrwxrwxrwx  0 1000   1000        0 Oct 14 22:57 ./link -> top.txt\n-rw-r--r--  0 1000   1000        2 Oct 14 22:57 ./top.txt\ndrwxr-xr-x  0 1000   1000        0 Oct 14 22:57 ./docs dir/\n-rw-r--r--  0 1000   1000        6 Oct 14 22:57 ./docs dir/read me.md\n";
    let items = parse_list(zip, now);
    let names: Vec<String> = items.iter().map(|i| i.path.display().to_string()).collect();
    assert_eq!(names, ["old.txt", "link", "top.txt", "docs dir", "docs dir/read me.md"], "./ stripped, the root entry dropped");
    assert!(items.iter().all(|i| i.dot), "stored with ./");
    let link = &items[1];
    assert!(link.link && !link.is_dir, "symlink without its target");
    assert!(items[3].is_dir && items[4].size == 6);
    // "Mar  5  2020" is that day; "Oct 14 22:57" is this year (or the last).
    let year_of = |t: std::time::SystemTime| crate::timefmt::format(t, "%Y");
    assert_eq!(year_of(items[0].modified.unwrap()), "2020");
    assert!(items[2].modified.unwrap() <= now + std::time::Duration::from_secs(86_400));
    // Folders that are only prefixes get rows; "../" escapes are refused.
    let bare = "-rw-r--r--  0 0      0           2 Oct 14 22:57 a/b/c.txt\n-rw-r--r--  0 0 0 1 Oct 14 22:57 ../evil\n";
    let items = parse_list(bare, now);
    let mut names: Vec<String> = items.iter().map(|i| format!("{}{}", i.path.display(), if i.is_dir { "/" } else { "" })).collect();
    names.sort();
    assert_eq!(names, ["a/", "a/b/", "a/b/c.txt"]);
    // bsdtar's escapes for odd names, and garbage lines.
    let odd = "-rw-r--r--  0 0      0           2 Oct 14 22:57 back\\\\slash \\303\\251.txt\nnonsense\n\n-rw-r--r-- 0\n";
    assert_eq!(parse_list(odd, now).iter().map(|i| i.path.display().to_string()).collect::<Vec<_>>(), ["back\\slash é.txt"]);
    assert_eq!(crate::archive::stem("photos.tar.gz"), "photos");
    assert_eq!(crate::archive::stem("Doc.ZIP"), "Doc");
    assert!(crate::archive::is_archive("x.tar.zst") && !crate::archive::is_archive(".zip") && !crate::archive::is_archive("notes.txt"));
}

#[test]
fn archives_round_trip_through_bsdtar() {
    if !crate::archive::available() {
        eprintln!("bsdtar missing: skipped");
        return;
    }
    let d = tmpdir("archive");
    let src = d.join("src");
    std::fs::create_dir_all(src.join("Trip photos")).unwrap();
    std::fs::write(src.join("Trip photos/beach – é.txt"), "sand").unwrap();
    std::fs::write(src.join("notes.txt"), "hello").unwrap();
    let names = vec![std::ffi::OsString::from("Trip photos"), std::ffi::OsString::from("notes.txt")];
    for ext in ["zip", "tar.gz", "tar.xz", "7z"] {
        let out = d.join(format!("pack.{ext}"));
        crate::archive::create(&out, &src, &names).unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert!(out.is_file() && !d.join(format!(".pack.{ext}.part")).exists(), "{ext}: written in one go");
        let top: Vec<String> = crate::archive::entries(&out, std::path::Path::new("")).unwrap().iter().map(|e| e.name.clone()).collect();
        assert!(top.contains(&"Trip photos".into()) && top.contains(&"notes.txt".into()), "{ext}: {top:?}");
        let inner = crate::archive::entries(&out, std::path::Path::new("Trip photos")).unwrap();
        assert_eq!(inner.len(), 1, "{ext}");
        assert_eq!(inner[0].path, out.join("Trip photos/beach – é.txt"), "{ext}: rows continue the archive's path");
        assert!(crate::archive::entries(&out, std::path::Path::new("nope")).is_err());
        let x = d.join(format!("x-{ext}"));
        crate::archive::extract(&out, &[std::path::PathBuf::from("Trip photos")], &x).unwrap();
        assert_eq!(std::fs::read_to_string(x.join("Trip photos/beach – é.txt")).unwrap(), "sand", "{ext}");
        assert!(!x.join("notes.txt").exists(), "{ext}: only what was asked");
        let (a, i) = crate::archive::split(&out.join("Trip photos/beach – é.txt")).unwrap();
        assert_eq!((a, i), (out.clone(), std::path::PathBuf::from("Trip photos/beach – é.txt")));
    }
    assert!(crate::archive::split(&src.join("notes.txt")).is_none());
    assert!(crate::archive::create(&d.join("bad.zip"), &src, &[std::ffi::OsString::from("missing")]).is_err(), "errors come back");
    assert!(!d.join("bad.zip").exists() && !d.join(".bad.zip.part").exists(), "nothing left behind");
}

#[test]
fn merge_sort_is_stable_and_matches_sort_by() {
    let mut x: u64 = 0x2545f4914f6cdd1d;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for n in [0usize, 1, 2, 23, 24, 25, 47, 48, 49, 100, 1000, 4097, 50_000] {
        for shape in 0..4 {
            // (key, original position): few distinct keys, so stability shows.
            let v: Vec<(u32, u32)> = (0..n as u32)
                .map(|i| match shape {
                    0 => ((rnd() % 7) as u32, i),
                    1 => (i, i),
                    2 => (n as u32 - i, i),
                    _ => ((i / 3) as u32, i),
                })
                .collect();
            let mut a = v.clone();
            let mut b = v.clone();
            crate::tab::merge_sort(&mut a, &|p: &(u32, u32), q: &(u32, u32)| p.0.cmp(&q.0));
            b.sort_by(|p, q| p.0.cmp(&q.0));
            assert_eq!(a, b, "n {n} shape {shape}");
        }
    }
}

/// Run git in `dir` (as a fixed test identity), failing the test if it fails.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok || args.first() == Some(&"merge"), "git {args:?}");
}

/// A work tree with one change of every kind: a merge conflict, a modification, a staged
/// new file, an untracked file, a staged deletion, a rename and a change two folders down.
pub(crate) fn git_fixture(d: &Path) {
    touch(&d.join("keep.txt"), "same\n");
    touch(&d.join("mod.txt"), "one\n");
    touch(&d.join("old.txt"), "to be renamed\n");
    touch(&d.join("gone.txt"), "to be removed\n");
    touch(&d.join("conflict.txt"), "base\n");
    touch(&d.join("sub/inner/deep.txt"), "deep\n");
    git(d, &["init", "-q"]);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    git(d, &["checkout", "-qb", "other"]);
    touch(&d.join("conflict.txt"), "theirs\n");
    git(d, &["commit", "-qam", "theirs"]);
    git(d, &["checkout", "-q", "main"]);
    touch(&d.join("conflict.txt"), "ours\n");
    git(d, &["commit", "-qam", "ours"]);
    git(d, &["merge", "-q", "other"]);
    touch(&d.join("mod.txt"), "two\n");
    touch(&d.join("staged.txt"), "new\n");
    git(d, &["add", "staged.txt"]);
    touch(&d.join("new.txt"), "untracked\n");
    git(d, &["rm", "-q", "gone.txt"]);
    git(d, &["mv", "old.txt", "renamed.txt"]);
    touch(&d.join("sub/inner/deep.txt"), "deeper\n");
}

#[test]
fn git_status_marks_every_kind_of_change() {
    use crate::gitstatus::{Mark, query};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("git");
    git_fixture(&d);
    let r = query(&d.join("sub")).expect("a work tree");
    assert_eq!(r.root, d);
    assert_eq!(r.branch, "main");
    let m = |p: &str| r.marks.get(&d.join(p)).copied();
    assert_eq!(m("conflict.txt"), Some(Mark::Conflict));
    assert_eq!(m("mod.txt"), Some(Mark::Modified));
    assert_eq!(m("staged.txt"), Some(Mark::Staged));
    assert_eq!(m("new.txt"), Some(Mark::Untracked));
    assert_eq!(m("gone.txt"), Some(Mark::Deleted));
    assert_eq!(m("renamed.txt"), Some(Mark::Renamed));
    assert_eq!(m("old.txt"), None, "a rename's source is not marked");
    assert_eq!(m("sub/inner/deep.txt"), Some(Mark::Modified));
    assert_eq!(m("sub"), Some(Mark::Inside));
    assert_eq!(m("sub/inner"), Some(Mark::Inside));
    assert_eq!(m("keep.txt"), None);
    // Reached through a symlink, marks use the path as the user sees it.
    let link = std::env::temp_dir().join(format!("maia-test-gitlink-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&d, &link).unwrap();
    let r = query(&link.join("sub/inner")).expect("a work tree through a link");
    assert_eq!(r.root, link);
    assert_eq!(r.marks.get(&link.join("mod.txt")), Some(&Mark::Modified));
    let _ = std::fs::remove_file(&link);
    // Outside a work tree, and inside .git itself, there is nothing to mark.
    let plain = tmpdir("nogit");
    assert!(query(&plain).is_none());
    assert!(query(&d.join(".git")).is_none());
    // A fresh repository with no commits yet still names its branch.
    git(&plain, &["init", "-q"]);
    touch(&plain.join("a.txt"), "a");
    let r = query(&plain).expect("an empty repository");
    assert_eq!((r.branch.as_str(), r.marks.get(&plain.join("a.txt"))), ("main", Some(&Mark::Untracked)));
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&plain);
}

/// Two folders with one difference of every kind (the compare journey uses them too):
/// a newer file, a file of another size, files and folders on one side only, a name that
/// is a file here and a folder there, and identical entries.
pub(crate) fn compare_fixture(a: &Path, b: &Path) {
    let t0 = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = |p: PathBuf, body: &str, when: std::time::SystemTime| {
        touch(&p, body);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
    };
    for d in [a, b] {
        file(d.join("same.txt"), "same", t0);
        std::fs::create_dir_all(d.join("both")).unwrap();
    }
    file(a.join("newer.txt"), "new", t0 + Duration::from_secs(3600));
    file(b.join("newer.txt"), "old", t0);
    file(a.join("size.txt"), "longer text", t0);
    file(b.join("size.txt"), "short", t0);
    file(a.join("only-a.txt"), "a", t0);
    file(b.join("only-b.txt"), "b", t0);
    std::fs::create_dir_all(a.join("folder-a")).unwrap();
    file(a.join("kind"), "a file", t0);
    std::fs::create_dir_all(b.join("kind")).unwrap();
}

#[test]
fn compare_marks_only_newer_and_size_differences() {
    use crate::compare::{Diff, side};
    let d = tmpdir("compare");
    let (a, b) = (d.join("a"), d.join("b"));
    compare_fixture(&a, &b);
    let (la, lb) = (crate::fsx::list_dir(&a).unwrap(), crate::fsx::list_dir(&b).unwrap());
    let (ea, eb): (Vec<_>, Vec<_>) = (la.iter().collect(), lb.iter().collect());
    let mut m = HashMap::new();
    side(&ea, &eb, &mut m);
    side(&eb, &ea, &mut m);
    let want: HashMap<PathBuf, Diff> = [
        (a.join("newer.txt"), Diff::Newer),
        (a.join("size.txt"), Diff::Size),
        (b.join("size.txt"), Diff::Size),
        (a.join("only-a.txt"), Diff::Only),
        (b.join("only-b.txt"), Diff::Only),
        (a.join("folder-a"), Diff::Only),
        (a.join("kind"), Diff::Only),
        (b.join("kind"), Diff::Only),
    ]
    .into_iter()
    .collect();
    assert_eq!(m, want, "identical files, the older side and folders on both sides stay unmarked");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn aliases_set_change_clear_and_refuse_duplicates() {
    use crate::popups::set_alias;
    let mut ud = crate::config::UserData::default();
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "  hh "), Ok(()));
    assert_eq!(ud.aliases.get("toggle_hidden").map(String::as_str), Some("hh"), "trimmed and stored by command id");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleSidebar, "HH"), Err(Cmd::ToggleHidden), "another command's alias, in any case, is refused");
    assert!(!ud.aliases.contains_key("toggle_sidebar"));
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "hid"), Ok(()));
    assert_eq!(set_alias(&mut ud, Cmd::ToggleSidebar, "hh"), Ok(()), "a changed alias frees the old one");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, "hid"), Ok(()), "setting the same alias again is fine");
    assert_eq!(set_alias(&mut ud, Cmd::ToggleHidden, ""), Ok(()));
    assert!(!ud.aliases.contains_key("toggle_hidden"), "empty text clears it");
    // An alias left behind by a command that no longer exists blocks nothing.
    ud.aliases.insert("gone_command".into(), "zz".into());
    assert_eq!(set_alias(&mut ud, Cmd::GoTo, "zz"), Ok(()));
}

#[test]
fn network_places_parse_from_gio() {
    use crate::network::{is_uri, label, normalize, parse_mounts};
    let text = "Drive(0): Micron MTFDKBA1T0TFH\n  Type: GProxyDrive (GProxyVolumeMonitorUDisks2)\n  Volume(0): 681 GB Volume\n    Type: GProxyVolume (GProxyVolumeMonitorUDisks2)\n    can_mount=1\nVolume(0): Pixel 7\n  Type: GProxyVolume (GProxyVolumeMonitorMTP)\n  activation_root=mtp://Google_Pixel_7_2A09/\n  can_mount=1\nVolume(1): Camera\n  Type: GProxyVolume (GProxyVolumeMonitorGPhoto2)\n  activation_root=gphoto2://Canon_EOS/\n  Mount(0): Camera -> gphoto2://Canon_EOS/\n    Type: GProxyShadowMount (GProxyVolumeMonitorGPhoto2)\nMount(0): media on nas -> smb://nas/media/\n  Type: GDaemonMount\n  default_location=smb://nas/media/\nMount(1): me on host -> sftp://me@host/\n  Type: GDaemonMount\nMount(2): Backup -> file:///run/media/me/Backup\n  Type: GProxyMount (GProxyVolumeMonitorUDisks2)\n";
    let p = parse_mounts(text);
    let got: Vec<(String, bool, bool)> = p.iter().map(|x| (x.uri.clone(), x.device, x.mounted)).collect();
    assert_eq!(
        got,
        [("mtp://Google_Pixel_7_2A09/".into(), true, false), ("gphoto2://Canon_EOS/".into(), true, true), ("smb://nas/media/".into(), false, true), ("sftp://me@host/".into(), false, true)],
        "local disks left to Drives, phones and shares kept"
    );
    assert_eq!(p[0].name, "Pixel 7");
    assert!(is_uri("smb://nas/media") && is_uri("SFTP://me@host") && !is_uri("/home/me") && !is_uri("smb://") && !is_uri("http://x"));
    assert_eq!(normalize(" ssh://me@host/srv "), "sftp://me@host/srv");
    assert_eq!(label("smb://nas/media/"), "media on nas");
    assert_eq!(label("sftp://me@host/"), "me@host");
    assert!(parse_mounts("garbage\nMount(\nVolume(3)\n").is_empty());
}

#[test]
fn administrator_commands_are_exact() {
    use crate::admin::{Task, denied};
    let s = |v: Vec<std::ffi::OsString>| v.into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let a = s(Task::Copy { srcs: vec!["/home/me/a b".into(), "/home/me/-x".into()], dest: "/etc".into() }.argv());
    assert!(a[0].ends_with("/cp"), "absolute program: {}", a[0]);
    assert_eq!(&a[1..], ["-a", "--", "/home/me/a b", "/home/me/-x", "/etc"], "-- before names that look like options");
    assert_eq!(&s(Task::Delete { paths: vec!["/srv/x".into()] }.argv())[1..], ["-rf", "--", "/srv/x"]);
    assert_eq!(&s(Task::Rename { from: "/etc/a".into(), to: "/etc/b".into() }.argv())[1..], ["-T", "--", "/etc/a", "/etc/b"]);
    assert!(s(Task::NewFolder("/opt/n".into()).argv())[0].ends_with("/mkdir"));
    assert!(denied(&std::io::Error::from_raw_os_error(libc::EACCES)) && denied(&std::io::Error::from_raw_os_error(libc::EPERM)));
    assert!(!denied(&std::io::Error::from_raw_os_error(libc::EROFS)), "a read-only disk isn't a permission problem");
}

#[test]
fn hand_written_png_is_valid_and_compact() {
    // A photo-like 256 x 192 image: smooth gradients with some noise.
    let (w, h) = (256u32, 192u32);
    let mut x: u32 = 7;
    let px: Vec<u8> = (0..w * h).flat_map(|i| {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        let (cx, cy) = (i % w, i / w);
        [(cx / 2) as u8, (cy + (x >> 29)) as u8, ((cx + cy) / 3) as u8, 255]
    }).collect();
    let img = crate::imgdec::Rgba { w, h, px };
    let ours = crate::thumbcache::encode_png(&img, &[("Thumb::URI", "file:///x/a%20b.jpg".into()), ("Software", "Maia".into())]).unwrap();
    let (back, text) = crate::imgdec::png_with_text(&ours).expect("decodes");
    assert_eq!((back.w, back.h), (w, h));
    assert_eq!(back.px, img.px, "lossless");
    assert!(text.iter().any(|(k, v)| k == "Thumb::URI" && v == "file:///x/a%20b.jpg"));
    // Within reach of png's own default encoder (which is what other apps write).
    let mut theirs = vec![];
    {
        let mut e = png::Encoder::new(&mut theirs, w, h);
        e.set_color(png::ColorType::Rgba);
        e.write_header().unwrap().write_image_data(&img.px).unwrap();
    }
    assert!(ours.len() < theirs.len() * 2, "ours {} vs png {}", ours.len(), theirs.len());
    eprintln!("png sizes: ours {} png crate {}", ours.len(), theirs.len());
    if let Some(d) = std::env::var_os("MAIA_ICON_DUMP") {
        std::fs::write(std::path::Path::new(&d).join("hand.png"), &ours).unwrap();
    }
}

/// Rows of `line` wrapped at `cols`, as text.
fn wrapped(line: &str, cols: usize) -> Vec<String> {
    let mut starts = vec![];
    crate::inspector::wrap_line(line, cols, &mut starts);
    starts.iter().enumerate().map(|(k, &a)| line[a as usize..starts.get(k + 1).map_or(line.len(), |&b| b as usize)].to_string()).collect()
}

#[test]
fn word_wrap_keeps_every_character_and_breaks_at_spaces() {
    let width = |s: &str| s.chars().map(crate::inspector::char_cols).sum::<usize>();
    // Real text: the licence and this crate's own sources, at narrow to wide columns.
    let text = [include_str!("../LICENSE"), include_str!("inspector.rs")].concat();
    let mut wrapped_some = 0;
    for cols in [8, 13, 40, 79] {
        for line in text.lines() {
            let rows = wrapped(line, cols);
            assert_eq!(rows.concat(), line, "nothing lost or doubled at {cols}");
            for (k, r) in rows.iter().enumerate() {
                assert!(width(r) <= cols, "{r:?} wider than {cols}");
                // A row ends at a space unless its next word would not fit on any row.
                if let Some(next) = rows.get(k + 1) {
                    let word = next.split(' ').next().unwrap_or("");
                    assert!(r.ends_with(' ') || !r.contains(' ') || width(word) + width(r.rsplit(' ').next().unwrap()) > cols, "{r:?} | {next:?} broke mid-word at {cols}");
                }
            }
            if rows.len() > 1 {
                wrapped_some += 1;
            }
            if width(line) <= cols {
                assert_eq!(rows.len(), 1, "{line:?} fits in {cols}");
            }
        }
    }
    assert!(wrapped_some > 1000, "long lines wrapped: {wrapped_some}");
    // A word longer than a row breaks inside it; wide characters count twice.
    assert_eq!(wrapped("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    assert_eq!(wrapped("the quick brown fox", 10), ["the quick ", "brown fox"]);
    assert_eq!(wrapped("日本語のテキスト", 6), ["日本語", "のテキ", "スト"]);
    assert_eq!(wrapped("", 10), [""]);
}

#[test]
fn checksums_match_known_values_and_pasted_formats() {
    use crate::checksum::{Algo, Job, clean};
    let d = tmpdir("checksums");
    // A name with a newline makes the tools escape their output line.
    let odd = d.join("we\nird name.txt");
    touch(&odd, "abc");
    let ctx = egui::Context::default();
    let want = [
        (Algo::Md5, "900150983cd24fb0d6963f7d28e17f72"),
        (Algo::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (Algo::Sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (Algo::Sha512, "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
    ];
    for (algo, hex) in want {
        assert_eq!(Algo::for_hex(hex), Some(algo));
        let job = Job::start(&odd, algo, &ctx);
        let t0 = Instant::now();
        let got = loop {
            if let Some(r) = job.poll() {
                break r;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "{algo:?} never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(got.as_deref(), Ok(hex), "{algo:?}");
    }
    // What download pages and tools print comes down to the digits.
    let sha = want[2].1;
    assert_eq!(clean(&format!("  {}  ", sha.to_uppercase())), sha);
    assert_eq!(clean(&format!("{sha}  ubuntu.iso")), sha);
    assert_eq!(clean(&format!("SHA256 (ubuntu.iso) = {sha}")), sha);
    assert_eq!(clean("ba7816bf 8f01cfea 414140de 5dae2223 b00361a3 96177a9c b410ff61 f20015ad"), sha);
    assert_eq!(Algo::for_hex(&clean("not a checksum")), None);
    assert_eq!(Algo::for_hex("xyz7816bf8f01cfea414140de5dae2223"), None);
    // A file that is gone says so.
    let job = Job::start(&d.join("gone"), Algo::Sha256, &ctx);
    let t0 = Instant::now();
    let r = loop {
        if let Some(r) = job.poll() {
            break r;
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(r, Err("No such file or directory".into()));
}

#[test]
fn filter_conditions_by_size_and_date() {
    use std::time::SystemTime;
    let d = tmpdir("filterq");
    let now = SystemTime::now();
    let day = Duration::from_secs(86_400);
    // name, bytes, age
    let files = [("tiny.txt", 10, 0), ("page.html", 3000, 2), ("photo.jpg", 2 << 20, 10), ("movie.mkv", 150 << 20, 400), ("empty.log", 0, 40)];
    for (name, n, age) in files {
        let f = std::fs::File::create(d.join(name)).unwrap();
        f.set_len(n).unwrap();
        f.set_modified(now - day * age).unwrap();
    }
    std::fs::create_dir(d.join("Big folder")).unwrap();
    let mut stats = HashMap::new();
    let mut t = Tab::new(77, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let mut shown = |t: &mut Tab, stats: &HashMap<PathBuf, crate::stats::DirStats>, f: &str| -> Vec<String> {
        t.filter = f.into();
        pump(t, stats);
        let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(shown(&mut t, &stats, "size>1M"), ["movie.mkv", "photo.jpg"]);
    assert_eq!(shown(&mut t, &stats, "size<2k"), ["empty.log", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "size<3k"), ["empty.log", "page.html", "tiny.txt"], "k is 1024");
    assert_eq!(shown(&mut t, &stats, "size<=3000"), ["empty.log", "page.html", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "size:0"), ["empty.log"]);
    assert_eq!(shown(&mut t, &stats, "SIZE>1.5mb size<100MiB"), ["photo.jpg"]);
    assert_eq!(shown(&mut t, &stats, "modified<7d"), ["Big folder", "page.html", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "modified>1y"), ["movie.mkv"]);
    assert_eq!(shown(&mut t, &stats, "date:today"), ["Big folder", "tiny.txt"]);
    assert_eq!(shown(&mut t, &stats, "mod>=30d size>1k"), ["movie.mkv"]);
    // Conditions and a name search together; a folder's size counts once it is known.
    assert_eq!(shown(&mut t, &stats, "o size>1k"), ["movie.mkv", "photo.jpg"]);
    stats.insert(d.join("Big folder"), crate::stats::DirStats { size: 5 << 30, ..Default::default() });
    assert_eq!(shown(&mut t, &stats, "size>1g"), ["Big folder"]);
    // Words that only look like conditions stay part of the name search.
    assert_eq!(shown(&mut t, &stats, "size>huge"), Vec::<String>::new());
    assert_eq!(crate::query::name_part("size>huge"), "size>huge");
    assert_eq!(crate::query::name_part("report modified<2w"), "report");
    assert_eq!(crate::query::name_part("a:b"), "a:b");
    // Dates are local days, months and years.
    let tm = crate::timefmt::local(now);
    let (y, m, dd) = (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
    assert_eq!(shown(&mut t, &stats, &format!("modified:{y:04}-{m:02}-{dd:02}")), ["Big folder", "tiny.txt"]);
    let before_year = shown(&mut t, &stats, &format!("modified<{y:04}"));
    assert!(before_year.contains(&"movie.mkv".to_string()) && !before_year.contains(&"tiny.txt".to_string()), "{before_year:?}");
    let this_month = shown(&mut t, &stats, &format!("modified>={y:04}-{m:02}"));
    assert!(this_month.contains(&"tiny.txt".to_string()) && !this_month.contains(&"movie.mkv".to_string()), "{this_month:?}");
    // Spans mark exactly the recognised words.
    let f = crate::query::parse("big size>10M  modified<7d x", now);
    assert_eq!(f.spans, vec![(4, 12), (14, 25)]);
    assert_eq!(f.name, "big x");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn chosen_view_stays_while_browsing_and_saved_views_still_apply() {
    use crate::config::{SaveScope, SavedView, UserData};
    let d = tmpdir("viewstays");
    for f in ["a/inner", "b", "pics/trip"] {
        std::fs::create_dir_all(d.join(f)).unwrap();
    }
    let mut ud = UserData::default();
    let mut pics = ViewSettings::default();
    pics.zoom = 0.9;
    ud.views.insert(d.join("pics"), SavedView { scope: SaveScope::Subfolders, view: pics });
    let stats = HashMap::new();
    let mut t = Tab::new(5, d.clone(), ud.view_for(&d).0, egui::Context::default());
    pump(&mut t, &stats);
    let go = |t: &mut Tab, p: PathBuf| t.navigate(p.clone(), crate::app::App::nav_view(&ud, &p));
    // Medium icons, say: kept into a folder, its child, and back up.
    t.settings.zoom = 0.4;
    go(&mut t, d.join("a"));
    assert_eq!(t.settings.zoom, 0.4, "the chosen view follows into another folder");
    go(&mut t, d.join("a/inner"));
    assert_eq!(t.settings.zoom, 0.4);
    t.up(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!(t.settings.zoom, 0.4, "and back up");
    // A folder with a saved view (and its subfolders) shows that one...
    go(&mut t, d.join("pics"));
    assert_eq!(t.settings.zoom, 0.9);
    go(&mut t, d.join("pics/trip"));
    assert_eq!(t.settings.zoom, 0.9, "inherited by subfolders");
    // ...and leaving it puts back the view chosen before, with Back and Forward too.
    go(&mut t, d.join("b"));
    assert_eq!(t.settings.zoom, 0.4, "the free view returns");
    t.back(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!((t.path.clone(), t.settings.zoom), (d.join("pics/trip"), 0.9));
    t.forward(|p| crate::app::App::nav_view(&ud, p));
    assert_eq!((t.path.clone(), t.settings.zoom), (d.join("b"), 0.4));
    // Changing the view again carries on from here.
    t.settings.zoom = 0.0;
    go(&mut t, d.join("a"));
    assert_eq!(t.settings.zoom, 0.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// Make encrypted archives of `src` with the real tools: (ZipCrypto zip, AES zip,
/// 7z, 7z with its names encrypted), or None when zip or 7z is missing.
fn encrypted_archives(d: &Path, src: &Path, pw: &str) -> Option<[PathBuf; 4]> {
    let run = |prog: &str, args: &[&str]| std::process::Command::new(prog).args(args).current_dir(src).stdout(std::process::Stdio::null()).status().ok().filter(|s| s.success());
    let out = [d.join("crypto.zip"), d.join("aes.zip"), d.join("plain-names.7z"), d.join("hidden-names.7z")];
    let p = |i: usize| out[i].to_str().unwrap().to_string();
    run("zip", &["-q", "-r", "-P", pw, &p(0), "."])?;
    run("7z", &["a", &format!("-p{pw}"), "-tzip", "-mem=AES256", &p(1), "."])?;
    run("7z", &["a", &format!("-p{pw}"), &p(2), "."])?;
    run("7z", &["a", &format!("-p{pw}"), "-mhe=on", &p(3), "."])?;
    Some(out)
}

#[test]
fn password_protected_archives_list_extract_and_refuse() {
    use crate::archive::{self, NEEDS_PASSWORD, WRONG_PASSWORD};
    let d = tmpdir("encrypted");
    let src = d.join("src");
    std::fs::create_dir_all(src.join("Trip photos")).unwrap();
    touch(&src.join("notes.txt"), "the secret plan");
    touch(&src.join("Trip photos/beach.txt"), "sand");
    let Some(all) = encrypted_archives(&d, &src, "hunter2") else {
        eprintln!("zip or 7z missing: skipped");
        return;
    };
    for a in &all {
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        // Zips and plain-named 7z list without the password; hidden names need it.
        let listed = archive::list(a);
        if name.starts_with("hidden") {
            assert_eq!(listed.err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        } else {
            let items = listed.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(items.iter().any(|i| i.path == Path::new("Trip photos/beach.txt")), "{name}");
        }
        // Without it: refused, and nothing half-written is left to open later.
        let x = d.join(format!("x-{name}"));
        assert_eq!(archive::extract(a, &[PathBuf::from("notes.txt")], &x).err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        assert_eq!(archive::extract_for_open(a, Path::new("notes.txt")).err().as_deref(), Some(NEEDS_PASSWORD), "{name}");
        assert!(!archive::open_cache(a).join("notes.txt").exists(), "{name}: nothing cached");
        archive::set_password(a, "wrong".into());
        let r = archive::extract_for_open(a, Path::new("notes.txt"));
        assert_eq!(r.err().as_deref(), Some(WRONG_PASSWORD), "{name}");
        assert!(!archive::open_cache(a).join("notes.txt").exists(), "{name}: no zero-filled file cached");
        // With it: listed and extracted whole, folders with their contents.
        archive::set_password(a, "hunter2".into());
        assert!(archive::list(a).unwrap().iter().any(|i| i.path == Path::new("Trip photos") && i.is_dir), "{name}");
        let f = archive::extract_for_open(a, Path::new("notes.txt")).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(std::fs::read_to_string(f).unwrap(), "the secret plan", "{name}");
        let y = d.join(format!("y-{name}"));
        archive::extract(a, &[PathBuf::from("Trip photos")], &y).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(std::fs::read_to_string(y.join("Trip photos/beach.txt")).unwrap(), "sand", "{name}");
    }
    // 7z's own listing format.
    let items = archive::parse_slt("Path = sub\nSize = 0\nModified = 2026-10-15 19:19:57.7977160\nAttributes = D drwxr-xr-x\n\nPath = sub/b.txt\nSize = 5\nModified = 2026-10-15 19:19:57.7977160\nAttributes = A -rw-r--r--\n\nPath = ../evil\nSize = 1\n\nPath = deep/x.txt\nSize = 2\n");
    let get = |p: &str| items.iter().find(|i| i.path == Path::new(p)).cloned();
    assert!(get("sub").is_some_and(|i| i.is_dir) && get("sub/b.txt").is_some_and(|i| i.size == 5 && i.modified.is_some()));
    assert!(get("deep").is_some_and(|i| i.is_dir), "implied folder");
    assert!(get("../evil").is_none(), "no climbing out");
    for c in &all {
        let _ = std::fs::remove_dir_all(archive::open_cache(c));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn tags_live_on_the_files_and_filter_the_list() {
    use crate::tags;
    let d = tmpdir("tags");
    let (a, b, c) = (d.join("a.txt"), d.join("b.txt"), d.join("c.txt"));
    for p in [&a, &b, &c] {
        touch(p, "x");
    }
    // Stored as KDE's file managers store them: one comma-separated attribute.
    if tags::write(&a, &["Red".into(), "work".into()]).is_err() {
        eprintln!("no user xattrs here: skipped");
        return;
    }
    let raw = |p: &Path| {
        let mut buf = [0u8; 256];
        let cp = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        let n = unsafe { libc::getxattr(cp.as_ptr(), c"user.xdg.tags".as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    };
    assert_eq!(raw(&a).as_deref(), Some("Red,work"));
    assert_eq!(tags::read(&a), ["Red", "work"]);
    assert_eq!(tags::parse(" Blue, ,taxes,Blue,"), ["Blue", "taxes"]);
    // One tag added to several files, then removed from all; the last one gone takes the
    // attribute with it.
    tags::toggle(&[a.clone(), b.clone()], "Green", true).unwrap();
    assert_eq!(tags::read(&a), ["Red", "work", "Green"]);
    assert_eq!(tags::read(&b), ["Green"]);
    tags::toggle(&[a.clone(), b.clone()], "green", false).unwrap();
    assert_eq!(tags::read(&b), Vec::<String>::new());
    assert_eq!(raw(&b), None, "no empty attribute left behind");
    // Tags written by another app are read too.
    let cp = std::ffi::CString::new(c.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::setxattr(cp.as_ptr(), c"user.xdg.tags".as_ptr(), b"Blue,taxes".as_ptr().cast(), 10, 0) }, 0);
    // The list filters by tag once the reader has them.
    let stats = HashMap::new();
    let mut t = Tab::new(91, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let ctx = egui::Context::default();
    let shown = |t: &mut Tab, f: &str| -> Vec<String> {
        t.filter = f.into();
        let start = Instant::now();
        loop {
            pump(t, &stats);
            tags::flush(&ctx);
            let pending = t.entries.iter().any(|e| tags::get(&e.path, |_| ()).is_none());
            if !pending || start.elapsed() > Duration::from_secs(5) {
                pump(t, &stats);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
        v.sort();
        v
    };
    assert_eq!(shown(&mut t, "tag:red"), ["a.txt"]);
    assert_eq!(shown(&mut t, "tag:TAXES"), ["c.txt"]);
    assert_eq!(shown(&mut t, "tag:red tag:work"), ["a.txt"]);
    assert_eq!(shown(&mut t, "tag:red tag:taxes"), Vec::<String>::new());
    assert_eq!(crate::query::name_part("report tag:work"), "report");
    // A change made elsewhere is picked up once the watcher says the file changed.
    tags::write(&a, &[]).unwrap();
    tags::invalidate(&c);
    assert_eq!(shown(&mut t, "tag:red"), Vec::<String>::new());
    assert!(tags::known().contains(&"taxes".to_string()));
    let _ = std::fs::remove_dir_all(&d);
}

/// A history for the graph, written in one `git fast-import`: `n` commits on main, a
/// side branch merged back every 25 commits, a tag every 100, a rename, and at the top
/// ten branches joined by one octopus merge (more lanes than the graph draws).
pub(crate) fn git_history_fixture(d: &Path, n: usize) {
    use std::io::Write;
    std::fs::create_dir_all(d).unwrap();
    git(d, &["init", "-q"]);
    let mut s = String::new();
    let mut mark = 0;
    let mut commit = |s: &mut String, branch: &str, msg: &str, from: Option<usize>, merges: &[usize], files: &[(&str, String)]| -> usize {
        mark += 1;
        let t = 1_700_000_000 + mark as i64 * 600;
        let who = ["Ada Lovelace", "Grace Hopper", "Linus"][mark % 3];
        s.push_str(&format!("commit refs/heads/{branch}\nmark :{mark}\nauthor {who} <dev@example.invalid> {t} +0000\ncommitter {who} <dev@example.invalid> {t} +0000\ndata {}\n{msg}\n", msg.len()));
        if let Some(f) = from {
            s.push_str(&format!("from :{f}\n"));
        }
        for m in merges {
            s.push_str(&format!("merge :{m}\n"));
        }
        for (path, body) in files {
            if let Some(old) = path.strip_prefix("R ") {
                s.push_str(&format!("R {old}\n"));
            } else {
                s.push_str(&format!("M 644 inline {path}\ndata {}\n{body}\n", body.len()));
            }
        }
        mark
    };
    let mut main = commit(&mut s, "main", "Start the notes", None, &[], &[("notes.txt", "notes\n".into()), ("old name.txt", "x\n".into())]);
    for k in 2..=n {
        if k % 25 == 0 {
            let topic = format!("topic-{k}");
            let mut t = main;
            for j in 1..=3 {
                t = commit(&mut s, &topic, &format!("Work on topic {k}, part {j}"), Some(t), &[], &[(&format!("src/topic_{k}.rs"), format!("fn part_{j}() {{}}\n"))]);
            }
            main = commit(&mut s, "main", &format!("Note {k} while topic {k} is open"), Some(main), &[], &[("notes.txt", format!("note {k}\n"))]);
            // fast-import takes a merge's tree from its first parent: bring the topic's file in.
            main = commit(&mut s, "main", &format!("Merge topic {k}"), Some(main), &[t], &[(&format!("src/topic_{k}.rs"), "fn part_3() {}\n".into())]);
        } else if k == 40 {
            main = commit(&mut s, "main", "Rename the old file", Some(main), &[], &[("R \"old name.txt\" \"new name.txt\"", String::new())]);
        } else {
            main = commit(&mut s, "main", &format!("Note {k}"), Some(main), &[], &[("notes.txt", format!("notes\nnote {k}\n"))]);
        }
        if k % 100 == 0 {
            s.push_str(&format!("reset refs/tags/v{}\nfrom :{main}\n", k / 100));
        }
    }
    let wide: Vec<usize> = (1..=10).map(|w| commit(&mut s, &format!("wide-{w}"), &format!("Wide branch {w}"), Some(main), &[], &[(&format!("wide/{w}.txt"), format!("{w}\n"))])).collect();
    let all: Vec<(String, String)> = (1..=10).map(|w| (format!("wide/{w}.txt"), format!("{w}\n"))).collect();
    commit(&mut s, "main", "Join ten branches at once", Some(main), &wide, &all.iter().map(|(p, b)| (p.as_str(), b.clone())).collect::<Vec<_>>());
    let mut c = std::process::Command::new("git").args(["fast-import", "--quiet"]).current_dir(d).stdin(std::process::Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    assert!(c.wait().unwrap().success(), "fast-import");
    git(d, &["reset", "-q", "--hard"]);
}

/// A PNG of `w`×`h` with a pattern made from `seed`.
pub(crate) fn png_bytes(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut out = vec![];
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let px: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| [(x * 9 + seed * 70) as u8, (y * 7 + seed * 30) as u8, (120 + seed * 50) as u8, 255])).collect();
    e.write_header().unwrap().write_image_data(&px).unwrap();
    out
}

/// A small project for the commit view, five commits by three authors. "Lay out the
/// panels" changes twelve files in several folders in every way git records: added,
/// modified, deleted, renamed with an edit, a picture changed and one added, a binary
/// file, a file that became a link. Then a long line, a deleted picture and one more
/// edit. main is in step with origin/main, v0.1 and v1.0 are tags, and origin/feature
/// is a branch only the remote has.
pub(crate) fn commit_view_fixture(d: &Path) {
    std::fs::create_dir_all(d).unwrap();
    let mut day = 0;
    let mut commit = |who: &str, msg: &str| {
        day += 1;
        let (name, email) = who.split_once(" <").unwrap();
        let date = format!("2026-09-{day:02}T10:00:00+00:00");
        let ok = std::process::Command::new("git")
            .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={}", email.trim_end_matches('>')), "-c", "commit.gpgsign=false", "add", "-A"])
            .current_dir(d)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git add");
        let ok = std::process::Command::new("git")
            .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={}", email.trim_end_matches('>')), "-c", "commit.gpgsign=false", "commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(d)
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git commit {msg}");
    };
    let lines = |n: usize, word: &str| (1..=n).map(|i| format!("{word} line {i}\n")).collect::<String>();
    git(d, &["init", "-q"]);
    touch(&d.join("README.md"), "# Project\n\nA small project.\n");
    touch(&d.join("src/main.rs"), &format!("fn main() {{\n{}}}\n", lines(8, "    // main")));
    touch(&d.join("src/ui/view.rs"), &lines(6, "// view"));
    touch(&d.join("docs/guide.md"), &lines(12, "Guide"));
    touch(&d.join("old.txt"), &lines(3, "old"));
    touch(&d.join("notes.md"), "# Notes\n");
    touch(&d.join("tool.sh"), "#!/bin/sh\necho tool\n");
    std::fs::create_dir_all(d.join("assets/icons")).unwrap();
    std::fs::write(d.join("assets/logo.png"), png_bytes(48, 32, 1)).unwrap();
    std::fs::write(d.join("data.bin"), [0u8, 1, 2, 3, 0, 9, 8, 7]).unwrap();
    commit("Ada Lovelace <ada@example.invalid>", "Start the project");
    git(d, &["tag", "v0.1"]);
    touch(&d.join("src/main.rs"), &format!("mod ui;\n\nfn main() {{\n{}    ui::run();\n    ui::stop();\n}}\n", lines(7, "    // main")));
    touch(&d.join("src/ui/panel.rs"), &lines(12, "// panel"));
    touch(&d.join("src/ui/view.rs"), &format!("{}// view two\n// view three\n", lines(4, "// view")));
    std::fs::remove_file(d.join("old.txt")).unwrap();
    std::fs::remove_file(d.join("docs/guide.md")).unwrap();
    touch(&d.join("docs/handbook.md"), &format!("{}The handbook ends here.\n", lines(12, "Guide")));
    std::fs::write(d.join("assets/logo.png"), png_bytes(64, 40, 2)).unwrap();
    std::fs::write(d.join("assets/icons/icon.png"), png_bytes(24, 24, 3)).unwrap();
    std::fs::write(d.join("data.bin"), [0u8, 1, 2, 3, 0, 9, 8, 6, 5]).unwrap();
    std::fs::remove_file(d.join("tool.sh")).unwrap();
    std::os::unix::fs::symlink("src/main.rs", d.join("tool.sh")).unwrap();
    touch(&d.join("tests/layout.rs"), &lines(5, "// layout test"));
    touch(&d.join("tests/panel.rs"), &lines(4, "// panel test"));
    touch(&d.join("README.md"), "# Project\n\nA small project with panels.\n");
    commit("Grace Hopper <grace@example.invalid>", "Lay out the panels\n\nSplit the view into panels. The old layout code goes,\nand the guide is now the handbook.");
    git(d, &["tag", "v1.0"]);
    let long = "A long line in the notes keeps going well past the right edge of the panel, and wrapped it stays readable. ".repeat(3);
    touch(&d.join("notes.md"), &format!("# Notes\n{long}\n"));
    commit("Linus <linus@example.invalid>", "Write the long notes");
    git(d, &["update-ref", "refs/remotes/origin/feature", "HEAD"]);
    std::fs::remove_file(d.join("assets/logo.png")).unwrap();
    commit("Ada Lovelace <ada@example.invalid>", "Remove the logo");
    touch(&d.join("src/ui/panel.rs"), &format!("{}// tuned\n", lines(12, "// panel")));
    commit("Grace Hopper <grace@example.invalid>", "Tune the panel");
    git(d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
}

/// The history's commits as the graph reads them, all at once.
fn git_log_all(d: &Path) -> Vec<crate::gitlog::Commit> {
    let out = std::process::Command::new("git").arg("-C").arg(d).args(crate::gitlog::log_args()).output().unwrap();
    crate::gitlog::read_page(&mut &out.stdout[..], usize::MAX)
}

#[test]
fn git_history_lanes_edges_and_changes() {
    use crate::gitlog::{Lanes, RefKind, Row};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog");
    touch(&d.join("a"), "a\n");
    git(&d, &["init", "-q"]);
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "c1"]);
    touch(&d.join("a"), "a\nb\n");
    git(&d, &["commit", "-qam", "c2"]);
    git(&d, &["tag", "v1"]);
    git(&d, &["checkout", "-qb", "feat"]);
    touch(&d.join("f"), "f\n");
    git(&d, &["add", "f"]);
    git(&d, &["commit", "-qm", "f1"]);
    touch(&d.join("f"), "f\nf\n");
    git(&d, &["commit", "-qam", "f2"]);
    git(&d, &["checkout", "-q", "main"]);
    touch(&d.join("a"), "a\nb\nc\n");
    git(&d, &["commit", "-qam", "c3"]);
    git(&d, &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);
    git(&d, &["mv", "a", "b"]);
    git(&d, &["commit", "-qm", "ren"]);
    let log = git_log_all(&d);
    let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["ren", "merge feat", "f2", "f1", "c3", "c2", "c1"]);
    assert_eq!(log[0].refs, [(RefKind::Head, "main".to_string())]);
    assert_eq!(log[2].refs, [(RefKind::Branch, "feat".to_string())]);
    assert_eq!(log[5].refs, [(RefKind::Tag, "v1".to_string())]);
    assert_eq!((log[1].parents.len(), log[6].parents.len(), log[0].author.as_str()), (2, 0, "Test"));
    let mut lanes = Lanes::default();
    let rows: Vec<Row> = log.iter().map(|c| lanes.place(c)).collect();
    let r = |col, up, ins: &[u16], outs: &[u16], pass: &[u16]| Row { col, up, ins: ins.to_vec(), outs: outs.to_vec(), pass: pass.to_vec() };
    // The trunk stays in lane 0; the branch takes lane 1 from the merge down to where it
    // began, and curves back into the trunk there.
    assert_eq!(rows[0], r(0, false, &[], &[0], &[]));
    assert_eq!(rows[1], r(0, true, &[], &[0, 1], &[]));
    assert_eq!(rows[2], r(1, true, &[], &[1], &[0]));
    assert_eq!(rows[3], r(1, true, &[], &[1], &[0]));
    assert_eq!(rows[4], r(0, true, &[], &[0], &[1]));
    assert_eq!(rows[5], r(0, true, &[1], &[0], &[]));
    assert_eq!(rows[6], r(0, true, &[], &[], &[]));
    // What each commit changed, and a file's diff.
    let (body, ch) = crate::gitlog::changes(&d, &log[0].hash, &log[0].parents);
    assert_eq!((body.as_str(), ch.len(), ch[0].status, ch[0].path.as_str(), ch[0].from.as_deref()), ("", 1, b'R', "b", Some("a")));
    let (_, ch) = crate::gitlog::changes(&d, &log[1].hash, &log[1].parents);
    assert_eq!(ch.iter().map(|c| (c.status, c.path.as_str())).collect::<Vec<_>>(), [(b'A', "f")], "a merge shows what it brought in");
    let (_, ch) = crate::gitlog::changes(&d, &log[6].hash, &log[6].parents);
    assert_eq!((ch.len(), ch[0].status), (1, b'A'), "the root commit adds its files");
    let (_, ch) = crate::gitlog::changes(&d, &log[4].hash, &log[4].parents);
    let (lines, _, cut) = crate::gitlog::diff(&d, &log[4].hash, &log[4].parents, &ch[0]);
    assert_eq!(lines, ["@@ -1,2 +1,3 @@", " a", " b", "+c"]);
    assert!(!cut);
    assert_eq!(crate::gitlog::ago(1000, 1000 + 3 * 86_400 + 5), "3 days ago");
    assert_eq!(crate::gitlog::ago(1000, 1000 + 3600), "1 hour ago");
    assert_eq!(crate::gitlog::ago(1000, 1010), "just now");
    // Not a repository, or one without commits: nothing to read.
    let empty = tmpdir("gitlog-empty");
    git(&empty, &["init", "-q"]);
    assert!(git_log_all(&empty).is_empty());
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn git_history_pages_and_wide_merges() {
    use crate::gitlog::{Lanes, PAGE, read_page};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog-big");
    git_history_fixture(&d, 700);
    let all = git_log_all(&d);
    assert!(all.len() > 2 * PAGE + 50, "{} commits", all.len());
    // Read page by page from one running git, lanes carried across: the same graph as
    // all at once.
    let mut child = std::process::Command::new("git").arg("-C").arg(&d).args(crate::gitlog::log_args()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut r = std::io::BufReader::new(child.stdout.take().unwrap());
    let (mut paged, mut one) = (Lanes::default(), Lanes::default());
    let mut n = 0;
    loop {
        let page = read_page(&mut r, PAGE);
        for c in &page {
            assert_eq!(paged.place(c), one.place(&all[n]), "row {n}");
            assert_eq!(c.hash, all[n].hash);
            n += 1;
        }
        if page.len() < PAGE {
            break;
        }
    }
    let _ = child.wait();
    assert_eq!(n, all.len());
    // The octopus at the top fans out into eleven lanes; the trunk keeps lane 0 and the
    // lanes are free again further down.
    let mut lanes = Lanes::default();
    let rows: Vec<_> = all.iter().map(|c| lanes.place(c)).collect();
    assert_eq!(all[0].subject, "Join ten branches at once");
    assert_eq!(rows[0].outs.len(), 11);
    assert!(rows[0].outs.iter().any(|&l| l as usize >= crate::gitlog::LANES), "more lanes than drawn");
    let trunk: Vec<usize> = all.iter().enumerate().filter(|(_, c)| c.subject.starts_with("Note ")).map(|(i, _)| i).collect();
    assert!(trunk.iter().all(|&i| rows[i].col == 0), "main stays in the first lane");
    let topic = all.iter().position(|c| c.subject == "Work on topic 50, part 3").unwrap();
    assert_eq!(rows[topic].col, 1);
    assert!(rows.iter().skip(20).all(|r| r.pass.len() <= 2), "lanes are given back");
    let tags: Vec<&str> = all.iter().flat_map(|c| c.refs.iter().filter(|r| r.0 == crate::gitlog::RefKind::Tag).map(|r| r.1.as_str())).collect();
    assert_eq!(tags, ["v7", "v6", "v5", "v4", "v3", "v2", "v1"]);
    let ren = all.iter().find(|c| c.subject == "Rename the old file").unwrap();
    let (_, ch) = crate::gitlog::changes(&d, &ren.hash, &ren.parents);
    assert_eq!((ch[0].status, ch[0].path.as_str(), ch[0].from.as_deref()), (b'R', "new name.txt", Some("old name.txt")));
    let _ = std::fs::remove_dir_all(&d);
}

/// Line counts as `git diff-tree -z --numstat` writes them (binary files, a rename with
/// its two paths) joined to the name-status list, then against real git.
#[test]
fn git_numstat_and_renames_in_a_commit() {
    use crate::gitlog::{Change, join_numstat, parse_changes, parse_numstat};
    let num = b"-\t-\tb.bin\x001\t0\t\x00a.txt\x00d/a2.txt\x000\t1\td/x\x001\t0\tnew\x00";
    let counts = parse_numstat(num);
    assert_eq!(counts, [(None, "b.bin".to_string()), (Some((1, 0)), "d/a2.txt".into()), (Some((0, 1)), "d/x".into()), (Some((1, 0)), "new".into())]);
    let mut files = parse_changes(b"M\x00b.bin\x00R075\x00a.txt\x00d/a2.txt\x00D\x00d/x\x00A\x00new\x00");
    join_numstat(&mut files, &counts);
    let ch = |status: u8, path: &str, from: Option<&str>, lines: Option<(u32, u32)>| Change { status, path: path.into(), from: from.map(String::from), lines };
    assert_eq!(files, [ch(b'M', "b.bin", None, None), ch(b'R', "d/a2.txt", Some("a.txt"), Some((1, 0))), ch(b'D', "d/x", None, Some((0, 1))), ch(b'A', "new", None, Some((1, 0)))]);
    // A count list cut short or out of order is matched by path.
    let mut files = parse_changes(b"A\x00new\x00D\x00d/x\x00");
    join_numstat(&mut files, &counts[2..]);
    assert_eq!((files[0].lines, files[1].lines), (Some((1, 0)), Some((0, 1))));
    if !crate::media::have("git") {
        eprintln!("git not installed: the rest skipped");
        return;
    }
    let d = tmpdir("commitview");
    commit_view_fixture(&d);
    let log = git_log_all(&d);
    let panels = log.iter().find(|c| c.subject == "Lay out the panels").unwrap();
    let (body, files) = crate::gitlog::changes(&d, &panels.hash, &panels.parents);
    assert!(body.starts_with("Split the view into panels."), "{body:?}");
    let get = |p: &str| files.iter().find(|f| f.path == p).unwrap_or_else(|| panic!("{p} in {files:?}"));
    assert_eq!(files.len(), 12);
    assert_eq!((get("src/main.rs").status, get("src/main.rs").lines), (b'M', Some((4, 1))));
    assert_eq!((get("src/ui/panel.rs").status, get("src/ui/panel.rs").lines), (b'A', Some((12, 0))));
    assert_eq!((get("old.txt").status, get("old.txt").lines), (b'D', Some((0, 3))));
    assert_eq!((get("docs/handbook.md").status, get("docs/handbook.md").from.as_deref(), get("docs/handbook.md").lines), (b'R', Some("docs/guide.md"), Some((1, 0))));
    assert_eq!((get("assets/logo.png").status, get("assets/logo.png").lines), (b'M', None), "a picture is binary");
    assert_eq!(get("data.bin").lines, None);
    assert_eq!(get("tool.sh").status, b'T', "a file that became a link");
    assert_eq!(files.iter().filter_map(|f| f.lines).map(|l| l.0).sum::<u32>(), 30);
    // Both sides of the changed picture, only the new side of the added one; the file
    // as of the commit; one file's commits followed through its rename.
    let logo = get("assets/logo.png");
    let side = |old: bool, f: &Change| crate::gitlog::picture_side(&d, &panels.hash, &panels.parents, f, old, 512).map(|p| p.1);
    assert_eq!((side(true, logo), side(false, logo)), (Some([48, 32]), Some([64, 40])));
    assert_eq!(side(true, get("assets/icons/icon.png")), None);
    assert_eq!(side(false, get("assets/icons/icon.png")), Some([24, 24]));
    match crate::gitlog::blob(&d, &panels.hash, "src/main.rs") {
        crate::gitlog::Blob::Text(l, spans, cut) => assert!(l[0] == "mod ui;" && spans.is_some() && !cut),
        _ => panic!("main.rs as text"),
    }
    assert!(matches!(crate::gitlog::blob(&d, &panels.hash, "data.bin"), crate::gitlog::Blob::Nothing("Binary file")));
    assert!(matches!(crate::gitlog::blob(&d, &panels.hash, "assets/logo.png"), crate::gitlog::Blob::Picture(_, [64, 40])));
    let out = std::process::Command::new("git").arg("-C").arg(&d).args(crate::gitlog::file_log_args("docs/handbook.md")).output().unwrap();
    let follow: Vec<String> = crate::gitlog::read_page(&mut &out.stdout[..], 100).into_iter().map(|c| c.subject).collect();
    assert_eq!(follow, ["Lay out the panels", "Start the project"]);
    let _ = std::fs::remove_dir_all(&d);
}

/// A commit's header: author and email, and its refs from git's decorations (a remote's
/// HEAD dropped, a branch in step with its remote one pill, a remote-only branch with
/// its own, tags).
#[test]
fn git_header_refs_and_author() {
    use crate::gitlog::{RefKind, labels, parse};
    let rec = "\n0123456789abcdef0123456789abcdef01234567\x1faaaaaaa bbbbbbb\x1fAda Lovelace\x1fada@example.invalid\x1f1700000000\x1fHEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1.0, refs/remotes/origin/feature, refs/stash\x1fLay out the panels";
    let c = parse(rec.as_bytes()).expect("a record");
    assert_eq!((c.author.as_str(), c.email.as_str(), c.time, c.parents.len(), c.subject.as_str()), ("Ada Lovelace", "ada@example.invalid", 1_700_000_000, 2, "Lay out the panels"));
    assert_eq!(c.refs, [(RefKind::Head, "main".to_string()), (RefKind::Remote, "origin/main".into()), (RefKind::Tag, "v1.0".into()), (RefKind::Remote, "origin/feature".into())]);
    assert_eq!(labels(&c.refs), [(RefKind::Head, "main", Some("origin")), (RefKind::Tag, "v1.0", None), (RefKind::Remote, "origin/feature", None)]);
    let detached = parse(b"0123456789abcdef0123456789abcdef01234567\x1f\x1fX\x1f\x1f0\x1fHEAD\x1froot").unwrap();
    assert_eq!((detached.refs.clone(), detached.parents.len()), (vec![(RefKind::Head, "HEAD".to_string())], 0));
    assert!(parse(b"short\x1f\x1fX\x1f\x1f0\x1f\x1fs").is_none(), "not a hash");
}

/// An author keeps one colour: from the email, whatever its case or spacing, else from
/// the name; the hash is fixed, so it stays across sessions. Initials read the first
/// and last words.
#[test]
fn git_author_colour_is_stable() {
    use crate::gitlog::{author_color, author_hue, initials};
    let a = author_hue("ada@example.invalid", "Ada Lovelace");
    assert_eq!(a, author_hue(" ADA@Example.invalid ", "someone else"));
    assert_eq!(author_hue("", "Linus"), author_hue("", "linus"), "no email: the name");
    // Pinned: a change to the hash would recolour every author.
    assert_eq!((a, author_hue("grace@example.invalid", ""), author_hue("linus@example.invalid", "")), (8, 0, 9));
    let hues: HashSet<usize> = (0..40).map(|i| author_hue(&format!("dev{i}@example.invalid"), "")).collect();
    assert!(hues.len() >= 6, "authors spread over the colours: {hues:?}");
    let (dark, light) = (crate::theme::palette(crate::theme::ThemeName::Midnight), crate::theme::palette(crate::theme::ThemeName::Paper));
    assert_eq!(author_color(&dark, "ada@example.invalid", ""), author_color(&dark, "ADA@example.invalid", ""));
    assert_ne!(author_color(&dark, "ada@example.invalid", ""), author_color(&light, "ada@example.invalid", ""), "toned to the theme");
    assert_eq!(initials("Ada Lovelace"), "AL");
    assert_eq!(initials("Grace Brewster Murray Hopper"), "GH");
    assert_eq!(initials("linus"), "L");
    assert_eq!(initials("first.last"), "FL");
    assert_eq!(initials("  "), "?");
    assert_eq!(initials("émile zola"), "ÉZ");
}

/// Many files grouped by folder: folders first, a folder holding only a folder as one
/// heading, each heading knowing how many rows and files it holds. Few files, or files
/// in one folder, stay a list. And the counts' five blocks and summary.
#[test]
fn git_files_grouped_by_folder() {
    use crate::gitlog::{Change, Node, blocks, group, grouped, summary};
    let f = |p: &str| Change { status: b'M', path: p.into(), from: None, lines: Some((1, 1)) };
    let files: Vec<Change> = ["README.md", "src/main.rs", "src/ui/view.rs", "src/ui/panel.rs", "docs/a/b/guide.md", "tests/x.rs", "tests/y.rs", "Cargo.toml", "src/lib.rs"].iter().map(|p| f(p)).collect();
    assert!(grouped(&files));
    assert!(!grouped(&files[..8]), "eight files stay a list");
    let one: Vec<Change> = (0..20).map(|i| f(&format!("src/f{i}.rs"))).collect();
    assert!(!grouped(&one), "all in one folder");
    let dir = |path: &str, name: &str, depth: u16, len: usize, files: usize| Node::Dir { path: path.into(), name: name.into(), depth, len, files };
    let file = |idx: usize, depth: u16| Node::File { idx, depth };
    assert_eq!(
        group(&files),
        [
            dir("docs/a/b", "docs/a/b", 0, 1, 1),
            file(4, 1),
            dir("src", "src", 0, 5, 4),
            dir("src/ui", "ui", 1, 2, 2),
            file(3, 2),
            file(2, 2),
            file(8, 1),
            file(1, 1),
            dir("tests", "tests", 0, 2, 2),
            file(5, 1),
            file(6, 1),
            file(7, 0),
            file(0, 0),
        ]
    );
    assert_eq!(blocks(0, 0), (0, 0));
    assert_eq!(blocks(3, 0), (3, 0), "fewer lines than blocks");
    assert_eq!(blocks(100, 0), (5, 0));
    assert_eq!(blocks(1, 99), (1, 4), "a side with lines keeps a block");
    assert_eq!(blocks(99, 1), (4, 1));
    assert_eq!(blocks(30, 30), (3, 2));
    assert_eq!((summary(1), summary(12)), ("1 file changed".to_string(), "12 files changed".to_string()));
    assert!(crate::gitlog::is_picture("assets/Logo.PNG") && crate::gitlog::is_picture("a.svg") && !crate::gitlog::is_picture("png") && !crate::gitlog::is_picture("x.png/notes"));
}

/// The history's git runs a page ahead of nothing: after the first page it waits on its
/// pipe until asked, and closing the history kills it.
#[test]
fn git_history_reads_on_demand_and_stops() {
    use crate::gitlog::{History, PAGE};
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("gitlog-feed");
    git_history_fixture(&d, 700);
    let ctx = egui::Context::default();
    let mut h = History::new(d.clone(), "main".into(), false);
    let wait = |h: &mut History, n: usize| {
        let t0 = Instant::now();
        while h.state().0 < n && t0.elapsed() < Duration::from_secs(10) {
            h.poll(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        h.state().0
    };
    assert_eq!(wait(&mut h, PAGE), PAGE);
    std::thread::sleep(Duration::from_millis(300));
    h.poll(&ctx);
    assert_eq!(h.state().0, PAGE, "nothing more read until asked");
    let pid = h.git_pid();
    assert!(pid != 0 && std::path::Path::new(&format!("/proc/{pid}")).exists(), "git waits on its pipe");
    h.more(&ctx);
    assert_eq!(wait(&mut h, 2 * PAGE), 2 * PAGE);
    drop(h);
    let t0 = Instant::now();
    let gone = || std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.split_whitespace().nth(2) == Some("Z"));
    while !gone() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gone(), "git is stopped with its history");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_repositorys_own_config_runs_nothing_when_maia_reads_it() {
    let d = tmpdir("evilrepo");
    let g = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&d).stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success()), "git {args:?}");
    g(&["init", "-q"]);
    touch(&d.join("notes.txt"), "one");
    g(&["add", "notes.txt"]);
    g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "first"]);
    // As a downloaded repository could arrive: an fsmonitor hook and filters (one named
    // with a space) that each leave a marker if run, and files whose stat no longer
    // matches the index, so status re-reads them through the filters.
    let mark = |n: &str| format!("touch '{}'", d.join(n).display());
    g(&["config", "core.fsmonitor", &mark("ran-fsmonitor")]);
    g(&["config", "filter.x.clean", &format!("sh -c \"{}; cat\"", mark("ran-clean"))]);
    g(&["config", "filter.my x.process", &mark("ran-process")]);
    touch(&d.join(".gitattributes"), "*.txt filter=x\n*.md filter=my\\ x\n");
    touch(&d.join("notes.txt"), "two");
    touch(&d.join("readme.md"), "new");
    let repo = crate::gitstatus::query(&d).expect("read the status");
    assert!(!repo.marks.is_empty(), "the status was still read");
    let (_, changed) = crate::gitlog::changes(&d, "HEAD", &[]);
    assert!(!changed.is_empty());
    for m in ["ran-fsmonitor", "ran-clean", "ran-process"] {
        assert!(!d.join(m).exists(), "{m}: the repository's config ran a command");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ahead_behind_and_branch_labels_from_real_remotes() {
    use crate::gitlog::{RefKind, labels};
    let d = tmpdir("remotes");
    let run = |at: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(at).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // A "server" and two clones of it: ours, and someone else's that pushes once.
    std::fs::create_dir_all(d.join("server.git")).unwrap();
    run(&d.join("server.git"), &["init", "-q", "--bare"]);
    run(&d, &["clone", "-q", "server.git", "ours"]);
    let ours = d.join("ours");
    touch(&ours.join("a.txt"), "a");
    run(&ours, &["add", "."]);
    run(&ours, &["commit", "-qm", "first"]);
    run(&ours, &["push", "-q", "-u", "origin", "main"]);
    run(&ours, &["branch", "docs"]);
    run(&ours, &["push", "-q", "origin", "docs"]);
    run(&d, &["clone", "-q", "server.git", "theirs"]);
    touch(&d.join("theirs/b.txt"), "b");
    run(&d.join("theirs"), &["add", "."]);
    run(&d.join("theirs"), &["commit", "-qm", "theirs"]);
    run(&d.join("theirs"), &["push", "-q"]);
    for n in ["c", "e"] {
        touch(&ours.join(n), n);
        run(&ours, &["add", "."]);
        run(&ours, &["commit", "-qm", n]);
    }
    run(&ours, &["fetch", "-q"]);
    let repo = crate::gitstatus::query(&ours).expect("status");
    assert_eq!((repo.branch.as_str(), repo.ahead, repo.behind), ("main", 2, 1), "two to push, one to pull");
    // Status lines git writes, parsed.
    let line = |s: &str| crate::gitstatus::parse(ours.clone(), format!("## {s}\0").as_bytes());
    assert_eq!((line("main...origin/main [behind 7]").ahead, line("main...origin/main [behind 7]").behind), (0, 7));
    assert_eq!((line("main...origin/main [gone]").ahead, line("main...origin/main [gone]").behind), (0, 0));
    assert_eq!((line("topic").branch.as_str(), line("topic").ahead), ("topic", 0), "no upstream");
    // The refs on each commit, from git's own decorations: docs sits with origin/docs (one
    // pill), main is ahead of origin/main (two pills, the remote one marked as such).
    let log = run(&ours, &["log", "-z", "--decorate=full", crate::gitlog::FORMAT, "--branches", "--remotes", "--tags", "HEAD", "--"]);
    let commits: Vec<crate::gitlog::Commit> = log.split('\0').filter_map(|r| crate::gitlog::parse(r.as_bytes())).collect();
    let on = |subject: &str| commits.iter().find(|c| c.subject == subject).map(|c| labels(&c.refs).into_iter().map(|(k, n, r)| (k, n.to_string(), r.map(String::from))).collect::<Vec<_>>()).unwrap();
    assert_eq!(on("e"), vec![(RefKind::Head, "main".into(), None)]);
    assert_eq!(on("first"), vec![(RefKind::Branch, "docs".into(), Some("origin".into()))]);
    assert_eq!(on("theirs"), vec![(RefKind::Remote, "origin/main".into(), None)]);
    let _ = std::fs::remove_dir_all(&d);
}

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let b: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    b.chunks(4).flat_map(|c| {
        let n = c.iter().enumerate().fold(0u32, |n, (k, &x)| n | (val(x) as u32) << (18 - 6 * k));
        (0..c.len() - 1).map(move |k| (n >> (16 - 8 * k)) as u8)
    }).collect()
}

/// What test pictures can show; a picture's red tells which (it survives the shrinking
/// a picture gets on its way to the model).
const CAPTIONS: &[&str] = &["a puppy playing in the sand", "a spreadsheet of numbers", "icon"];

/// The largest side of a picture the fake Ollama was sent.
static PICTURE_SIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the fake Ollama "sees" in a picture.
fn caption(png: &[u8]) -> String {
    let Some(img) = crate::imgdec::decode_bytes(png) else { return String::new() };
    PICTURE_SIDE.fetch_max(img.w.max(img.h), std::sync::atomic::Ordering::Relaxed);
    let red = img.px.first().copied().unwrap_or(0) as usize;
    CAPTIONS.get(red / 40).map_or(String::new(), |c| c.to_string())
}

/// A picture for the tests, 640 by 480 (a photo's shape) showing `caption`, one of CAPTIONS.
pub(crate) fn captioned_png(path: &Path, caption: &str) {
    sized_png(path, caption, 640, 480)
}

pub(crate) fn sized_png(path: &Path, caption: &str, w: u32, h: u32) {
    let red = CAPTIONS.iter().position(|c| *c == caption).expect("a known caption") as u8 * 40 + 20;
    let mut seed = 0x2545f491u32;
    let px = (0..w * h).flat_map(|_| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        [red, (seed >> 8) as u8, (seed >> 16) as u8, 255]
    }).collect();
    let img = crate::imgdec::Rgba { w, h, px };
    std::fs::write(path, crate::thumbcache::encode_png(&img, &[]).unwrap()).unwrap();
}

/// A stand-in for Ollama on a local port, speaking its HTTP API: /api/tags, a chunked
/// /api/pull with progress, and /api/embed. Its "model" puts words about one topic in
/// the same direction (money, holidays, cooking, pets, meetings), so a question finds a
/// file about the same thing without sharing its words. Returns its address, whether the
/// model is "downloaded", and how many texts it has embedded.
pub(crate) fn fake_ollama(has_model: bool) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let model = std::sync::Arc::new(AtomicBool::new(has_model));
    let embedded = std::sync::Arc::new(AtomicUsize::new(0));
    let (m, n) = (model.clone(), embedded.clone());
    const TOPICS: &[&[&str]] = &[
        &["invoice", "bill", "receipt", "payment", "tax", "taxes", "refund", "bank", "money", "government", "owe", "paid"],
        &["holiday", "vacation", "trip", "beach", "travel", "flight", "hotel", "sea", "sand", "summer"],
        &["recipe", "cooking", "pasta", "dinner", "bake", "kitchen", "soup", "eat", "food", "sauce"],
        &["dog", "cat", "pet", "puppy", "vet", "animal", "walk", "leash"],
        &["meeting", "agenda", "minutes", "schedule", "calendar", "standup", "monday"],
    ];
    let vector = |text: &str| -> Vec<f32> {
        let body = text.rsplit_once(" | query: ").or_else(|| text.rsplit_once(" | text: ")).map_or(text, |(_, b)| b);
        let mut v = vec![0.0f32; 768];
        for w in body.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase) {
            match TOPICS.iter().position(|t| t.contains(&w.as_str())) {
                Some(t) => v[t] += 3.0,
                None => {
                    let h = w.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
                    v[32 + (h as usize % 700)] += 0.4;
                }
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / norm).collect()
    };
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let (method, path) = { let mut p = line.split_whitespace(); (p.next().unwrap_or("").to_string(), p.next().unwrap_or("").to_string()) };
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let mut s = s;
            let reply = |s: &mut std::net::TcpStream, code: &str, json: String| {
                let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
            };
            match (method.as_str(), path.as_str()) {
                ("GET", "/api/tags") => {
                    let models = if m.load(Relaxed) { r#"[{"name":"hf.co/ggml-org/embeddinggemma-2-GGUF:Q8_0","size":864000000}]"# } else { "[]" };
                    reply(&mut s, "200 OK", format!(r#"{{"models":{models}}}"#));
                }
                ("POST", "/api/pull") => {
                    // Streamed as Ollama streams it: chunked, one JSON object per line.
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    let mut lines = vec![r#"{"status":"pulling manifest"}"#.to_string()];
                    lines.extend((1..=4).map(|k| format!(r#"{{"status":"pulling","digest":"sha256:ab","total":300000000,"completed":{}}}"#, k * 75_000_000)));
                    lines.push(r#"{"status":"success"}"#.into());
                    for l in lines {
                        let chunk = format!("{l}\n");
                        let _ = write!(s, "{:x}\r\n{chunk}\r\n", chunk.len());
                        let _ = s.flush();
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let _ = write!(s, "0\r\n\r\n");
                    m.store(true, Relaxed);
                }
                ("POST", "/api/embed") if m.load(Relaxed) => {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    // Text as it is; a picture by the caption its PNG carries (the test's
                    // stand-in for what the real model sees in it).
                    let inputs: Vec<String> = v["input"].as_array().map(|a| a.iter().map(|x| match x.as_str() {
                        Some(t) => t.to_string(),
                        None => caption(&unbase64(x["image"].as_str().unwrap_or(""))),
                    }).collect()).unwrap_or_default();
                    n.fetch_add(inputs.len(), Relaxed);
                    let embs: Vec<Vec<f32>> = inputs.iter().map(|t| vector(t)).collect();
                    reply(&mut s, "200 OK", serde_json::json!({ "model": "embeddinggemma", "embeddings": embs }).to_string());
                }
                ("POST", "/api/embed") => reply(&mut s, "404 Not Found", r#"{"error":"model not found, try pulling it first"}"#.into()),
                _ => reply(&mut s, "404 Not Found", r#"{"error":"not found"}"#.into()),
            }
        }
    });
    (addr, model, embedded)
}

#[test]
fn search_by_meaning_through_ollama() {
    use crate::ollama::{self, Status};
    use crate::semantic::{self, Engine, Phase};
    use std::sync::atomic::Ordering::Relaxed;
    let set_host = |h: &str| *ollama::TEST_HOST.lock().unwrap() = Some(h.to_string());
    // Nothing listening: not running, said plainly and quickly.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    set_host(&closed);
    let t0 = Instant::now();
    assert_eq!(ollama::status(), Status::NotRunning);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(ollama::embed(&["x".into()]).err().as_deref(), Some("Ollama isn't running"));
    // Running without the model: asked to fetch it, which streams progress.
    let (addr, _model, embedded) = fake_ollama(false);
    set_host(&addr);
    assert_eq!(ollama::status(), Status::NoModel);
    assert!(ollama::embed(&["x".into()]).err().unwrap().contains("not found"));
    let mut seen = vec![];
    ollama::pull(|d, a| seen.push((d, a)), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert_eq!(seen.last(), Some(&(300_000_000, 300_000_000)));
    assert_eq!(seen.len(), 4, "every progress line, through the chunked stream");
    assert_eq!(ollama::status(), Status::Ready);
    let e = ollama::embed(&["task: search result | query: a bill".into(), "title: none | text: a beach".into()]).unwrap();
    assert_eq!(e.len(), 2);
    for v in &e {
        assert_eq!(v.len(), ollama::DIMS);
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3, "length 1 after cutting to {}", ollama::DIMS);
    }
    // Passages: the whole text covered, each about a passage long, on character edges.
    let text = [include_str!("../LICENSE"), "日本語のテキスト。".repeat(300).as_str()].concat();
    let ps = semantic::cut(&text);
    assert!(ps.len() > 20);
    let mut reach = 0;
    for &(a, n) in &ps {
        assert!(text.is_char_boundary(a as usize) && text.is_char_boundary((a + n) as usize));
        assert!(n as usize <= 1800 + 4);
        assert!(a as usize <= reach, "a gap before {a}");
        reach = reach.max((a + n) as usize);
    }
    assert!(reach + 20 >= text.len() || ps.len() == 120, "covered to {reach} of {}", text.len());
    // A folder of files; the engine reads them and finds them by meaning.
    let d = tmpdir("meaning");
    touch(&d.join("letter from the bank.txt"), "Dear customer, your tax refund has been paid into your account. Keep this receipt.");
    touch(&d.join("summer.md"), "# Plans\n\nTwo weeks at the beach, flight on Friday, hotel near the sea.");
    touch(&d.join("Notes/dinner.txt"), "Pasta with tomato sauce: boil, stir, eat. A kitchen classic.");
    touch(&d.join("node_modules/skip.txt"), "invoice invoice invoice");
    touch(&d.join(".hidden/skip.txt"), "invoice invoice invoice");
    captioned_png(&d.join("IMG_0412.png"), "a puppy playing in the sand");
    // Pictures that aren't someone's: an icon, an app's artwork, frames cut from a video.
    sized_png(&d.join("app.png"), "icon", 64, 64);
    std::fs::create_dir_all(d.join("theme/icons")).unwrap();
    sized_png(&d.join("theme/icons/folder.png"), "icon", 256, 256);
    std::fs::create_dir_all(d.join("clip/frames")).unwrap();
    for k in 0..45 {
        sized_png(&d.join(format!("clip/frames/frame_{k:04}.png")), "a spreadsheet of numbers", 220, 220);
    }
    touch(&d.join("clip/frames/notes.txt"), "Frames cut from the holiday video at 30 per second.");
    // The same photo copied elsewhere: read once.
    std::fs::create_dir_all(d.join("Backup")).unwrap();
    std::fs::copy(d.join("IMG_0412.png"), d.join("Backup/IMG_0412.png")).unwrap();
    let _ = std::fs::remove_file(std::env::temp_dir().join("maia/meaning.idx"));
    let ctx = egui::Context::default();
    let start = embedded.load(Relaxed);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck at {:?}", engine.phase());
        std::thread::sleep(Duration::from_millis(10));
    }
    let idx = engine.index();
    assert_eq!(idx.files(), 6, "text and pictures; hidden and build folders, icons and video frames skipped");
    assert_eq!(embedded.load(Relaxed) - start, 5, "four texts and one picture: the copy shares its fingerprint");
    assert_eq!(PICTURE_SIDE.load(Relaxed), 512, "pictures shrunk before they go to the model");
    let best = |e: &Engine, q: &str| semantic::search(&e.index(), q, 3).unwrap().first().map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned());
    assert_eq!(best(&engine, "money back from the government").as_deref(), Some("letter from the bank.txt"), "no word in common, same topic");
    assert_eq!(best(&engine, "travel in summer").as_deref(), Some("summer.md"));
    assert_eq!(best(&engine, "cooking tonight").as_deref(), Some("dinner.txt"));
    assert_eq!(best(&engine, "photo of my dog").as_deref(), Some("IMG_0412.png"), "a picture by what it shows");
    let dogs = semantic::search(&engine.index(), "photo of my dog", 10).unwrap().iter().filter(|h| h.path.ends_with("IMG_0412.png")).count();
    assert_eq!(dogs, 1, "the copy in Backup isn't shown twice");
    let hit = &semantic::search(&engine.index(), "food", 1).unwrap()[0];
    assert!(hit.passage.starts_with("Pasta with tomato sauce"), "the passage shown: {:?}", hit.passage);
    // Saved, and on the next start only changed files are read again.
    let before = embedded.load(Relaxed);
    drop(engine);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(semantic::load(&std::env::temp_dir().join("maia/meaning.idx")).files(), 6, "saved to the cache");
    std::thread::sleep(Duration::from_secs(1));
    touch(&d.join("summer.md"), "# Plans\n\nWe changed our mind: a puppy instead of a trip. The dog needs a vet.");
    std::fs::remove_file(d.join("Notes/dinner.txt")).unwrap();
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(embedded.load(Relaxed) - before, 1, "only the changed file was read again");
    assert_eq!(engine.index().files(), 5, "the deleted file is gone from the index");
    assert_eq!(best(&engine, "summer plans changed for a pet").as_deref(), Some("summer.md"));
    drop(engine);
    // Pictures left out when "Include pictures" is off.
    let engine = Engine::start(vec![d.clone()], false, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(engine.index().files(), 3, "text only");
    drop(engine);
    // Code, data and vendored folders are left out: only writing is read. On battery a
    // new picture waits for the charger while that is still read.
    let body = (0..40).map(|k| format!("    // Step {k}: fetch the lyrics for the playing song and keep them in memory\n")).collect::<String>();
    touch(&d.join("lyrics.py"), &body);
    touch(&d.join("package-lock.json"), "{ \"name\": \"lyrics for the playing song and more words\" }");
    touch(&d.join("vendor/README.md"), "Lyrics for the playing song, fetched from a service and kept in memory for later.");
    touch(&d.join("song notes.md"), "Lyrics for the playing song, fetched from a service and kept in memory for later.");
    captioned_png(&d.join("IMG_0413.png"), "a puppy playing in the sand");
    semantic::TEST_BATTERY.store(true, Relaxed);
    let before = embedded.load(Relaxed);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(embedded.load(Relaxed) - before, 1, "the note only: no code, data or vendored files");
    assert_eq!(engine.waiting(), 4, "pictures wait (the run with pictures off left them all out)");
    assert_eq!(engine.index().files(), 4, "the text is read meanwhile");
    semantic::TEST_BATTERY.store(false, Relaxed);
    engine.poke();
    let t0 = Instant::now();
    while engine.waiting() > 0 || engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(engine.index().files(), 7, "plugged in: the picture is read");
    drop(engine);
    // Without the model the engine waits for the user's say-so, then fetches it.
    let (addr2, model2, _) = fake_ollama(false);
    set_host(&addr2);
    let engine = Engine::start(vec![d.clone()], true, &ctx);
    let t0 = Instant::now();
    while engine.phase() != Phase::NoModel {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!model2.load(Relaxed), "nothing downloaded without asking");
    engine.download();
    let t0 = Instant::now();
    while engine.phase() != Phase::Ready {
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", engine.phase());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(model2.load(Relaxed));
    drop(engine);
    *ollama::TEST_HOST.lock().unwrap() = None;
    assert_eq!(semantic::folders(" ~/Documents , /srv/notes,, "), vec![crate::fsx::home().join("Documents"), PathBuf::from("/srv/notes")]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn machine_named_pictures_told_from_photos() {
    use crate::semantic::machine_named;
    for n in ["frame_0042.png", "clip-000123.jpg", "000017.png", "3f9a1c0e-7b2d-4c1a.webp", "out.0001.png", "shot 004.png"] {
        assert!(machine_named(Path::new(n)), "{n}");
    }
    for n in ["IMG_1234.jpg", "PXL_20260912_101500.jpg", "DSC0042.JPG", "Screenshot_2026-10-15.png", "wallhaven-lygxjl.png", "Akeno.png", "17.png", "beach day.png"] {
        assert!(!machine_named(Path::new(n)), "{n}");
    }
}

#[test]
fn names_found_anywhere_under_home() {
    let d = tmpdir("names");
    for f in ["Notes/Read Me.md", "Notes/readme-old.txt", "Code/maia/README.md", "Code/maia/src/main.rs", "Code/maia/node_modules/readme.md", ".hidden/readme.md", "Photos/Été à Paris.jpg"] {
        touch(&d.join(f), "x");
    }
    let names = crate::namefind::Names::start(d.clone(), &egui::Context::default());
    let t0 = Instant::now();
    while names.walking() {
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    let find = |q: &str| names.find(q, 10).into_iter().map(|(p, _)| p.strip_prefix(&d).unwrap().display().to_string()).collect::<Vec<_>>();
    // Any case; the name that is just the word first; build and hidden folders left out.
    assert_eq!(find("readme"), vec!["Code/maia/README.md", "Notes/readme-old.txt"]);
    assert_eq!(find("read me")[0], "Notes/Read Me.md", "every word; the name that is just them first");
    assert_eq!(find("me read"), find("read me"), "in any order");
    // A word may be in a folder's name, as long as one is in the file's.
    assert_eq!(find("maia main"), vec!["Code/maia/src/main.rs"]);
    assert_eq!(find("maia"), vec!["Code/maia"], "folders too");
    assert_eq!(find("été"), vec!["Photos/Été à Paris.jpg"], "beyond ASCII");
    assert!(find("").is_empty() && find("nothing like it").is_empty());
    let _ = std::fs::remove_dir_all(&d);
}


/// Every theme reads well: its text on its list, and its code colours (the theme's own and
/// the classic set) on the preview's background, by the usual contrast measure.
#[test]
fn themes_and_their_code_colours_are_readable() {
    use crate::theme::{Syntax, ThemeName, palette};
    fn lum(c: egui::Color32) -> f32 {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }
    let contrast = |a: egui::Color32, b: egui::Color32| {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    };
    let mut seen = std::collections::HashSet::new();
    for t in ThemeName::ALL {
        assert!(seen.insert(t.label()), "names differ");
        let p = palette(t);
        assert_eq!(p.dark, !t.is_light(), "{}", t.label());
        assert!(contrast(p.text, p.panel) >= 7.0, "{}: text {:.1}", t.label(), contrast(p.text, p.panel));
        assert!(contrast(p.text_dim, p.panel) >= 3.0, "{}: dim text {:.1}", t.label(), contrast(p.text_dim, p.panel));
        for s in [p.syntax, Syntax::classic(p.dark)] {
            for (what, c, min) in [("keyword", s.keyword, 3.0), ("type", s.kind, 3.0), ("string", s.string, 3.0), ("number", s.number, 3.0), ("comment", s.comment, 2.0)] {
                assert!(contrast(c, p.panel) >= min, "{}: {what} {:.2}", t.label(), contrast(c, p.panel));
            }
        }
    }
    assert_eq!(palette(ThemeName::TokyoNight).syntax, Syntax::of(ThemeName::TokyoNight));
    assert_ne!(Syntax::of(ThemeName::Gruvbox), Syntax::of(ThemeName::Mocha), "each its own");
}

/// A noreply address names its GitHub account: by number when it has one, else by name.
/// Anything else, or a malformed one, names nobody.
#[test]
fn github_noreply_addresses_name_the_picture() {
    use crate::avatars::{Source, noreply_url, source};
    assert_eq!(noreply_url("12345+octo@users.noreply.github.com").as_deref(), Some("https://avatars.githubusercontent.com/u/12345?s=96"));
    assert_eq!(noreply_url(" 12345+octo@Users.NoReply.GitHub.com ").as_deref(), Some("https://avatars.githubusercontent.com/u/12345?s=96"), "any case, trimmed");
    assert_eq!(noreply_url("octo-cat@users.noreply.github.com").as_deref(), Some("https://avatars.githubusercontent.com/octo-cat?s=96"));
    for none in ["octo@example.invalid", "octo@noreply.github.com", "octo@users.noreply.github.com.example.invalid", "x1+octo@users.noreply.github.com", "+octo@users.noreply.github.com", "-octo@users.noreply.github.com", "oc/to@users.noreply.github.com", "oc to@users.noreply.github.com", "@users.noreply.github.com", "users.noreply.github.com", ""] {
        assert_eq!(noreply_url(none), None, "{none:?}");
    }
    let repo = ("octo-org".to_string(), "demo".to_string());
    let sha = "0123456789abcdef0123456789abcdef01234567";
    // The cheapest way first: a noreply address needs no lookup, even on github.com.
    assert_eq!(source("12345+octo@users.noreply.github.com", sha, Some(&repo)), Some(Source::Picture("https://avatars.githubusercontent.com/u/12345?s=96".into())));
    assert_eq!(source("ada@example.invalid", sha, Some(&repo)), Some(Source::Api(format!("https://api.github.com/repos/octo-org/demo/commits?sha={sha}&per_page=1"))));
    assert_eq!(source("ada@example.invalid", sha, None), None, "not on github.com: no picture");
    assert_eq!(source("ada@example.invalid", "not a sha", Some(&repo)), None);
    assert_eq!(source("no address", sha, Some(&repo)), None);
}

/// Only a remote on github.com itself is looked up, in every form git writes it; an
/// Enterprise host or a look-alike is not github.com.
#[test]
fn github_remote_urls() {
    use crate::avatars::github_repo;
    let ok = Some(("octo-org".to_string(), "demo".to_string()));
    for u in [
        "https://github.com/octo-org/demo.git",
        "https://github.com/octo-org/demo",
        "https://github.com/octo-org/demo/",
        "http://github.com/octo-org/demo.git",
        "https://GitHub.com/octo-org/demo.git",
        "https://www.github.com/octo-org/demo",
        "https://someone:not-a-real-token@github.com/octo-org/demo.git",
        "git@github.com:octo-org/demo.git",
        "git@github.com:octo-org/demo",
        "github.com:octo-org/demo.git",
        "ssh://git@github.com/octo-org/demo.git",
        "ssh://git@ssh.github.com:443/octo-org/demo.git",
        "git://github.com/octo-org/demo.git",
        "  https://github.com/octo-org/demo.git\n",
    ] {
        assert_eq!(github_repo(u), ok, "{u:?}");
    }
    assert_eq!(github_repo("https://github.com/octo-org/my.site.git"), Some(("octo-org".into(), "my.site".into())));
    for u in [
        "https://github.example.com/octo-org/demo.git",
        "git@github.example.com:octo-org/demo.git",
        "ssh://git@ghe.example.invalid/octo-org/demo.git",
        "https://github.com.example.invalid/octo-org/demo.git",
        "https://notgithub.com/octo-org/demo.git",
        "https://gitlab.com/octo-org/demo.git",
        "https://example.invalid/github.com/octo-org/demo.git",
        "https://github.com@example.invalid/octo-org/demo.git",
        "file:///srv/github.com/octo-org/demo.git",
        "/srv/git/demo.git",
        "../demo.git",
        "https://github.com/octo-org",
        "https://github.com/octo-org/demo/tree/main",
        "https://github.com/../demo",
        "https://github.com/octo org/demo",
        "",
    ] {
        assert_eq!(github_repo(u), None, "{u:?}");
    }
}

/// GitHub's answer about a commit gives the account's picture at the size drawn, from
/// GitHub's picture host only; an author without an account is said to have none.
#[test]
fn github_api_answer_gives_the_picture() {
    use crate::avatars::{avatar_from_api, sized_avatar};
    let one = br#"{"sha":"0123abc","commit":{"author":{"name":"Octo"}},"author":{"login":"octo","id":12345,"avatar_url":"https://avatars.githubusercontent.com/u/12345?v=4"}}"#;
    assert_eq!(avatar_from_api(one), Some(Some("https://avatars.githubusercontent.com/u/12345?v=4&s=96".into())));
    let list = br#"[{"sha":"0123abc","author":{"avatar_url":"https://avatars.githubusercontent.com/u/777"}},{"sha":"9999"}]"#;
    assert_eq!(avatar_from_api(list), Some(Some("https://avatars.githubusercontent.com/u/777?s=96".into())), "the list form: its first commit");
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":null}"#), Some(None), "an address no account claims");
    assert_eq!(avatar_from_api(br#"[{"sha":"0123abc"}]"#), Some(None));
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":{"avatar_url":"https://example.invalid/me.png"}}"#), Some(None), "another host is not followed");
    assert_eq!(avatar_from_api(br#"{"sha":"0123abc","author":{"avatar_url":"http://avatars.githubusercontent.com/u/1"}}"#), Some(None), "https only");
    for bad in [&br#"{"message":"API rate limit exceeded"}"#[..], b"[]", b"not json", b"", b"null", br#""text""#] {
        assert_eq!(avatar_from_api(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/octo").as_deref(), Some("https://avatars.githubusercontent.com/octo?s=96"));
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/"), None);
    assert_eq!(sized_avatar("https://avatars.githubusercontent.com/u/1 --output x"), None);
}

/// An answer holds for a week, then a picture is shown while it is asked for again and a
/// "none" is forgotten. A clock set back doesn't make an answer last for ever.
#[test]
fn github_picture_cache_staleness() {
    use crate::avatars::{Cached, MAX_AGE, fresh, key, read_cache, write_hit, write_miss};
    use std::time::SystemTime;
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let day = Duration::from_secs(86_400);
    assert!(fresh(now, now) && fresh(now - 6 * day, now));
    assert!(!fresh(now - MAX_AGE, now) && !fresh(now - 30 * day, now));
    assert!(fresh(now + day, now), "a little ahead: the clock moved back a bit");
    assert!(!fresh(now + 8 * day, now), "far ahead: asked again");
    assert_eq!(key(" Ada@Example.invalid "), key("ada@example.invalid"), "one answer per address, whatever its case");
    assert_ne!(key("ada@example.invalid"), key("grace@example.invalid"));
    assert!(key("ada@example.invalid").bytes().all(|b| b.is_ascii_hexdigit()), "no address in a file name");
    let d = tmpdir("avatar-cache");
    let k = key("ada@example.invalid");
    let age = |name: &str, ago: Duration| std::fs::File::options().write(true).open(d.join(format!("{k}.{name}"))).unwrap().set_modified(SystemTime::now() - ago).unwrap();
    let now = SystemTime::now();
    assert_eq!(read_cache(&d, &k, now), Cached::Nothing);
    write_hit(&d, &k, b"picture");
    assert_eq!(read_cache(&d, &k, now), Cached::Hit(b"picture".to_vec()));
    age("img", 8 * day);
    assert_eq!(read_cache(&d, &k, now), Cached::Stale(b"picture".to_vec()));
    write_miss(&d, &k);
    assert_eq!(read_cache(&d, &k, now), Cached::Miss, "none, this week");
    assert!(!d.join(format!("{k}.img")).exists(), "the old picture goes with it");
    age("none", 8 * day);
    assert_eq!(read_cache(&d, &k, now), Cached::Nothing, "a week on, asked again");
    write_hit(&d, &k, b"new");
    assert!(!d.join(format!("{k}.none")).exists());
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temporary file left behind");
    let _ = std::fs::remove_dir_all(&d);
}

/// One author's picture end to end against a stand-in for GitHub: fetched once, kept,
/// then read from the cache; a "none" kept the same way; too many requests and errors
/// keep nothing; a week-old picture stands in when GitHub can't be asked.
#[test]
fn github_find_asks_once_and_keeps_the_answer() {
    use crate::avatars::{Found, Http, Source, find, key, write_hit};
    use std::cell::RefCell;
    let d = tmpdir("avatar-find");
    let png = png_bytes(120, 80, 4);
    let now = std::time::SystemTime::now();
    let asked = RefCell::new(vec![]);
    let get = |answers: Vec<(&'static str, Http)>| {
        let asked = &asked;
        move |url: &str| {
            asked.borrow_mut().push(url.to_string());
            answers.iter().find(|(k, _)| url.contains(k)).map_or(Http::Failed, |(_, h)| h.clone())
        }
    };
    let pic = Source::Picture("https://avatars.githubusercontent.com/u/12345?s=96".into());
    let k = key("12345+octo@users.noreply.github.com");
    let g = get(vec![("/u/12345", Http::Ok(png.clone()))]);
    assert!(matches!(find(&d, &k, &pic, &g, now), Found::Picture(i) if (i.w, i.h) == (120, 80)));
    assert!(matches!(find(&d, &k, &pic, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 1, "the second time from the cache");
    // Through the API: the commit's account, then its picture.
    let api = Source::Api("https://api.github.com/repos/octo-org/demo/commits?sha=0123abc&per_page=1".into());
    let json = br#"[{"sha":"0123abc","author":{"avatar_url":"https://avatars.githubusercontent.com/u/777?v=4"}}]"#.to_vec();
    asked.borrow_mut().clear();
    let g = get(vec![("api.github.com", Http::Ok(json)), ("/u/777?v=4&s=96", Http::Ok(png.clone()))]);
    assert!(matches!(find(&d, &key("grace@example.invalid"), &api, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 2);
    // No account behind the address: kept as none, not asked again.
    asked.borrow_mut().clear();
    let g = get(vec![("api.github.com", Http::Ok(br#"{"sha":"0123abc","author":null}"#.to_vec()))]);
    assert!(matches!(find(&d, &key("linus@example.invalid"), &api, &g, now), Found::None));
    assert!(matches!(find(&d, &key("linus@example.invalid"), &api, &g, now), Found::None));
    assert_eq!(asked.borrow().len(), 1);
    // Too many requests, or an error: nothing kept, so it is asked again later.
    for (h, limited) in [(Http::Status(403), true), (Http::Status(429), true), (Http::Status(500), false), (Http::Failed, false)] {
        let g = get(vec![("api.github.com", h)]);
        let f = find(&d, &key("edsger@example.invalid"), &api, &g, now);
        assert!(if limited { matches!(f, Found::Limited) } else { matches!(f, Found::Failed) });
    }
    assert!(!std::fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(&key("edsger@example.invalid"))));
    // A commit GitHub doesn't have: none, for a week.
    let g = get(vec![("api.github.com", Http::Status(422))]);
    assert!(matches!(find(&d, &key("barbara@example.invalid"), &api, &g, now), Found::None));
    // A picture that doesn't decode is not kept.
    let g = get(vec![("/u/12345", Http::Ok(b"<html>".to_vec()))]);
    assert!(matches!(find(&d, &key("12345+other@users.noreply.github.com"), &pic, &g, now), Found::Failed));
    // A week-old picture while GitHub can't be reached, or limits us.
    let ks = key("13+old@users.noreply.github.com");
    write_hit(&d, &ks, &png);
    std::fs::File::options().write(true).open(d.join(format!("{ks}.img"))).unwrap().set_modified(now - Duration::from_secs(8 * 86_400)).unwrap();
    asked.borrow_mut().clear();
    let g = get(vec![]);
    assert!(matches!(find(&d, &ks, &pic, &g, now), Found::Picture(_)));
    assert_eq!(asked.borrow().len(), 1, "a stale picture is asked for again");
    let _ = std::fs::remove_dir_all(&d);
}

/// curl's answer splits into body and status; the command reads no user settings, speaks
/// only https, caps the answer and the time, and names itself plainly.
#[test]
fn github_requests_through_curl() {
    use crate::avatars::{Http, curl_args, parse_curl};
    assert_eq!(parse_curl(b"\x89PNG\n\x00data\n200"), Http::Ok(b"\x89PNG\n\x00data".to_vec()));
    assert_eq!(parse_curl(b"{\"message\":\"rate\"}\n403"), Http::Status(403));
    assert_eq!(parse_curl(b"\n000"), Http::Failed, "no answer at all");
    assert_eq!(parse_curl(b""), Http::Failed);
    assert_eq!(parse_curl(b"cut short"), Http::Failed);
    let a = curl_args("https://avatars.githubusercontent.com/u/12345?s=96");
    assert_eq!(a[0], "-q", "first, or ~/.curlrc is read");
    let has = |a: &[String], k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
    assert!(has(&a, "--proto", "=https") && has(&a, "--proto-redir", "=https") && a.iter().any(|x| x == "--location"));
    assert!(has(&a, "--max-time", "10") && has(&a, "--max-filesize", "1048576") && has(&a, "--user-agent", "Maia"));
    assert!(!a.iter().any(|x| matches!(x.as_str(), "-u" | "--user" | "-n" | "--netrc" | "--netrc-optional" | "-b" | "--cookie" | "-K" | "--config")), "no credentials of any kind");
    assert!(!a.iter().any(|x| x.to_ascii_lowercase().contains("authorization")));
    assert_eq!(a.last().map(String::as_str), Some("https://avatars.githubusercontent.com/u/12345?s=96"));
    assert!(curl_args("https://api.github.com/repos/o/r/commits?sha=0123abc&per_page=1").iter().any(|x| x == "Accept: application/vnd.github+json"));
    assert!(!a.iter().any(|x| x.starts_with("Accept")), "pictures need no API header");
}

/// A picture is cut to the middle square at the size drawn, round, with a soft edge.
#[test]
fn github_picture_is_cut_round() {
    let img = crate::imgdec::decode_bytes(&png_bytes(120, 80, 2)).unwrap();
    let c = crate::avatars::round(&img, 37);
    assert_eq!(c.size, [37, 37]);
    let a = |x: usize, y: usize| c.pixels[y * 37 + x].a();
    assert_eq!((a(0, 0), a(36, 0), a(0, 36), a(36, 36)), (0, 0, 0, 0), "corners outside the circle");
    assert_eq!(a(18, 18), 255);
    assert!((1..255).contains(&a(18, 0)) || a(18, 0) == 255, "the top edge is inside or soft");
    assert!(c.pixels.iter().any(|p| (1..255).contains(&p.a())), "a soft edge, not a jagged one");
    let small = crate::imgdec::decode_bytes(&png_bytes(20, 20, 2)).unwrap();
    assert_eq!(crate::avatars::round(&small, 96).size, [20, 20], "a small picture is not blown up here");
}

/// Every option is listed in Options exactly once, in one section, with its group's
/// rows together and each sub-option right under the switch it belongs to.
#[test]
fn every_option_has_one_section() {
    use crate::popups::{OPTS, Opt, Section, opt_parent};
    // Exhaustive: a new option does not build until it is numbered here.
    let index = |o: Opt| match o {
        Opt::Theme => 0, Opt::Font => 1, Opt::InspectorFontSize => 2, Opt::RectButtons => 3, Opt::AssetCache => 4,
        Opt::FollowSystem => 5, Opt::SyntaxTheme => 6, Opt::Round => 7, Opt::FontSize => 8, Opt::Spacing => 9,
        Opt::Animations => 10, Opt::Tooltips => 11, Opt::AltRows => 12, Opt::InactiveOpacity => 13, Opt::Breadcrumbs => 14,
        Opt::CompactPaths => 15, Opt::ShowExt => 16, Opt::ShowHidden => 17, Opt::ShowSystem => 18, Opt::FoldersFirst => 19,
        Opt::HighlightRecents => 20, Opt::RecentsOnTop => 21, Opt::TrackRecents => 22, Opt::CalcSizes => 23, Opt::SizesSsd => 24,
        Opt::SizesHdd => 25, Opt::SizesNet => 26, Opt::SelectionBoxes => 27, Opt::InspectorFont => 28, Opt::SystemIcons => 29,
        Opt::Thumbnails => 30, Opt::ThumbFill => 31, Opt::ScrollBeyond => 32, Opt::DateFormat => 33, Opt::ActOnPress => 34,
        Opt::SingleClick => 35, Opt::SingleClickFolders => 36, Opt::OpenFoldersNewTab => 37, Opt::DblClickParent => 38, Opt::ReverseScroll => 39,
        Opt::CharKey => 40, Opt::MiddleClick => 41, Opt::InspectorLayout => 42, Opt::Startup => 43, Opt::NewTab => 44,
        Opt::GitPictures => 45, Opt::DefaultFm => 46, Opt::SingleInstance => 47, Opt::Meaning => 48, Opt::MeaningFolders => 49,
        Opt::MeaningPictures => 50, Opt::ConfigFile => 51, Opt::ResetAll => 52, Opt::Install => 53, Opt::LineSpacing => 54, Opt::Updates => 55,
        Opt::Editor => 56,
        Opt::IgnoredFiles => 57,
        Opt::TrashAge => 58,
        Opt::PictureViewer => 61,
        Opt::Tour => 62,
        Opt::CustomActions => 63,
    };
    let numbered: Vec<usize> = (0..59).chain([61, 62, 63]).collect();
    let mut seen = vec![0; 64];
    for o in OPTS {
        seen[index(o.0)] += 1;
    }
    for (i, n) in seen.iter().enumerate() {
        assert_eq!(*n, numbered.contains(&i) as usize, "option {i} listed {n} times");
    }
    // Sections in the order of the left column, each a block; groups never split.
    let order: Vec<Section> = OPTS.iter().map(|o| o.1).fold(vec![], |mut v, s| {
        if v.last() != Some(&s) {
            v.push(s);
        }
        v
    });
    assert_eq!(order, Section::ALL.to_vec(), "every section has options, in one block, in order");
    let mut groups: Vec<(Section, &str)> = vec![];
    for o in OPTS {
        if groups.last() != Some(&(o.1, o.2)) {
            assert!(!groups.contains(&(o.1, o.2)), "{} / {:?} is split", o.1.name(), o.2);
            groups.push((o.1, o.2));
        }
    }
    for (k, o) in OPTS.iter().enumerate() {
        if let Some(p) = opt_parent(o.0) {
            let at = OPTS.iter().position(|x| x.0 == p).unwrap();
            assert!(at < k && OPTS[at + 1..k].iter().all(|x| opt_parent(x.0) == Some(p)), "{:?} sits right under its switch", o.3);
            assert_eq!((OPTS[at].1, OPTS[at].2), (o.1, o.2), "{:?} shares its switch's group", o.3);
        }
    }
    // Short, plain lines.
    for o in OPTS {
        assert!(o.4.len() <= 60 && !o.4.contains('\u{2014}'), "{:?}: {:?} is long", o.3, o.4);
        assert!(o.4.ends_with('.'), "{:?}", o.4);
    }
}

/// The search covers every section: the words people type find their option first,
/// and a word shared by options in several sections finds each of them.
#[test]
fn option_search_spans_sections() {
    use crate::popups::{OPTS, Opt, Section, find_options};
    let first = |q: &str| find_options(q).first().map(|&i| OPTS[i].0);
    for (q, o) in [
        ("color scheme", Opt::Theme),
        ("default file manager", Opt::DefaultFm),
        ("search by meaning", Opt::Meaning),
        ("reuse the open window", Opt::SingleInstance),
        ("show github pictures", Opt::GitPictures),
        ("ignored", Opt::IgnoredFiles),
        ("hidden", Opt::ShowHidden),
        ("folders first", Opt::FoldersFirst),
        ("install", Opt::Install),
        ("config", Opt::ConfigFile),
    ] {
        assert_eq!(first(q), Some(o), "{q:?}");
    }
    assert!(find_options("").is_empty() && find_options("   ").is_empty(), "no search, no results");
    assert!(find_options("qqzzx").is_empty());
    let sections = |q: &str| find_options(q).iter().map(|&i| OPTS[i].1).collect::<HashSet<Section>>();
    let size = sections("size");
    for s in [Section::Appearance, Section::Files, Section::Preview, Section::System] {
        assert!(size.contains(&s), "\"size\" reaches {}", s.name());
    }
    assert!(sections("font").is_superset(&[Section::Appearance, Section::Preview].into_iter().collect()));
    // A section or group name finds what is in it.
    assert!(find_options("thumbnails").iter().any(|&i| OPTS[i].0 == Opt::ThumbFill));
    assert!(find_options("recents").iter().any(|&i| OPTS[i].0 == Opt::TrackRecents));
    assert_eq!(Section::Appearance.step(-1), Section::System, "sections wrap");
    assert_eq!(Section::System.step(1), Section::Appearance);
    assert_eq!(Section::Files.step(1), Section::Mouse);
}

/// Updates: versions compared as numbers, GitHub's answer read only for this project's
/// downloads, and an install that checks the download before it replaces anything.
#[test]
fn updates_check_compare_and_install() {
    use crate::update::{Fetch, Release, State, Updater, curl_args, newer, parse_release, parse_sum, version};
    assert_eq!(version("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(version("0.10"), Some((0, 10, 0)));
    assert_eq!(version("2.0.0-beta.1"), Some((2, 0, 0)));
    assert_eq!(version("one"), None);
    assert_eq!(version("1.2.3.4"), None);
    assert!(newer("0.2.0", "v0.10.0"), "numbers, not text");
    assert!(!newer("0.2.0", "v0.2.0") && !newer("0.3.0", "0.2.9") && !newer("0.2.0", "garbage"));
    let json = |tag: &str, host: &str| format!(r#"{{"tag_name":"{tag}","body":"Faster search.\n\nNew themes.","assets":[
        {{"name":"maia-linux-x86_64","browser_download_url":"{host}/yappologistic/Maia/releases/download/{tag}/maia-linux-x86_64"}},
        {{"name":"maia-linux-x86_64.sha256","browser_download_url":"{host}/yappologistic/Maia/releases/download/{tag}/maia-linux-x86_64.sha256"}}]}}"#);
    let r = parse_release(json("v9.9.9", "https://github.com").as_bytes()).expect("a release");
    assert_eq!((r.version.as_str(), r.notes.starts_with("Faster search.")), ("v9.9.9", true));
    assert!(parse_release(json("v9.9.9", "https://evil.example").as_bytes()).is_none(), "downloads only from the project's releases");
    assert!(parse_release(br#"{"tag_name":"v9.9.9","assets":[]}"#).is_none(), "no build, no update");
    let h = "a".repeat(64);
    assert_eq!(parse_sum(&format!("{h}  maia-linux-x86_64\n")), Some(h.clone()));
    assert_eq!(parse_sum("not a sum"), None);
    let args = curl_args("https://api.github.com/repos/x/y/releases/latest", None);
    assert_eq!(args[0], "-q", "no ~/.curlrc");
    assert!(args.windows(2).any(|w| w == ["--proto", "=https"]) && args.iter().any(|a| a.starts_with("Accept:")));
    assert!(args.iter().all(|a| !a.to_lowercase().contains("authorization")), "no credentials");
    // An install against a stand-in for GitHub: the right bytes replace the program.
    let d = tmpdir("update");
    let exe = d.join("maia");
    std::fs::write(&exe, b"\x7fELF old").unwrap();
    let build = b"\x7fELF the new build".to_vec();
    let sum = |b: &[u8]| {
        let f = d.join("sum-input");
        std::fs::write(&f, b).unwrap();
        let out = std::process::Command::new("sha256sum").arg(&f).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
    };
    let serve = |bin: Vec<u8>, sum_text: String| -> Fetch {
        let api = json("v9.9.9", "https://github.com");
        std::sync::Arc::new(move |url: &str, to: Option<&Path>| {
            let body = if url.contains("api.github.com") { api.clone().into_bytes() } else if url.ends_with(".sha256") { sum_text.clone().into_bytes() } else { bin.clone() };
            match to {
                Some(p) => std::fs::write(p, &body).map(|_| vec![]).map_err(|e| e.to_string()),
                None => Ok(body),
            }
        })
    };
    let wait = |u: &Updater| {
        let t0 = Instant::now();
        while matches!(u.state(), State::Checking | State::Downloading(_)) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        u.state()
    };
    let ctx = egui::Context::default();
    let u = Updater::for_test(serve(build.clone(), format!("{}  maia-linux-x86_64", sum(&build))), exe.clone());
    u.check(&ctx);
    let State::Available(r) = wait(&u) else { panic!("{:?}", u.state()) };
    u.install(r.clone(), &ctx);
    assert_eq!(wait(&u), State::Ready("v9.9.9".into()));
    assert_eq!(std::fs::read(&exe).unwrap(), build, "the new build in place");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755, "and runnable");
    // A download that doesn't match its checksum changes nothing, and leaves nothing.
    std::fs::write(&exe, b"\x7fELF old").unwrap();
    let u = Updater::for_test(serve(b"\x7fELF tampered".to_vec(), format!("{}  maia-linux-x86_64", sum(&build))), exe.clone());
    u.install(r.clone(), &ctx);
    assert!(matches!(wait(&u), State::Failed(e) if e.contains("checksum")));
    assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF old");
    assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".maia-update")), "no download left behind");
    // A program in a folder Maia can't write: said so, nothing fetched.
    let u = Updater::for_test(serve(build.clone(), String::new()), PathBuf::from("/proc/maia"));
    u.install(r, &ctx);
    assert!(matches!(wait(&u), State::CantWrite(_)));
    let _ = std::fs::remove_dir_all(&d);
}

/// Updates of a packaged Maia: a .deb, .rpm or pacman install is found by asking dpkg, rpm or pacman who
/// owns the program, and gets the release's package, checked against SHA256SUMS and
/// handed to dpkg -i, rpm -U or pacman -U through pkexec once the program alone has been tried. An
/// AppImage replaces its own file. All of it on fakes: no package manager is run.
#[test]
fn updates_through_packages() {
    use crate::update::{Fetch, Installed, State, Updater, parse_release, sum_for};
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir("update-pkg");
    let sum = |b: &[u8]| {
        let f = d.join("sum-input");
        std::fs::write(&f, b).unwrap();
        let out = std::process::Command::new("sha256sum").arg(&f).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
    };
    let (deb, rpm, pac, app) = (Installed::Deb.package("v9.9.9").unwrap(), Installed::Rpm.package("v9.9.9").unwrap(), Installed::Pacman.package("v9.9.9").unwrap(), Installed::AppImage(PathBuf::new()).package("v9.9.9").unwrap());
    assert!(deb.starts_with("maia_9.9.9_") && deb.ends_with(".deb") && rpm.starts_with("maia-9.9.9-1.") && pac.starts_with("maia-9.9.9-1-") && pac.ends_with(".pkg.tar.zst") && app.starts_with("Maia-9.9.9-"));
    let names = [crate::update::ASSET.to_string(), crate::update::SUMS.to_string(), deb.clone(), rpm.clone(), pac.clone(), app.clone(), "SHA256SUMS".into()];
    let json = format!(r#"{{"tag_name":"v9.9.9","body":"","assets":[{}]}}"#, names.iter().map(|n| format!(r#"{{"name":"{n}","browser_download_url":"https://github.com/yappologistic/Maia/releases/download/v9.9.9/{n}"}}"#)).collect::<Vec<_>>().join(","));
    let r = parse_release(json.as_bytes()).expect("a release");
    assert_eq!(r.assets.len(), 7, "every download kept");
    let build = b"\x7fELF the new build".to_vec();
    let deb_bytes = b"!<arch>\ndebian-binary the new package".to_vec();
    let rpm_bytes = b"\xed\xab\xee\xdb the new package".to_vec();
    let pac_bytes = b"\x28\xb5\x2f\xfd the new package".to_vec();
    let app_bytes = b"\x7fELF AI\x02 the new AppImage".to_vec();
    let sums = format!("{}  {deb}\n{}  {rpm}\n{}  {pac}\n{}  {app}\n{}  {}\n", sum(&deb_bytes), sum(&rpm_bytes), sum(&pac_bytes), sum(&app_bytes), sum(&build), crate::update::ASSET);
    assert_eq!(sum_for(&sums, &rpm), Some(sum(&rpm_bytes)));
    assert_eq!(sum_for(&sums, "maia_1.0.0_amd64.deb"), None);
    // A stand-in for GitHub, serving `files` by name (a tampered one, say).
    let serve = |files: Vec<(String, Vec<u8>)>| -> Fetch {
        let (api, sums, bsum) = (json.clone(), sums.clone(), format!("{}  {}", sum(&build), crate::update::ASSET));
        std::sync::Arc::new(move |url: &str, to: Option<&Path>| {
            let name = url.rsplit('/').next().unwrap_or("");
            let body = if url.contains("api.github.com") {
                api.clone().into_bytes()
            } else if name == "SHA256SUMS" {
                sums.clone().into_bytes()
            } else if name.ends_with(".sha256") {
                bsum.clone().into_bytes()
            } else {
                files.iter().find(|(n, _)| n == name).map(|(_, b)| b.clone()).ok_or("404")?
            };
            match to {
                Some(p) => std::fs::write(p, &body).map(|_| vec![]).map_err(|e| e.to_string()),
                None => Ok(body),
            }
        })
    };
    let all = || vec![(crate::update::ASSET.to_string(), build.clone()), (deb.clone(), deb_bytes.clone()), (rpm.clone(), rpm_bytes.clone()), (pac.clone(), pac_bytes.clone()), (app.clone(), app_bytes.clone())];
    let wait = |u: &Updater| {
        let t0 = Instant::now();
        while matches!(u.state(), State::Downloading(_) | State::Installing(_)) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        u.state()
    };
    // The program where only root writes, as a package puts it.
    let bin = d.join("usr/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("maia");
    std::fs::write(&exe, b"\x7fELF old").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Fakes: the manager's answer to who owns the program, its install (which keeps a
    // copy of what it was given), and pkexec, which logs and runs it or refuses.
    let fakes = |name: &str, owner: &str| {
        let f = d.join(name);
        std::fs::create_dir_all(&f).unwrap();
        let script = |p: &str, body: &str| {
            std::fs::write(f.join(p), format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(f.join(p), std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let keep = format!("echo \"$0 $*\" >> {0}/log; cp \"$2\" {0}/installed", f.display());
        if owner == "dpkg" {
            script("dpkg-query", "echo \"maia: $2\"");
            script("dpkg", &keep);
        } else if owner == "pacman" {
            script("pacman", &format!("[ \"$1\" = -Qoq ] && {{ printf maia; exit 0; }}\necho \"$0 $*\" >> {0}/log; cp \"$3\" {0}/installed", f.display()));
        } else {
            script("rpm", &format!("[ \"$1\" = -qf ] && {{ printf maia; exit 0; }}\n{keep}"));
        }
        script("pkexec", &format!("echo \"pkexec $*\" >> {0}/log; [ -e {0}/deny ] && exit 126; exec \"$@\"", f.display()));
        f
    };
    let ctx = egui::Context::default();
    let run = |fake: Option<&Path>, u: &Updater| {
        crate::drives::FAKE.with(|f| *f.borrow_mut() = fake.map(Path::to_path_buf));
        u.install(r.clone(), &ctx);
        crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
        wait(u)
    };
    // A .deb install: the package, checked, to dpkg -i through pkexec; nothing left behind.
    let f = fakes("deb-tools", "dpkg");
    let u = Updater::for_test(serve(all()), exe.clone());
    assert_eq!(run(Some(&f), &u), State::Ready("v9.9.9".into()));
    let log = std::fs::read_to_string(f.join("log")).unwrap();
    assert!(log.starts_with(&format!("pkexec {} -i {}", f.join("dpkg").display(), d.join("usr/updates").join(&deb).display())), "{log}");
    assert_eq!(std::fs::read(f.join("installed")).unwrap(), deb_bytes, "the release's package installed");
    assert!(!d.join("usr/updates").join(&deb).exists(), "the download removed");
    assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF old", "the program itself left to dpkg");
    // The password dialog dismissed: said so.
    std::fs::write(f.join("deny"), b"").unwrap();
    let u = Updater::for_test(serve(all()), exe.clone());
    assert!(matches!(run(Some(&f), &u), State::Failed(e) if e.contains("not given")));
    // A tampered package never reaches pkexec.
    std::fs::remove_file(f.join("deny")).unwrap();
    std::fs::remove_file(f.join("log")).unwrap();
    let mut bad = all();
    bad[1].1 = b"!<arch>\ntampered".to_vec();
    let u = Updater::for_test(serve(bad), exe.clone());
    assert!(matches!(run(Some(&f), &u), State::Failed(e) if e.contains("checksum")));
    assert!(!f.join("log").exists(), "pkexec not asked");
    // A program alone that's broken stops the update before the package is fetched.
    let mut bad = all();
    bad[0].1 = b"\x7fELF broken".to_vec();
    let u = Updater::for_test(serve(bad), exe.clone());
    assert!(matches!(run(Some(&f), &u), State::Failed(e) if e.contains("checksum")));
    assert!(!f.join("log").exists());
    // An .rpm install: rpm -U.
    let f = fakes("rpm-tools", "rpm");
    let u = Updater::for_test(serve(all()), exe.clone());
    assert_eq!(run(Some(&f), &u), State::Ready("v9.9.9".into()));
    assert!(std::fs::read_to_string(f.join("log")).unwrap().contains(&format!("{} -U ", f.join("rpm").display())));
    assert_eq!(std::fs::read(f.join("installed")).unwrap(), rpm_bytes);
    // A pacman install: pacman -U, answering its question for it (no terminal to ask in).
    let f = fakes("pacman-tools", "pacman");
    let u = Updater::for_test(serve(all()), exe.clone());
    assert_eq!(run(Some(&f), &u), State::Ready("v9.9.9".into()));
    assert!(std::fs::read_to_string(f.join("log")).unwrap().contains(&format!("{} -U --noconfirm ", f.join("pacman").display())));
    assert_eq!(std::fs::read(f.join("installed")).unwrap(), pac_bytes);
    // No manager owns it: nothing to do but say so.
    let u = Updater::for_test(serve(all()), exe.clone());
    assert_eq!(run(None, &u), State::CantWrite(bin.clone()));
    // An AppImage: its own file replaced, the program inside it untouched.
    let apps = d.join("Applications");
    std::fs::create_dir_all(&apps).unwrap();
    let file = apps.join("Maia.AppImage");
    std::fs::write(&file, b"\x7fELF old AppImage").unwrap();
    let u = Updater::for_test(serve(all()), exe.clone()).with_appimage(file.clone());
    assert_eq!(run(None, &u), State::Ready("v9.9.9".into()));
    assert_eq!(std::fs::read(&file).unwrap(), app_bytes);
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF old");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn io_errors_read_without_their_number() {
    let e = std::fs::read_dir("/proc/self/nonexistent-dir").unwrap_err().to_string();
    assert!(e.ends_with("(os error 2)"), "std still writes the number: {e}");
    assert_eq!(crate::fsx::plain_error(&e), "No such file or directory");
    assert_eq!(crate::fsx::plain_error("Permission denied (os error 13)"), "Permission denied");
    // Only a trailing number in that form goes.
    assert_eq!(crate::fsx::plain_error("Wrong password"), "Wrong password");
    assert_eq!(crate::fsx::plain_error("odd (os error x)"), "odd (os error x)");
    assert_eq!(crate::fsx::plain_error("(os error 2) first"), "(os error 2) first");
}

/// Contrast ratio of two colours (WCAG relative luminance).
fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
    let lum = |c: egui::Color32| {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[test]
fn selected_rows_read_in_every_theme() {
    for t in crate::theme::ThemeName::ALL {
        let p = crate::theme::palette(t);
        let (on, dim) = (p.on_sel(), p.on_sel_dim());
        assert!(contrast(on, p.selected) >= 4.5, "{}: text on the selection {:.2}", t.label(), contrast(on, p.selected));
        assert!(contrast(dim, p.selected) >= 3.0, "{}: secondary text on the selection {:.2}", t.label(), contrast(dim, p.selected));
        // The selected text stays apart from its secondary text.
        assert!(contrast(on, p.selected) > contrast(dim, p.selected));
    }
}

#[test]
fn time_left_reads_as_minutes_and_hours() {
    use crate::ui::time_left;
    assert_eq!(time_left(0.4), "0s");
    assert_eq!(time_left(42.0), "42s");
    assert_eq!(time_left(61.0), "2 min");
    assert_eq!(time_left(3599.0), "60 min");
    assert_eq!(time_left(3725.0), "1 h 2 min");
    assert_eq!(time_left(-3.0), "0s");
}

#[test]
fn an_svg_says_its_own_size() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="48" viewBox="0 0 64 48"><rect width="64" height="48"/></svg>"#;
    assert_eq!(crate::svg::size(svg), Some([64, 48]));
    let vb = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 150"/>"#;
    assert_eq!(crate::svg::size(vb), Some([300, 150]), "the viewBox when there is no size");
    let (w, h, _) = crate::svg::render(svg, 2048).unwrap();
    assert_eq!((w, h), (2048, 1536), "drawn larger than it says it is");
}

#[test]
fn a_broken_link_is_not_executable() {
    let d = tmpdir("brokenlink");
    std::os::unix::fs::symlink(d.join("nowhere"), d.join("gone")).unwrap();
    std::fs::write(d.join("there"), "x").unwrap();
    std::os::unix::fs::symlink(d.join("there"), d.join("good")).unwrap();
    let gone = crate::fsx::Entry::from_path(&d.join("gone")).unwrap();
    assert!(gone.is_link && gone.is_broken_link() && !gone.is_executable(), "lrwxrwxrwx is not a program");
    let good = crate::fsx::Entry::from_path(&d.join("good")).unwrap();
    assert!(good.is_link && !good.is_broken_link());
    let _ = std::fs::remove_dir_all(&d);
}

/// How long Search Code takes over a big tree (MAIA_BENCH_DIR, else /usr/include), warm,
/// for plain text, whole words and expressions that match little, so all of it is read.
#[test]
#[ignore]
fn code_search_speed() {
    use crate::codesearch::{Query, Search};
    let dir = PathBuf::from(std::env::var("MAIA_BENCH_DIR").unwrap_or_else(|_| "/usr/include".into()));
    let ctx = egui::Context::default();
    let qs = [
        ("plain", Query { text: "xyzzy_frob".into(), ..Default::default() }),
        ("plain, match case", Query { text: "XYZZY_FROB".into(), case: true, ..Default::default() }),
        ("whole word", Query { text: "frobnicate".into(), word: true, ..Default::default() }),
        ("regex literal", Query { text: "xyzzy_frob".into(), regex: true, ..Default::default() }),
        ("regex class", Query { text: r"struct\s+xyzzy_\w+".into(), regex: true, ..Default::default() }),
        ("regex alternation", Query { text: "xyzzy|plugh|frobozz".into(), regex: true, case: true, ..Default::default() }),
        ("regex with no literal", Query { text: "[A-Z][a-z]+_[0-9]{3}x".into(), regex: true, case: true, ..Default::default() }),
        ("todo", Query { todo: true, ..Default::default() }),
    ];
    for round in 0..2 {
        for (label, q) in &qs {
            let t = Instant::now();
            let s = Search::start(dir.clone(), false, q, &ctx).unwrap();
            let r = s.wait();
            if round == 1 {
                eprintln!("{label}: {:.0} ms, {} files with matches", t.elapsed().as_secs_f64() * 1000.0, r.len());
            }
        }
    }
}

// ------------------------------------------------------------------ developer tools

/// Run a search to the end and give back (rel path, line numbers) per file.
fn code_hits(dir: &Path, q: &crate::codesearch::Query) -> Vec<(String, Vec<u32>)> {
    let ctx = egui::Context::default();
    let s = crate::codesearch::Search::start(dir.to_path_buf(), false, q, &ctx).unwrap();
    s.wait().into_iter().map(|f| (f.rel, f.hits.iter().map(|h| h.line).collect())).collect()
}

fn cq(text: &str) -> crate::codesearch::Query {
    crate::codesearch::Query { text: text.into(), ..Default::default() }
}

/// Plain text ignores case unless asked, whole words need a boundary on both sides, an
/// expression finds what it describes, letters outside ASCII fold too, and a broken
/// expression says what is wrong instead of searching.
#[test]
fn code_search_modes() {
    use crate::codesearch::{Matcher, hits_in};
    let text = "let Needle = 1;\nneedles(x);\n  NEEDLE_2 // needle\nnöt here\nÄrger und ärger\n";
    let lines = |m: &Matcher| -> Vec<u32> {
        let (mut low, mut sp) = (vec![], vec![]);
        hits_in(text, m, None, 100, &mut low, &mut sp).0.iter().map(|h| h.line).collect()
    };
    assert_eq!(lines(&Matcher::new("needle", false, false, false).unwrap()), vec![1, 2, 3]);
    assert_eq!(lines(&Matcher::new("Needle", true, false, false).unwrap()), vec![1]);
    assert_eq!(lines(&Matcher::new("needle", false, true, false).unwrap()), vec![1, 3], "needles and NEEDLE_2 are other words");
    assert_eq!(lines(&Matcher::new("needle", true, true, false).unwrap()), vec![3]);
    assert_eq!(lines(&Matcher::new(r"needles?\(", false, false, true).unwrap()), vec![2]);
    assert_eq!(lines(&Matcher::new(r"^\s*NEEDLE_\d", true, false, true).unwrap()), vec![3], "^ starts each line");
    assert_eq!(lines(&Matcher::new("ärger", false, false, false).unwrap()), vec![5]);
    let (mut low, mut sp) = (vec![], vec![]);
    let (h, _) = hits_in(text, &Matcher::new("ärger", false, false, false).unwrap(), None, 100, &mut low, &mut sp);
    assert_eq!(h[0].spans.len(), 2, "both cases of Ä are found: {:?}", h[0]);
    assert_eq!(lines(&Matcher::new("nöt", false, false, false).unwrap()), vec![4]);
    // Several matches on a line are one hit with a span each, on the line as shown.
    let (h, total) = hits_in(text, &Matcher::new("needle", false, false, false).unwrap(), None, 100, &mut low, &mut sp);
    assert_eq!(total, 3);
    let third = &h[2];
    assert_eq!(third.text, "NEEDLE_2 // needle", "indentation is left off");
    assert_eq!(third.spans.iter().map(|&(a, b)| &third.text[a..b]).collect::<Vec<_>>(), vec!["NEEDLE", "needle"]);
    let err = Matcher::new("needle(", false, false, true).err().unwrap();
    assert!(!err.is_empty() && !err.contains('\n'), "{err:?}");
    assert!(Matcher::new("", false, false, false).is_err());
}

/// The literal text an expression needs, for passing over files without it: the longest
/// plain run, one per top-level branch, never a character a quantifier may leave out.
#[test]
fn code_search_expression_prefilter() {
    use crate::codesearch::required_literals as req;
    let one = |p: &str| req(p).map(|v| v.join("|"));
    assert_eq!(one("needle").as_deref(), Some("needle"));
    assert_eq!(one(r"struct\s+Foo\w*").as_deref(), Some("struct"));
    assert_eq!(one(r"fn \w+_parse\(").as_deref(), Some("_parse("));
    assert_eq!(one("colou?r").as_deref(), Some("colo"), "u? may be missing");
    assert_eq!(one("abc|defg").as_deref(), Some("abc|defg"));
    assert_eq!(one(r"(get|set)Value").as_deref(), Some("Value"), "inside a group nothing is certain");
    assert_eq!(one(r"\d{4}-\d{2}"), None, "nothing literal to look for");
    assert_eq!(one("ab|[a-z]+"), None, "a branch without a literal");
    assert_eq!(one(r"a\.b\.c").as_deref(), Some("a.b.c"));
    assert_eq!(one("x{2}yzw").as_deref(), Some("yzw"), "a counted character is left out");
    assert_eq!(one("x{2}yz"), None, "two characters are too few to look for");
    // The prefilter never changes what is found.
    let text = "Colour and color\nset_value getValue setValue\n2026-10-16\n";
    for pat in ["colou?r", r"(get|set)Value", r"\d{4}-\d{2}", "Colour|setV"] {
        let m = crate::codesearch::Matcher::new(pat, false, false, true).unwrap();
        let re = regex_lite::Regex::new(&format!("(?mi){pat}")).unwrap();
        let (mut low, mut sp) = (vec![], vec![]);
        m.find(text, &mut low, &mut sp);
        let want: Vec<(usize, usize)> = re.find_iter(text).map(|m| (m.start(), m.end())).collect();
        assert_eq!(sp, want, "{pat}");
    }
}

/// gitignore rules as git reads them: names anywhere, slashes anchor, a trailing slash
/// means folders only, `**` crosses folders, `!` takes a file back, the last rule wins.
#[test]
fn gitignore_rules() {
    use crate::codesearch::{decide, parse_gitignore, wildmatch};
    assert!(wildmatch(b"*.rs", b"main.rs") && !wildmatch(b"*.rs", b"src/main.rs"));
    assert!(wildmatch(b"src/**/*.rs", b"src/a/b/main.rs") && wildmatch(b"src/**/*.rs", b"src/main.rs"));
    assert!(wildmatch(b"**/build", b"x/y/build") && wildmatch(b"**/build", b"build"));
    assert!(wildmatch(b"docs/**", b"docs/a/b") && !wildmatch(b"docs/**", b"docsx/a"));
    assert!(wildmatch(b"f[a-c]?.txt", b"fbz.txt") && !wildmatch(b"f[!a-c]?.txt", b"fbz.txt"));
    assert!(wildmatch(b"\\*lit", b"*lit") && !wildmatch(b"\\*lit", b"xlit"));
    let rules = parse_gitignore("# comment\n\ntarget/\n*.log\n!keep.log\n/build\ndocs/**/*.tmp\n\\#hash\ntrailing   \n**/node_modules\nsrc/gen/\n");
    let ig = |p: &str, dir: bool| decide(&rules, p, dir);
    assert_eq!(ig("target", true), Some(true));
    assert_eq!(ig("a/b/target", true), Some(true), "a name without a slash matches at any depth");
    assert_eq!(ig("target", false), None, "target/ is for folders only");
    assert_eq!(ig("x/debug.log", false), Some(true));
    assert_eq!(ig("x/keep.log", false), Some(false), "taken back by !");
    assert_eq!(ig("build", true), Some(true));
    assert_eq!(ig("sub/build", true), None, "/build is anchored to its folder");
    assert_eq!(ig("docs/a/b/x.tmp", false), Some(true));
    assert_eq!(ig("#hash", false), Some(true));
    assert_eq!(ig("trailing", false), Some(true), "trailing spaces are dropped");
    assert_eq!(ig("deep/er/node_modules", true), Some(true));
    assert_eq!(ig("src/gen", true), Some(true));
    assert_eq!(ig("other/src/gen", true), None, "a slash inside anchors it");
    assert_eq!(ig("main.rs", false), None);
}

/// A real project walked as Search Code walks it: its .gitignores (nested ones win, and
/// the repository's exclude file counts), hidden and binary files left out by default,
/// all of them searched with All files; the file filter by type and glob.
#[test]
fn code_search_walk_skips_ignored_hidden_and_binary() {
    use crate::codesearch::Query;
    let d = tmpdir("codewalk");
    std::fs::create_dir_all(d.join(".git/info")).unwrap();
    touch(&d.join(".git/info/exclude"), "secret.txt\n");
    touch(&d.join(".gitignore"), "target/\n*.log\n");
    touch(&d.join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n");
    touch(&d.join("src/lib.rs"), "// needle in lib\n");
    touch(&d.join("src/.gitignore"), "gen.rs\n!keep.log\n");
    touch(&d.join("src/gen.rs"), "needle generated\n");
    touch(&d.join("src/keep.log"), "needle kept\n");
    touch(&d.join("notes.log"), "needle in a log\n");
    touch(&d.join("target/debug/out.rs"), "needle built\n");
    touch(&d.join(".hidden/h.rs"), "needle hidden\n");
    touch(&d.join("secret.txt"), "needle excluded\n");
    touch(&d.join("docs/guide.md"), "the needle, in prose\n");
    std::fs::write(d.join("blob.bin"), b"\x00\x01needle\x02").unwrap();
    let files = |q: &Query| code_hits(&d, q).into_iter().map(|f| f.0).collect::<Vec<_>>();
    assert_eq!(files(&cq("needle")), vec!["docs/guide.md", "src/keep.log", "src/lib.rs", "src/main.rs"], "in path order");
    let all = Query { all: true, ..cq("needle") };
    let got = files(&all);
    for f in ["notes.log", "target/debug/out.rs", ".hidden/h.rs", "secret.txt", "src/gen.rs", "blob.bin"] {
        assert!(got.contains(&f.to_string()), "All files reaches {f}: {got:?}");
    }
    let ctx = egui::Context::default();
    let bin = crate::codesearch::Search::start(d.clone(), false, &all, &ctx).unwrap().wait().into_iter().find(|f| f.rel == "blob.bin").unwrap();
    assert!(bin.binary && bin.hits.is_empty() && bin.total == 1, "a binary file is listed without lines");
    assert_eq!(files(&Query { files: "rust".into(), ..cq("needle") }), vec!["src/lib.rs", "src/main.rs"]);
    assert_eq!(files(&Query { files: "!*.rs, !docs/".into(), ..cq("needle") }), vec!["src/keep.log"]);
    assert_eq!(files(&Query { files: "src/**".into(), ..cq("needle") }), vec!["src/keep.log", "src/lib.rs", "src/main.rs"]);
    assert_eq!(files(&Query { files: ".md".into(), ..cq("needle") }), vec!["docs/guide.md"]);
    // From a folder inside the repository the .gitignores above it still apply.
    let ctx = egui::Context::default();
    let sub: Vec<String> = crate::codesearch::Search::start(d.join("src"), false, &cq("needle"), &ctx).unwrap().wait().into_iter().map(|f| f.rel).collect();
    assert_eq!(sub, vec!["keep.log", "lib.rs", "main.rs"]);
    // And Project scope searches from the repository's top.
    let top = crate::codesearch::Search::start(d.join("src"), true, &cq("needle"), &ctx).unwrap();
    let root = loop {
        if let Some(r) = top.root() {
            break r;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(root, d);
    assert_eq!(top.wait().len(), 4);
    let _ = std::fs::remove_dir_all(&d);
}

/// Each hit carries its neighbours, without indentation; a long line is cut around the
/// match with the spans moved to match; Windows line ends are dropped.
#[test]
fn code_search_context_lines() {
    use crate::codesearch::{Matcher, hits_in};
    let long = format!("{}needle{}", "x".repeat(300), "y".repeat(300));
    let text = format!("first\r\n\tbefore it\r\n    the needle here\r\n\r\nend\n{long}\nlast\n");
    let (mut low, mut sp) = (vec![], vec![]);
    let (h, _) = hits_in(&text, &Matcher::new("needle", false, false, false).unwrap(), None, 100, &mut low, &mut sp);
    assert_eq!(h.len(), 2);
    assert_eq!((h[0].line, h[0].text.as_str(), h[0].before.as_deref(), h[0].after.as_deref()), (3, "the needle here", Some("before it"), None), "an empty neighbour is not shown");
    assert_eq!(&h[0].text[h[0].spans[0].0..h[0].spans[0].1], "needle");
    let l = &h[1];
    assert_eq!(l.line, 6);
    assert!(l.cut.0 && l.cut.1 && l.text.len() <= 240, "cut on both sides: {} bytes", l.text.len());
    assert_eq!(&l.text[l.spans[0].0..l.spans[0].1], "needle", "the span follows the cut");
    assert_eq!((l.before.as_deref(), l.after.as_deref()), (Some("end"), Some("last")));
    // The first line has nothing before it.
    let (h, _) = hits_in("needle\nnext\n", &Matcher::new("needle", false, false, false).unwrap(), None, 100, &mut low, &mut sp);
    assert_eq!((h[0].before.as_deref(), h[0].after.as_deref()), (None, Some("next")));
}

/// A search stops at MAX_MATCHES lines and says so; a file shows PER_FILE lines and
/// counts the rest.
#[test]
fn code_search_result_cap() {
    use crate::codesearch::{MAX_MATCHES, PER_FILE, Search};
    let d = tmpdir("codecap");
    touch(&d.join("big.txt"), &"needle\n".repeat(PER_FILE + 50));
    let ctx = egui::Context::default();
    let one = Search::start(d.clone(), false, &cq("needle"), &ctx).unwrap().wait();
    assert_eq!((one[0].hits.len(), one[0].total), (PER_FILE, PER_FILE + 50));
    for i in 0..(MAX_MATCHES / 40 + 10) {
        touch(&d.join(format!("many/f{i:04}.txt")), &"a needle\n".repeat(40));
    }
    let s = Search::start(d.clone(), false, &cq("needle"), &ctx).unwrap();
    while !s.done() {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(s.capped(), "the cap was reached");
    let shown: usize = s.take().iter().map(|f| f.hits.len()).sum();
    assert!(shown <= MAX_MATCHES && shown > MAX_MATCHES - 200, "{shown} lines kept");
    let _ = std::fs::remove_dir_all(&d);
}

/// TODO, FIXME, HACK and XXX in capitals and as words, whatever the comment style; an
/// owner in brackets and the colon are not part of what it says. The TODO finder counts
/// each and narrows by the query.
#[test]
fn todo_parsing_and_finder() {
    use crate::codesearch::{Query, todo_tag};
    assert_eq!(todo_tag("// TODO: fix the cap"), Some((0, "fix the cap")));
    assert_eq!(todo_tag("# FIXME(ana) - wrong on Mondays"), Some((1, "wrong on Mondays")));
    assert_eq!(todo_tag("/* HACK: until 2.0 */"), Some((2, "until 2.0")));
    assert_eq!(todo_tag("<!-- XXX remove -->"), Some((3, "remove")));
    assert_eq!(todo_tag("let x = 1; // FIXME TODO both"), Some((1, "TODO both")), "the first tag on the line");
    assert_eq!(todo_tag("TODOS and xTODO and todo"), None, "only whole words in capitals");
    let d = tmpdir("todos");
    touch(&d.join("a.rs"), "// TODO: one\nfn x() {} // FIXME: two\n// todo: not this\n// TODOS: nor this\n");
    touch(&d.join("b.py"), "# HACK: three\n# XXX four about the cache\n# TODO cache too\n");
    let ctx = egui::Context::default();
    let got = crate::codesearch::Search::start(d.clone(), false, &Query { todo: true, ..Default::default() }, &ctx).unwrap().wait();
    let lines: Vec<(String, Vec<u32>)> = got.iter().map(|f| (f.rel.clone(), f.hits.iter().map(|h| h.line).collect())).collect();
    assert_eq!(lines, vec![("a.rs".to_string(), vec![1, 2]), ("b.py".to_string(), vec![1, 2, 3])]);
    let tags = got.iter().fold([0u32; 4], |mut a, f| {
        a.iter_mut().zip(f.tags).for_each(|(x, y)| *x += y);
        a
    });
    assert_eq!(tags, [2, 1, 1, 1]);
    let narrowed = code_hits(&d, &Query { todo: true, text: "cache".into(), ..Default::default() });
    assert_eq!(narrowed, vec![("b.py".to_string(), vec![2, 3])]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Every editor gets the line its own way; Auto takes $VISUAL, then $EDITOR, then the
/// first installed; a command with {file} and {line} is filled in; terminal editors are
/// marked to run in a terminal.
#[test]
fn editor_command_lines() {
    use crate::editor::{command, resolve};
    let none = |_: &str| false;
    let line = |setting: &str| {
        let c = resolve(setting, None, None, &none).unwrap();
        (command(&c, "/p/main.rs", Some(42)).join(" "), c.terminal)
    };
    for (ed, want, term) in [
        ("code", "code -g /p/main.rs:42", false),
        ("codium", "codium -g /p/main.rs:42", false),
        ("cursor", "cursor -g /p/main.rs:42", false),
        ("zed", "zed /p/main.rs:42", false),
        ("zeditor", "zeditor /p/main.rs:42", false),
        ("subl", "subl /p/main.rs:42", false),
        ("idea", "idea --line 42 /p/main.rs", false),
        ("clion", "clion --line 42 /p/main.rs", false),
        ("kate", "kate --line 42 /p/main.rs", false),
        ("gedit", "gedit +42 /p/main.rs", false),
        ("emacs", "emacs +42 /p/main.rs", false),
        ("nvim", "nvim +42 /p/main.rs", true),
        ("vim", "vim +42 /p/main.rs", true),
        ("hx", "hx /p/main.rs:42", true),
        ("helix", "helix /p/main.rs:42", true),
        ("nano", "nano +42 /p/main.rs", true),
        ("micro", "micro +42 /p/main.rs", true),
        ("kak", "kak +42 /p/main.rs", true),
        ("/opt/bin/code --new-window", "/opt/bin/code --new-window -g /p/main.rs:42", false),
        ("flatpak run org.x.Ed --line={line} {file}", "flatpak run org.x.Ed --line=42 /p/main.rs", false),
        ("myedit", "myedit /p/main.rs", false),
    ] {
        assert_eq!(line(ed), (want.to_string(), term), "{ed}");
    }
    let c = resolve("code", None, None, &none).unwrap();
    assert_eq!(command(&c, "/p/dir", None), vec!["code", "/p/dir"], "without a line, just the path");
    // Auto: the environment first, an unknown $EDITOR in a terminal; then what is installed,
    // windowed editors before terminal ones.
    let has = |b: &str| matches!(b, "nvim" | "zeditor" | "micro");
    assert_eq!(resolve("", Some("hx"), Some("nano"), &has).unwrap().label, "Helix");
    assert_eq!(resolve("", None, Some("nano"), &has).unwrap().label, "nano");
    let odd = resolve("", None, Some("ed -p x"), &has).unwrap();
    assert!(odd.terminal && command(&odd, "/f", Some(3)) == vec!["ed", "-p", "x", "/f"]);
    let auto = resolve("", None, None, &has).unwrap();
    assert_eq!((auto.label.as_str(), command(&auto, "/f", Some(3)).join(" ")), ("Zed", "zeditor /f:3".to_string()));
    assert_eq!(resolve("", None, None, &|b: &str| b == "nvim").unwrap().label, "Neovim");
    assert!(resolve("", None, None, &none).is_none());
    assert!(resolve("  ", None, None, &none).is_none(), "spaces are empty");
}

/// Tasks from real manifests: scripts in their order with the package manager the
/// lockfile names, Cargo's commands with each binary and example, Makefile targets and
/// justfile recipes without the private and special ones, pyproject's script tables,
/// deno tasks under comments, composer scripts, Taskfile tasks and Go's commands; and the
/// folders up to the project's top.
#[test]
fn task_detection_from_manifests() {
    use crate::tasks::*;
    let d = tmpdir("tasks");
    let names = |g: &Group| g.tasks.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    touch(&d.join("web/package.json"), r#"{"name":"w","scripts":{"dev":"vite","build":"vite build","test":"vitest run"}}"#);
    touch(&d.join("web/pnpm-lock.yaml"), "");
    let g = detect_dir(&d.join("web"));
    assert_eq!((g[0].source.as_str(), names(&g[0])), ("package.json · pnpm", vec!["dev".to_string(), "build".into(), "test".into()]), "the manifest's order");
    assert_eq!(g[0].tasks[1].argv, vec!["pnpm", "run", "build"]);
    assert_eq!(g[0].tasks[1].detail, "vite build");
    for (lock, pm) in [("yarn.lock", "yarn"), ("bun.lockb", "bun"), ("package-lock.json", "npm")] {
        let e = tmpdir(&format!("pm-{pm}"));
        touch(&e.join(lock), "");
        assert_eq!(package_manager(&e, "{}"), pm);
        let _ = std::fs::remove_dir_all(&e);
    }
    assert_eq!(package_manager(&d, r#"{"packageManager":"yarn@4.1.0"}"#), "yarn", "packageManager wins");
    assert_eq!(package_manager(&d, "{}"), "npm");
    touch(&d.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"helper\"\npath = \"src/helper.rs\"\n\n[dependencies]\nserde = { version = \"1\", features = [\n  \"derive\",\n] }\n");
    touch(&d.join("src/main.rs"), "fn main() {}");
    touch(&d.join("src/bin/extra.rs"), "fn main() {}");
    touch(&d.join("examples/demo.rs"), "fn main() {}");
    touch(&d.join("examples/multi/main.rs"), "fn main() {}");
    let cargo = cargo_tasks(&d, &std::fs::read_to_string(d.join("Cargo.toml")).unwrap());
    let cn: Vec<&str> = cargo.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(cn, vec!["build", "run app", "run helper", "run extra", "check", "test", "example demo", "example multi"]);
    assert_eq!(cargo[2].argv, vec!["cargo", "run", "--bin", "helper"]);
    assert_eq!(cargo[6].argv, vec!["cargo", "run", "--example", "demo"]);
    let single = tmpdir("cargo1");
    touch(&single.join("src/main.rs"), "");
    let one = cargo_tasks(&single, "[package]\nname = \"x\"\n");
    assert_eq!(one.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["build", "run", "check", "test"]);
    assert_eq!(cargo_tasks(&single, "[workspace]\nmembers = [\"a\", \"b\"]\n").len(), 3, "a workspace builds, checks and tests");
    assert!(cargo_tasks(&single, "[dependencies]\n").is_empty());
    let _ = std::fs::remove_dir_all(&single);
    let mk = make_targets("CC := gcc\nVAR = a:b\n.PHONY: all clean\n# Build it all\nall: app lib\n\tgcc x\napp lib: main.o\n%.o: %.c\n\t$(CC) -c $<\nclean: ## remove output\n\trm -f x\ninstall::\n$(OUT): x\n");
    assert_eq!(mk.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["all", "app", "lib", "clean", "install"]);
    assert_eq!((mk[0].detail.as_str(), mk[3].detail.as_str()), ("Build it all", "remove output"));
    let just = just_recipes("set shell := [\"bash\", \"-c\"]\nalias b := build\nversion := \"1\"\n\n# Build everything\nbuild:\n    cargo build\n\ntest filter='':\n    cargo test {{filter}}\n\n_helper:\n    echo\n\n[private]\nhidden:\n    echo\n\n@deploy env +args: build\n    echo {{env}}\n");
    assert_eq!(just.iter().map(|t| (t.name.as_str(), t.detail.as_str())).collect::<Vec<_>>(), vec![("build", "Build everything"), ("test", "filter=''"), ("deploy", "env +args")]);
    assert_eq!(just[0].argv, vec!["just", "build"]);
    touch(&d.join("py/pyproject.toml"), "[project]\nname = \"p\"\n[project.scripts]\nserve = \"p.app:main\"\n\n[tool.poe.tasks]\nlint = \"ruff check .\"\nfmt = { cmd = \"ruff format\" }\n");
    touch(&d.join("py/uv.lock"), "");
    let py = pyproject_scripts(&d.join("py"), &std::fs::read_to_string(d.join("py/pyproject.toml")).unwrap());
    assert_eq!(py.len(), 2);
    assert_eq!((py[0].0.as_str(), py[0].1[0].argv.clone()), ("pyproject.toml · uv", vec!["uv".to_string(), "run".into(), "serve".into()]));
    assert_eq!(py[1].1.iter().map(|t| t.argv.join(" ")).collect::<Vec<_>>(), vec!["poe lint", "poe fmt"]);
    let deno = deno_tasks("{\n  // dev server\n  \"tasks\": { \"start\": \"deno run -A main.ts\", /* x */ \"url\": \"echo https://x.y\" }\n}");
    assert_eq!(deno.iter().map(|t| (t.name.as_str(), t.detail.as_str())).collect::<Vec<_>>(), vec![("start", "deno run -A main.ts"), ("url", "echo https://x.y")]);
    let tf = taskfile_tasks("version: '3'\ntasks:\n  build:\n    desc: Build the app\n    cmds:\n      - go build\n  \"lint\":\n    cmds: [golangci-lint run]\nvars:\n  X: y\n");
    assert_eq!(tf.iter().map(|t| (t.name.as_str(), t.detail.as_str())).collect::<Vec<_>>(), vec![("build", "Build the app"), ("lint", "")]);
    touch(&d.join("go/go.mod"), "module x\n");
    touch(&d.join("go/main.go"), "package main\n");
    touch(&d.join("go/composer.json"), r#"{"scripts":{"test":"phpunit","cs":["phpcs","phpstan"]}}"#);
    let go = detect_dir(&d.join("go"));
    assert_eq!(go.iter().map(|g| g.source.as_str()).collect::<Vec<_>>(), vec!["composer.json", "go.mod"]);
    assert_eq!(go[0].tasks[1].detail, "phpcs && phpstan");
    assert_eq!(go[1].tasks.iter().map(|t| t.argv.join(" ")).collect::<Vec<_>>(), vec!["go build ./...", "go run .", "go test ./...", "go vet ./..."]);
    // From web/src: web's scripts first, then the project's top (a repository).
    std::fs::create_dir_all(d.join(".git")).unwrap();
    std::fs::create_dir_all(d.join("web/src")).unwrap();
    let all = detect(&d.join("web/src"));
    assert_eq!(all.iter().map(|g| (g.dir.clone(), g.source.clone())).collect::<Vec<_>>(), vec![(d.join("web"), "package.json · pnpm".to_string()), (d.clone(), "Cargo.toml".to_string())]);
    let _ = std::fs::remove_dir_all(&d);
}

/// A task runs held open in the terminal: its exit status, then the user's shell.
#[test]
fn tasks_keep_their_terminal_open() {
    let v = crate::mime::held(&["npm".into(), "run".into(), "a b".into()]);
    assert_eq!(&v[..2], &["sh", "-c"]);
    assert_eq!(&v[3..], &["sh", "npm", "run", "a b"], "the task's words are passed whole, never through the shell");
    assert!(v[2].starts_with("\"$@\"") && v[2].contains("exec \"${SHELL:-/bin/sh}\""));
    let out = std::process::Command::new(&v[0]).args(&v[1..3]).arg("sh").args(["printf", "%s|", "a b"]).env("SHELL", "true").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b|\n[exit 0]\n");
}

/// Build folders are what a tool makes beside its manifest: target with a Cargo.toml (not
/// without), node_modules once (not the ones inside it), dist and build beside a
/// package.json or pyproject, environments by their pyvenv.cfg, caches by CACHEDIR.TAG;
/// sizes are added up after.
#[test]
fn build_folder_detection() {
    use crate::buildclean::{Msg, Scan};
    let d = tmpdir("builds");
    touch(&d.join("rust/Cargo.toml"), "[package]\n");
    touch(&d.join("rust/target/debug/app"), &"x".repeat(5000));
    touch(&d.join("rust/src/target/keep.rs"), "a source folder called target");
    touch(&d.join("web/package.json"), "{}");
    touch(&d.join("web/node_modules/a/index.js"), &"y".repeat(3000));
    touch(&d.join("web/node_modules/a/node_modules/b/i.js"), "z");
    touch(&d.join("web/dist/app.js"), "1");
    touch(&d.join("web/.next/cache/x"), "2");
    touch(&d.join("plain/dist/readme.txt"), "dist without a manifest is someone's");
    touch(&d.join("plain/build/notes.txt"), "same");
    touch(&d.join("py/pyproject.toml"), "");
    touch(&d.join("py/.venv/pyvenv.cfg"), "home = /usr/bin\n");
    touch(&d.join("py/.venv/lib/site.py"), "s");
    touch(&d.join("py/env/pyvenv.cfg"), "");
    touch(&d.join("py/pkg/__pycache__/m.cpython-313.pyc"), "c");
    touch(&d.join("py/.pytest_cache/v"), "p");
    touch(&d.join("misc/cache/CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55\n");
    touch(&d.join(".hidden/node_modules/x.js"), "hidden folders are not walked");
    let ctx = egui::Context::default();
    let s = Scan::start(d.clone(), &ctx);
    let (mut found, mut sizes) = (vec![], std::collections::HashMap::new());
    let t = Instant::now();
    loop {
        match s.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Msg::Found(f)) => found.push(f),
            Ok(Msg::Size(p, n)) => {
                sizes.insert(p, n);
            }
            Ok(Msg::Done) | Err(_) => break,
        }
    }
    assert!(t.elapsed() < Duration::from_secs(5));
    let mut got: Vec<(String, &str)> = found.iter().map(|f| (crate::project::relative(&f.path, &d), f.kind)).collect();
    got.sort();
    assert_eq!(got, vec![
        ("misc/cache".to_string(), "Cache"),
        ("py/.pytest_cache".to_string(), "Python tool cache"),
        ("py/.venv".to_string(), "Python environment"),
        ("py/env".to_string(), "Python environment"),
        ("py/pkg/__pycache__".to_string(), "Python bytecode"),
        ("rust/target".to_string(), "Rust build"),
        ("web/.next".to_string(), "Next.js build"),
        ("web/dist".to_string(), "Build output"),
        ("web/node_modules".to_string(), "Node packages"),
    ]);
    assert_eq!(sizes.get(&d.join("rust/target")), Some(&5000));
    assert_eq!(sizes.get(&d.join("web/node_modules")), Some(&3001), "everything inside, nested packages too");
    assert_eq!(sizes.len(), found.len(), "every folder is sized");
    assert!(found.iter().all(|f| f.modified.is_some()));
    let _ = std::fs::remove_dir_all(&d);
}

/// Paths written from the project's top: its repository, else its manifest, else the
/// folder itself; never the home folder or / for a folder inside them.
#[test]
fn path_variants_from_the_project_root() {
    use crate::project::{git_root, relative, root, with_line};
    let d = tmpdir("paths");
    std::fs::create_dir_all(d.join("repo/.git")).unwrap();
    touch(&d.join("repo/crates/a/Cargo.toml"), "");
    touch(&d.join("repo/crates/a/src/lib.rs"), "");
    touch(&d.join("loose/pkg/package.json"), "{}");
    std::fs::create_dir_all(d.join("loose/pkg/src")).unwrap();
    std::fs::create_dir_all(d.join("nothing/here")).unwrap();
    assert_eq!(root(&d.join("repo/crates/a/src")), d.join("repo"), "the repository beats the nearer manifest");
    assert_eq!(root(&d.join("loose/pkg/src")), d.join("loose/pkg"));
    assert_eq!(root(&d.join("nothing/here")), d.join("nothing/here"));
    assert_eq!(git_root(&d.join("nothing/here")), None);
    let f = d.join("repo/crates/a/src/lib.rs");
    assert_eq!(relative(&f, &root(&d.join("repo/crates/a/src"))), "crates/a/src/lib.rs");
    assert_eq!(with_line(&relative(&f, &d.join("repo")), 12), "crates/a/src/lib.rs:12");
    assert_eq!(relative(&f, &d.join("elsewhere")), f.to_string_lossy(), "outside the root: the whole path");
    assert_eq!(relative(&d, &d), d.to_string_lossy());
    let _ = std::fs::remove_dir_all(&d);
}

/// A repository for changing things in: its own identity, no signing, and hooks only
/// from its own `.git/hooks` (the machine's git config must not leak into a test).
pub(crate) fn work_repo(d: &Path) {
    git(d, &["init", "-q"]);
    for (k, v) in [("user.name", "Test"), ("user.email", "test@example.invalid"), ("commit.gpgsign", "false"), ("core.hooksPath", ".git/hooks")] {
        git(d, &["config", k, v]);
    }
}

fn work_of(d: &Path) -> crate::gitwork::Work {
    crate::gitwork::work(d).expect("a work tree")
}

fn letters(v: &[crate::gitlog::Change]) -> Vec<(String, char)> {
    v.iter().map(|c| (c.path.clone(), c.status as char)).collect()
}

/// Forty numbered lines: two changes far apart make two hunks.
fn numbered(change: &[(usize, &str)]) -> String {
    (1..=40).map(|i| change.iter().find(|c| c.0 == i).map_or(format!("line {i}\n"), |c| format!("{}\n", c.1))).collect()
}

#[test]
fn git_changes_stage_and_unstage_files_and_hunks() {
    use crate::gitwork::{Op, file_diff, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitwork");
    work_repo(&d);
    touch(&d.join("src/lib.rs"), &numbered(&[]));
    touch(&d.join("old.txt"), "old\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    touch(&d.join("src/lib.rs"), &numbered(&[(3, "line three"), (36, "line thirty-six")]));
    std::fs::remove_file(d.join("old.txt")).unwrap();
    touch(&d.join("notes.md"), "one\ntwo\nthree");
    let w = work_of(&d);
    assert!(w.staged.is_empty() && w.head.is_some());
    assert_eq!(letters(&w.unstaged), [("old.txt".into(), 'D'), ("src/lib.rs".into(), 'M'), ("notes.md".into(), '?')]);
    let lib = w.unstaged[1].clone();
    assert_eq!(lib.lines, Some((2, 2)), "the counts");
    assert_eq!(w.unstaged[2].lines, Some((3, 0)), "an untracked file counts its lines");
    // Two hunks, each applying alone: stage the second only.
    let diff = file_diff(&d, &lib, false, "HEAD");
    assert_eq!(diff.hunks.len(), 2, "two hunks");
    assert!(diff.hunks[1].lines.contains(&"+line thirty-six".to_string()) && diff.hunks[1].head.starts_with("@@ -33,"));
    let patch = diff.hunks[1].patch().expect("applies alone").to_vec();
    run(&d, &Op::Hunk { patch, staged: false }).unwrap();
    let w = work_of(&d);
    assert_eq!(letters(&w.staged), [("src/lib.rs".into(), 'M')]);
    assert_eq!(w.staged[0].lines, Some((1, 1)), "one hunk staged");
    let staged = file_diff(&d, &w.staged[0], true, "HEAD");
    assert_eq!(staged.hunks.len(), 1);
    assert!(staged.hunks[0].lines.contains(&"+line thirty-six".to_string()));
    let left = file_diff(&d, &lib, false, "HEAD");
    assert_eq!(left.hunks.len(), 1, "the other hunk is still unstaged");
    assert!(left.hunks[0].lines.contains(&"+line three".to_string()));
    // And out again.
    let patch = staged.hunks[0].patch().unwrap().to_vec();
    run(&d, &Op::Hunk { patch, staged: true }).unwrap();
    assert!(work_of(&d).staged.is_empty(), "the hunk unstaged");
    assert_eq!(std::fs::read_to_string(d.join("src/lib.rs")).unwrap(), numbered(&[(3, "line three"), (36, "line thirty-six")]), "the file itself untouched");
    // Whole files: a deletion and an untracked file.
    run(&d, &Op::Stage(vec!["old.txt".into(), "notes.md".into()])).unwrap();
    let w = work_of(&d);
    assert_eq!(letters(&w.staged), [("notes.md".into(), 'A'), ("old.txt".into(), 'D')]);
    assert_eq!(letters(&w.unstaged), [("src/lib.rs".into(), 'M')]);
    run(&d, &Op::Unstage(vec!["notes.md".into()])).unwrap();
    assert_eq!(letters(&work_of(&d).staged), [("old.txt".into(), 'D')]);
    assert_eq!(run(&d, &Op::StageAll).unwrap(), "Staged every change");
    let w = work_of(&d);
    assert_eq!(w.staged.len(), 3);
    assert!(w.unstaged.is_empty());
    run(&d, &Op::UnstageAll).unwrap();
    let w = work_of(&d);
    assert!(w.staged.is_empty() && w.unstaged.len() == 3, "{w:?}");
    // Before the first commit there is no HEAD to unstage to.
    let e = tmpdir("gitwork-new");
    work_repo(&e);
    touch(&e.join("a.txt"), "a\n");
    let w = work_of(&e);
    assert_eq!((w.head.clone(), letters(&w.unstaged)), (None, vec![("a.txt".into(), '?')]));
    run(&e, &Op::Stage(vec!["a.txt".into()])).unwrap();
    let w = work_of(&e);
    assert_eq!(letters(&w.staged), [("a.txt".into(), 'A')]);
    assert_eq!(w.staged[0].lines, Some((1, 0)));
    let base = crate::gitwork::base(&e, None).unwrap();
    assert_eq!(file_diff(&e, &w.staged[0], true, &base).hunks[0].lines, ["+a"]);
    run(&e, &Op::Unstage(vec!["a.txt".into()])).unwrap();
    run(&e, &Op::StageAll).unwrap();
    run(&e, &Op::UnstageAll).unwrap();
    assert!(work_of(&e).staged.is_empty());
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&e);
}

#[test]
fn git_discard_keeps_what_it_discards() {
    use crate::gitwork::{Op, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitdiscard");
    work_repo(&d);
    touch(&d.join("a.txt"), "one\n");
    touch(&d.join("b.txt"), "bee\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    // A staged change and an unstaged one on top: only the unstaged one goes.
    touch(&d.join("a.txt"), "two\n");
    git(&d, &["add", "a.txt"]);
    touch(&d.join("a.txt"), "three\n");
    touch(&d.join("b.txt"), "buzz\n");
    let a = work_of(&d).unstaged.into_iter().find(|c| c.path == "a.txt").unwrap();
    let said = run(&d, &Op::Discard(a)).unwrap();
    assert!(said.contains("kept in the stash"), "{said}");
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "two\n", "back to the staged version");
    assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "buzz\n", "another file is left alone");
    let w = work_of(&d);
    assert_eq!(letters(&w.staged), [("a.txt".into(), 'M')], "the staged change stays");
    let refs = crate::gitwork::refs(&d);
    assert_eq!(refs.stashes.len(), 1);
    assert!(refs.stashes[0].subject.ends_with("Discarded in Maia: a.txt"), "{:?}", refs.stashes[0]);
    let out = std::process::Command::new("git").arg("-C").arg(&d).args(["show", &format!("{}:a.txt", refs.stashes[0].hash)]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "three\n", "the discarded text is in the stash");
    // An untracked file goes to the trash, from where it comes back (only when the
    // trash used is not the home folder's).
    use std::os::unix::fs::MetadataExt;
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    if dev(&d) == dirs::data_dir().and_then(|h| dev(&h)) {
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    touch(&d.join("scratch.txt"), "draft\n");
    let s = work_of(&d).unstaged.into_iter().find(|c| c.status == b'?').unwrap();
    assert!(run(&d, &Op::Discard(s)).unwrap().contains("trash"));
    assert!(!d.join("scratch.txt").exists());
    let it = crate::trashcan::list().into_iter().find(|i| i.original == d.join("scratch.txt")).expect("in the trash");
    crate::trashcan::restore(&it).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("scratch.txt")).unwrap(), "draft\n");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn git_commit_amend_and_a_failing_hook() {
    use crate::gitwork::{Op, head_message, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitcommit");
    work_repo(&d);
    touch(&d.join("a.txt"), "a\n");
    let commit = |s: &str, b: &str, amend: bool| run(&d, &Op::Commit { summary: s.into(), body: b.into(), amend });
    assert_eq!(commit("First", "", false).unwrap_err(), "Nothing is staged");
    assert_eq!(commit("  ", "", false).unwrap_err(), "Write a summary first");
    run(&d, &Op::Stage(vec!["a.txt".into()])).unwrap();
    let said = commit("Add the first file", "Why it is there.\n\nTwo paragraphs.", false).unwrap();
    assert!(said.starts_with("Committed "), "{said}");
    assert_eq!(head_message(&d).unwrap(), ("Add the first file".to_string(), "Why it is there.\n\nTwo paragraphs.".to_string()));
    assert!(work_of(&d).is_empty());
    // Amending changes the message (and takes what is staged), not the count.
    touch(&d.join("b.txt"), "b\n");
    run(&d, &Op::Stage(vec!["b.txt".into()])).unwrap();
    assert!(commit("Add two files", "", true).unwrap().starts_with("Amended "));
    let count = std::process::Command::new("git").arg("-C").arg(&d).args(["rev-list", "--count", "HEAD"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    assert_eq!(head_message(&d).unwrap().0, "Add two files");
    // A hook that refuses: its words come back, and nothing is committed.
    let hook = d.join(".git/hooks/pre-commit");
    touch(&hook, "#!/bin/sh\necho 'lint: trailing space in c.txt' >&2\nexit 1\n");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    touch(&d.join("c.txt"), "c \n");
    run(&d, &Op::Stage(vec!["c.txt".into()])).unwrap();
    let e = commit("Add c", "", false).unwrap_err();
    assert!(e.contains("lint: trailing space in c.txt"), "{e}");
    assert_eq!(letters(&work_of(&d).staged), [("c.txt".into(), 'A')], "still staged");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn git_branches_switch_create_track_and_stash() {
    use crate::gitwork::{Op, refs, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitbranches");
    let server = d.join("server.git");
    std::fs::create_dir_all(&server).unwrap();
    git(&server, &["init", "-q", "--bare"]);
    let ours = d.join("ours");
    std::fs::create_dir_all(&ours).unwrap();
    work_repo(&ours);
    touch(&ours.join("a.txt"), "a\n");
    git(&ours, &["add", "-A"]);
    git(&ours, &["commit", "-qm", "Start"]);
    git(&ours, &["remote", "add", "origin", server.to_str().unwrap()]);
    git(&ours, &["push", "-q", "-u", "origin", "main"]);
    git(&ours, &["branch", "topic"]);
    git(&ours, &["push", "-q", "origin", "topic"]);
    git(&ours, &["branch", "-q", "-D", "topic"]);
    touch(&ours.join("a.txt"), "a2\n");
    git(&ours, &["commit", "-qam", "Ahead"]);
    let r = refs(&ours);
    assert_eq!(r.local.len(), 1);
    assert!(r.local[0].current && r.local[0].name == "main" && r.local[0].upstream == "origin/main" && (r.local[0].ahead, r.local[0].behind) == (1, 0), "{:?}", r.local);
    assert_eq!(r.remote, ["origin/topic"], "a remote branch nothing tracks");
    // A new branch from HEAD, then back.
    assert_eq!(run(&ours, &Op::Create("docs".into())).unwrap(), "Created and switched to docs");
    assert!(run(&ours, &Op::Create("-x".into())).is_err() && run(&ours, &Op::Create("a..b".into())).is_err(), "not branch names");
    let r = refs(&ours);
    assert!(r.local.iter().any(|b| b.name == "docs" && b.current));
    touch(&ours.join("a.txt"), "on docs\n");
    git(&ours, &["commit", "-qam", "Docs"]);
    // Switching would overwrite a local change: git refuses, and says why.
    touch(&ours.join("a.txt"), "local\n");
    let e = run(&ours, &Op::Switch("main".into())).unwrap_err();
    assert!(e.contains("would be overwritten") && e.contains("a.txt"), "{e}");
    assert!(!e.starts_with("error:"));
    assert_eq!(std::fs::read_to_string(ours.join("a.txt")).unwrap(), "local\n", "nothing lost");
    // Stash it, switch, come back, pop it.
    git(&ours, &["stash", "push", "-q", "-m", "Local tweak"]);
    let s = refs(&ours).stashes;
    assert_eq!((s.len(), s[0].sel.as_str()), (1, "stash@{0}"));
    assert!(s[0].subject.contains("Local tweak"));
    run(&ours, &Op::Switch("main".into())).unwrap();
    assert_eq!(std::fs::read_to_string(ours.join("a.txt")).unwrap(), "a2\n");
    run(&ours, &Op::Switch("docs".into())).unwrap();
    // A selector that moved since it was read is refused.
    let mut stale = s[0].clone();
    stale.hash = "0".repeat(40);
    assert!(run(&ours, &Op::StashDrop(stale)).unwrap_err().contains("changed"));
    run(&ours, &Op::StashApply(s[0].clone())).unwrap();
    assert_eq!(std::fs::read_to_string(ours.join("a.txt")).unwrap(), "local\n");
    assert_eq!(refs(&ours).stashes.len(), 1, "apply keeps it");
    git(&ours, &["checkout", "-q", "--", "a.txt"]);
    run(&ours, &Op::StashPop(s[0].clone())).unwrap();
    assert!(refs(&ours).stashes.is_empty(), "pop drops it");
    git(&ours, &["stash", "push", "-q"]);
    let s = refs(&ours).stashes;
    run(&ours, &Op::StashDrop(s[0].clone())).unwrap();
    assert!(refs(&ours).stashes.is_empty());
    // A remote branch checked out tracks it.
    assert_eq!(run(&ours, &Op::Track("origin/topic".into())).unwrap(), "Switched to topic");
    let r = refs(&ours);
    assert!(r.local.iter().any(|b| b.name == "topic" && b.current && b.upstream == "origin/topic"));
    assert!(r.remote.is_empty());
    // Another work tree of the repository is listed with its branch.
    git(&ours, &["worktree", "add", "-q", d.join("wt").to_str().unwrap(), "docs"]);
    let r = refs(&ours);
    assert_eq!(r.worktrees, [crate::gitwork::Worktree { path: d.join("wt"), branch: "docs".into() }]);
    assert_eq!(refs(&d.join("wt")).worktrees.iter().map(|w| w.path.clone()).collect::<Vec<_>>(), [ours.clone()]);
    let _ = std::fs::remove_dir_all(&d);
}

/// What git prints for `args` in `d`, trimmed.
fn git_said(d: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(d).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The commit menu's changes that need no other side: a commit checked out on its own
/// (refused while a change would be lost), branches and tags made at a commit, renamed
/// and deleted (an unmerged branch only when forced), and the branch reset to a commit,
/// keeping the files or not, the old commit said and uncommitted work kept in the stash.
#[test]
fn git_commits_checked_out_branched_tagged_and_reset() {
    use crate::gitwork::{Op, UNMERGED, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitcommits");
    work_repo(&d);
    let mut hashes = vec![];
    for (n, text) in [("Start", "one\n"), ("Second", "two\n"), ("Third", "three\n")] {
        touch(&d.join("a.txt"), text);
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-qm", n]);
        hashes.push(git_said(&d, &["rev-parse", "HEAD"]));
    }
    let (first, second, third) = (hashes[0].clone(), hashes[1].clone(), hashes[2].clone());
    let short = |h: &str| h[..7].to_string();
    // Checking out a commit git would overwrite a change for: its words, nothing lost.
    touch(&d.join("a.txt"), "mine\n");
    let e = run(&d, &Op::Checkout(second.clone())).unwrap_err();
    assert!(e.contains("would be overwritten") && e.contains("a.txt"), "{e}");
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "mine\n");
    git(&d, &["checkout", "-q", "--", "a.txt"]);
    assert_eq!(run(&d, &Op::Checkout(second.clone())).unwrap(), format!("Checked out {}, on no branch", short(&second)));
    assert_eq!((git_said(&d, &["rev-parse", "HEAD"]), git_said(&d, &["branch", "--show-current"])), (second.clone(), String::new()));
    run(&d, &Op::Switch("main".into())).unwrap();
    // A branch at a commit, and one switched to.
    assert_eq!(run(&d, &Op::Branch { name: " old ".into(), at: first.clone(), switch: false }).unwrap(), format!("Created old at {}", short(&first)));
    assert_eq!(git_said(&d, &["rev-parse", "old"]), first);
    assert!(run(&d, &Op::Branch { name: "a..b".into(), at: first.clone(), switch: false }).unwrap_err().contains("can't be a branch name"));
    assert_eq!(run(&d, &Op::Branch { name: "fix".into(), at: second.clone(), switch: true }).unwrap(), format!("Created fix at {} and switched to it", short(&second)));
    assert_eq!(git_said(&d, &["branch", "--show-current"]), "fix");
    touch(&d.join("b.txt"), "only on fix\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Only on fix"]);
    run(&d, &Op::Switch("main".into())).unwrap();
    // Tags: a light one, and one with a message.
    assert_eq!(run(&d, &Op::Tag { name: "v1".into(), at: first.clone(), message: String::new() }).unwrap(), format!("Tagged {} as v1", short(&first)));
    run(&d, &Op::Tag { name: "v2".into(), at: third.clone(), message: "The second release\n".into() }).unwrap();
    assert_eq!((git_said(&d, &["cat-file", "-t", "v1"]), git_said(&d, &["cat-file", "-t", "v2"])), ("commit".to_string(), "tag".to_string()));
    assert_eq!(git_said(&d, &["tag", "-l", "--format=%(contents)", "v2"]), "The second release");
    assert!(run(&d, &Op::Tag { name: "v1".into(), at: third.clone(), message: String::new() }).unwrap_err().contains("already exists"));
    assert_eq!(run(&d, &Op::DeleteTag("v1".into())).unwrap(), format!("Deleted the tag v1 · it was at {}", short(&first)));
    assert_eq!(git_said(&d, &["tag"]), "v2");
    // Deleting: a merged branch goes, one with its own commits only when forced.
    assert_eq!(run(&d, &Op::DeleteBranch { name: "old".into(), force: false }).unwrap(), format!("Deleted old · it was at {}", short(&first)));
    assert_eq!(run(&d, &Op::DeleteBranch { name: "fix".into(), force: false }).unwrap_err(), UNMERGED);
    assert!(git_said(&d, &["branch"]).contains("fix"), "kept");
    let tip = git_said(&d, &["rev-parse", "--short", "fix"]);
    assert_eq!(run(&d, &Op::DeleteBranch { name: "fix".into(), force: true }).unwrap(), format!("Deleted fix · it was at {tip}"));
    assert!(run(&d, &Op::DeleteBranch { name: "main".into(), force: true }).is_err(), "not the branch HEAD is on");
    git(&d, &["branch", "topic"]);
    assert_eq!(run(&d, &Op::RenameBranch { from: "topic".into(), to: "feature".into() }).unwrap(), "Renamed topic to feature");
    assert_eq!(git_said(&d, &["branch", "--format=%(refname:short)"]), "feature\nmain");
    // Reset keeping the files: the later commits' change is left unstaged.
    assert_eq!(run(&d, &Op::Reset { to: first.clone(), hard: false }).unwrap(), format!("Reset main to {} · it was at {}", short(&first), short(&third)));
    assert_eq!(git_said(&d, &["rev-parse", "HEAD"]), first);
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "three\n");
    assert_eq!(letters(&work_of(&d).unstaged), [("a.txt".into(), 'M')]);
    // And back, discarding: the uncommitted change goes to the stash first.
    let said = run(&d, &Op::Reset { to: third.clone(), hard: true }).unwrap();
    assert_eq!(said, format!("Reset main to {} · it was at {}, your changes kept in the stash", short(&third), short(&first)));
    assert!(work_of(&d).is_empty());
    assert!(git_said(&d, &["stash", "list"]).contains("Before resetting main in Maia"));
    // With nothing uncommitted, nothing is stashed.
    assert_eq!(run(&d, &Op::Reset { to: second.clone(), hard: true }).unwrap(), format!("Reset main to {} · it was at {}", short(&second), short(&third)));
    assert_eq!(git_said(&d, &["stash", "list"]).lines().count(), 1);
    assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "two\n");
    let _ = std::fs::remove_dir_all(&d);
}

/// Cherry-picking and reverting: a clean pick, one already there (said, and nothing
/// left in progress), one that stops at a conflict and is given up on, then settled
/// and continued; a revert, and one that stops and is skipped.
#[test]
fn git_cherry_pick_and_revert_with_conflicts() {
    use crate::gitsync::{CONFLICTS, Doing, Side, doing};
    use crate::gitwork::{Op, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitpick");
    work_repo(&d);
    touch(&d.join("notes.txt"), "one\ntwo\nthree\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    let start = git_said(&d, &["rev-parse", "HEAD"]);
    git(&d, &["switch", "-q", "-c", "topic"]);
    touch(&d.join("notes.txt"), "one\nTWO from topic\nthree\n");
    git(&d, &["commit", "-qam", "Shout two"]);
    let shout = git_said(&d, &["rev-parse", "HEAD"]);
    touch(&d.join("c.txt"), "see\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Add c"]);
    let add_c = git_said(&d, &["rev-parse", "HEAD"]);
    git(&d, &["switch", "-q", "main"]);
    touch(&d.join("notes.txt"), "one\ntwo on main\nthree\n");
    git(&d, &["commit", "-qam", "Two on main"]);
    let pick = |h: &str| run(&d, &Op::CherryPick { hash: h.into(), merge: false });
    assert_eq!(pick(&add_c).unwrap(), format!("Cherry-picked {} onto main", &add_c[..7]));
    assert_eq!(std::fs::read_to_string(d.join("c.txt")).unwrap(), "see\n");
    assert_eq!(pick(&add_c).unwrap_err(), format!("main already has what {} changes.", &add_c[..7]));
    assert_eq!(doing(&d), None, "nothing left in progress");
    assert_eq!(pick(&start).unwrap_err(), format!("{} is already on main.", &start[..7]));
    // A conflict: given up on, everything as before.
    assert_eq!(pick(&shout).unwrap_err(), CONFLICTS);
    assert_eq!((doing(&d), work_of(&d).doing), (Some(Doing::CherryPick), Some(Doing::CherryPick)));
    assert_eq!(run(&d, &Op::Abort).unwrap(), "Stopped cherry-picking and went back");
    assert_eq!((doing(&d), std::fs::read_to_string(d.join("notes.txt")).unwrap()), (None, "one\ntwo on main\nthree\n".to_string()));
    // Again, settled with theirs and continued: the commit keeps its message.
    assert_eq!(pick(&shout).unwrap_err(), CONFLICTS);
    assert_eq!(run(&d, &Op::Continue).unwrap_err(), "Some files are still in conflict.");
    run(&d, &Op::Settle { path: "notes.txt".into(), at: 0, side: Side::Theirs }).unwrap();
    assert_eq!(run(&d, &Op::Continue).unwrap(), "Finished the cherry-pick");
    assert_eq!((doing(&d), git_said(&d, &["log", "-1", "--format=%s"])), (None, "Shout two".to_string()));
    assert_eq!(std::fs::read_to_string(d.join("notes.txt")).unwrap(), "one\nTWO from topic\nthree\n");
    // A revert: a new commit that undoes one.
    let picked_c = git_said(&d, &["rev-parse", "HEAD~1"]);
    assert_eq!(run(&d, &Op::Revert { hash: picked_c.clone(), merge: false }).unwrap(), format!("Reverted {}", &picked_c[..7]));
    assert!(!d.join("c.txt").exists());
    assert_eq!(git_said(&d, &["log", "-1", "--format=%s"]), "Revert \"Add c\"");
    // One that stops: the line it would take back changed since. Skipped.
    let shouted = git_said(&d, &["rev-parse", "HEAD~1"]);
    touch(&d.join("notes.txt"), "one\ntwo, quietly\nthree\n");
    git(&d, &["commit", "-qam", "Quiet two"]);
    assert_eq!(run(&d, &Op::Revert { hash: shouted, merge: false }).unwrap_err(), CONFLICTS);
    assert_eq!(doing(&d), Some(Doing::Revert));
    assert_eq!(run(&d, &Op::Skip).unwrap(), "Skipped that commit");
    assert_eq!((doing(&d), git_said(&d, &["log", "-1", "--format=%s"])), (None, "Quiet two".to_string()));
    let _ = std::fs::remove_dir_all(&d);
}

/// Lines of a hunk staged on their own, and one of them unstaged again: the index
/// gets exactly the picked lines, the file itself is left alone.
#[test]
fn git_lines_staged_and_unstaged() {
    use crate::gitwork::{Op, file_diff, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitlines");
    work_repo(&d);
    touch(&d.join("f.txt"), &numbered(&[]));
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    let changed = numbered(&[(3, "line three"), (4, "line 4\nnew A\nnew B")]);
    touch(&d.join("f.txt"), &changed);
    let f = work_of(&d).unstaged[0].clone();
    let diff = file_diff(&d, &f, false, "HEAD");
    let h = &diff.hunks[0];
    assert_eq!(h.lines[..7], [" line 1", " line 2", "-line 3", "+line three", " line 4", "+new A", "+new B"]);
    assert!(h.splits());
    let pick = |h: &crate::gitwork::Hunk, at: &[usize]| (0..h.lines.len()).map(|i| at.contains(&i)).collect::<Vec<bool>>();
    assert!(h.lines_patch(&pick(h, &[0, 1]), false).is_none(), "no change picked");
    // The removed line 3 and the first new line.
    let patch = h.lines_patch(&pick(h, &[2, 5]), false).unwrap();
    assert_eq!(run(&d, &Op::Lines { patch, staged: false, n: 2 }).unwrap(), "Staged 2 lines");
    let cached = git_said(&d, &["diff", "--cached"]);
    let hunk = |s: &str| s[s.find("@@").unwrap()..].to_string();
    assert_eq!(hunk(&cached), "@@ -1,7 +1,7 @@\n line 1\n line 2\n-line 3\n line 4\n+new A\n line 5\n line 6\n line 7");
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), changed, "the file itself untouched");
    // Back out: the new line only.
    let w = work_of(&d);
    let staged = file_diff(&d, &w.staged[0], true, "HEAD");
    let h = &staged.hunks[0];
    let at = h.lines.iter().position(|l| l == "+new A").unwrap();
    let patch = h.lines_patch(&pick(h, &[at]), true).unwrap();
    assert_eq!(run(&d, &Op::Lines { patch, staged: true, n: 1 }).unwrap(), "Unstaged 1 line");
    assert_eq!(hunk(&git_said(&d, &["diff", "--cached"])), "@@ -1,6 +1,5 @@\n line 1\n line 2\n-line 3\n line 4\n line 5\n line 6");
    assert_eq!(hunk(&git_said(&d, &["diff"])), "@@ -1,6 +1,9 @@\n line 1\n line 2\n+line three\n line 4\n+new A\n+new B\n line 5\n line 6\n line 7");
    // A file added whole goes only whole.
    touch(&d.join("new.txt"), "a\nb\n");
    run(&d, &Op::Stage(vec!["new.txt".into()])).unwrap();
    let n = work_of(&d).staged.into_iter().find(|c| c.path == "new.txt").unwrap();
    assert!(!file_diff(&d, &n, true, "HEAD").hunks[0].splits());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn git_ignored_files_are_known_and_left_out_when_hidden() {
    use crate::gitstatus::{ignored, inside_ignored, query};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("gitignored");
    work_repo(&d);
    touch(&d.join(".gitignore"), "node_modules/\n*.log\n");
    for i in 0..300 {
        touch(&d.join(format!("node_modules/p{i}/index.js")), "");
    }
    touch(&d.join("src/app.js"), "");
    touch(&d.join("src/debug.log"), "");
    touch(&d.join("README.md"), "");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    let r = query(&d).expect("a work tree");
    let mut set: Vec<&Path> = r.ignored.iter().map(Path::new).collect();
    set.sort();
    assert_eq!(set, [d.join("node_modules").as_path(), d.join("src/debug.log").as_path()], "an ignored folder is one entry");
    assert!(ignored(Some(&r), &d.join("node_modules"), &d));
    assert!(ignored(Some(&r), &d.join("node_modules/p3/index.js"), &d), "inside an ignored folder, seen from above");
    assert!(ignored(Some(&r), &d.join("src/debug.log"), &d.join("src")));
    assert!(!ignored(Some(&r), &d.join("src/app.js"), &d.join("src")));
    assert!(!ignored(Some(&r), &d.join("node_modules/p3"), &d.join("node_modules")), "inside it, nothing is singled out");
    assert!(inside_ignored(&r, &d.join("node_modules/p3")) && !inside_ignored(&r, &d.join("src")) && !inside_ignored(&r, &d));
    // Hidden: left out of the view, counted as hidden, and back when shown.
    let stats = HashMap::new();
    let mut t = Tab::new(1, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let names = |t: &Tab| t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&t), ["node_modules", "src", "README.md"]);
    t.git_hide = Some(r.ignored.clone());
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(names(&t), ["src", "README.md"]);
    assert_eq!(t.hidden_count, 3, ".git, .gitignore and node_modules");
    t.git_hide = None;
    pump(&mut t, &stats);
    assert_eq!(names(&t).len(), 3);
    let _ = std::fs::remove_dir_all(&d);
}

/// The guard holds for changes too: a repository's fsmonitor never runs, and what would
/// pass through its own filters is refused instead of stored unfiltered.
#[test]
fn a_repositorys_own_config_runs_nothing_when_maia_changes_it() {
    use crate::gitwork::{FILTERED, Op, run};
    if !crate::media::have("git") {
        return;
    }
    let d = tmpdir("evilwork");
    work_repo(&d);
    touch(&d.join("notes.txt"), "one\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    let mark = |n: &str| format!("touch '{}'", d.join(n).display());
    git(&d, &["config", "core.fsmonitor", &mark("ran-fsmonitor")]);
    touch(&d.join("notes.txt"), "two\n");
    run(&d, &Op::Stage(vec!["notes.txt".into()])).unwrap();
    run(&d, &Op::Commit { summary: "Two".into(), body: String::new(), amend: false }).unwrap();
    git(&d, &["branch", "side"]);
    run(&d, &Op::Switch("side".into())).unwrap();
    assert!(!d.join("ran-fsmonitor").exists(), "the fsmonitor hook ran");
    // Filters of its own: staging through them emptied would store the wrong bytes.
    git(&d, &["config", "filter.x.clean", &format!("sh -c \"{}; cat\"", mark("ran-clean"))]);
    touch(&d.join(".gitattributes"), "*.txt filter=x\n");
    touch(&d.join("notes.txt"), "three\n");
    assert_eq!(run(&d, &Op::Stage(vec!["notes.txt".into()])).unwrap_err(), FILTERED);
    assert_eq!(run(&d, &Op::StageAll).unwrap_err(), FILTERED);
    assert_eq!(run(&d, &Op::Switch("main".into())).unwrap_err(), FILTERED);
    let w = work_of(&d);
    assert!(w.staged.is_empty(), "nothing staged: {w:?}");
    // What only moves the index or names a branch still works.
    run(&d, &Op::Create("other".into())).unwrap();
    assert!(!d.join("ran-clean").exists() && !d.join("ran-fsmonitor").exists(), "the repository's config ran a command");
    let _ = std::fs::remove_dir_all(&d);
}

/// A work tree with changes of every kind for the Changes page: a file changed in two
/// places (two hunks), a long line (for wrapping), a deletion, an untracked file and a
/// staged change.
pub(crate) fn changes_fixture(d: &Path) {
    std::fs::create_dir_all(d).unwrap();
    work_repo(d);
    touch(&d.join("src/lib.rs"), &numbered(&[]));
    touch(&d.join("src/view.rs"), "fn view() {}\n");
    touch(&d.join("README.md"), "# Project\n");
    touch(&d.join("old.txt"), "old\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "Start the project"]);
    touch(&d.join("src/lib.rs"), &numbered(&[(3, "line three"), (36, "line thirty-six")]));
    let long = "A long line in a change keeps going past the edge of the panel, and wrapped it stays readable. ".repeat(4);
    touch(&d.join("README.md"), &format!("# Project\n{long}\n"));
    std::fs::remove_file(d.join("old.txt")).unwrap();
    touch(&d.join("notes.md"), "one\ntwo\nthree\n");
    touch(&d.join("src/view.rs"), "fn view() { draw(); }\n");
    git(d, &["add", "src/view.rs"]);
}

// ---------------------------------------------------------------- developer previews

/// A README as projects write them: every kind of block, emphasis inside links inside
/// lists, references, pictures on disk and on the web, a table with alignments.
pub(crate) const README_MD: &str = r#"# Maia
A file manager that *stays out of the way*, with **bold claims** and ***both***.

[![Build](https://img.example.invalid/ci.svg)](https://ci.example.invalid/maia)

<p align="center"><img src="shot.png" alt="The window"></p>

Install it with `cargo build --release`, or read [the guide](docs/guide.md) and
the [notes][n]. See https://example.invalid/maia. for more, or <mail@example.invalid>.

Setext title
------------

> A quote that goes on
lazily, with a list:
> - one
> - two

3. third
4. fourth
   - nested **strong** item
   - [link in a list](#setext-title)

- [x] done task
- [ ] open task

| Key | Does | Count |
|:----|:----:|------:|
| `Space` | opens the inspector | 1 |
| Ctrl+P \| G | goes | 22 |

```rust
fn main() {
    println!("hi"); // greet
}
```

    indented code

~~gone~~ and snake_case_words and 2 * 3 * 4 stay plain.

***

![logo](logo.png "The logo")

[n]: notes.md "Notes"
"#;

/// A folder with one file of every developer kind, committed by two people, a database
/// and a growing log beside them.
pub(crate) fn dev_fixture(d: &Path) {
    let _ = std::fs::remove_dir_all(d);
    std::fs::create_dir_all(d.join("docs")).unwrap();
    touch(&d.join("README.md"), README_MD);
    std::fs::write(d.join("logo.png"), png_bytes(64, 40, 2)).unwrap();
    std::fs::write(d.join("shot.png"), png_bytes(320, 180, 3)).unwrap();
    touch(&d.join("docs/guide.md"), "# Guide\n");
    touch(&d.join("notes.md"), "# Notes\n");
    touch(&d.join("package.json"), "{\n  // settings files have comments\n  \"name\": \"maia-web\",\n  \"version\": \"0.2.0\",\n  \"private\": true,\n  \"id\": 12345678901234567890,\n  \"scripts\": { \"build\": \"vite build\", \"test\": \"vitest\" },\n  \"files\": [\"dist\", \"README.md\"],\n  \"nothing\": null,\n  \"nested\": { \"deeper\": { \"deepest\": [1, 2.5, -3e2] } },\n}\n");
    touch(&d.join("events.jsonl"), "{\"t\": 1, \"msg\": \"start\"}\n{\"t\": 2, \"msg\": \"tick\", \"tags\": [\"a\", \"b\"]}\n\n{\"t\": 3, \"msg\": \"stop\"}\n");
    touch(&d.join("config.toml"), "# Settings\ntitle = \"Maia \\\"dev\\\"\"\nwhen = 1979-05-27T07:32:00Z\nhex = 0xdead_beef\nsite.name = \"x\"\n\n[server]\nport = 8080\nhosts = [\n  \"alpha\", # first\n  \"beta\",\n]\npath = '''\nC:\\raw\\path\n'''\n\n[[plugin]]\nname = \"one\"\n[[plugin]]\nname = \"two\"\ninline = { a = 1, b.c = true }\n");
    touch(&d.join("ci.yml"), "name: CI\non:\n  push:\n    branches: [main, 'release/*']\ndefaults: &defaults\n  runs-on: ubuntu-latest\n  timeout: 30\njobs:\n  build:\n    <<: *defaults\n    steps:\n      - uses: actions/checkout@v4\n      - name: Test\n        run: |\n          cargo test\n          cargo build\n      - when: 2026-10-16\n        ok: yes\n        none: ~\n");
    touch(&d.join("people.csv"), "id,name,email,joined,score\n1,Ada Lovelace,ada@example.invalid,1843-01-01,99.5\n2,\"Grace \"\"Amazing\"\" Hopper\",grace@example.invalid,1952-05-01,98\n3,\"Line\nbreak\",x@example.invalid,2000-01-01,7\n4,Linus,linus@example.invalid,1991-08-25,\n");
    touch(&d.join("prices.tsv"), "item\tprice\tqty\nkeyboard\t49.90\t2\nmouse\t19.00\t5\n");
    let wide: String = (0..30).map(|c| format!("column {c}")).collect::<Vec<_>>().join(";") + "\n" + &(0..40).map(|r| (0..30).map(|c| format!("{}", r * c)).collect::<Vec<_>>().join(";")).collect::<Vec<_>>().join("\n") + "\n";
    touch(&d.join("wide.csv"), &wide);
    touch(&d.join(".env"), "# Local secrets\nexport API_KEY=sk-live-0123456789\nDATABASE_URL=\"postgres://u:pw@localhost/db\"\nMULTI=\"first line\nsecond line\"\nEMPTY=\nnot a pair here\n");
    touch(&d.join("prod.env"), "TOKEN=abc\n");
    touch(&d.join("app.log"), &(1..=40).map(|i| format!("2026-10-16 10:00:{:02} INFO request {i} served\n", i % 60)).collect::<String>());
    touch(&d.join("broken.json"), "{\"a\": 1,, }\n");
    std::fs::write(d.join("data.db"), b"SQLite format 3\0fake pages follow").unwrap();
    std::fs::write(d.join("other.db"), b"not a database at all, just text\n").unwrap();
    touch(&d.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n");
    if !crate::media::have("git") {
        return;
    }
    let commit = |who: &str, mail: &str, date: &str, msg: &str| {
        for args in [vec!["add", "-A"], vec!["commit", "-qm", msg]] {
            let ok = std::process::Command::new("git")
                .args(["-c", &format!("user.name={who}"), "-c", &format!("user.email={mail}"), "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
                .args(&args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .current_dir(d)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "git {args:?}");
        }
    };
    git(d, &["init", "-q"]);
    commit("Ada Lovelace", "ada@example.invalid", "2024-03-01T10:00:00+00:00", "Start the library");
    touch(&d.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n");
    commit("Grace Hopper", "grace@example.invalid", "2026-10-10T10:00:00+00:00", "Add two");
    // A line changed and not committed.
    touch(&d.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2 // two\n}\n");
}

/// What sqlite3 3.50 printed for the fixture's queries (the schema listing, the first
/// rows with a blob, a NULL, a comma and a line break), and a fake that answers with it.
pub(crate) const SQLITE_LIST: &str = "table,\"odd \"\"name\"\"\"\ntable,people\nview,adults\n";
pub(crate) const SQLITE_ROWS: &str = "id,name,age,photo\n1,Ada,36,\"<12 bytes>\"\n2,\"Grace, R.\",45,NULL\n3,\"Line\nbreak\",28,NULL\n";

pub(crate) fn fake_sqlite(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/sqlite3.log\"\nfor a; do last=\"$a\"; done\ncase \"$1\" in -version) echo '3.50.6 2025-09-22 fake'; exit 0;; esac\ncase \"$last\" in\n*sqlite_master*) printf '%s' '{}';;\n*count*people*) echo 3;;\n*count*adults*) echo 2;;\n*count*) echo 0;;\n*pragma_table_info*) printf 'id\\nname\\nage\\nphoto\\n';;\n*people*|*adults*) printf '%s' '{}';;\n*) exit 1;;\nesac\n",
        SQLITE_LIST.replace('\'', "'\\''"),
        SQLITE_ROWS.replace('\'', "'\\''")
    );
    let p = dir.join("sqlite3");
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn md_lines(s: &str) -> Vec<String> {
    crate::inspector::text_lines(s.as_bytes()).unwrap()
}

#[test]
fn markdown_reads_every_block_and_inline_form() {
    use crate::markdown::{Align, Block, CODE, EM, STRIKE, STRONG, plain};
    let d = tmpdir("md");
    dev_fixture(&d);
    let doc = crate::markdown::parse(&md_lines(README_MD), Some(&d));
    let b = &doc.blocks;
    let kinds: Vec<&str> = b.iter().map(|b| match b {
        Block::Heading(..) => "h",
        Block::Para(_) => "p",
        Block::Code { .. } => "code",
        Block::Quote(_) => "quote",
        Block::List { .. } => "list",
        Block::Table { .. } => "table",
        Block::Rule => "rule",
        Block::Images(_) => "img",
        Block::Math(_) => "math",
    }).collect();
    assert_eq!(kinds, ["h", "p", "img", "img", "p", "h", "quote", "list", "list", "table", "code", "code", "p", "rule", "img"], "{b:#?}");
    // Emphasis, strong and both; a heading's text.
    let Block::Heading(1, h) = &b[0] else { panic!() };
    assert_eq!(plain(h), "Maia");
    let Block::Para(p) = &b[1] else { panic!() };
    assert!(p.iter().any(|r| r.text == "stays out of the way" && r.st == EM));
    assert!(p.iter().any(|r| r.text == "bold claims" && r.st == STRONG));
    assert!(p.iter().any(|r| r.text == "both" && r.st == STRONG | EM));
    // A linked badge from the web, a picture inside HTML.
    let Block::Images(badge) = &b[2] else { panic!() };
    assert_eq!(badge.len(), 1);
    assert_eq!(doc.images[badge[0].0 as usize - 1].src, "https://img.example.invalid/ci.svg");
    assert_eq!(doc.links[badge[0].1 as usize - 1].url, "https://ci.example.invalid/maia");
    let Block::Images(shot) = &b[3] else { panic!() };
    let im = &doc.images[shot[0].0 as usize - 1];
    assert_eq!((im.src.as_str(), im.alt.as_str()), ("shot.png", "The window"));
    assert!(im.path.is_some(), "a picture beside the file is found");
    // Code, an inline link to a file, a reference, a bare address less its full stop, an
    // address in brackets.
    let Block::Para(p) = &b[4] else { panic!() };
    assert!(p.iter().any(|r| r.text == "cargo build --release" && r.st == CODE));
    let link = |text: &str| p.iter().find(|r| r.text == text).map(|r| &doc.links[r.link as usize - 1]).unwrap_or_else(|| panic!("{text}: {p:?}"));
    assert_eq!(link("the guide").url, "docs/guide.md");
    assert_eq!(link("the guide").path.as_deref(), Some(d.join("docs/guide.md").as_path()));
    assert_eq!(link("notes").url, "notes.md");
    assert_eq!(link("https://example.invalid/maia").url, "https://example.invalid/maia");
    assert_eq!(link("mail@example.invalid").url, "mailto:mail@example.invalid");
    // Setext heading; a quote with a lazy line and a list in it.
    assert!(matches!(&b[5], Block::Heading(2, h) if plain(h) == "Setext title"));
    let Block::Quote(q) = &b[6] else { panic!() };
    assert!(matches!(&q[0], Block::Para(p) if plain(p) == "A quote that goes on lazily, with a list:"), "{q:?}");
    assert!(matches!(&q[1], Block::List { start: None, items } if items.len() == 2));
    // An ordered list from 3 with a nested list of strong text and an anchor link.
    let Block::List { start: Some(3), items } = &b[7] else { panic!("{:?}", b[7]) };
    assert_eq!(items.len(), 2);
    let Block::List { items: nested, .. } = &items[1].1[1] else { panic!("{:?}", items[1].1) };
    assert!(matches!(&nested[0].1[0], Block::Para(p) if p.iter().any(|r| r.text == "strong" && r.st == STRONG)));
    assert!(matches!(&nested[1].1[0], Block::Para(p) if p.iter().any(|r| doc.links.get(r.link as usize - 1).is_some_and(|l| l.url == "#setext-title"))));
    // Tasks.
    let Block::List { items, .. } = &b[8] else { panic!() };
    assert_eq!(items.iter().map(|i| i.0).collect::<Vec<_>>(), [Some(true), Some(false)]);
    // A table: alignments, code in a cell, an escaped bar.
    let Block::Table { aligns, head, rows } = &b[9] else { panic!() };
    assert_eq!(aligns, &[Align::Left, Align::Center, Align::Right]);
    assert_eq!(head.iter().map(|c| plain(c)).collect::<Vec<_>>(), ["Key", "Does", "Count"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0][0].st, CODE);
    assert_eq!(plain(&rows[1][0]), "Ctrl+P | G");
    // Fenced code with its language's colours; indented code.
    let Block::Code { lang, lines, spans } = &b[10] else { panic!() };
    assert_eq!((lang.as_str(), lines.len()), ("rust", 3));
    assert!(spans.as_ref().is_some_and(|s| s.iter().flatten().count() >= 3));
    assert!(matches!(&b[11], Block::Code { lines, .. } if lines == &["indented code"]));
    // Strikethrough; underscores inside words and lone stars stay text.
    let Block::Para(p) = &b[12] else { panic!() };
    assert!(p.iter().any(|r| r.text == "gone" && r.st == STRIKE));
    assert!(plain(p).contains("snake_case_words") && plain(p).contains("2 * 3 * 4"), "{p:?}");
    // A picture on its own; the link definition is no text.
    let Block::Images(logo) = &b[14] else { panic!() };
    assert!(doc.images[logo[0].0 as usize - 1].path.is_some());
    assert!(!doc.blocks.iter().any(|b| matches!(b, Block::Para(p) if plain(p).contains("[n]"))));
    assert_eq!(crate::markdown::slug(&[crate::markdown::Run { text: "Setext title".into(), ..Default::default() }]), "setext-title");
}

#[test]
fn markdown_survives_broken_and_hostile_input() {
    // Unclosed everything, deep nesting, private-use characters that look like Maia's
    // own picture marker: text comes out, nothing panics.
    let nasty = format!("{}\n\n{}\n\n**open *em `code [link](x \"t\n\n\u{F8FF}0 \u{F8FF}99 ![a](\n\n|a|b|\n|-|\n|c|\n\n```\nnever closed", ">".repeat(200), "- ".repeat(100));
    let doc = crate::markdown::parse(&md_lines(&nasty), None);
    assert!(!doc.blocks.is_empty());
    let big = "a line of a long document\n".repeat(40_000);
    let doc = crate::markdown::parse(&md_lines(&big), None);
    assert!(doc.cut, "past the cap: the rest is left to Source");
}

#[test]
fn json_tree_keeps_order_exact_numbers_and_comments() {
    use crate::datatree::{K, V, flatten, parse_json};
    let d = tmpdir("json");
    dev_fixture(&d);
    let v = parse_json(&std::fs::read_to_string(d.join("package.json")).unwrap()).unwrap();
    let V::Map(m) = &v else { panic!() };
    assert_eq!(m.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["name", "version", "private", "id", "scripts", "files", "nothing", "nested"], "file order, not sorted");
    assert_eq!(m[3].1, V::Num("12345678901234567890".into()), "a big number stays exact");
    let mut t = flatten(&v, None);
    let deepest = t.nodes.iter().position(|n| n.key == "deepest").unwrap();
    assert_eq!(t.path(deepest), ".nested.deeper.deepest");
    assert_eq!(t.path(deepest + 2), ".nested.deeper.deepest[1]");
    assert_eq!(t.nodes[deepest].count, 3);
    assert_eq!(t.nodes[deepest + 3].text, "-3e2");
    assert_eq!(t.nodes[0].kind, K::Str);
    assert_eq!(t.nodes[0].text, "\"maia-web\"");
    // Small: open whole. Folding hides a subtree; keys walk and fold it.
    let all = t.rows().len();
    assert_eq!(all, t.nodes.len());
    let scripts = t.nodes.iter().position(|n| n.key == "scripts").unwrap();
    t.sel = scripts;
    t.focus = true;
    t.key(egui::Key::ArrowLeft);
    assert_eq!(t.rows().len(), all - 2, "folded");
    t.key(egui::Key::ArrowRight);
    t.key(egui::Key::ArrowRight);
    assert_eq!(t.path(t.sel), ".scripts.build", "opened, then stepped in");
    t.key(egui::Key::ArrowLeft);
    assert_eq!(t.sel, scripts, "left steps out to the parent");
    assert!(parse_json("{\"a\": 1,, }").unwrap_err().contains("line 1"));
    // JSON Lines: one record per line, blank lines skipped.
    let v = crate::datatree::parse_jsonl(&std::fs::read_to_string(d.join("events.jsonl")).unwrap()).unwrap();
    assert!(matches!(&v, V::List(l) if l.len() == 3));
    // A huge array stops at the cap with what fits.
    let huge = format!("[{}]", (0..300_000).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    let v = parse_json(&huge).unwrap();
    let t = flatten(&v, None);
    assert!(t.nodes.len() < crate::datatree::MAX_NODES && t.nodes.len() > 150_000, "{}", t.nodes.len());
    // Escapes and a surrogate pair.
    assert_eq!(parse_json(r#""a\"\n\u00e9\ud83d\ude00""#).unwrap(), V::Str("a\"\né😀".into()));
}

#[test]
fn toml_tree_reads_tables_arrays_of_tables_and_every_value() {
    use crate::datatree::{V, parse_toml};
    let d = tmpdir("toml");
    dev_fixture(&d);
    let v = parse_toml(&std::fs::read_to_string(d.join("config.toml")).unwrap()).unwrap();
    let V::Map(m) = &v else { panic!() };
    let get = |m: &Vec<(String, V)>, k: &str| m.iter().find(|x| x.0 == k).map(|x| x.1.clone()).unwrap_or_else(|| panic!("{k}"));
    assert_eq!(m.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["title", "when", "hex", "site", "server", "plugin"]);
    assert_eq!(get(m, "title"), V::Str("Maia \"dev\"".into()));
    assert_eq!(get(m, "when"), V::Date("1979-05-27T07:32:00Z".into()));
    assert_eq!(get(m, "hex"), V::Num("0xdead_beef".into()));
    let V::Map(server) = get(m, "server") else { panic!() };
    assert_eq!(get(&server, "port"), V::Num("8080".into()));
    assert_eq!(get(&server, "hosts"), V::List(vec![V::Str("alpha".into()), V::Str("beta".into())]));
    assert_eq!(get(&server, "path"), V::Str("C:\\raw\\path\n".into()));
    let V::List(plugins) = get(m, "plugin") else { panic!() };
    assert_eq!(plugins.len(), 2);
    let V::Map(two) = &plugins[1] else { panic!() };
    let V::Map(inline) = get(two, "inline") else { panic!() };
    assert_eq!(get(&inline, "a"), V::Num("1".into()));
    assert!(matches!(get(&inline, "b"), V::Map(c) if c[0] == ("c".into(), V::Bool(true))));
    // The project's own manifest and lock file.
    let cargo = parse_toml(include_str!("../Cargo.toml")).unwrap();
    let V::Map(c) = &cargo else { panic!() };
    assert_eq!(c[0].0, "package", "tables in the order written");
    let lock = parse_toml(include_str!("../Cargo.lock")).unwrap();
    assert!(matches!(&lock, V::Map(l) if l.iter().any(|(k, v)| k == "package" && matches!(v, V::List(p) if p.len() > 50))));
    // Mistakes are reported with their line.
    assert!(parse_toml("a = 1\na = 2\n").unwrap_err().contains("line 2"));
    assert!(parse_toml("[x]\nk = \n").is_err());
    assert!(parse_toml("s = \"open\n").is_err());
}

#[test]
fn yaml_tree_follows_anchors_block_scalars_and_documents() {
    use crate::datatree::{V, parse_yaml};
    let d = tmpdir("yaml");
    dev_fixture(&d);
    let v = parse_yaml(&std::fs::read_to_string(d.join("ci.yml")).unwrap()).unwrap();
    let V::Map(m) = &v else { panic!() };
    let get = |m: &Vec<(String, V)>, k: &str| m.iter().find(|x| x.0 == k).map(|x| x.1.clone()).unwrap_or_else(|| panic!("{k} in {m:?}"));
    assert_eq!(m.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["name", "on", "defaults", "jobs"]);
    let V::Map(on) = get(m, "on") else { panic!() };
    let V::Map(push) = get(&on, "push") else { panic!() };
    assert_eq!(get(&push, "branches"), V::List(vec![V::Str("main".into()), V::Str("release/*".into())]));
    let V::Map(jobs) = get(m, "jobs") else { panic!() };
    let V::Map(build) = get(&jobs, "build") else { panic!() };
    // The alias shows what the anchor names.
    assert!(matches!(get(&build, "<<"), V::Map(x) if x.len() == 2 && x[1] == ("timeout".into(), V::Num("30".into()))));
    let V::List(steps) = get(&build, "steps") else { panic!() };
    let V::Map(test) = &steps[1] else { panic!() };
    assert_eq!(get(test, "run"), V::Str("cargo test\ncargo build\n".into()));
    let V::Map(last) = &steps[2] else { panic!() };
    assert_eq!(get(last, "when"), V::Date("2026-10-16".into()));
    assert_eq!(get(last, "ok"), V::Str("yes".into()), "YAML 1.2: yes is a word");
    assert_eq!(get(last, "none"), V::Null);
    // Several documents are a list of them; a bad file says where.
    assert!(matches!(parse_yaml("a: 1\n---\nb: 2\n").unwrap(), V::List(l) if l.len() == 2));
    assert!(parse_yaml("a: [1, 2\nb: 3\n").is_err());
}

#[test]
fn csv_delimiters_quotes_headers_and_caps() {
    use crate::table::{Table, detect, parse};
    let d = tmpdir("csv");
    dev_fixture(&d);
    let text = std::fs::read_to_string(d.join("people.csv")).unwrap();
    assert_eq!(detect(&text, "people.csv"), b',');
    let t = Table::from_text(&text, "people.csv", false);
    assert_eq!(t.header.as_deref(), Some(&["id", "name", "email", "joined", "score"].map(String::from)[..]));
    assert_eq!(t.rows.len(), 4);
    assert_eq!(t.rows[1][1], "Grace \"Amazing\" Hopper", "doubled quotes");
    assert_eq!(t.rows[2][1], "Line\nbreak", "a line break inside quotes");
    assert_eq!(t.numeric, [true, false, false, false, true], "numbers right-aligned; an empty cell does not count");
    assert_eq!(t.note, None);
    assert_eq!(detect(&std::fs::read_to_string(d.join("wide.csv")).unwrap(), "wide.csv"), b';');
    assert_eq!(detect("a|b|c\n1|2|3\n", "x.txt"), b'|');
    assert_eq!(detect("a,b\n1,2\n", "prices.tsv"), b'\t', "the extension decides for .tsv");
    let tsv = Table::from_text(&std::fs::read_to_string(d.join("prices.tsv")).unwrap(), "prices.tsv", false);
    assert_eq!(tsv.cols, 3);
    // A comma inside a semicolon file is not a delimiter.
    assert_eq!(detect("name;city\n\"Doe, J\";Paris\nRoe;Rome\n", "x.csv"), b';');
    // No header when the first row is data; CRLF; a file cut off says so.
    let t = Table::from_text("1,2\r\n3,4\r\n", "n.csv", true);
    assert!(t.header.is_none() && t.rows.len() == 2 && t.rows[1] == ["3", "4"]);
    assert!(t.note.is_some());
    let (rows, more) = parse(&"x\n".repeat(50), b',', 10);
    assert!(rows.len() == 10 && more);
}

#[test]
fn sqlite_lists_tables_and_rows_read_only() {
    use crate::sqlite::{parse_list, parse_version, quote_ident, rows_query, uri};
    let items = parse_list(crate::tests::SQLITE_LIST);
    assert_eq!(items.iter().map(|i| (i.name.as_str(), i.view)).collect::<Vec<_>>(), [("odd \"name\"", false), ("people", false), ("adults", true)]);
    assert_eq!(quote_ident("odd \"name\""), "\"odd \"\"name\"\"\"");
    assert!(rows_query("people", &["photo".into()]).contains("typeof(\"photo\") = 'blob'"));
    assert_eq!(uri(Path::new("/a b/c?d#e.db")), "file:/a%20b/c%3Fd%23e.db?mode=ro&immutable=1");
    assert_eq!(parse_version("3.50.6 2025-09-22 18:50:24 9cc55437 (64-bit)"), Some((3, 50)));
    assert!(crate::sqlite::is_db_header(b"SQLite format 3\0..."));
    // Through the fake: the arguments say read-only, immutable and safe.
    let d = tmpdir("sqlite");
    let fake = d.join("fake");
    fake_sqlite(&fake);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(fake.clone()));
    let tools = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let db = d.join("x.db");
    std::fs::write(&db, b"SQLite format 3\0").unwrap();
    let (items, safe) = crate::sqlite::tables(&tools, &db).unwrap();
    assert!(safe && items.len() == 3);
    let t = crate::sqlite::read_rows(&tools, &db, "people").unwrap();
    assert_eq!(t.header.as_ref().map(|h| h.len()), Some(4));
    assert_eq!(t.rows.len(), 3);
    assert_eq!(t.rows[0][3], "<12 bytes>");
    assert_eq!(t.rows[2][1], "Line\nbreak");
    let log = std::fs::read_to_string(fake.join("sqlite3.log")).unwrap();
    for l in log.lines().filter(|l| !l.contains("-version")) {
        assert!(l.contains("-readonly") && l.contains("-safe") && l.contains("immutable=1") && l.contains("-init /dev/null"), "{l}");
    }
    // The real program, when this machine has it: a database made here.
    if crate::media::have("sqlite3") {
        let real = d.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let bin = std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|p| p.join("sqlite3")).find(|p| p.is_file()).unwrap();
        std::os::unix::fs::symlink(&bin, real.join("sqlite3")).unwrap();
        let db = d.join("real.db");
        let ok = std::process::Command::new(&bin).arg(&db).arg("CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT, photo BLOB); INSERT INTO people VALUES(1,'Ada',x'00ff00ff'),(2,'Grace, R.',NULL); CREATE VIEW named AS SELECT name FROM people;").status().is_ok_and(|s| s.success());
        assert!(ok);
        let before = std::fs::read(&db).unwrap();
        crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(real.clone()));
        let tools = crate::drives::Tools::current().unwrap();
        crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
        let (items, _) = crate::sqlite::tables(&tools, &db).unwrap();
        assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["people", "named"]);
        let t = crate::sqlite::read_rows(&tools, &db, "people").unwrap();
        assert_eq!(t.rows, [["1", "Ada", "<4 bytes>"], ["2", "Grace, R.", "NULL"]]);
        assert_eq!(std::fs::read(&db).unwrap(), before, "the file is unchanged");
        let names: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("real.db")).collect();
        assert_eq!(names, ["real.db"], "no journal or lock files beside it");
    } else {
        eprintln!("sqlite3 not installed: the real database was not tried");
    }
}

#[test]
fn env_values_are_masked_keys_are_not() {
    use crate::envfile::{Line, MASK, classify, is_env_name, shown};
    for (n, yes) in [(".env", true), (".env.local", true), ("prod.env", true), ("env", false), (".envrc", false), ("my.environment", false)] {
        assert_eq!(is_env_name(n), yes, "{n}");
    }
    let d = tmpdir("env");
    dev_fixture(&d);
    let lines = md_lines(&std::fs::read_to_string(d.join(".env")).unwrap());
    let k = classify(&lines);
    assert_eq!(k[0], Line::Comment);
    let masked: Vec<String> = lines.iter().zip(&k).map(|(l, &k)| shown(l, k, false)).collect();
    assert_eq!(masked[1], format!("export API_KEY={MASK}"));
    assert_eq!(masked[2], format!("DATABASE_URL={MASK}"));
    assert_eq!(masked[3], format!("MULTI={MASK}"));
    assert_eq!(masked[4], MASK, "the rest of a quoted value over lines");
    assert_eq!(masked[5], "EMPTY=", "nothing to hide");
    assert_eq!(masked[6], MASK, "a line that is no pair is hidden whole");
    let all = masked.join("\n");
    for secret in ["sk-live", "pw@localhost", "second line", "not a pair"] {
        assert!(!all.contains(secret), "{secret} leaked");
    }
    assert_eq!(shown(&lines[1], k[1], true), lines[1]);
}

#[test]
fn followed_file_appends_rotates_and_stops() {
    use std::io::Write;
    let d = tmpdir("follow");
    let p = d.join("app.log");
    std::fs::write(&p, "one\ntwo\n").unwrap();
    let ctx = egui::Context::default();
    let tail = crate::tail::Tail::start(p.clone(), Some(8), ctx).unwrap();
    let wait = |what: &str| -> (bool, Vec<u8>) {
        let t0 = Instant::now();
        loop {
            if let Some(got) = tail.take() {
                return got;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"three\nfou").unwrap();
    let (reset, data) = wait("appended lines");
    assert!(!reset);
    assert_eq!(data, b"three\nfou");
    std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"r\n").unwrap();
    assert_eq!(wait("the rest of the line").1, b"r\n");
    // Truncated: read again from the start.
    std::fs::write(&p, "fresh\n").unwrap();
    let (reset, data) = wait("truncation");
    assert!(reset);
    assert_eq!(data, b"fresh\n");
    // Rotated: a new file under the name.
    std::fs::rename(&p, d.join("app.log.1")).unwrap();
    std::fs::write(&p, "rotated and longer than before\n").unwrap();
    let (reset, data) = wait("rotation");
    assert!(reset && data.ends_with(b"rotated and longer than before\n"));
    // Dropped: the worker ends (its thread finishes; nothing more is read). Only threads
    // by its name are counted: tests running beside this one start and end their own.
    let threads = || std::fs::read_dir("/proc/self/task").map(|r| r.flatten().filter(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == "follow")).count()).unwrap_or(0);
    let before = threads();
    drop(tail);
    let t0 = Instant::now();
    while threads() >= before && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(threads() < before, "the follow worker ended");
}

#[test]
fn blame_reads_authors_ages_and_uncommitted_lines() {
    if !crate::media::have("git") {
        eprintln!("git not installed: skipped");
        return;
    }
    let d = tmpdir("blame");
    dev_fixture(&d);
    let b = crate::blame::run(&d, &d.join("src/lib.rs")).unwrap();
    assert_eq!(b.lines.len(), 7);
    let who: Vec<&str> = b.lines.iter().map(|&k| b.commits[k as usize].author.as_str()).collect();
    assert_eq!(&who[..3], ["Ada Lovelace"; 3]);
    assert_eq!(who[4], "Grace Hopper");
    let six = &b.commits[b.lines[5] as usize];
    assert!(!six.committed(), "the edited line is not committed");
    let first = &b.commits[b.lines[0] as usize];
    assert_eq!(first.summary, "Start the library");
    assert_eq!(first.time, 1709287200);
    assert_eq!(crate::blame::short_ago(first.time, first.time + 3 * 86_400), "3d");
    assert_eq!(crate::blame::short_ago(first.time, first.time + 400 * 86_400), "1y");
    assert_eq!(crate::blame::short_author("Grace Hopper"), "Grace");
    // Outside the repository, or not tracked: no blame.
    touch(&d.join("new.txt"), "x\n");
    assert!(crate::blame::run(&d, &d.join("new.txt")).is_err());
}

/// Copy As Code Blocks on real files: each under its path from the project's top in a
/// fence its text can't close, a folder giving the files git keeps (not hidden or
/// ignored ones), a big file cut at a line's end, a binary one left out and said so.
#[test]
fn share_code_blocks() {
    use crate::share::{Blocks, FILE_CAP, code_blocks};
    let d = tmpdir("share-blocks");
    git(&d, &["init", "-q"]);
    touch(&d.join(".gitignore"), "target/\n*.log\n");
    touch(&d.join("src/main.rs"), "fn main() {}\n");
    touch(&d.join("src/lib.rs"), "pub fn f() {}");
    touch(&d.join("src/.hidden.rs"), "secret\n");
    touch(&d.join("src/run.log"), "noise\n");
    touch(&d.join("target/out.rs"), "built\n");
    touch(&d.join("README.md"), "Use it like this:\n\n```sh\ncargo run\n```\n");
    touch(&d.join("Makefile"), "all:\n\ttrue\n");
    std::fs::write(d.join("logo.png"), b"\x89PNG\x00\x01").unwrap();
    let big: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    touch(&d.join("big.txt"), &big);

    let b = code_blocks(&[d.join("src"), d.join("README.md"), d.join("Makefile")], &d);
    assert_eq!(b.text, "src/lib.rs\n```rust\npub fn f() {}\n```\n\nsrc/main.rs\n```rust\nfn main() {}\n```\n\nREADME.md\n````markdown\nUse it like this:\n\n```sh\ncargo run\n```\n````\n\nMakefile\n```makefile\nall:\n\ttrue\n```\n");
    assert_eq!((b.files, b.cut, b.skipped.len()), (4, 0, 0));
    assert_eq!(b.note(), "Copied 4 files as code blocks");

    let b = code_blocks(&[d.join("big.txt"), d.join("logo.png")], &d);
    assert_eq!(b.skipped, vec![("logo.png".to_string(), "binary")]);
    assert_eq!((b.files, b.cut), (1, 1));
    let first = b.text.lines().next().unwrap();
    assert!(first.starts_with("big.txt (the first 100 KB of "), "{first}");
    let body: Vec<&str> = b.text.lines().skip(2).collect();
    assert_eq!(body.last(), Some(&"```"), "the fence is closed");
    assert!(body[..body.len() - 1].iter().all(|l| l.starts_with("line ")), "cut at a line's end");
    assert!(b.text.len() <= FILE_CAP + 200);
    assert_eq!(b.note(), "Copied 1 file as code blocks, 1 file cut at 100 KB; left out logo.png (binary)");
    let none = code_blocks(&[d.join("logo.png")], &d);
    assert_eq!(none, Blocks { text: String::new(), files: 0, cut: 0, skipped: vec![("logo.png".into(), "binary")] });
    assert_eq!(crate::share::fence_lang("app.tsx"), "tsx");
    assert_eq!(crate::share::fence_lang("Dockerfile"), "dockerfile");
    assert_eq!(crate::share::fence_lang("notes"), "");
    let _ = std::fs::remove_dir_all(&d);
}

/// Remote URLs as git prints them, and the website and link each one gives.
#[test]
fn share_web_links_from_remotes() {
    use crate::share::{Forge, file_url, web_base};
    let ok = |u: &str| web_base(u).unwrap_or_else(|e| panic!("{u}: {e}"));
    assert_eq!(ok("git@github.com:acme/tidepool.git"), ("https://github.com/acme/tidepool".into(), Forge::GitHub));
    assert_eq!(ok("https://github.com/acme/tidepool"), ("https://github.com/acme/tidepool".into(), Forge::GitHub));
    assert_eq!(ok("https://user:token@github.com/acme/tidepool.git/"), ("https://github.com/acme/tidepool".into(), Forge::GitHub));
    assert_eq!(ok("ssh://git@github.com:22/acme/tidepool.git"), ("https://github.com/acme/tidepool".into(), Forge::GitHub));
    assert_eq!(ok("git@github.com-work:acme/tidepool.git"), ("https://github.com/acme/tidepool".into(), Forge::GitHub), "an ssh alias for a second account");
    assert_eq!(ok("git@gitlab.com:group/sub/proj.git"), ("https://gitlab.com/group/sub/proj".into(), Forge::GitLab));
    assert_eq!(ok("https://gitlab.example.com:8443/team/proj"), ("https://gitlab.example.com:8443/team/proj".into(), Forge::GitLab));
    assert_eq!(ok("https://codeberg.org/ana/notes.git"), ("https://codeberg.org/ana/notes".into(), Forge::Gitea));
    assert_eq!(ok("git@bitbucket.org:team/repo.git"), ("https://bitbucket.org/team/repo".into(), Forge::Bitbucket));
    assert_eq!(ok("https://git.sr.ht/~ana/notes"), ("https://git.sr.ht/~ana/notes".into(), Forge::SourceHut));
    assert!(web_base("/srv/git/notes.git").is_err());
    assert!(web_base("file:///srv/git/notes.git").is_err());
    assert_eq!(web_base("git@example.com:x/y.git").unwrap_err(), "No web page is known for example.com");

    let sha = "0123456789abcdef0123456789abcdef01234567";
    let gh = "https://github.com/acme/tidepool";
    assert_eq!(file_url(gh, Forge::GitHub, sha, "src/store.rs", false, Some(18)), format!("{gh}/blob/{sha}/src/store.rs#L18"));
    assert_eq!(file_url(gh, Forge::GitHub, sha, "src", true, Some(18)), format!("{gh}/tree/{sha}/src"), "no line on a folder");
    assert_eq!(file_url(gh, Forge::GitHub, sha, "", true, None), format!("{gh}/tree/{sha}"));
    assert_eq!(file_url(gh, Forge::GitHub, sha, "docs/Field notes #2.md", false, None), format!("{gh}/blob/{sha}/docs/Field%20notes%20%232.md"));
    assert_eq!(file_url("https://gitlab.com/g/p", Forge::GitLab, sha, "a.rs", false, Some(3)), format!("https://gitlab.com/g/p/-/blob/{sha}/a.rs#L3"));
    assert_eq!(file_url("https://codeberg.org/a/n", Forge::Gitea, sha, "a.rs", false, Some(3)), format!("https://codeberg.org/a/n/src/commit/{sha}/a.rs#L3"));
    assert_eq!(file_url("https://bitbucket.org/t/r", Forge::Bitbucket, sha, "a.rs", false, Some(3)), format!("https://bitbucket.org/t/r/src/{sha}/a.rs#lines-3"));
    assert_eq!(file_url("https://git.sr.ht/~a/n", Forge::SourceHut, sha, "a.rs", false, Some(3)), format!("https://git.sr.ht/~a/n/tree/{sha}/item/a.rs#L3"));
}

/// Copy Web Link in a real repository: the commit checked out, the line a search
/// showed, and a caution while the commit isn't pushed or a file isn't committed.
#[test]
fn share_web_links_in_a_repository() {
    let d = tmpdir("share-links");
    work_repo(&d);
    touch(&d.join("src/store.rs"), "a\nb\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    let sha = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&d).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap().trim().to_string();
    let store = d.join("src/store.rs");
    assert_eq!(crate::share::web_links(&[(store.clone(), false)], None).unwrap_err(), "The repository has no remote");
    git(&d, &["remote", "add", "origin", "git@github.com:acme/tidepool.git"]);
    let base = format!("https://github.com/acme/tidepool/blob/{sha}");
    let (links, caution) = crate::share::web_links(&[(store.clone(), false)], Some((store.clone(), 2))).unwrap();
    assert_eq!(links, format!("{base}/src/store.rs#L2"));
    assert_eq!(caution.as_deref(), Some("this commit isn't pushed, so the link won't open until it is"));
    // Pushed: what the remote has is the commit.
    git(&d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let (links, caution) = crate::share::web_links(&[(store.clone(), false), (d.join("src"), true), (d.clone(), true)], None).unwrap();
    assert_eq!(links, format!("{base}/src/store.rs\nhttps://github.com/acme/tidepool/tree/{sha}/src\nhttps://github.com/acme/tidepool/tree/{sha}"));
    assert_eq!(caution, None);
    touch(&d.join("src/new.rs"), "x\n");
    let (_, caution) = crate::share::web_links(&[(d.join("src/new.rs"), false)], None).unwrap();
    assert_eq!(caution.as_deref(), Some("src/new.rs isn't committed, so its link won't open yet"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Add To .gitignore: lines git reads as the exact names (checked with git itself),
/// added once, after a last line that had no line break, and a word about files git
/// still tracks.
#[test]
fn share_gitignore_lines() {
    use crate::share::{add_ignores, ignore_ext_line, ignore_line, ignore_note, shared_ext};
    assert_eq!(ignore_line("build", true), "/build/");
    assert_eq!(ignore_line("notes [draft]*.txt", false), "/notes \\[draft]\\*.txt");
    assert_eq!(ignore_line("trailing ", false), "/trailing\\ ");
    assert_eq!(ignore_ext_line("log"), "*.log");
    let d = tmpdir("share-ignore");
    work_repo(&d);
    touch(&d.join(".gitignore"), "target/");
    touch(&d.join("notes [draft]*.txt"), "x\n");
    touch(&d.join("notes d.txt"), "x\n");
    touch(&d.join("tracked.env"), "A=1\n");
    touch(&d.join("logs/a.log"), "x\n");
    git(&d, &["add", "tracked.env"]);
    git(&d, &["commit", "-qm", "Start"]);
    let lines = vec![ignore_line("notes [draft]*.txt", false), ignore_line("logs", true), ignore_line("tracked.env", false)];
    let paths = vec![d.join("notes [draft]*.txt"), d.join("logs"), d.join("tracked.env")];
    let (added, tracked) = add_ignores(&d, &lines, &paths).unwrap();
    assert_eq!((added, tracked.clone()), (3, vec!["tracked.env".to_string()]));
    assert_eq!(std::fs::read_to_string(d.join(".gitignore")).unwrap(), "target/\n/notes \\[draft]\\*.txt\n/logs/\n/tracked.env\n");
    assert_eq!(ignore_note(&lines, added, &tracked), "Added 3 lines to .gitignore; git still tracks tracked.env until it's removed with git rm --cached");
    let ignored = |p: &str| std::process::Command::new("git").arg("-C").arg(&d).args(["check-ignore", "-q", "--no-index", p]).status().unwrap().success();
    assert!(ignored("notes [draft]*.txt") && ignored("logs/a.log"));
    assert!(!ignored("notes d.txt"), "the brackets and star are the name's own, not a pattern");
    assert_eq!(add_ignores(&d, &lines[..1], &paths[..1]).unwrap(), (0, vec![]), "already there");
    assert_eq!(ignore_note(&lines[..1], 0, &[]), ".gitignore already has /notes \\[draft]\\*.txt");
    let p = |s: &str| PathBuf::from(s);
    assert_eq!(shared_ext([(p("/a/x.log").as_path(), false), (p("/a/y.log").as_path(), false)].into_iter()), Some("log".into()));
    assert_eq!(shared_ext([(p("/a/x.log").as_path(), false), (p("/a/y.txt").as_path(), false)].into_iter()), None);
    assert_eq!(shared_ext([(p("/a/.env").as_path(), false)].into_iter()), None, "a dotfile has no extension");
    assert_eq!(shared_ext([(p("/a/x.log").as_path(), true)].into_iter()), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// Compare Files on real files with git's diff: what changed from the first file to the
/// second, counted; the same files said to be the same; binary files and a missing one
/// refused with a reason; the title naming the folders when the names match.
#[test]
fn compare_two_files() {
    use crate::filediff::{Compare, diff, parse};
    let d = tmpdir("compare-files");
    touch(&d.join("a/config.toml"), "port = 8080\nworkers = 4\nlog = \"info\"\n");
    touch(&d.join("b/config.toml"), "port = 9090\nworkers = 4\nlog = \"info\"\ncache = true\n");
    touch(&d.join("copy.toml"), "port = 8080\nworkers = 4\nlog = \"info\"\n");
    std::fs::write(d.join("x.bin"), b"\x00\x01\x02").unwrap();
    std::fs::write(d.join("y.bin"), b"\x00\x01\x03").unwrap();
    let r = diff(&d.join("a/config.toml"), &d.join("b/config.toml")).unwrap();
    assert_eq!(r.lines, ["@@ -1,3 +1,4 @@", "-port = 8080", "+port = 9090", " workers = 4", " log = \"info\"", "+cache = true"]);
    assert_eq!((r.added, r.removed, r.truncated), (2, 1, false));
    assert_eq!(diff(&d.join("a/config.toml"), &d.join("copy.toml")).unwrap(), Default::default(), "the same");
    assert_eq!(diff(&d.join("x.bin"), &d.join("y.bin")).unwrap_err(), "They're binary files, and they differ");
    assert!(diff(&d.join("a/config.toml"), &d.join("gone.toml")).is_err());
    // git's header (the file names) is left out; a long diff stops with a line saying so.
    let long: String = format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,60000 @@\n{}", "+x\n".repeat(60_000));
    let p = parse(&long).unwrap();
    assert!(p.truncated && p.lines.len() == 50_001 && p.lines[0].starts_with("@@"));
    assert_eq!(p.lines.last().map(String::as_str), Some("… the diff goes on past 50,000 lines"));
    let ctx = egui::Context::default();
    assert_eq!(Compare::start(d.join("a/config.toml"), d.join("b/config.toml"), &ctx).title(), "a/config.toml → b/config.toml");
    assert_eq!(Compare::start(d.join("a/config.toml"), d.join("copy.toml"), &ctx).title(), "config.toml → copy.toml");
    assert!(crate::inspector::is_patch("fix.patch") && crate::inspector::is_patch("A.DIFF"));
    assert!(!crate::inspector::is_patch(".patch") && !crate::inspector::is_patch("patch.txt"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Find Duplicates on real files: copies found whatever their names and folders, the
/// oldest first in each set; files of one size but other contents, or alike only in
/// their first 64 KB, kept apart; hard links to one file, empty files, hidden folders
/// and installed packages left out; the sets with the most to free first.
#[test]
fn duplicates_found_by_contents() {
    use crate::dupes::{Msg, find};
    let d = tmpdir("dupes");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400 * 30);
    let photo = vec![7u8; 300_000];
    touch(&d.join("Pictures/beach.jpg"), "");
    std::fs::write(d.join("Pictures/beach.jpg"), &photo).unwrap();
    std::fs::File::options().write(true).open(d.join("Pictures/beach.jpg")).unwrap().set_modified(old).unwrap();
    std::fs::create_dir_all(d.join("Downloads")).unwrap();
    std::fs::write(d.join("Downloads/IMG_2041.jpg"), &photo).unwrap();
    std::fs::write(d.join("Downloads/beach (1).jpg"), &photo).unwrap();
    // The same first 64 KB, then different.
    let mut other = photo.clone();
    other[200_000] = 8;
    std::fs::write(d.join("Downloads/almost.jpg"), &other).unwrap();
    touch(&d.join("notes.txt"), "same words\n");
    touch(&d.join("Documents/notes copy.txt"), "same words\n");
    touch(&d.join("Documents/other.txt"), "else words\n");
    std::fs::hard_link(d.join("Documents/other.txt"), d.join("Documents/linked.txt")).unwrap();
    touch(&d.join("empty-a"), "");
    touch(&d.join("empty-b"), "");
    touch(&d.join(".cache/notes.txt"), "same words\n");
    touch(&d.join("web/package.json"), "{}\n");
    touch(&d.join("web/node_modules/x/notes.txt"), "same words\n");
    let mut sets = vec![];
    let stop = std::sync::atomic::AtomicBool::new(false);
    find(&d, &stop, &mut |m| {
        if let Msg::Set(s) = m {
            sets.push(s);
        }
    });
    let names = |s: &crate::dupes::Set| s.files.iter().map(|c| crate::project::relative(&c.path, &d)).collect::<Vec<_>>();
    assert_eq!(sets.len(), 2, "{:?}", sets.iter().map(names).collect::<Vec<_>>());
    assert_eq!(names(&sets[0])[0], "Pictures/beach.jpg", "the oldest is kept");
    let mut rest = names(&sets[0])[1..].to_vec();
    rest.sort();
    assert_eq!(rest, ["Downloads/IMG_2041.jpg", "Downloads/beach (1).jpg"]);
    assert_eq!((sets[0].size, sets[0].spare()), (300_000, 600_000));
    let mut small = names(&sets[1]);
    small.sort();
    assert_eq!(small, ["Documents/notes copy.txt", "notes.txt"], "not the hidden or installed copies");
    let _ = std::fs::remove_dir_all(&d);
}

/// Batch Rename's names: a number padded to the run of #, the old name kept in place of
/// {name}, find and replace in plain text or with an expression's groups, a change of
/// case, extensions kept (and none taken from a folder or a dotfile); and the names
/// that can't be used, each with why.
#[test]
fn batch_rename_names_and_clashes() {
    use crate::batchname::{Case, Rules, names, problems, split};
    let items: Vec<(String, bool)> = ["IMG_2041.JPG", "IMG_2042.jpg", "beach day.png", "Old photos", ".hidden"].iter().map(|n| (n.to_string(), *n == "Old photos")).collect();
    assert_eq!(split("IMG_2041.JPG", false), ("IMG_2041", ".JPG"));
    assert_eq!(split("v1.2", true), ("v1.2", ""), "a folder keeps its dots");
    assert_eq!(split(".hidden", false), (".hidden", ""));
    let r = |f: &dyn Fn(&mut Rules)| {
        let mut r = Rules { start: "1".into(), ..Default::default() };
        f(&mut r);
        names(&items, &r).unwrap()
    };
    assert_eq!(r(&|_| {}), ["IMG_2041.JPG", "IMG_2042.jpg", "beach day.png", "Old photos", ".hidden"], "nothing asked, nothing changes");
    assert_eq!(r(&|r| r.name = "Trip ###".into()), ["Trip 001.JPG", "Trip 002.jpg", "Trip 003.png", "Trip 004", "Trip 005"]);
    assert_eq!(r(&|r| {
        r.name = "{name} #".into();
        r.start = "9".into();
    }), ["IMG_2041 9.JPG", "IMG_2042 10.jpg", "beach day 11.png", "Old photos 12", ".hidden 13"]);
    assert_eq!(r(&|r| {
        r.find = "img_".into();
        r.replace = "Photo ".into();
    }), ["Photo 2041.JPG", "Photo 2042.jpg", "beach day.png", "Old photos", ".hidden"], "plain text, any case");
    assert_eq!(r(&|r| {
        r.find = "img_".into();
        r.case_sensitive = true;
    })[0], "IMG_2041.JPG");
    assert_eq!(r(&|r| {
        r.find = r"IMG_(\d+)".into();
        r.replace = "$1 from the camera".into();
        r.regex = true;
    })[..2], ["2041 from the camera.JPG", "2042 from the camera.jpg"]);
    assert_eq!(r(&|r| {
        r.find = "$".into();
        r.replace = "$1".into();
    })[0], "IMG_2041.JPG", "a plain $ is just a $, and is not in the name");
    assert_eq!(r(&|r| r.case = Case::Title)[..3], ["Img_2041.JPG", "Img_2042.jpg", "Beach Day.png"], "the extension keeps its case");
    assert_eq!(r(&|r| r.case = Case::Lower)[2], "beach day.png");
    let mut bad = Rules { find: "(".into(), regex: true, ..Default::default() };
    assert!(names(&items, &bad).is_err(), "a broken expression says so");
    bad.regex = false;
    assert_eq!(names(&items, &bad).unwrap()[0], "IMG_2041.JPG", "as plain text it is just a bracket");
    // Clashes: two the same, one taken by a file not being renamed, an empty one, a slash.
    let olds: Vec<(String, bool)> = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"].iter().map(|n| (n.to_string(), false)).collect();
    let new: Vec<String> = ["x.txt", "x.txt", "taken.txt", ".txt", "f/g.txt"].iter().map(|n| n.to_string()).collect();
    let others: std::collections::HashSet<String> = ["taken.txt", "a.txt"].iter().map(|n| n.to_string()).collect();
    assert_eq!(problems(&olds, &new, &others), [Some("same as another"), Some("same as another"), Some("already there"), None, Some("can't have /")]);
    // Swapping two names among the renamed is fine.
    let swap: Vec<String> = ["b.txt", "a.txt"].iter().map(|n| n.to_string()).collect();
    assert_eq!(problems(&olds[..2], &swap, &others), [None, None]);
    let empty: Vec<String> = vec!["".to_string()];
    assert_eq!(problems(&olds[..1], &empty, &others), [Some("no name")]);
}

/// Emptying the trash by age on a real trash folder: what was trashed before the cutoff
/// goes with its .trashinfo, a newer item stays, and one whose date can't be read stays.
#[test]
fn trash_emptied_by_age() {
    let d = tmpdir("trash-age");
    let t = d.join("Trash");
    let item = |name: &str, date: &str| {
        touch(&t.join("files").join(name).join("inside.txt"), "x");
        touch(&t.join("info").join(format!("{name}.trashinfo")), &format!("[Trash Info]\nPath=/home/someone/{name}\nDeletionDate={date}\n"));
    };
    item("old", "2026-08-01T10:00:00");
    item("new", "2026-10-15T10:00:00");
    item("undated", "not a date");
    let cutoff = crate::timefmt::parse_local("2026-09-16T00:00:00").unwrap();
    assert_eq!(crate::trashcan::empty_older(std::slice::from_ref(&t), cutoff), 1);
    assert!(!t.join("files/old").exists() && !t.join("info/old.trashinfo").exists());
    for keep in ["new", "undated"] {
        assert!(t.join("files").join(keep).join("inside.txt").exists() && t.join("info").join(format!("{keep}.trashinfo")).exists(), "{keep} stays");
    }
    assert_eq!(crate::config::Config::default().trash_days, 0, "off unless chosen");
    let _ = std::fs::remove_dir_all(&d);
}

/// A stand-in for LibreOffice: it logs its arguments, writes a two-page PDF (a real one,
/// poppler reads it) named as LibreOffice names its output, fails on a "broken" file and
/// sleeps on a "slow" one, its sleep's pid written down so a test can see it killed.
pub(crate) fn fake_soffice(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("sample.pdf"), crate::sim::sample_pdf(&["0.2 0.4 0.8", "0.8 0.3 0.2"])).unwrap();
    let script = r#"#!/bin/sh
D="$(dirname "$0")"
echo "$*" >> "$D/soffice.log"
out=""; prev=""
for a; do [ "$prev" = --outdir ] && out="$a"; prev="$a"; last="$a"; done
case "$last" in
*slow*) sleep 30 & echo $! > "$D/sleep.pid"; wait; exit 0;;
*broken*) exit 1;;
esac
name=$(basename "$last")
cp "$D/sample.pdf" "$out/${name%.*}.pdf"
"#;
    let p = dir.join("soffice");
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Whether a process is gone (or only a zombie waiting to be reaped), after up to 3 s.
pub(crate) fn process_gone(pid: &str) -> bool {
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < 3 {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => return true,
            Ok(s) if s.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')) => return true,
            _ => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    false
}

/// Office documents through LibreOffice (a stand-in that writes a real PDF): converted
/// once into the cache with a profile of Maia's own, read back the second time without
/// starting it, again when the file changes; a file it can't read refused; a slow one
/// killed with its children, when the preview goes away and at the time limit.
#[test]
fn office_documents_convert_to_pdf_once() {
    use crate::office::{TIMEOUT, cached, convert, is_office, program};
    let d = tmpdir("office");
    let fake = d.join("tools");
    fake_soffice(&fake);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(fake.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.join("none")));
    assert_eq!(program(&crate::drives::Tools::current().unwrap()), None, "no LibreOffice: no conversion");
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    assert_eq!(program(&t), Some("soffice"));
    for x in ["docx", "doc", "odt", "rtf", "xlsx", "xls", "ods", "pptx", "ppt", "odp"] {
        assert!(is_office(x), "{x}");
    }
    assert!(!is_office("pdf") && !is_office("txt") && !is_office("docm"));
    let src = d.join("Q3 report.v2.docx");
    touch(&src, "PK not really a document");
    let meta = |p: &Path| std::fs::metadata(p).map(|m| (m.len(), m.modified().ok())).unwrap();
    let (size, mtime) = meta(&src);
    let log = || std::fs::read_to_string(fake.join("soffice.log")).unwrap_or_default();
    let pdf = convert(&t, "soffice", &src, size, mtime, TIMEOUT, &|| false).unwrap();
    assert_eq!(pdf, cached(&src, size, mtime));
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF-1.4"));
    if crate::media::have("pdfinfo") {
        assert_eq!(crate::media::pdf_pages(&pdf), Some(2), "the PDF reads");
    }
    let args = log();
    assert_eq!(args.lines().count(), 1);
    let profile = crate::clipboard::path_to_uri(&crate::office::cache_dir().join("profile"));
    assert!(args.starts_with(&format!("-env:UserInstallation={profile} --headless --norestore --nolockcheck --convert-to pdf --outdir ")), "{args}");
    assert!(args.trim_end().ends_with(&src.display().to_string()), "{args}");
    assert!(profile.starts_with("file:///") && !profile.contains("/.config/libreoffice"));
    // The second look: from the cache, LibreOffice not started.
    assert_eq!(convert(&t, "soffice", &src, size, mtime, TIMEOUT, &|| false).unwrap(), pdf);
    assert_eq!(log().lines().count(), 1);
    // Changed: converted again, to a file of its own.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&src, "PK a longer document than before").unwrap();
    let (size2, mtime2) = meta(&src);
    let pdf2 = convert(&t, "soffice", &src, size2, mtime2, TIMEOUT, &|| false).unwrap();
    assert_ne!(pdf2, pdf);
    assert_eq!(log().lines().count(), 2);
    let stage = crate::office::cache_dir().join(format!(".part-{}", pdf2.file_stem().unwrap().to_string_lossy()));
    assert!(!stage.exists(), "the staging folder is gone");
    let broken = d.join("broken.xlsx");
    touch(&broken, "x");
    let (bs, bm) = meta(&broken);
    assert_eq!(convert(&t, "soffice", &broken, bs, bm, TIMEOUT, &|| false).unwrap_err(), "Can't preview this document");
    // Slow: stopped (the preview went away), then timed out; LibreOffice's children die too.
    let slow = d.join("slow.pptx");
    touch(&slow, "x");
    let (ss, sm) = meta(&slow);
    let t0 = std::time::Instant::now();
    let r = convert(&t, "soffice", &slow, ss, sm, TIMEOUT, &|| t0.elapsed().as_millis() > 400);
    assert!(r.is_err() && t0.elapsed().as_secs() < 5);
    let pid = std::fs::read_to_string(fake.join("sleep.pid")).unwrap();
    assert!(process_gone(pid.trim()), "the child's child was killed");
    let _ = std::fs::remove_file(fake.join("sleep.pid"));
    let t0 = std::time::Instant::now();
    let r = convert(&t, "soffice", &slow, ss, sm, std::time::Duration::from_millis(500), &|| false);
    assert_eq!(r.unwrap_err(), "LibreOffice took too long to read this document");
    assert!(t0.elapsed().as_secs() < 5);
    let pid = std::fs::read_to_string(fake.join("sleep.pid")).unwrap();
    assert!(process_gone(pid.trim()));
    assert!(!cached(&slow, ss, sm).exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// A stand-in program `name` in `dir`: it appends its name and arguments, one per line
/// and a blank line after, to `calls.log`, then runs `body` (sh).
pub(crate) fn fake_tool(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\nD=\"$(dirname \"$0\")\"\n{{ echo {name}; for a; do printf '%s\\n' \"$a\"; done; echo; }} >> \"$D/calls.log\"\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The calls the stand-ins in `dir` received, each as its program and arguments.
pub(crate) fn fake_calls(dir: &Path) -> Vec<Vec<String>> {
    let log = std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default();
    log.split("\n\n").filter(|c| !c.trim().is_empty()).map(|c| c.lines().map(String::from).collect()).collect()
}

/// Set As Wallpaper picks each desktop's own way: Noctalia (and the Quickshell one before
/// it) over the compositor it runs on; GNOME's two pictures, Cinnamon, MATE, KDE, XFCE's
/// every monitor and workspace, sway's quoted command, swww and hyprpaper; nothing for a
/// desktop it doesn't know or a tool that isn't there. Then the programs are run
/// (stand-ins): an optional step may fail, a needed one says why, and hyprctl's refusal
/// printed with exit 0 counts as failing.
#[test]
fn wallpaper_each_desktop_its_way() {
    use crate::wallpaper::{Desk, Way, pick, set, steps, xfce_props};
    let desk = |de: &str, run: &[&str]| Desk { desktop: de.into(), running: run.iter().map(|s| s.to_string()).collect(), outputs: vec!["DP-1".into(), "eDP-1".into()] };
    let all = |_: &str| true;
    let has = |list: &'static [&'static str]| move |n: &str| list.contains(&n);
    assert_eq!(pick(&desk("Hyprland", &["Hyprland", "noctalia"]), all), Some(Way::Noctalia));
    assert_eq!(pick(&desk("Hyprland", &["noctalia"]), has(&["hyprctl"])), None, "noctalia not on PATH");
    assert_eq!(pick(&desk("niri", &["qs:noctalia-shell", "qs"]), all), Some(Way::NoctaliaQs));
    assert_eq!(pick(&desk("Hyprland", &["hyprpaper"]), all), Some(Way::Hyprpaper));
    assert_eq!(pick(&desk("Hyprland", &["hyprpaper", "swww-daemon"]), all), Some(Way::Swww));
    assert_eq!(pick(&desk("Hyprland", &[]), all), None, "nothing draws a wallpaper");
    assert_eq!(pick(&desk("sway", &[]), all), Some(Way::Sway));
    assert_eq!(pick(&desk("ubuntu:GNOME", &[]), all), Some(Way::Gnome));
    assert_eq!(pick(&desk("GNOME", &[]), has(&["swww"])), None, "no gsettings");
    assert_eq!(pick(&desk("X-Cinnamon", &[]), all), Some(Way::Cinnamon));
    assert_eq!(pick(&desk("MATE", &[]), all), Some(Way::Mate));
    assert_eq!(pick(&desk("KDE", &["swww-daemon"]), all), Some(Way::Kde));
    assert_eq!(pick(&desk("XFCE", &[]), all), Some(Way::Xfce));
    assert_eq!(pick(&desk("LXQt", &[]), all), None);
    assert_eq!(pick(&desk("", &[]), all), None);
    let p = Path::new("/home/ana/Pictures/My \"best\" shot.jpg");
    let plain = Path::new("/home/ana/Pictures/dunes.jpg");
    let args = |w: Way, p: &Path, xf: &str| -> Vec<Vec<String>> { steps(w, p, &desk("", &[]), xf).iter().map(|s| s.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()).collect() };
    assert_eq!(args(Way::Noctalia, plain, ""), [["noctalia", "msg", "wallpaper-set", "/home/ana/Pictures/dunes.jpg"]]);
    assert_eq!(args(Way::Noctalia, p, ""), [["noctalia", "msg", "wallpaper-set", "DP-1", p.to_str().unwrap()], ["noctalia", "msg", "wallpaper-set", "eDP-1", p.to_str().unwrap()]], "a path with a space goes to each output");
    assert_eq!(args(Way::NoctaliaQs, plain, ""), [["qs", "-c", "noctalia-shell", "ipc", "call", "wallpaper", "set", "/home/ana/Pictures/dunes.jpg", "all"]]);
    let uri = "file:///home/ana/Pictures/My%20%22best%22%20shot.jpg";
    assert_eq!(args(Way::Gnome, p, ""), [["gsettings", "set", "org.gnome.desktop.background", "picture-uri", uri], ["gsettings", "set", "org.gnome.desktop.background", "picture-uri-dark", uri]]);
    assert_eq!(args(Way::Cinnamon, p, ""), [["gsettings", "set", "org.cinnamon.desktop.background", "picture-uri", uri]]);
    assert_eq!(args(Way::Mate, p, ""), [["gsettings", "set", "org.mate.background", "picture-filename", p.to_str().unwrap()]]);
    assert_eq!(args(Way::Kde, p, ""), [["plasma-apply-wallpaperimage", p.to_str().unwrap()]]);
    assert_eq!(args(Way::Sway, p, ""), [["swaymsg", r#"output * bg "/home/ana/Pictures/My \"best\" shot.jpg" fill"#]]);
    assert_eq!(args(Way::Swww, plain, ""), [["swww", "img", "/home/ana/Pictures/dunes.jpg"]]);
    assert_eq!(args(Way::Hyprpaper, plain, ""), [["hyprctl", "hyprpaper", "preload", "/home/ana/Pictures/dunes.jpg"], ["hyprctl", "hyprpaper", "wallpaper", ",/home/ana/Pictures/dunes.jpg"]]);
    // xfconf-query -c xfce4-desktop -l, from a laptop with an external screen.
    let listing = "/backdrop/screen0/monitorHDMI-1/workspace0/color-style\n/backdrop/screen0/monitorHDMI-1/workspace0/image-style\n/backdrop/screen0/monitorHDMI-1/workspace0/last-image\n/backdrop/screen0/monitoreDP-1/workspace0/last-image\n/backdrop/screen0/monitoreDP-1/workspace1/last-image\n/backdrop/single-workspace-mode\n/desktop-icons/style\n";
    assert_eq!(xfce_props(listing), ["/backdrop/screen0/monitorHDMI-1/workspace0/last-image", "/backdrop/screen0/monitoreDP-1/workspace0/last-image", "/backdrop/screen0/monitoreDP-1/workspace1/last-image"]);
    assert_eq!(args(Way::Xfce, plain, listing).len(), 3);
    assert_eq!(args(Way::Xfce, plain, listing)[1], ["xfconf-query", "-c", "xfce4-desktop", "-p", "/backdrop/screen0/monitoreDP-1/workspace0/last-image", "-s", "/home/ana/Pictures/dunes.jpg"]);
    assert_eq!(args(Way::Xfce, plain, "")[0][3..6], ["-n", "-t", "string"], "no property yet: made");
    // Run through stand-ins.
    let d = tmpdir("wallpaper");
    let fake = d.join("tools");
    fake_tool(&fake, "gsettings", "case \"$3\" in picture-uri-dark) echo \"No such key\" >&2; exit 1;; esac");
    fake_tool(&fake, "xfconf-query", &format!("[ \"$3\" = -l ] && printf '{}'\nexit 0", listing.replace('\n', "\\n")));
    fake_tool(&fake, "hyprctl", "echo \"wallpaper failed (not preloaded)\"");
    fake_tool(&fake, "plasma-apply-wallpaperimage", "echo 'Unable to find the wallpaper plugin' >&2; exit 1");
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(fake.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    assert_eq!(set(&t, &desk("GNOME", &[]), p), Ok(Way::Gnome), "the dark picture is optional");
    assert_eq!(fake_calls(&fake).len(), 2);
    assert_eq!(set(&t, &desk("XFCE", &[]), plain), Ok(Way::Xfce));
    let calls = fake_calls(&fake);
    assert_eq!(calls.len(), 6, "listed, then each of three set");
    assert_eq!(calls[2], ["xfconf-query", "-c", "xfce4-desktop", "-l"]);
    assert_eq!(set(&t, &desk("Hyprland", &["hyprpaper"]), plain).unwrap_err(), "Couldn't set the wallpaper: wallpaper failed (not preloaded)");
    assert_eq!(set(&t, &desk("KDE", &[]), plain).unwrap_err(), "Couldn't set the wallpaper: Unable to find the wallpaper plugin");
    assert_eq!(set(&t, &desk("sway", &[]), plain).unwrap_err(), "Maia can't set the wallpaper on this desktop", "swaymsg isn't there");
    assert!(crate::wallpaper::is_wallpaper("jpg") && crate::wallpaper::is_wallpaper("webp") && !crate::wallpaper::is_wallpaper("gif") && !crate::wallpaper::is_wallpaper("svg"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Email: xdg-email with each file attached; without it, the mail app's own way for
/// Thunderbird (and a flatpak one), Evolution, KMail and Claws Mail, and nothing for one
/// that can't take attachments; no mail app at all is said so and nothing starts.
#[test]
fn email_attaches_files() {
    use crate::sendto::{MAIL_APP, email, email_args};
    let f = [PathBuf::from("/home/ana/Q3 report.pdf"), PathBuf::from("/home/ana/photo,1.jpg")];
    let s = |v: Option<Vec<std::ffi::OsString>>| v.map(|v| v.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>());
    assert_eq!(s(email_args(&f, true, None)).unwrap(), ["xdg-email", "--attach", "/home/ana/Q3 report.pdf", "--attach", "/home/ana/photo,1.jpg"]);
    let tb = ["thunderbird", "-compose", "attachment='file:///home/ana/Q3%20report.pdf,file:///home/ana/photo%2C1.jpg'"];
    assert_eq!(s(email_args(&f, false, Some("thunderbird %u"))).unwrap(), tb);
    assert_eq!(s(email_args(&f, false, Some("/usr/bin/flatpak run --branch=stable org.mozilla.Thunderbird %u"))).unwrap()[..5], ["/usr/bin/flatpak", "run", "--branch=stable", "org.mozilla.Thunderbird", "-compose"]);
    assert_eq!(s(email_args(&f, false, Some("evolution %U"))).unwrap(), ["evolution", "mailto:?attach=file:///home/ana/Q3%20report.pdf&attach=file:///home/ana/photo%2C1.jpg"]);
    assert_eq!(s(email_args(&f[..1], false, Some("kmail -caption \"%c\" %u"))).unwrap()[3..], ["--attach", "file:///home/ana/Q3%20report.pdf"]);
    assert_eq!(s(email_args(&f[..1], false, Some("claws-mail --compose %u"))).unwrap(), ["claws-mail", "--compose", "--attach", "/home/ana/Q3 report.pdf"]);
    assert_eq!(email_args(&f, false, Some("firefox %u")), None, "a browser can't attach files");
    assert_eq!(email_args(&f, false, None), None);
    let d = tmpdir("email");
    let fake = d.join("tools");
    fake_tool(&fake, "xdg-email", "exit 0");
    crate::drives::FAKE.with(|x| *x.borrow_mut() = Some(fake.clone()));
    let n = crate::mime::LAUNCHED.lock().unwrap().len();
    assert_eq!(email(&f).unwrap_err(), "No mail app is set up");
    MAIL_APP.with(|m| *m.borrow_mut() = Some("thunderbird %u".into()));
    email(&f).unwrap();
    let launched = crate::mime::LAUNCHED.lock().unwrap().clone();
    assert!(launched[n..].iter().any(|l| l.contains("xdg-email") && l.contains("\"--attach\" \"/home/ana/Q3 report.pdf\" \"--attach\" \"/home/ana/photo,1.jpg\"")), "{:?}", &launched[n..]);
    // No xdg-email: Thunderbird itself.
    crate::drives::FAKE.with(|x| *x.borrow_mut() = Some(d.join("none")));
    email(&f).unwrap();
    assert!(crate::mime::LAUNCHED.lock().unwrap()[n..].iter().any(|l| l.starts_with("\"thunderbird\" \"-compose\"")));
    MAIL_APP.with(|m| *m.borrow_mut() = Some("firefox %u".into()));
    assert_eq!(email(&f).unwrap_err(), "Install xdg-utils to attach files to an email");
    MAIL_APP.with(|m| *m.borrow_mut() = None);
    crate::drives::FAKE.with(|x| *x.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&d);
}

/// Print: CUPS's printers read from lpstat (names, and the default when it is one of
/// them), lp's arguments, which files are offered, and the printers read through a
/// stand-in lpstat, under the C locale.
#[test]
fn print_reads_cups_printers() {
    use crate::sendto::{Printers, cups, lp_args, parse_printers, printable, printers};
    let e = "Office_Laser\nHP_ENVY_6000\nPDF\n";
    assert_eq!(parse_printers(e, "system default destination: HP_ENVY_6000\n"), Printers { names: vec!["Office_Laser".into(), "HP_ENVY_6000".into(), "PDF".into()], default: Some("HP_ENVY_6000".into()) });
    assert_eq!(parse_printers(e, "no system default destination\n").default, None);
    assert_eq!(parse_printers("", "no system default destination\n"), Printers::default());
    assert_eq!(parse_printers("lpstat: No destinations added.\n", "").names, Vec::<String>::new(), "a complaint is no printer");
    assert_eq!(parse_printers("A\n", "system default destination: Gone\n").default, None);
    let a: Vec<String> = lp_args("Office_Laser", 2, &[PathBuf::from("/x/a b.pdf"), PathBuf::from("/x/c.png")]).iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(a, ["-d", "Office_Laser", "-n", "2", "/x/a b.pdf", "/x/c.png"]);
    for (n, x) in [("a.pdf", "pdf"), ("b.PS", "ps"), ("c.jpg", "jpg"), ("notes.txt", "txt"), ("main.rs", "rs"), ("README.md", "md")] {
        assert!(printable(n, x), "{n}");
    }
    for (n, x) in [("a.docx", "docx"), ("b.zip", "zip"), ("c.mp4", "mp4"), ("d.ods", "ods")] {
        assert!(!printable(n, x), "{n}");
    }
    let d = tmpdir("print");
    let fake = d.join("tools");
    fake_tool(&fake, "lpstat", "[ \"$LC_ALL\" = C ] || exit 3\ncase \"$1\" in -e) printf 'Office_Laser\\nHP_ENVY_6000\\n';; -d) echo 'system default destination: HP_ENVY_6000';; esac");
    crate::drives::FAKE.with(|x| *x.borrow_mut() = Some(fake.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|x| *x.borrow_mut() = None);
    assert!(!cups(&t), "lp missing");
    fake_tool(&fake, "lp", "exit 0");
    assert!(cups(&t));
    assert_eq!(printers(&t).default.as_deref(), Some("HP_ENVY_6000"));
    assert_eq!(printers(&t).names.len(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

/// `jpeg` with an EXIF block saying it is stored turned (`orient`, 1 to 8), as a camera
/// held upright writes it.
pub(crate) fn with_exif_orientation(jpeg: &[u8], orient: u16) -> Vec<u8> {
    let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08".to_vec();
    tiff.extend_from_slice(&1u16.to_be_bytes());
    tiff.extend_from_slice(&0x0112u16.to_be_bytes());
    tiff.extend_from_slice(&3u16.to_be_bytes());
    tiff.extend_from_slice(&1u32.to_be_bytes());
    tiff.extend_from_slice(&orient.to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut app1 = b"Exif\x00\x00".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// The real ImageMagick on the PATH, as a stand-in folder holding only it (and none).
pub(crate) fn imagemagick_tools(dir: &Path) -> Option<PathBuf> {
    let real = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("magick")).find(|m| m.is_file()))?;
    std::fs::create_dir_all(dir).unwrap();
    let _ = std::os::unix::fs::symlink(real, dir.join("magick"));
    Some(dir.to_path_buf())
}

/// Picture fixes through the real ImageMagick on real files: a photo stored turned (EXIF
/// orientation 6) measured upright, turned right in place with its original in the
/// trash, turned again without a second copy there, and put back by Undo byte for
/// byte; copies at half, within a box and a longer side, named by their size and never
/// over another file; a smaller picture left alone by Fit; a crop of the upright
/// picture; a name ImageMagick would misread; a broken file refused with nothing
/// changed. Sizes as typed.
#[test]
fn picture_fixes_with_imagemagick() {
    use crate::picfix::{Size, crop, crop_px, cropped_name, dims, magick, parse_size, resize, resized_name, rotate, target, undo_replaced};
    assert_eq!(parse_size("1280"), Some(Size::Long(1280)));
    for s in ["1280 × 720", "1280x720", "1280 X 720", "1280*720", " 1280 720 "] {
        assert_eq!(parse_size(s), Some(Size::Within(1280, 720)), "{s:?}");
    }
    assert_eq!(parse_size("50%"), Some(Size::Scale(0.5)));
    assert_eq!(parse_size("12.5 %"), Some(Size::Scale(0.125)));
    for s in ["", "0", "big", "1x2x3", "-5", "40000", "0%"] {
        assert_eq!(parse_size(s), None, "{s:?}");
    }
    assert_eq!(target([4032, 3024], Size::Fit(1920)), Some([1920, 1440]));
    assert_eq!(target([1000, 800], Size::Fit(1920)), None, "already within");
    assert_eq!(target([4032, 3024], Size::Scale(0.25)), Some([1008, 756]));
    assert_eq!(target([300, 200], Size::Long(600)), Some([600, 400]), "a longer side asked for grows it");
    assert_eq!(target([300, 200], Size::Within(100, 100)), Some([100, 67]));
    assert_eq!(target([300, 200], Size::Scale(1.0)), None);
    assert_eq!(resized_name("photo.jpg", [1920, 1440]), "photo (1920 px).jpg");
    assert_eq!(resized_name("scan.v2.png", [600, 800]), "scan.v2 (800 px).png");
    assert_eq!(cropped_name("photo.jpg"), "photo (cropped).jpg");
    assert_eq!(cropped_name("README"), "README (cropped)");
    assert_eq!(crop_px(egui::Rect::from_min_max(egui::pos2(0.25, 0.0), egui::pos2(0.75, 0.5)), [20, 40]), Some([5, 0, 10, 20]));
    assert_eq!(crop_px(egui::Rect::from_min_max(egui::pos2(0.5, 0.5), egui::pos2(0.51, 0.9)), [20, 40]), None, "under a pixel wide");
    let d = tmpdir("picfix");
    let Some(fake) = imagemagick_tools(&d.join("tools")) else {
        eprintln!("picture fixes: no ImageMagick, skipped");
        return;
    };
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(fake.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.join("none")));
    assert_eq!(magick(&crate::drives::Tools::current().unwrap()), None);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let prog = magick(&t).unwrap();
    // A photo 40 × 20 as stored, upright 20 × 40.
    let raw = d.join("raw.jpg");
    assert!(std::process::Command::new(fake.join("magick")).args(["-size", "40x20", "xc:#c84"]).arg(&raw).status().unwrap().success());
    let photo = d.join("photo.jpg");
    let original = with_exif_orientation(&std::fs::read(&raw).unwrap(), 6);
    std::fs::write(&photo, &original).unwrap();
    let shown = |p: &Path| crate::imgdec::load(p, 64).map(|x| x.1);
    assert_eq!(shown(&photo), Some([20, 40]));
    assert_eq!(dims(&t, prog, &photo).unwrap(), [20, 40], "measured upright");
    let trashed = |p: &Path| crate::trashcan::list().into_iter().filter(|it| it.original == p).count();
    rotate(&t, prog, &photo, true, false).unwrap();
    assert_eq!(shown(&photo), Some([40, 20]), "turned right from upright, no orientation left to apply twice");
    assert_eq!(trashed(&photo), 1, "the original kept in the trash");
    rotate(&t, prog, &photo, true, true).unwrap();
    assert_eq!(shown(&photo), Some([20, 40]));
    assert_eq!(trashed(&photo), 1, "a second turn keeps no second copy");
    assert!(!d.join(".photo.jpg.part").exists());
    assert_eq!(undo_replaced(&[photo.clone()]), 0);
    assert_eq!(std::fs::read(&photo).unwrap(), original, "the original back, byte for byte");
    // Copies.
    let big = d.join("big.png");
    std::fs::write(&big, png_bytes(400, 300, 3)).unwrap();
    let half = resize(&t, prog, &big, Size::Scale(0.5)).unwrap().unwrap();
    assert_eq!(half, d.join("big (200 px).png"));
    assert_eq!(shown(&half), Some([200, 150]));
    assert_eq!(resize(&t, prog, &big, Size::Fit(1920)).unwrap(), None);
    let boxed = resize(&t, prog, &big, Size::Within(100, 100)).unwrap().unwrap();
    assert_eq!((boxed.file_name().unwrap().to_str().unwrap(), shown(&boxed)), ("big (100 px).png", Some([100, 75])));
    let again = resize(&t, prog, &big, Size::Scale(0.5)).unwrap().unwrap();
    assert_eq!(again, d.join("big (200 px) (1).png"), "never over the first copy");
    let long = resize(&t, prog, &photo, Size::Long(80)).unwrap().unwrap();
    assert_eq!((long.file_name().unwrap().to_str().unwrap(), shown(&long)), ("photo (80 px).jpg", Some([40, 80])), "resized upright");
    // A crop of the upright picture.
    let c = crop(&t, prog, &photo, egui::Rect::from_min_max(egui::pos2(0.25, 0.0), egui::pos2(0.75, 0.5))).unwrap();
    assert_eq!((c.file_name().unwrap().to_str().unwrap(), shown(&c)), ("photo (cropped).jpg", Some([10, 20])));
    // A name with brackets and a colon.
    let odd = d.join("shot [1]: png.png");
    std::fs::write(&odd, png_bytes(30, 10, 1)).unwrap();
    rotate(&t, prog, &odd, false, false).unwrap();
    assert_eq!(shown(&odd), Some([10, 30]));
    // Broken: refused, nothing changed or trashed.
    let broken = d.join("broken.png");
    std::fs::write(&broken, b"\x89PNG\r\n\x1a\nnot really").unwrap();
    assert!(rotate(&t, prog, &broken, true, false).is_err());
    assert_eq!(std::fs::read(&broken).unwrap(), b"\x89PNG\r\n\x1a\nnot really");
    assert_eq!(trashed(&broken), 0);
    assert!(!d.join(".broken.png.part").exists());
    assert!(resize(&t, prog, &broken, Size::Scale(0.5)).is_err());
    for it in crate::trashcan::list().into_iter().filter(|it| it.original.starts_with(&d)) {
        let _ = crate::trashcan::purge(&it);
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Two clones of one bare repository, each with its own identity and no signing.
fn sync_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let d = tmpdir(name);
    std::fs::create_dir_all(d.join("server.git")).unwrap();
    git(&d.join("server.git"), &["init", "-q", "--bare", "-b", "main"]);
    let a = d.join("a");
    std::fs::create_dir_all(&a).unwrap();
    work_repo(&a);
    touch(&a.join("notes.txt"), "one\ntwo\nthree\n");
    git(&a, &["add", "-A"]);
    git(&a, &["commit", "-qm", "Start"]);
    git(&a, &["remote", "add", "origin", d.join("server.git").to_str().unwrap()]);
    git(&a, &["push", "-q", "-u", "origin", "main"]);
    git(&d, &["clone", "-q", d.join("server.git").to_str().unwrap(), "b"]);
    let b = d.join("b");
    for (k, v) in [("user.name", "Test"), ("user.email", "test@example.invalid"), ("commit.gpgsign", "false"), ("core.hooksPath", ".git/hooks")] {
        git(&b, &["config", k, v]);
    }
    (d, a, b)
}

/// Fetch, pull and push against a real bare repository: what came in counted, a pull
/// that can't fast-forward and a push the remote refuses each handed back to be asked
/// about, then merged, rebased or replaced; a new branch published to origin.
#[test]
fn fetch_pull_and_push() {
    use crate::gitsync::{BEHIND, DIVERGED};
    use crate::gitwork::{Op, run};
    let (d, a, b) = sync_fixture("sync");
    let commit = |at: &Path, file: &str, text: &str, msg: &str| {
        touch(&at.join(file), text);
        git(at, &["add", "-A"]);
        git(at, &["commit", "-qm", msg]);
    };
    commit(&a, "a.txt", "from a\n", "A's work");
    assert_eq!(run(&a, &Op::Push(false)).unwrap(), "Pushed 1 commit to origin/main");
    assert_eq!(run(&a, &Op::Push(false)).unwrap(), "Nothing to push");
    assert_eq!(run(&b, &Op::Fetch).unwrap(), "Fetched: 1 commit to pull");
    assert_eq!(run(&b, &Op::Pull(None)).unwrap(), "Pulled 1 commit");
    assert!(b.join("a.txt").exists());
    assert_eq!(run(&b, &Op::Pull(None)).unwrap(), "Already up to date");
    // Both move on: the pull asks, the push asks.
    commit(&a, "a2.txt", "more from a\n", "More from A");
    run(&a, &Op::Push(false)).unwrap();
    commit(&b, "b.txt", "from b\n", "B's work");
    assert_eq!(run(&b, &Op::Pull(None)).unwrap_err(), DIVERGED);
    assert_eq!(run(&b, &Op::Push(false)).unwrap_err(), BEHIND);
    assert_eq!(run(&b, &Op::Pull(Some(true))).unwrap(), "Rebased onto origin/main");
    assert!(b.join("a2.txt").exists() && b.join("b.txt").exists());
    assert_eq!(run(&b, &Op::Push(false)).unwrap(), "Pushed 1 commit to origin/main");
    // Merged instead of rebased.
    commit(&b, "b2.txt", "x\n", "B again");
    run(&b, &Op::Push(false)).unwrap();
    commit(&a, "a3.txt", "y\n", "A again");
    assert_eq!(run(&a, &Op::Pull(None)).unwrap_err(), DIVERGED);
    assert_eq!(run(&a, &Op::Pull(Some(false))).unwrap(), "Merged origin/main");
    let parents = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&a).args(["log", "-1", "--format=%P"]).output().unwrap().stdout).unwrap();
    assert_eq!(parents.split_whitespace().count(), 2, "a merge commit");
    // Replacing what the remote has, after rewriting history it already holds.
    run(&a, &Op::Push(false)).unwrap();
    git(&a, &["commit", "-q", "--amend", "-m", "A again, reworded"]);
    assert_eq!(run(&a, &Op::Push(false)).unwrap_err(), BEHIND);
    assert_eq!(run(&a, &Op::Push(true)).unwrap(), "Pushed, replacing origin/main");
    // A new branch has no upstream: Push publishes it, Pull says why it can't.
    git(&a, &["switch", "-q", "-c", "topic"]);
    assert_eq!(run(&a, &Op::Pull(None)).unwrap_err(), "topic doesn't track a remote branch yet. Push publishes it.");
    assert_eq!(run(&a, &Op::Push(false)).unwrap(), "Published topic to origin");
    assert_eq!(crate::gitsync::upstream(&a).as_deref(), Some("origin/topic"));
    // Somewhere it can't reach: said plainly.
    git(&a, &["remote", "set-url", "origin", d.join("gone.git").to_str().unwrap()]);
    assert_eq!(run(&a, &Op::Fetch).unwrap_err(), "The remote isn't there, or you can't see it.");
    let _ = std::fs::remove_dir_all(&d);
}

/// Conflicts read from the text git leaves (with or without a base section) and
/// settled one at a time, keeping one side, the other, or both, the rest untouched.
#[test]
fn conflicts_read_and_settled() {
    use crate::gitsync::{Side, conflicts, resolve};
    let text = "top\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs 1\n>>>>>>> topic\nmiddle\n<<<<<<< HEAD\nmine\n||||||| base\nold\n=======\nyours\n>>>>>>> 1a2b3c4 (Fix it)\nend\n";
    let c = conflicts(text);
    assert_eq!(c.len(), 2);
    assert_eq!((c[0].ours.as_str(), c[0].theirs.as_str(), c[0].line), ("HEAD", "topic", 2));
    assert_eq!((c[0].ours_lines.clone(), c[0].theirs_lines.clone()), (vec!["ours 1".to_string(), "ours 2".into()], vec!["theirs 1".to_string()]));
    assert_eq!((c[1].ours_lines.clone(), c[1].theirs_lines.clone()), (vec!["mine".to_string()], vec!["yours".to_string()]), "the base section left out");
    assert_eq!(c[1].theirs, "1a2b3c4 (Fix it)");
    let first = resolve(text, 0, Side::Theirs).unwrap();
    assert!(first.starts_with("top\ntheirs 1\nmiddle\n<<<<<<< HEAD\n"), "{first}");
    assert_eq!(conflicts(&first).len(), 1);
    assert_eq!(resolve(&first, 0, Side::Both).unwrap(), "top\ntheirs 1\nmiddle\nmine\nyours\nend\n");
    assert_eq!(resolve(text, 1, Side::Ours).unwrap(), text.replace("<<<<<<< HEAD\nmine\n||||||| base\nold\n=======\nyours\n>>>>>>> 1a2b3c4 (Fix it)\n", "mine\n"));
    assert!(resolve(text, 2, Side::Ours).is_none(), "no third conflict");
    assert!(conflicts("no markers\n======= alone\n").is_empty());
}

/// A pull that stops at conflicts: they are shown in place of the file's diff, the
/// file can't be staged with its markers in, settling the last one stages it, and
/// Continue makes the merge commit; another one, aborted, goes back to before.
#[test]
fn merge_conflicts_settled_and_continued() {
    use crate::gitsync::{CONFLICTS, Doing, Side, doing};
    use crate::gitwork::{Op, file_diff, run, work};
    let (d, a, b) = sync_fixture("conflicts");
    touch(&a.join("notes.txt"), "one\nTWO from a\nthree\n");
    git(&a, &["commit", "-qam", "A edits"]);
    run(&a, &Op::Push(false)).unwrap();
    touch(&b.join("notes.txt"), "one\ntwo from b\nthree\n");
    git(&b, &["commit", "-qam", "B edits"]);
    assert_eq!(run(&b, &Op::Pull(Some(false))).unwrap_err(), CONFLICTS);
    assert_eq!(doing(&b), Some(Doing::Merge));
    let w = work(&b).unwrap();
    assert_eq!(w.doing, Some(Doing::Merge));
    let f = w.unstaged.iter().find(|c| c.path == "notes.txt").unwrap().clone();
    assert_eq!(f.status, b'U');
    let diff = file_diff(&b, &f, false, "HEAD");
    assert_eq!(diff.conflicts.len(), 1);
    assert_eq!((diff.conflicts[0].ours_lines.clone(), diff.conflicts[0].theirs_lines.clone()), (vec!["two from b".to_string()], vec!["TWO from a".to_string()]));
    assert_eq!(run(&b, &Op::Stage(vec!["notes.txt".into()])).unwrap_err(), "It still has conflict markers: settle them first.");
    assert_eq!(run(&b, &Op::Continue).unwrap_err(), "Some files are still in conflict.");
    assert_eq!(run(&b, &Op::Settle { path: "notes.txt".into(), at: 0, side: Side::Both }).unwrap(), "Resolved notes.txt");
    assert_eq!(std::fs::read_to_string(b.join("notes.txt")).unwrap(), "one\ntwo from b\nTWO from a\nthree\n");
    assert_eq!(run(&b, &Op::Continue).unwrap(), "Finished the merge");
    assert_eq!(doing(&b), None);
    // Another one, given up on.
    touch(&a.join("notes.txt"), "one\nagain from a\nthree\n");
    git(&a, &["commit", "-qam", "A again"]);
    run(&a, &Op::Push(false)).unwrap();
    touch(&b.join("notes.txt"), "one\nagain from b\nthree\n");
    git(&b, &["commit", "-qam", "B again"]);
    let before = std::fs::read_to_string(b.join("notes.txt")).unwrap();
    assert_eq!(run(&b, &Op::Pull(Some(true))).unwrap_err(), CONFLICTS);
    assert_eq!(doing(&b), Some(Doing::Rebase));
    assert_eq!(run(&b, &Op::Abort).unwrap(), "Stopped rebasing and went back");
    assert_eq!((doing(&b), std::fs::read_to_string(b.join("notes.txt")).unwrap()), (None, before));
    let _ = std::fs::remove_dir_all(&d);
}

/// Show Only Changed Files: the list keeps what git marks as changed and the folders
/// with changes inside, and everything comes back when it is turned off.
#[test]
fn only_changed_files_listed() {
    let d = tmpdir("only-changed");
    work_repo(&d);
    for f in ["src/app.rs", "src/lib.rs", "docs/guide.md", "README.md", "LICENSE"] {
        touch(&d.join(f), "x\n");
    }
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "Start"]);
    touch(&d.join("src/lib.rs"), "y\n");
    touch(&d.join("NEW.txt"), "new\n");
    let r = crate::gitstatus::query(&d).expect("a repository");
    let stats = HashMap::new();
    let mut t = Tab::new(1, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let names = |t: &Tab| t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&t), ["docs", "src", "LICENSE", "NEW.txt", "README.md"]);
    t.git_only = Some(std::sync::Arc::new(r.marks.keys().cloned().collect()));
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(names(&t), ["src", "NEW.txt"]);
    t.git_only = None;
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(names(&t).len(), 5);
}

/// Formulas found in Markdown as GitHub writes them: `$…$` in a sentence (but not
/// "$5 and $10"), `$$…$$` and `\(…\)`, blocks between `$$` or `\[` lines and fenced
/// as `math`; code spans and escaped dollars left alone.
#[test]
fn formulas_found_in_markdown() {
    use crate::markdown::{Block, MATH, parse};
    use crate::texmath::{block_at, inline_at};
    assert_eq!(inline_at("$x^2$ is", 0), Some((5, "x^2".into(), false)));
    assert_eq!(inline_at("costs $5 and $10", 6), None, "money");
    assert_eq!(inline_at("$ x$", 0), None, "a space after the opening one");
    assert_eq!(inline_at("$x $", 0), None, "a space before the closing one");
    assert_eq!(inline_at("$a\\$b$", 0), Some((6, "a\\$b".into(), false)), "an escaped dollar inside");
    assert_eq!(inline_at("$$\\sum_i x_i$$", 0), Some((14, "\\sum_i x_i".into(), true)));
    assert_eq!(inline_at("\\(\\alpha\\)", 0), Some((10, "\\alpha".into(), false)));
    let lines = ["$$", "\\int_0^1 x\\,dx", "= \\tfrac12", "$$", "after"];
    assert_eq!(block_at(&lines, 0), Some(("\\int_0^1 x\\,dx\n= \\tfrac12".into(), 4)));
    assert_eq!(block_at(&["\\[ e^{i\\pi} + 1 = 0 \\]"], 0), Some(("e^{i\\pi} + 1 = 0".into(), 1)));
    assert_eq!(block_at(&["$$ unclosed"], 0), None);
    let md: Vec<String> = "Energy is $E=mc^2$, not `$code$`, and \\$4.\n\n$$\na^2+b^2=c^2\n$$\n\n```math\n\\frac{1}{2}\n```\n".lines().map(String::from).collect();
    let doc = parse(&md, None);
    assert_eq!(doc.maths, vec![("E=mc^2".to_string(), false), ("a^2+b^2=c^2".into(), true), ("\\frac{1}{2}".into(), true)]);
    let Block::Para(runs) = &doc.blocks[0] else { panic!("{:?}", doc.blocks[0]) };
    let m: Vec<(&str, u32)> = runs.iter().filter(|r| r.st & MATH != 0).map(|r| (r.text.as_str(), r.link)).collect();
    assert_eq!(m, [("E=mc^2", 1)]);
    let text: String = runs.iter().filter(|r| r.st & MATH == 0).map(|r| r.text.as_str()).collect();
    assert!(text.contains("$code$") && text.ends_with("and $4."), "{text}");
    assert!(matches!(doc.blocks[1], Block::Math(2)) && matches!(doc.blocks[2], Block::Math(3)));
    assert_eq!(crate::texmath::depths("Preview: Snippet 1 ended.(368640+1505299x1347574).\nother\nPreview: Snippet 2 ended.(10+0x5)."), vec![(368640.0, 1505299.0), (10.0, 0.0)]);
}

/// A stand-in for pdflatex: a real PDF with a page per formula and a log in the preview
/// package's words; a formula with `\broken` in it fails as TeX does. It notes each run.
pub(crate) fn fake_tex(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for n in 1..=4 {
        std::fs::write(dir.join(format!("pages-{n}.pdf")), crate::sim::sized_pdf(&vec!["0 0 0"; n], [40, 14], [1, 1, 38, 12])).unwrap();
    }
    let script = format!(
        "#!/bin/sh\necho run >> {calls}\nif grep -q 'broken' f.tex; then echo '! Undefined control sequence.' > f.log; exit 1; fi\nn=$(grep -c 'begin{{preview}}' f.tex)\ncp {dir}/pages-$n.pdf f.pdf\n: > f.log\ni=1\nwhile [ $i -le $n ]; do echo \"Preview: Snippet $i ended.(458752+196608x1000000).\" >> f.log; i=$((i+1)); done\n",
        calls = dir.join("calls").display(),
        dir = dir.display()
    );
    std::fs::write(dir.join("pdflatex"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("pdflatex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::os::unix::fs::symlink("/usr/bin/pdftocairo", dir.join("pdftocairo"));
}

/// Formulas typeset through a TeX and poppler's pdftocairo: one run for them all, each
/// a white picture with its depth from the log; one TeX can't read shows why while the
/// rest are typeset; the second time they come from the cache without TeX.
#[test]
fn formulas_typeset_through_tex_and_poppler() {
    use crate::texmath::{Engine, engine, typeset};
    if !crate::media::have("pdftocairo") {
        return;
    }
    let d = tmpdir("texmath");
    fake_tex(&d.join("tools"));
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.join("tools")));
    let tools = crate::drives::Tools::current().unwrap();
    assert_eq!(engine(&tools), Some(Engine::Pdflatex));
    // Unique to this run, so nothing comes from an earlier one's cache.
    let n = format!("{:?}", std::time::Instant::now());
    let items = vec![(format!("x^2 % {n}"), false), (format!("\\int_0^1 f % {n}"), true)];
    let got = typeset(&tools, Engine::Pdflatex, &items, &|| false);
    let runs = || std::fs::read_to_string(d.join("tools/calls")).unwrap_or_default().lines().count();
    assert_eq!(runs(), 1, "one TeX run for the lot");
    for g in &got {
        let t = g.as_ref().unwrap();
        assert!(t.img.w > 100 && t.img.h > 30);
        assert!(t.img.px.chunks(4).all(|p| p[..3] == [255, 255, 255]), "white, to be tinted");
        assert!(t.img.px.chunks(4).any(|p| p[3] == 255), "with its ink solid");
        // Depth from the log: (3 pt + 1 pt border) of (7 + 3 + 2 border) pt.
        let share = t.depth / t.img.h as f32;
        assert!((share - 4.0 / 12.0).abs() < 0.01, "{share}");
    }
    // From the cache: no TeX.
    let again = typeset(&tools, Engine::Pdflatex, &items, &|| false);
    assert!(again.iter().all(|g| g.is_ok()));
    assert_eq!(runs(), 1);
    // One broken among good ones: the batch fails, each is tried alone.
    let mixed = vec![(format!("a+b % {n}"), false), (format!("\\broken % {n}"), false)];
    let got = typeset(&tools, Engine::Pdflatex, &mixed, &|| false);
    assert!(got[0].is_ok());
    assert_eq!(got[1].as_ref().err().map(String::as_str), Some("TeX: Undefined control sequence"));
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&d);
}

/// The palette's search over the real commands: a whole title first, words started in
/// any order, letters from the starts of words, the category's name, an alias typed in
/// full ahead of everything; scattered letters find nothing.
#[test]
fn palette_finds_commands() {
    use crate::commands::Cmd;
    use crate::popups::find_commands;
    let mut ud = crate::config::UserData::with_defaults();
    let find = |ud: &crate::config::UserData, q: &str| find_commands(ud, q);
    assert_eq!(find(&ud, "Toggle Hidden Files").first(), Some(&Cmd::ToggleHidden));
    assert_eq!(find(&ud, "hid tog"), vec![Cmd::ToggleHidden]);
    assert_eq!(find(&ud, "tohi").first(), Some(&Cmd::ToggleHidden));
    assert_eq!(&find(&ud, "sort size")[..3], &[Cmd::SortSize, Cmd::SortSizeAsc, Cmd::SortSizeDesc]);
    let git = find(&ud, "git");
    assert_eq!(&git[..3], &[Cmd::GitBranches, Cmd::GitHistory, Cmd::GitChanges]);
    assert!(!git.contains(&Cmd::Quit), "no G from General and I, T from Quit");
    // Every View command answers to the category's name, after the titles with the word.
    let view = find(&ud, "view");
    assert!(view[..7].iter().all(|c| c.info().title.starts_with("View ")) && view[..7].contains(&Cmd::ViewDetails));
    assert!(view.contains(&Cmd::ZoomIn));
    assert!(find(&ud, "zqx").is_empty());
    crate::popups::set_alias(&mut ud, Cmd::ToggleSidebar, "sb").unwrap();
    assert_eq!(find(&ud, "SB").first(), Some(&Cmd::ToggleSidebar));
}

/// Recent Projects on a real home folder: repositories found at any sensible depth, a
/// work tree's `.git` file followed to its branch, nothing found inside a repository,
/// in hidden or installed-package folders, through links or deeper than five levels;
/// the branch read from HEAD, detached ones as a short commit; the last visited first,
/// then by git's own activity; the list kept in the cache and read back.
#[test]
fn projects_found_ranked_and_cached() {
    use crate::projects::{branch_of, find, load_cache, rank, read, save_cache};
    let h = tmpdir("projects");
    let repo = |p: &str, head: &str| {
        touch(&h.join(p).join(".git/HEAD"), head);
        touch(&h.join(p).join(".git/index"), "");
    };
    repo("Code/maia", "ref: refs/heads/main\n");
    repo("Work/client/site", "3f2a9c41d7e8b0a1c2d3e4f5a6b7c8d9e0f1a2b3\n");
    repo("Code/maia/vendor/lib", "ref: refs/heads/main\n");
    repo(".hidden/secret", "ref: refs/heads/main\n");
    touch(&h.join("web/package.json"), "{}\n");
    repo("web/node_modules/pkg", "ref: refs/heads/main\n");
    repo("a/b/c/d/e/f/deep", "ref: refs/heads/main\n");
    repo("src/github.com/ana/tool", "ref: refs/heads/fix/menu\n");
    // A second work tree of maia: its .git is a file naming the real one.
    touch(&h.join("Code/maia/.git/worktrees/spike/HEAD"), "ref: refs/heads/spike\n");
    touch(&h.join("Code/spike/.git"), &format!("gitdir: {}\n", h.join("Code/maia/.git/worktrees/spike").display()));
    std::os::unix::fs::symlink(h.join("Code"), h.join("Code link")).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut found: Vec<String> = find(&h, &stop).iter().map(|p| crate::project::relative(p, &h)).collect();
    found.sort();
    assert_eq!(found, ["Code/maia", "Code/spike", "Work/client/site", "src/github.com/ana/tool"]);
    assert_eq!(branch_of("ref: refs/heads/fix/menu\n"), "fix/menu");
    assert_eq!(branch_of("3f2a9c41d7e8b0a1c2d3e4f5a6b7c8d9e0f1a2b3\n"), "3f2a9c4");
    assert_eq!(read(&h.join("Code/spike")).unwrap().branch, "spike", "a work tree's own branch");
    assert_eq!(read(&h.join("Work/client/site")).unwrap().branch, "3f2a9c4");
    assert!(read(&h.join("web")).is_none());
    // Activity: the site was committed to last; maia an hour ago, the tool a day ago.
    let ago = |p: &str, secs: u64| {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs);
        for f in ["HEAD", "index"] {
            let f = std::fs::File::options().write(true).open(h.join(p).join(".git").join(f));
            if let Ok(f) = f {
                f.set_modified(t).unwrap();
            }
        }
    };
    ago("Code/maia", 3600);
    ago("src/github.com/ana/tool", 86_400);
    ago("Work/client/site", 60);
    std::fs::File::options().write(true).open(h.join("Code/maia/.git/worktrees/spike/HEAD")).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(7 * 86_400)).unwrap();
    let all: Vec<_> = find(&h, &stop).iter().filter_map(|p| read(p)).collect();
    let names = |v: &[crate::projects::Project]| v.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&rank(all.clone(), &[])), ["site", "maia", "tool", "spike"], "by activity");
    // Visited: the tool's subfolder last, maia before it; the rest by activity after them.
    let recents = vec![h.join("src/github.com/ana/tool/src"), h.join("Documents"), h.join("Code/maia")];
    assert_eq!(names(&rank(all.clone(), &recents)), ["tool", "maia", "site", "spike"]);
    // The cache: the roots, in order, read back.
    let cache = h.join("cache/maia/projects.txt");
    let roots: Vec<PathBuf> = rank(all, &recents).iter().map(|p| p.root.clone()).collect();
    save_cache(&cache, &roots).unwrap();
    assert_eq!(load_cache(&cache), roots);
    assert!(load_cache(&h.join("cache/none.txt")).is_empty());
    let _ = std::fs::remove_dir_all(&h);
}

/// Unsaved work asked of git itself: a clean work tree has none; an untracked file, an
/// edit and a staged file each count; commits not pushed to its remote and stashes count
/// too, and a commit to pull is told apart from one to push.
#[test]
fn projects_unsaved_work_asked_of_git() {
    use crate::projects::{Status, parse_status, status};
    if !crate::media::have("git") {
        return;
    }
    let base = tmpdir("project-changes");
    let (server, d, other) = (base.join("server.git"), base.join("work"), base.join("other"));
    std::fs::create_dir_all(&server).unwrap();
    git(&server, &["init", "-q", "--bare", "-b", "main"]);
    std::fs::create_dir_all(&d).unwrap();
    touch(&d.join("README"), "hello\n");
    git(&d, &["init", "-q", "-b", "main"]);
    for (k, v) in [("user.name", "Test"), ("user.email", "test@example.invalid"), ("commit.gpgsign", "false")] {
        git(&d, &["config", k, v]);
    }
    git(&d, &["add", "."]);
    git(&d, &["commit", "-q", "-m", "first"]);
    git(&d, &["remote", "add", "origin", server.to_str().unwrap()]);
    git(&d, &["push", "-q", "-u", "origin", "main"]);
    assert_eq!(status(&d), Some(Status { upstream: true, ..Default::default() }));
    assert!(!status(&d).unwrap().unsaved());
    touch(&d.join("new.txt"), "x\n");
    assert_eq!(status(&d).map(|s| s.changed), Some(1), "untracked");
    touch(&d.join("README"), "hello again\n");
    git(&d, &["add", "README"]);
    assert_eq!(status(&d).map(|s| s.changed), Some(2), "staged and untracked");
    git(&d, &["stash", "-q", "-u"]);
    let s = status(&d).unwrap();
    assert_eq!((s.changed, s.stashes), (0, 1), "stashed away");
    assert!(s.unsaved(), "a stash is unsaved work");
    git(&d, &["stash", "pop", "-q"]);
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-q", "-m", "second"]);
    let s = status(&d).unwrap();
    assert_eq!((s.changed, s.ahead, s.behind), (0, 1, 0), "not pushed");
    git(&d, &["push", "-q"]);
    // Someone else pushes: one to pull, nothing unsaved here.
    git(&base, &["clone", "-q", server.to_str().unwrap(), "other"]);
    for (k, v) in [("user.name", "Other"), ("user.email", "other@example.invalid"), ("commit.gpgsign", "false")] {
        git(&other, &["config", k, v]);
    }
    touch(&other.join("theirs.txt"), "t\n");
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "theirs"]);
    git(&other, &["push", "-q"]);
    git(&d, &["fetch", "-q"]);
    let s = status(&d).unwrap();
    assert_eq!((s.ahead, s.behind, s.unsaved()), (0, 1, false));
    assert_eq!(status(&base.join("nowhere")), None);
    // A rename's record carries its old path, which is not a second change.
    let v2 = b"# branch.oid 1a2b\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +3 -2\02 R. N... 100644 100644 100644 aa bb R100 new name.txt\0old name.txt\0? scratch\0";
    assert_eq!(parse_status(v2), Status { changed: 2, ahead: 3, behind: 2, stashes: 0, upstream: true });
    let _ = std::fs::remove_dir_all(&base);
}

/// What's using a path, against processes this test starts: a `sleep` working in a
/// folder with a file open on its input, and a copy of `sleep` run from inside the
/// folder. Each is found with how it holds it; stopping one ends it, one that ignores
/// SIGTERM outlasts it and is ended by SIGKILL; nothing else is ever signalled.
#[test]
fn holders_of_files_and_folders() {
    use crate::holders::{Hold, OURS, REFUSED, alive, holding, info, stop};
    let Some(sleep) = ["/usr/bin/sleep", "/bin/sleep"].iter().map(PathBuf::from).find(|p| p.exists()) else { return };
    let d = tmpdir("holders");
    touch(&d.join("Photos/beach.jpg"), "jpeg");
    touch(&d.join("notes.txt"), "words\n");
    touch(&d.join("other.txt"), "else\n");
    std::fs::create_dir_all(d.join("bin")).unwrap();
    std::fs::copy(&sleep, d.join("bin/napper")).unwrap();
    let mut a = std::process::Command::new(&sleep).arg("30").current_dir(d.join("Photos")).stdin(std::fs::File::open(d.join("notes.txt")).unwrap()).spawn().unwrap();
    // A child another test forks while the copy is being written holds it open for
    // writing until it runs its own program: "text file busy" for a moment.
    let napper = || std::process::Command::new(d.join("bin/napper")).arg("30").current_dir(&d).stdin(std::process::Stdio::null()).spawn();
    let mut tries = 0;
    let mut b = loop {
        match napper() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            r => break r.unwrap(),
        }
    };
    OURS.lock().unwrap().extend([a.id(), b.id()]);
    let pids = [a.id(), b.id(), std::process::id()];
    let t0 = Instant::now();
    while (info(a.id()).is_none_or(|p| p.name != "sleep") || info(b.id()).is_none_or(|p| p.name != "napper")) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pa = info(a.id()).unwrap();
    assert_eq!(pa.name, "sleep");
    assert!(pa.mine());
    let of = |paths: &[PathBuf]| -> Vec<(u32, Hold)> { holding(paths, &pids).into_iter().map(|(p, h)| (p.pid, h)).collect() };
    assert_eq!(of(&[d.join("Photos")]), [(a.id(), Hold::Inside(d.join("Photos")))]);
    assert_eq!(of(&[d.join("notes.txt")]), [(a.id(), Hold::Open(d.join("notes.txt")))]);
    assert_eq!(of(&[d.join("bin")]), [(b.id(), Hold::Runs(d.join("bin/napper")))]);
    assert!(of(&[d.join("other.txt")]).is_empty());
    let both = of(&[d.clone()]);
    assert_eq!(both.len(), 2, "{both:?}");
    assert_eq!(info(b.id()).unwrap().name, "napper");
    // Stopped: SIGTERM ends it (a zombie until reaped counts as gone).
    assert!(stop(&pa, libc::SIGTERM, Duration::from_secs(3)));
    let _ = a.wait();
    assert!(!alive(&pa));
    // A process ignoring SIGTERM is still there; SIGKILL ends it.
    let mut c = std::process::Command::new("sh").args(["-c", "trap '' TERM; exec sleep 30"]).current_dir(&d).spawn().unwrap();
    OURS.lock().unwrap().push(c.id());
    let t0 = Instant::now();
    while info(c.id()).is_none_or(|p| p.name != "sleep") && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pc = info(c.id()).unwrap();
    assert!(!stop(&pc, libc::SIGTERM, Duration::from_millis(600)), "outlasts SIGTERM");
    assert!(alive(&pc));
    assert!(stop(&pc, libc::SIGKILL, Duration::from_secs(2)));
    let _ = c.wait();
    // Anything the test didn't start is never signalled, Maia's own process included.
    let me = info(std::process::id()).unwrap();
    assert!(!stop(&me, libc::SIGTERM, Duration::from_millis(100)));
    assert!(REFUSED.lock().unwrap().contains(&(std::process::id(), libc::SIGTERM)));
    // A PID that went to another process isn't signalled either.
    let pb = info(b.id()).unwrap();
    let stale = crate::holders::Proc { start: pb.start + 1, ..pb.clone() };
    assert!(stop(&stale, libc::SIGTERM, Duration::from_millis(100)), "the one asked about is gone");
    assert!(alive(&pb), "and the one there now is left alone");
    assert!(stop(&pb, libc::SIGTERM, Duration::from_secs(3)));
    let _ = b.wait();
    assert!(crate::holders::is_busy(&std::io::Error::from_raw_os_error(libc::EBUSY)));
    assert!(!crate::holders::is_busy(&std::io::Error::from_raw_os_error(libc::EACCES)));
    assert!(crate::holders::says_busy("Error unmounting /dev/sdx1: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: Error unmounting /dev/sdx1: target is busy"));
    assert!(!crate::holders::says_busy("Wrong fs type, bad option"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Ports from the kernel's own tables: a TCP listener and a UDP socket bound by this test
/// are listed with their numbers and addresses, a connected socket isn't; the owner is
/// found through the socket's inode, and a port held by a child only is the child's. Then
/// captured lines: IPv6 any, a v4 address mapped into v6, a socket that isn't listening.
#[test]
fn ports_and_their_owners() {
    use crate::holders::{OURS, listening, parse_table, socket_owners, stop};
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let (tp, up) = (tcp.local_addr().unwrap().port(), udp.local_addr().unwrap().port());
    let client = std::net::TcpStream::connect(("127.0.0.1", tp)).unwrap();
    let cp = client.local_addr().unwrap().port();
    let all = listening();
    let t = all.iter().find(|p| p.port == tp && p.tcp).unwrap_or_else(|| panic!("{tp} not in {all:?}"));
    assert_eq!(t.addr, std::net::IpAddr::from([127, 0, 0, 1]));
    assert!(all.iter().any(|p| p.port == up && !p.tcp), "udp {up}");
    assert!(!all.iter().any(|p| p.port == cp), "a connection isn't a listener");
    let me = std::process::id();
    let owners = socket_owners(&HashSet::from([t.inode]), &[me]);
    assert_eq!(owners.get(&t.inode), Some(&vec![me]));
    // Handed to a child and closed here: the child's alone.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hp = held.local_addr().unwrap().port();
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    // Only this child gets it: the listener stays close-on-exec for any other.
    let fd = held.as_raw_fd();
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("30");
    unsafe {
        cmd.pre_exec(move || if libc::dup2(fd, 200) < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().unwrap();
    drop(held);
    OURS.lock().unwrap().push(child.id());
    let port = listening().into_iter().find(|p| p.port == hp).expect("still listening in the child");
    let owners = socket_owners(&HashSet::from([port.inode]), &[me, child.id()]);
    assert_eq!(owners.get(&port.inode), Some(&vec![child.id()]));
    let proc = crate::holders::info(child.id()).unwrap();
    assert!(stop(&proc, libc::SIGTERM, Duration::from_secs(3)));
    let _ = child.wait();
    assert!(!listening().iter().any(|p| p.port == hp), "gone with it");
    let captured6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 52345 1 0000000000000000 100 0 0 10 0\n   1: 0000000000000000FFFF00000100007F:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 52346 1 0000000000000000 100 0 0 10 0\n   2: 0000000000000000FFFF00000100007F:A3B2 0000000000000000FFFF00000100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 52347 1 0000000000000000 20 4 30 10 -1\n";
    let v = parse_table(captured6, true);
    assert_eq!(v.len(), 2);
    assert_eq!((v[0].port, v[0].addr, v[0].uid, v[0].inode), (8080, std::net::IpAddr::from(std::net::Ipv6Addr::UNSPECIFIED), 1000, 52345));
    assert_eq!((v[1].port, v[1].addr, v[1].uid), (80, std::net::IpAddr::from([127, 0, 0, 1]), 0));
    let captured_udp = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n  981: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   975        0 9921 2 0000000000000000 0\n  982: 0100A8C0:D2A1 0101A8C0:0035 01 00000000:00000000 00:00000000 00000000  1000        0 9922 2 0000000000000000 0\n";
    let v = parse_table(captured_udp, false);
    assert_eq!(v.len(), 1, "the connected one is left out");
    assert_eq!((v[0].port, v[0].addr), (5353, std::net::IpAddr::from([0, 0, 0, 0])));
    drop((tcp, udp, client));
}

/// Smart folders: names by part or by pattern, kinds, age and size, tested on a real
/// folder: hidden folders, installed packages and links left out; a search's filter
/// turned into one; sizes as people write them; the dialog's fields checked; and kept in
/// the user data file, where an older file without them still loads and a kind from a
/// later version doesn't stop it loading.
#[test]
fn smart_folders_find_what_they_say() {
    use crate::smart::{SmartFolder, glob, parse_bytes, search};
    let d = tmpdir("smart");
    let old = std::time::SystemTime::now() - Duration::from_secs(86_400 * 90);
    let file = |p: &str, len: usize, aged: bool| {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![b'x'; len]).unwrap();
        if aged {
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
        }
    };
    file("Invoices/2026/invoice-031.pdf", 2_000_000, false);
    file("Invoices/2026/Invoice-030.PDF", 1000, false);
    file("Invoices/2025/invoice-001.pdf", 3_000_000, true);
    file("Invoices/notes.txt", 10, false);
    file("Photos/IMG_0001.jpg", 500_000, false);
    file("Photos/IMG_0002.png", 400_000, true);
    file("Photos/holiday.mp4", 9_000_000, false);
    file(".cache/invoice-cached.pdf", 10, false);
    file("web/package.json", 2, false);
    file("web/node_modules/pdfjs/invoice-test.pdf", 10, false);
    std::fs::create_dir_all(d.join("Photos/invoice-folder.pdf")).unwrap();
    std::os::unix::fs::symlink(d.join("Invoices"), d.join("Links")).unwrap();
    let found = |sf: &SmartFolder| -> Vec<String> {
        let mut v = vec![];
        let (stop, cut) = (std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicBool::new(false));
        search(sf, &mut |b| {
            v.extend(b.into_iter().map(|e| e.rel().map(|r| r.into_owned()).unwrap_or_default()));
            true
        }, &stop, &cut);
        v.sort();
        v
    };
    let sf = |has: &str, kinds: &[&str], days: Option<u32>, bigger: Option<u64>| SmartFolder { id: 1, name: "x".into(), root: d.clone(), name_has: has.into(), kinds: kinds.iter().map(|k| k.to_string()).collect(), days, bigger };
    assert_eq!(found(&sf("invoice", &[], None, None)), ["Invoices/2025/invoice-001.pdf", "Invoices/2026/Invoice-030.PDF", "Invoices/2026/invoice-031.pdf"], "any case; no folders, hidden, installed or linked copies");
    assert_eq!(found(&sf("invoice-0??.pdf", &[], None, None)), ["Invoices/2025/invoice-001.pdf", "Invoices/2026/Invoice-030.PDF", "Invoices/2026/invoice-031.pdf"]);
    assert_eq!(found(&sf("IMG_*", &[], None, None)), ["Photos/IMG_0001.jpg", "Photos/IMG_0002.png"]);
    assert_eq!(found(&sf("", &["pictures"], None, None)), ["Photos/IMG_0001.jpg", "Photos/IMG_0002.png"]);
    assert_eq!(found(&sf("", &["pictures", "videos"], Some(30), None)), ["Photos/IMG_0001.jpg", "Photos/holiday.mp4"], "the old picture left out");
    assert_eq!(found(&sf("", &["pdfs"], Some(30), Some(1_000_000))), ["Invoices/2026/invoice-031.pdf"]);
    assert_eq!(found(&sf("", &["documents"], None, None)).len(), 4, "the PDFs and the notes");
    assert_eq!(found(&sf("", &["code"], None, None)), ["web/package.json"], "not the installed package's");
    assert!(glob(&"a*b?c".chars().collect::<Vec<_>>(), &"axxbyc".chars().collect::<Vec<_>>()));
    assert!(!glob(&"a*b?c".chars().collect::<Vec<_>>(), &"axxbc".chars().collect::<Vec<_>>()));
    assert!(glob(&"*".chars().collect::<Vec<_>>(), &[]));
    // From a search in the filter.
    let from = crate::smart::from_filter(&d, "invoice size>1M modified<30d");
    assert_eq!((from.name.as_str(), from.name_has.as_str(), from.bigger, from.days), ("invoice", "invoice", Some(1 << 20), Some(30)));
    assert_eq!(found(&from), ["Invoices/2026/invoice-031.pdf"]);
    assert_eq!(crate::smart::from_filter(&d.join("Photos"), "size>=2k").name, "Photos");
    assert_eq!(parse_bytes("10 MB"), Some(10 << 20));
    assert_eq!(parse_bytes("1.5G"), Some(1_610_612_736));
    assert_eq!(parse_bytes("500k"), Some(512_000));
    assert_eq!(parse_bytes("lots"), None);
    // The dialog's fields.
    let crate::popups::Popup::SmartEdit(mut e) = crate::smart::edit(&sf("invoice", &["pdfs"], Some(30), Some(10 << 20)), true) else { panic!() };
    assert_eq!((e.days.as_str(), e.bigger.as_str()), ("30", "10.0 MB"));
    assert_eq!(e.build().unwrap(), SmartFolder { name: "x".into(), ..sf("invoice", &["pdfs"], Some(30), Some(10 << 20)) });
    e.days = "soon".into();
    assert!(e.build().unwrap_err().starts_with("Days"));
    e.days.clear();
    e.bigger = "big".into();
    assert!(e.build().unwrap_err().starts_with("Size"));
    e.bigger.clear();
    e.root = d.join("nowhere").display().to_string();
    assert!(e.build().unwrap_err().starts_with("There is no folder"));
    e.root = d.display().to_string();
    e.name = "  ".into();
    assert!(e.build().is_err());
    // Kept in the user data file; older files load.
    let path = d.join("userdata.json");
    let mut ud = crate::config::UserData::with_defaults();
    ud.smart.push(sf("invoice", &["pdfs"], Some(30), None));
    crate::config::save_json(&path, &ud).unwrap();
    let back: crate::config::UserData = crate::config::load_json(&path);
    assert_eq!(back.smart, ud.smart);
    std::fs::write(&path, r#"{"bookmarks": ["/tmp"], "recents": []}"#).unwrap();
    let older: crate::config::UserData = crate::config::load_json(&path);
    assert_eq!((older.bookmarks.len(), older.smart.len()), (1, 0));
    std::fs::write(&path, r#"{"bookmarks": ["/tmp"], "smart": [{"id": 4, "name": "Scans", "root": "/tmp", "kinds": ["pdfs", "holograms"], "colour": "red"}]}"#).unwrap();
    let later: crate::config::UserData = crate::config::load_json(&path);
    assert_eq!((later.bookmarks.len(), later.smart[0].name.as_str(), later.smart[0].kinds.len()), (1, "Scans", 2));
    let _ = std::fs::remove_dir_all(&d);
}

/// Fake snapshot trees: Snapper's numbered snapshots with info.xml, Timeshift's dated
/// btrfs and rsync snapshots with info.json, each found through a mount table written
/// the way /proc/self/mountinfo is.
pub fn snapshot_fixture(d: &Path) -> (PathBuf, String) {
    let vol = d.join("vol");
    let file = vol.join("docs/report.txt");
    let at = |p: &Path, days: u64| {
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_792_001 - days * 86_400);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    };
    touch(&file, "fourth draft, the one in use\n");
    at(&file, 0);
    let snap = |n: u32, date: &str, desc: &str, text: Option<(&str, u64)>| {
        let s = vol.join(format!(".snapshots/{n}"));
        std::fs::create_dir_all(s.join("snapshot/docs")).unwrap();
        std::fs::write(s.join("info.xml"), format!("<?xml version=\"1.0\"?>\n<snapshot>\n  <type>single</type>\n  <num>{n}</num>\n  <date>{date}</date>\n  <description>{desc}</description>\n  <cleanup>timeline</cleanup>\n</snapshot>\n")).unwrap();
        if let Some((t, days)) = text {
            let f = s.join("snapshot/docs/report.txt");
            std::fs::write(&f, t).unwrap();
            at(&f, days);
        }
    };
    // 5 is the file as it is now; 4 has no such file; 2 holds what 3 does under an
    // older time; 1 is as long as 3 with other words in it.
    snap(5, "2026-10-12 06:00:00", "timeline", Some(("fourth draft, the one in use\n", 0)));
    snap(4, "2026-10-11 06:00:00", "timeline", None);
    snap(3, "2026-10-10 06:00:00", "before &amp; after", Some(("third\n", 2)));
    snap(2, "2026-10-09 06:00:00", "", Some(("third\n", 3)));
    snap(1, "2026-10-08 06:00:00", "first", Some(("frist\n", 4)));
    std::fs::create_dir_all(vol.join(".snapshots/0/snapshot/docs")).unwrap();
    std::fs::write(vol.join(".snapshots/0/snapshot/docs/report.txt"), "zero").unwrap();
    // Timeshift on btrfs: the file system's top mounted beside it.
    let ts = d.join("top/timeshift-btrfs/snapshots/2026-10-07_09-00-00");
    touch(&ts.join("@home/docs/report.txt"), "an old btrfs copy\n");
    std::fs::write(ts.join("info.json"), r#"{"created" : "1791363600", "sys-uuid" : "x", "tags" : "D", "comments" : "before update"}"#).unwrap();
    // Timeshift with rsync, on a backup disk.
    let rs = d.join("backup/timeshift/snapshots/2026-10-06_10-00-00");
    touch(&rs.join("localhost").join(file.strip_prefix("/").unwrap()), "rsync copy, the oldest\n");
    std::fs::write(rs.join("info.json"), r#"{"tags" : "W M"}"#).unwrap();
    let esc = |p: &Path| p.display().to_string().replace(' ', "\\040");
    let table = format!(
        "40 1 0:33 /@ / rw,noatime shared:1 - btrfs /dev/sda2 rw,subvolid=256,subvol=/@\n\
         38 40 0:7 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw,size=16195776k\n\
         60 40 0:50 /@home {} rw,noatime shared:9 - btrfs /dev/sdb1 rw,subvolid=257,subvol=/@home\n\
         61 40 0:50 / {} rw,noatime shared:10 - btrfs /dev/sdb1 rw,subvolid=5,subvol=/\n\
         62 40 8:17 / {} rw,relatime shared:11 - ext4 /dev/sdc1 rw\n\
         63 40 0:60 / {}/net rw shared:12 - nfs4 nas:/x rw\n",
        esc(&vol),
        esc(&d.join("top")),
        esc(&d.join("backup")),
        esc(d)
    );
    (file, table)
}

/// A folder for Show Space Used: a folder of 500,000 bytes in two levels, a file of
/// 400,000, a folder of 10 bytes, an empty one, and a link to the big folder.
pub fn space_fixture(d: &Path) {
    std::fs::create_dir_all(d.join("Big/Sub")).unwrap();
    std::fs::write(d.join("Big/a.bin"), vec![1u8; 300_000]).unwrap();
    std::fs::write(d.join("Big/Sub/b.bin"), vec![2u8; 200_000]).unwrap();
    std::fs::write(d.join("movie.mkv"), vec![3u8; 400_000]).unwrap();
    std::fs::create_dir_all(d.join("Small")).unwrap();
    std::fs::write(d.join("Small/c.txt"), "ten bytes!").unwrap();
    std::fs::create_dir_all(d.join("Empty")).unwrap();
    let _ = std::os::unix::fs::symlink(d.join("Big"), d.join("big-link"));
}

#[test]
fn space_used_lists_biggest_first_as_sizes_land() {
    use crate::spaceuse::State;
    let d = tmpdir("space");
    space_fixture(&d);
    let ctx = egui::Context::default();
    let mut stats = crate::stats::Stats::new(ctx.clone());
    let mut st = State::new(d.clone(), &ctx);
    let t0 = Instant::now();
    while st.counting() {
        assert!(t0.elapsed() < Duration::from_secs(10), "sizes never came");
        stats.pump();
        st.pump(&mut stats, &ctx);
        std::thread::sleep(Duration::from_millis(5));
    }
    let shown: Vec<(&str, Option<u64>)> = st.order.iter().map(|&k| (st.items[k as usize].entry.name.as_str(), st.items[k as usize].size)).collect();
    assert_eq!(shown, vec![("Big", Some(500_000)), ("movie.mkv", Some(400_000)), ("Small", Some(10)), ("big-link", Some(0)), ("Empty", Some(0))], "folders by everything under them, a link by itself");
    assert_eq!(st.total(), 900_010);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn picture_viewer_steps_fits_and_fades_its_name() {
    use crate::viewer::{Pic, State, fit_scale, is_picture};
    assert!(["jpg", "jpeg", "png", "webp", "gif", "bmp", "svg"].iter().all(|e| is_picture(e)));
    assert!(!is_picture("txt") && !is_picture("mp4") && !is_picture("pdf"));
    let pics: Vec<Pic> = (0..3).map(|i| Pic { path: PathBuf::from(format!("/p/{i}.png")), name: format!("{i}.png"), modified: None }).collect();
    let mut st = State::new(1, pics, 1, 10.0);
    assert_eq!(st.label_alpha(11.0, true), 1.0, "shown while the pointer has just moved");
    assert!((st.label_alpha(12.2, true) - 0.5).abs() < 1e-3, "fading out");
    assert_eq!(st.label_alpha(13.0, true), 0.0, "then gone");
    assert_eq!(st.label_alpha(12.1, false), 0.0, "without animations it goes at once");
    st.zoom = 3.0;
    st.go(2, 20.0, true);
    assert_eq!((st.at, st.zoom, st.fading_from()), (2, 1.0, Some(1)), "the next fits the window, the last fading out");
    st.go(5, 21.0, true);
    assert_eq!(st.at, 2, "no picture past the last");
    st.go(0, 22.0, false);
    assert_eq!((st.at, st.fading_from()), (0, None), "no fade without animations");
    let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 500.0));
    assert_eq!(fit_scale(area, [4000, 1000]), 0.25, "a wide picture by its width");
    assert_eq!(fit_scale(area, [1000, 2000]), 0.25, "a tall one by its height");
    assert_eq!(fit_scale(area, [40, 30]), 1.0, "a small one never blown up");
}

/// A photograph turned on its side by its EXIF tag reaches the viewer upright: the
/// thumbnail cache's decode, as the viewer asks for it.
#[test]
fn viewer_pictures_decode_upright_off_the_frame() {
    let d = tmpdir("viewer-exif");
    // A JPEG 64 × 32 as stored whose tag says it is turned a quarter (orientation 6).
    let Some(t) = imagemagick_tools(&d.join("bin")) else { return };
    let src = d.join("wide.png");
    std::fs::write(&src, png_bytes(64, 32, 2)).unwrap();
    let jpg = d.join("side.jpg");
    let ok = std::process::Command::new(t.join("magick")).arg(&src).arg(&jpg).status().is_ok_and(|s| s.success());
    if !ok {
        return;
    }
    let tagged = with_exif_orientation(&std::fs::read(&jpg).unwrap(), 6);
    std::fs::write(&jpg, tagged).unwrap();
    let ctx = egui::Context::default();
    let mut thumbs = crate::thumbs::Thumbs::new(ctx.clone(), 1);
    let mtime = std::fs::metadata(&jpg).unwrap().modified().ok();
    let t0 = Instant::now();
    let dims = loop {
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            thumbs.pump(ctx);
        });
        if let Some(l) = thumbs.get(&jpg, mtime, 1600.0) {
            break (l.dims, l.tex.size());
        }
        assert!(t0.elapsed() < Duration::from_secs(10) && !thumbs.failed(&jpg), "never decoded");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(dims, ([32, 64], [32, 64]), "upright, at its full size: 1600 px asked for, the bucket above it");
    let _ = std::fs::remove_dir_all(&d);
}

/// The shelf keeps one of each, reads the details of what was dropped on it and lets go
/// of what is gone, on its own thread.
#[test]
fn shelf_gathers_once_and_drops_what_vanished() {
    let d = tmpdir("shelf");
    touch(&d.join("a/report.md"), "# r");
    touch(&d.join("b/photo.png"), "png");
    std::fs::create_dir_all(d.join("c/folder")).unwrap();
    let mut sh = crate::shelf::Shelf::default();
    let items = vec![(d.join("a/report.md"), None), (d.join("b/photo.png"), None), (d.join("c/folder"), None)];
    assert_eq!(sh.add(items), 3);
    assert_eq!(sh.add(vec![(d.join("a/report.md"), None)]), 0, "once only");
    std::fs::remove_file(d.join("b/photo.png")).unwrap();
    let ctx = egui::Context::default();
    let t0 = Instant::now();
    while sh.items.len() == 3 || sh.items.iter().any(|i| i.entry.is_none()) {
        assert!(t0.elapsed() < Duration::from_secs(5), "never looked");
        sh.pump(&ctx);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(sh.paths(), vec![d.join("a/report.md"), d.join("c/folder")], "the vanished one gone, the order kept");
    assert!(sh.items[1].entry.as_ref().is_some_and(|e| e.is_dir), "details read for the icon");
    sh.remove(&d.join("a/report.md"));
    assert_eq!(sh.paths(), vec![d.join("c/folder")]);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn mountinfo_lines_read_with_their_subvolumes() {
    use crate::versions::parse_mountinfo;
    let m = parse_mountinfo("40 1 0:33 /@ / rw,noatime shared:1 - btrfs /dev/sda2 rw,subvol=/@\n63 40 8:17 / /run/media/me/My\\040Disk rw,nosuid,nodev,relatime shared:40 - exfat /dev/sdc1 rw\nnot a line\n");
    assert_eq!(m.len(), 2);
    assert_eq!((m[0].point.as_path(), m[0].root.as_path(), m[0].dev.as_str(), m[0].fstype.as_str()), (Path::new("/"), Path::new("/@"), "0:33", "btrfs"));
    assert_eq!(m[1].point, PathBuf::from("/run/media/me/My Disk"));
    assert_eq!(m[1].fstype, "exfat");
    assert_eq!(crate::versions::parse_utc("2026-10-12 08:00:01"), Some(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_792_001)));
    assert_eq!(crate::versions::parse_utc("2000-02-29 23:59:59"), Some(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(951_868_799)));
    assert_eq!(crate::versions::parse_utc("2026-13-01 00:00:00"), None);
}

#[test]
fn previous_versions_from_snapper_and_timeshift_skip_the_unchanged() {
    use crate::versions::{Tool, find, parse_mountinfo};
    let d = tmpdir("versions");
    let (file, table) = snapshot_fixture(&d);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let f = find(&file, &parse_mountinfo(&table), &stop);
    let got: Vec<(String, String, Tool)> = f.versions.iter().map(|v| (std::fs::read_to_string(&v.entry.path).unwrap(), v.label.clone(), v.tool)).collect();
    let want = [("third\n", "before & after", Tool::Snapper), ("frist\n", "first", Tool::Snapper), ("an old btrfs copy\n", "before update", Tool::Timeshift), ("rsync copy, the oldest\n", "weekly, monthly", Tool::Timeshift)];
    assert_eq!(got, want.map(|(a, b, c)| (a.to_string(), b.to_string(), c)).to_vec(), "newest first; the copy like the file now, and the one like the next newer, left out");
    assert_eq!(f.versions[0].when, crate::versions::parse_utc("2026-10-10 06:00:00"), "Snapper's dates are in UTC");
    assert_eq!(f.versions[2].when, Some(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_363_600)), "Timeshift's own time when it gives one");
    assert_eq!(f.versions[3].when, crate::timefmt::parse_local("2026-10-06_10-00-00"), "else the folder's name, in local time");
    assert_eq!(f.snapshots, 7, "five Snapper snapshots and one of each Timeshift kind");
    assert!(f.locked.is_empty());
    // A file no snapshot has, and one on no known mount.
    touch(&d.join("vol/docs/new.txt"), "new");
    let f = find(&d.join("vol/docs/new.txt"), &parse_mountinfo(&table), &stop);
    assert!(f.versions.is_empty() && f.snapshots > 0);
    assert!(find(&file, &[], &stop).versions.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn snapshots_only_root_reads_are_said_plainly() {
    use crate::versions::{Tool, find, locked_note, parse_mountinfo};
    use std::os::unix::fs::PermissionsExt;
    let d = tmpdir("versions-locked");
    let (file, table) = snapshot_fixture(&d);
    let dot = d.join("vol/.snapshots");
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o000)).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let f = find(&file, &parse_mountinfo(&table), &stop);
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(f.locked, vec![Tool::Snapper]);
        assert!(!f.versions.is_empty() && f.versions.iter().all(|v| v.tool == Tool::Timeshift), "the others still found");
        assert_eq!(locked_note(&f.locked).as_deref(), Some("Snapper's snapshots here can only be read by an administrator."));
    }
    assert_eq!(locked_note(&[Tool::Snapper, Tool::Timeshift]).as_deref(), Some("The snapshots here can only be read by an administrator."));
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_version_restored_in_place_or_as_a_copy() {
    use crate::versions::{copy_name, find, parse_mountinfo, restore};
    let d = tmpdir("versions-restore");
    let (file, table) = snapshot_fixture(&d);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let f = find(&file, &parse_mountinfo(&table), &stop);
    let v = &f.versions[1];
    let day = crate::timefmt::format(v.when.unwrap(), "%Y-%m-%d");
    assert_eq!(copy_name(&file, v.when), format!("report (from {day}).txt"));
    assert_eq!(copy_name(Path::new("/x/Makefile"), None), "Makefile (from a snapshot)");
    let copy = restore(&v.entry.path, &file, v.when, true).unwrap();
    assert_eq!(copy.parent(), file.parent());
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), "frist\n");
    assert_eq!(std::fs::metadata(&copy).unwrap().modified().unwrap(), v.entry.modified.unwrap(), "the version's time kept");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fourth draft, the one in use\n", "a copy leaves the file alone");
    let again = restore(&v.entry.path, &file, v.when, true).unwrap();
    assert_eq!(again.file_name().unwrap().to_string_lossy(), format!("report (from {day}) (1).txt"), "never over the first copy");
    // In place: the file goes to the trash, the version takes its name.
    restore(&v.entry.path, &file, v.when, false).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "frist\n");
    assert!(!d.join("vol/docs/.report.txt.restore").exists());
    let trashed = crate::trashcan::list().into_iter().filter(|it| it.original == file).count();
    assert_eq!(trashed, 1, "what it replaced is in the trash");
    assert_eq!(crate::picfix::undo_replaced(std::slice::from_ref(&file)), 0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fourth draft, the one in use\n", "undone");
    // A version gone from under it: nothing changed.
    assert!(restore(&d.join("nowhere"), &file, None, false).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fourth draft, the one in use\n");
    for it in crate::trashcan::list().into_iter().filter(|it| it.original.starts_with(&d)) {
        let _ = crate::trashcan::purge(&it);
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The real converters on the PATH (ImageMagick, ffmpeg and ffprobe, poppler's
/// pdfunite and pdfseparate, libheif's heif-dec) linked into a stand-in folder; None
/// when one is missing.
pub(crate) fn convert_tools(dir: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    for t in ["magick", "ffmpeg", "ffprobe", "pdfunite", "pdfseparate", "heif-dec"] {
        let real = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(t)).find(|m| m.is_file()))?;
        let _ = std::os::unix::fs::symlink(real, dir.join(t));
    }
    Some(dir.to_path_buf())
}

/// Real inputs for conversions, made by the real tools: a see-through PNG, a JPEG stored
/// turned (EXIF orientation 6), a two-frame GIF, a HEIC, a tone with a cover and tags
/// as MP3, a short video with odd sides, and PDFs of three and two pages.
pub(crate) fn convert_inputs(d: &Path, tools: &Path) {
    let run = |prog: &str, args: &[&str]| {
        let ok = std::process::Command::new(tools.join(prog)).args(args).current_dir(d).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
        assert!(ok, "{prog} {args:?}");
    };
    run("magick", &["-size", "120x80", "xc:none", "-fill", "#e04030", "-draw", "rectangle 0,0 59,79", "clear.png"]);
    run("magick", &["-size", "400x240", "gradient:#e94-#36c", "raw.jpg"]);
    let turned = with_exif_orientation(&std::fs::read(d.join("raw.jpg")).unwrap(), 6);
    std::fs::write(d.join("turned.jpg"), turned).unwrap();
    std::fs::remove_file(d.join("raw.jpg")).unwrap();
    run("magick", &["-delay", "10", "-size", "64x48", "xc:red", "xc:blue", "moving.gif"]);
    run("magick", &["-size", "90x60", "gradient:#2a7-#fc3", "phone.heic"]);
    run("ffmpeg", &["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=64x64", "-frames:v", "1", "cover.png"]);
    run("ffmpeg", &["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-i", "cover.png", "-map", "0", "-map", "1", "-c:a", "libmp3lame", "-c:v", "png", "-disposition:v", "attached_pic", "-id3v2_version", "3", "-metadata", "title=Test Tone", "-metadata", "artist=Maia", "tone.mp3"]);
    std::fs::remove_file(d.join("cover.png")).unwrap();
    run("ffmpeg", &["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=1:size=161x121:rate=15", "-f", "lavfi", "-i", "sine=duration=1", "-c:v", "mpeg4", "-c:a", "pcm_s16le", "-shortest", "clip.avi"]);
    std::fs::write(d.join("three.pdf"), crate::sim::sample_pdf(&["1 0 0", "0 1 0", "0 0 1"])).unwrap();
    std::fs::write(d.join("two.pdf"), crate::sim::sample_pdf(&["0.5 0.5 0", "0 0.5 0.5"])).unwrap();
}

/// A conversion job run to its end, as the app runs it.
fn convert_job(srcs: &[PathBuf], to: crate::convert::Target, out: Option<PathBuf>, tools: &Path) -> ops::Progress {
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(tools.to_path_buf()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let job = ops::spawn(1, OpKind::Convert { srcs: srcs.to_vec(), to, out, tools: t }, egui::Context::default());
    let t0 = Instant::now();
    while !job.progress.lock().unwrap().finished {
        assert!(t0.elapsed() < Duration::from_secs(120), "{to:?} took too long");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::mem::take(&mut *job.progress.lock().unwrap())
}

/// Hidden part files left in `d` (a failed or cancelled conversion leaves none).
fn parts_in(d: &Path) -> Vec<String> {
    std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with('.')).collect()
}

/// What each selection is offered: only formats other than its own, only files of one
/// kind, merging for two PDFs or more and splitting for one, nothing without the tool.
#[test]
fn conversions_offered_by_kind_and_tool() {
    use crate::convert::{Target as T, offered};
    let d = tmpdir("convert-offer");
    let Some(all) = convert_tools(&d.join("all")) else { return };
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(all.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.join("none")));
    std::fs::create_dir_all(d.join("none")).unwrap();
    let none = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    assert_eq!(offered(&["png"], &t), vec![T::Jpg, T::Webp, T::PicturePdf]);
    assert_eq!(offered(&["jpeg", "jpg"], &t), vec![T::Png, T::Webp, T::PicturePdf]);
    assert_eq!(offered(&["png", "jpg", "heic"], &t), vec![T::Jpg, T::Png, T::Webp, T::PicturePdf]);
    assert_eq!(offered(&["flac"], &t), vec![T::Mp3, T::Opus]);
    assert_eq!(offered(&["mp3", "wav"], &t), vec![T::Mp3, T::Opus, T::Flac]);
    assert_eq!(offered(&["mkv"], &t), vec![T::Mp4, T::Webm, T::SoundMp3]);
    assert_eq!(offered(&["pdf"], &t), vec![T::SplitPdf]);
    assert_eq!(offered(&["pdf", "pdf"], &t), vec![T::MergePdf]);
    assert!(offered(&["mp3", "pdf"], &t).is_empty(), "two kinds");
    assert!(offered(&["txt"], &t).is_empty());
    assert!(offered(&[], &t).is_empty());
    for exts in [vec!["png"], vec!["mp3"], vec!["mp4"], vec!["pdf"], vec!["pdf", "pdf"]] {
        assert!(offered(&exts, &none).is_empty(), "{exts:?} without the tools");
    }
    assert_eq!(T::of(Cmd::ConvertSoundMp3), Some(T::SoundMp3));
    assert!(T::ALL.iter().all(|t| T::of(t.cmd()) == Some(*t)));
}

/// Pictures through the real ImageMagick: transparency on white in a JPG, a photo stored
/// turned made upright, one frame of an animated GIF, a HEIC read, a name already taken
/// kept; then several pictures as one PDF written by Maia, each page A4 the right way
/// round with the picture on it; libheif read a HEIC an ImageMagick without HEIC refuses.
#[test]
fn pictures_converted_through_imagemagick() {
    use crate::convert::{Target as T, jpeg_size, page_for};
    let d = tmpdir("convert-pictures");
    let Some(tools) = convert_tools(&d.join("tools")) else { return };
    let w = d.join("work");
    std::fs::create_dir_all(&w).unwrap();
    convert_inputs(&w, &tools);
    let shown = |p: &Path| crate::imgdec::load(p, 4096).unwrap_or_else(|| panic!("{} decodes", p.display()));
    std::fs::write(w.join("clear.jpg"), "taken").unwrap();
    let p = convert_job(&[w.join("clear.png")], T::Jpg, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.produced, vec![(w.join("clear.png"), w.join("clear (1).jpg"))]);
    let (img, dims) = shown(&w.join("clear (1).jpg"));
    assert_eq!(dims, [120, 80]);
    let px = |x: u32, y: u32| img.px[((y * img.w + x) * 4) as usize..][..3].to_vec();
    assert!(px(100, 40).iter().all(|&c| c > 240), "see-through became white: {:?}", px(100, 40));
    assert!(px(20, 40)[0] > 180 && px(20, 40)[2] < 90, "the red stays: {:?}", px(20, 40));
    assert_eq!(std::fs::read_to_string(w.join("clear.jpg")).unwrap(), "taken");
    let p = convert_job(&[w.join("turned.jpg"), w.join("moving.gif"), w.join("phone.heic")], T::Png, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.done_items, 3);
    assert_eq!(shown(&w.join("turned.png")).1, [240, 400], "upright, and stored so");
    assert_eq!(shown(&w.join("moving.png")).1, [64, 48]);
    assert_eq!(shown(&w.join("phone.png")).1, [90, 60]);
    assert!(std::fs::read(w.join("moving.png")).unwrap().starts_with(b"\x89PNG"));
    let p = convert_job(&[w.join("phone.heic")], T::Webp, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(shown(&w.join("phone.webp")).1, [90, 60]);
    // An ImageMagick that can't read HEIC: libheif decodes it instead.
    let picky = d.join("picky");
    convert_tools(&picky).unwrap();
    std::fs::remove_file(picky.join("magick")).unwrap();
    let real = std::fs::read_link(tools.join("magick")).unwrap();
    fake_tool(&picky, "magick", &format!("t=\"$D/in.$$\"; cat > \"$t\"\nif head -c 12 \"$t\" | grep -q ftyp; then rm -f \"$t\"; echo \"magick: no decode delegate for this image format 'HEIC'\" >&2; exit 1; fi\n{} \"$@\" < \"$t\"; r=$?; rm -f \"$t\"; exit $r", real.display()));
    let p = convert_job(&[w.join("phone.heic")], T::Jpg, None, &picky);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(shown(&w.join("phone.jpg")).1, [90, 60]);
    assert!(parts_in(&w).is_empty(), "{:?}", parts_in(&w));
    // Without libheif the refusal is said.
    std::fs::remove_file(picky.join("heif-dec")).unwrap();
    let p = convert_job(&[w.join("phone.heic")], T::Png, None, &picky);
    assert_eq!(p.errors.len(), 1);
    assert!(p.errors[0].contains("no decode delegate"), "{:?}", p.errors);
    // Pictures as one PDF: a page each, landscape for the wide one, A4.
    let out = w.join("Album.pdf");
    let p = convert_job(&[w.join("clear.png"), w.join("turned.jpg"), w.join("phone.heic")], T::PicturePdf, Some(out.clone()), &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.produced.len(), 1);
    assert_eq!(crate::media::pdf_pages(&out), Some(3));
    let info = std::process::Command::new("pdfinfo").args(["-f", "1", "-l", "3"]).arg(&out).output().unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    let sizes: Vec<&str> = info.lines().filter(|l| l.starts_with("Page") && l.contains("size:")).map(|l| l.split("size:").nth(1).unwrap().trim()).collect();
    assert_eq!(sizes.len(), 3, "{info}");
    assert!(sizes[0].starts_with("841.89 x 595.28"), "the wide picture on a landscape page: {sizes:?}");
    assert!(sizes[1].starts_with("595.28 x 841.89"), "the upright photo on a portrait one: {sizes:?}");
    // The page shows the picture: red on the left, white where it was clear.
    let page = crate::media::pdf_page(&out, 1, 400).expect("page 1 renders");
    let at = |fx: f32, fy: f32| {
        let (x, y) = ((page.w as f32 * fx) as u32, (page.h as f32 * fy) as u32);
        page.px[((y * page.w + x) * 4) as usize..][..3].to_vec()
    };
    assert!(at(0.25, 0.5)[0] > 180 && at(0.25, 0.5)[1] < 100, "red on the page: {:?}", at(0.25, 0.5));
    assert!(at(0.75, 0.5).iter().all(|&c| c > 230), "white beside it: {:?}", at(0.75, 0.5));
    let jpeg = std::fs::read(w.join("phone.jpg")).unwrap();
    assert_eq!(jpeg_size(&jpeg), Some((90, 60, 3)));
    assert_eq!(jpeg_size(b"not a jpeg"), None);
    assert_eq!(jpeg_size(&jpeg[..20]), None);
    let ((pw, ph), [x, y, dw, dh]) = page_for(1000, 500);
    assert!((pw - 841.89).abs() < 0.01 && (ph - 595.28).abs() < 0.01);
    assert!((dw / dh - 2.0).abs() < 0.01 && x > 0.0 && y > 0.0 && x + dw <= pw && y + dh <= ph);
    let (_, [x, _, dw, _]) = page_for(10, 4000);
    assert!(x > 28.0 && dw < 10.0, "a sliver stays in the middle");
    assert!(parts_in(&w).is_empty(), "{:?}", parts_in(&w));
}

/// Sound and video through the real ffmpeg: tags and the cover kept in a FLAC and an
/// MP3, an Opus without the cover it can't hold, a video with odd sides as MP4 (H.264
/// at even sides) and WebM (VP9), its sound alone as MP3. A broken file fails with
/// ffmpeg's words and leaves nothing; a long conversion shows how far it got, and
/// Cancel stops ffmpeg and takes its part file away.
#[test]
fn sound_and_video_converted_through_ffmpeg() {
    use crate::convert::{Target as T, duration, progress_us};
    let d = tmpdir("convert-media");
    let Some(tools) = convert_tools(&d.join("tools")) else { return };
    let w = d.join("work");
    std::fs::create_dir_all(&w).unwrap();
    convert_inputs(&w, &tools);
    let probe = |p: &Path| crate::media::probe(p).unwrap_or_else(|| panic!("{} probes", p.display()));
    let streams = |p: &Path| {
        let o = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=codec_name,codec_type:stream_disposition=attached_pic", "-of", "csv=p=0"]).arg(p).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect::<Vec<_>>()
    };
    let p = convert_job(&[w.join("tone.mp3")], T::Flac, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let flac = w.join("tone.flac");
    let pr = probe(&flac);
    assert_eq!((pr.codec.as_str(), pr.title.as_deref(), pr.artist.as_deref()), ("flac", Some("Test Tone"), Some("Maia")));
    assert!(streams(&flac).iter().any(|s| s.starts_with("png,video,1")), "the cover kept: {:?}", streams(&flac));
    assert!((pr.duration - 2.0).abs() < 0.2, "{}", pr.duration);
    let p = convert_job(&[flac.clone()], T::Mp3, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.produced, vec![(flac.clone(), w.join("tone (1).mp3"))], "beside the first, not over it");
    let back = probe(&w.join("tone (1).mp3"));
    assert_eq!((back.codec.as_str(), back.title.as_deref()), ("mp3", Some("Test Tone")));
    assert!(streams(&w.join("tone (1).mp3")).iter().any(|s| s.ends_with(",video,1")));
    let p = convert_job(&[w.join("tone.mp3")], T::Opus, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let opus = w.join("tone.opus");
    assert_eq!(probe(&opus).codec, "opus");
    assert_eq!(streams(&opus).len(), 1, "no cover in Opus: {:?}", streams(&opus));
    // Video, odd sides made even.
    let p = convert_job(&[w.join("clip.avi")], T::Mp4, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mp4 = probe(&w.join("clip.mp4"));
    assert_eq!((mp4.codec.as_str(), mp4.video, mp4.audio), ("h264", Some((160, 120)), true));
    let p = convert_job(&[w.join("clip.avi")], T::Webm, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let webm = probe(&w.join("clip.webm"));
    assert_eq!((webm.codec.as_str(), webm.video), ("vp9", Some((161, 121))));
    let p = convert_job(&[w.join("clip.mp4")], T::SoundMp3, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let snd = probe(&w.join("clip.mp3"));
    assert!(snd.video.is_none() && snd.audio && snd.codec == "mp3");
    // A broken file: ffmpeg's own words, nothing left behind.
    std::fs::write(w.join("broken.wav"), "RIFF but not really").unwrap();
    let p = convert_job(&[w.join("broken.wav"), w.join("tone.mp3")], T::Flac, None, &tools);
    assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
    assert!(p.errors[0].starts_with("broken.wav: ") && p.errors[0].len() > 14, "{:?}", p.errors);
    assert!(!w.join("broken.flac").exists());
    assert_eq!(p.done_items, 2, "the next file still converted");
    assert!(parts_in(&w).is_empty(), "{:?}", parts_in(&w));
    assert_eq!(progress_us("out_time_us=1500000"), Some(1_500_000));
    assert_eq!(progress_us("out_time_ms=20"), Some(20));
    assert_eq!(progress_us("progress=continue"), None);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(tools.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    assert!(duration(&t, &w.join("tone.mp3")).is_some_and(|d| (d - 2.0).abs() < 0.2));
    assert_eq!(duration(&t, &w.join("broken.wav")), None);
    // A long one: progress inside the file, then Cancel.
    let ok = std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=120:size=640x480:rate=30", "-c:v", "mpeg4", "-q:v", "20"]).arg(w.join("long.avi")).status().unwrap().success();
    assert!(ok);
    let job = ops::spawn(2, OpKind::Convert { srcs: vec![w.join("long.avi")], to: T::Webm, out: None, tools: t }, egui::Context::default());
    assert_eq!(job.label, "Converting long.avi to WebM");
    let t0 = Instant::now();
    while job.progress.lock().unwrap().within.unwrap_or(0.0) <= 0.0 {
        assert!(t0.elapsed() < Duration::from_secs(60), "no progress from ffmpeg");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(w.join(".long.webm.part").exists(), "written to a part file");
    let ff = |s: &str| std::fs::read_dir("/proc").unwrap().flatten().any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "ffmpeg") && std::fs::read(e.path().join("cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(s)));
    assert!(ff(".long.webm.part"));
    job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let t0 = Instant::now();
    while !job.progress.lock().unwrap().finished {
        assert!(t0.elapsed() < Duration::from_secs(10), "Cancel didn't stop it");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!w.join(".long.webm.part").exists() && !w.join("long.webm").exists());
    let t0 = Instant::now();
    while ff(".long.webm.part") && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!ff(".long.webm.part"), "ffmpeg killed");
    assert!(job.progress.lock().unwrap().errors.is_empty(), "a cancel is no error");
    let _ = std::fs::remove_dir_all(&d);
}

/// PDFs through poppler: two merged into one with every page in order, under the name
/// given; one split into a folder of one-page PDFs named after it, a % in its name
/// included; a broken PDF refused with nothing left behind.
#[test]
fn pdfs_merged_and_split_through_poppler() {
    use crate::convert::Target as T;
    let d = tmpdir("convert-pdf");
    let Some(tools) = convert_tools(&d.join("tools")) else { return };
    let w = d.join("work");
    std::fs::create_dir_all(&w).unwrap();
    convert_inputs(&w, &tools);
    let out = w.join("Both.pdf");
    let p = convert_job(&[w.join("three.pdf"), w.join("two.pdf")], T::MergePdf, Some(out.clone()), &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(crate::media::pdf_pages(&out), Some(5));
    // Page four is the second file's first: yellowish.
    let pg = crate::media::pdf_page(&out, 4, 200).unwrap();
    let c = &pg.px[(((pg.h / 2) * pg.w + pg.w / 2) * 4) as usize..][..3];
    assert!(c[0] > 100 && c[1] > 100 && c[2] < 40, "{c:?}");
    std::fs::rename(w.join("three.pdf"), w.join("50% off.pdf")).unwrap();
    std::fs::create_dir(w.join("50% off pages")).unwrap();
    let p = convert_job(&[w.join("50% off.pdf")], T::SplitPdf, None, &tools);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let folder = w.join("50% off pages (1)");
    assert_eq!(p.produced, vec![(w.join("50% off.pdf"), folder.clone())]);
    let mut names: Vec<String> = std::fs::read_dir(&folder).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["50% off 1.pdf", "50% off 2.pdf", "50% off 3.pdf"]);
    for n in &names {
        assert_eq!(crate::media::pdf_pages(&folder.join(n)), Some(1), "{n}");
    }
    std::fs::write(w.join("broken.pdf"), "%PDF-1.4 nothing").unwrap();
    let p = convert_job(&[w.join("broken.pdf")], T::SplitPdf, None, &tools);
    assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
    let p = convert_job(&[w.join("broken.pdf"), w.join("two.pdf")], T::MergePdf, Some(w.join("Bad.pdf")), &tools);
    assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
    assert!(!w.join("Bad.pdf").exists() && parts_in(&w).is_empty(), "{:?}", parts_in(&w));
    let _ = std::fs::remove_dir_all(&d);
}

/// A JPEG made by ImageMagick with a date taken and, when given, a place written into its
/// EXIF by exiv2; None without the two tools.
pub(crate) fn exif_photo(path: &Path, size: &str, taken: &str, gps: Option<[&str; 4]>) -> Option<()> {
    if !crate::media::have("magick") || !crate::media::have("exiv2") {
        return None;
    }
    let ok = std::process::Command::new("magick").args(["-size", size, "gradient:#e94-#36c"]).arg(path).status().ok()?.success();
    assert!(ok, "magick");
    let mut c = std::process::Command::new("exiv2");
    c.arg(format!("-Mset Exif.Photo.DateTimeOriginal Ascii {taken}"));
    if let Some([lat_ref, lat, lon_ref, lon]) = gps {
        c.arg(format!("-Mset Exif.GPSInfo.GPSLatitudeRef {lat_ref}")).arg(format!("-Mset Exif.GPSInfo.GPSLatitude {lat}"));
        c.arg(format!("-Mset Exif.GPSInfo.GPSLongitudeRef {lon_ref}")).arg(format!("-Mset Exif.GPSInfo.GPSLongitude {lon}"));
    }
    let ok = c.arg("mo").arg(path).stderr(std::process::Stdio::null()).status().ok()?.success();
    assert!(ok, "exiv2");
    Some(())
}

/// EXIF as exiv2 writes it, read back: the date a photo was taken, its place (south and
/// west negative), its orientation, in a JPEG and a WebP; a photo without EXIF and a
/// file cut short give nothing and don't fail.
#[test]
fn exif_dates_and_places_from_real_photos() {
    let d = tmpdir("exif");
    let jpg = d.join("sydney.jpg");
    if exif_photo(&jpg, "64x48", "2023:07:14 10:20:30", Some(["S", "33/1 51/1 3540/100", "E", "151/1 12/1 4020/100"])).is_none() {
        return;
    }
    let x = crate::exif::read(&jpg).expect("EXIF read");
    assert_eq!(x.taken, Some([2023, 7, 14, 10, 20, 30]));
    let (lat, lon) = x.gps.expect("a place");
    assert!((lat + 33.8598).abs() < 1e-3 && (lon - 151.2112).abs() < 1e-3, "{lat} {lon}");
    assert_eq!(x.orientation, 1);
    let ok = std::process::Command::new("exiv2").args(["-Mset Exif.Image.Orientation 6", "-Mset Exif.GPSInfo.GPSLongitudeRef W", "mo"]).arg(&jpg).status().unwrap().success();
    assert!(ok);
    let x = crate::exif::read(&jpg).unwrap();
    assert_eq!(x.orientation, 6);
    assert!(x.gps.unwrap().1 < -151.0, "west is negative");
    // The decoder turns it by the same field.
    assert_eq!(crate::imgdec::load(&jpg, 64).unwrap().1, [48, 64]);
    let webp = d.join("plain.webp");
    assert!(std::process::Command::new("magick").args(["-size", "40x30", "xc:teal"]).arg(&webp).status().unwrap().success());
    assert!(crate::exif::read(&webp).is_none_or(|x| x.taken.is_none()), "no EXIF, no date");
    let ok = std::process::Command::new("exiv2").args(["-Mset Exif.Photo.DateTimeOriginal Ascii 2021:12:31 23:59:00", "mo"]).arg(&webp).stderr(std::process::Stdio::null()).status().unwrap().success();
    if ok {
        assert_eq!(crate::exif::read(&webp).and_then(|x| x.taken), Some([2021, 12, 31, 23, 59, 0]), "WebP's EXIF chunk");
    }
    let bytes = std::fs::read(&jpg).unwrap();
    for cut in [2, 10, 40, 100] {
        let _ = crate::exif::block(&bytes[..cut]).map(crate::exif::parse);
    }
    assert_eq!(crate::exif::parse(b"Exif\0\0MM\0*\0\0\0\x08\xff\xff"), crate::exif::Exif { orientation: 1, ..Default::default() });
    assert!(crate::exif::read(&d).is_none(), "a folder is no photo");
}

/// Tidy Folder's plans for a real folder: by kind, installers before archives, folders,
/// hidden and unknown files left alone, a folder already named for a kind keeping its
/// own; photos by the month they were taken from their EXIF, else by when they last
/// changed, said so.
#[test]
fn tidy_plans_by_kind_and_month() {
    use crate::tidy::{Mode, kind_folder, month_folder, read_plans};
    for (n, k) in [("setup.deb", Some("Installers")), ("Game.AppImage", Some("Installers")), ("yay-12.4-1-x86_64.pkg.tar.zst", Some("Installers")), ("Tool.flatpakref", Some("Installers")), ("photos.tar.gz", Some("Archives")), ("a.ZIP", Some("Archives")), ("IMG_1.JPG", Some("Pictures")), ("scan.heic", Some("Pictures")), ("report.pdf", Some("Documents")), ("notes.txt", Some("Documents")), ("song.flac", Some("Music")), ("clip.MOV", Some("Videos")), ("main.rs", None), ("README", None), (".zip", None), ("Harbour.cbz", Some("Documents")), ("book.epub", Some("Documents"))] {
        assert_eq!(kind_folder(n), k, "{n}");
    }
    assert_eq!(month_folder(2024, 6), "2024/2024-06");
    let d = tmpdir("tidy");
    let dl = d.join("Downloads");
    std::fs::create_dir_all(dl.join("Old stuff")).unwrap();
    for f in ["setup.deb", "photos.tar.gz", "report.pdf", "song.mp3", "clip.mp4", "main.rs", ".hidden.png", "screen.png"] {
        touch(&dl.join(f), f);
    }
    let shot = dl.join("trip.jpg");
    if exif_photo(&shot, "32x24", "2022:03:09 08:00:00", None).is_none() {
        return;
    }
    // Last changed on 2025-05-20 at noon, local time.
    let may = crate::timefmt::parse_local("2025-05-20T12:00:00").unwrap();
    for f in ["screen.png", "clip.mp4", "trip.jpg"] {
        std::fs::File::options().write(true).open(dl.join(f)).unwrap().set_modified(may).unwrap();
    }
    let p = read_plans(&dl).unwrap();
    let kind: Vec<(String, String)> = p.kind.iter().map(|m| (m.src.file_name().unwrap().to_string_lossy().into_owned(), m.to.clone())).collect();
    let want: Vec<(String, String)> = [("clip.mp4", "Videos"), ("photos.tar.gz", "Archives"), ("report.pdf", "Documents"), ("screen.png", "Pictures"), ("setup.deb", "Installers"), ("song.mp3", "Music"), ("trip.jpg", "Pictures")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(kind, want);
    let month: Vec<(String, String, bool)> = p.month.iter().map(|m| (m.src.file_name().unwrap().to_string_lossy().into_owned(), m.to.clone(), m.taken)).collect();
    assert_eq!(month, [("clip.mp4".to_string(), "2025/2025-05".to_string(), false), ("screen.png".into(), "2025/2025-05".into(), false), ("trip.jpg".into(), "2022/2022-03".into(), true)]);
    // Already a Pictures folder: its pictures stay.
    let pics = d.join("Pictures");
    std::fs::create_dir_all(&pics).unwrap();
    touch(&pics.join("a.png"), "x");
    touch(&pics.join("b.pdf"), "x");
    let p = read_plans(&pics).unwrap();
    assert_eq!(p.kind.iter().map(|m| m.to.as_str()).collect::<Vec<_>>(), ["Documents"]);
    assert_eq!(p.month.len(), 1, "by month it is still sorted");
    assert!(read_plans(&d.join("gone")).is_err());
    let _ = Mode::Kind;
}

/// The moves run as one job: folders made as needed, a name already taken kept apart,
/// and Undo putting every file back and taking away the folders it made (not one that
/// was there before).
#[test]
fn tidy_moves_and_undo() {
    let d = tmpdir("tidy-job");
    std::fs::create_dir_all(d.join("Music")).unwrap();
    touch(&d.join("Music/song.mp3"), "old");
    for f in ["song.mp3", "a.png", "b.pdf"] {
        touch(&d.join(f), f);
    }
    let moves = vec![(d.join("song.mp3"), d.join("Music")), (d.join("a.png"), d.join("2024/2024-06")), (d.join("b.pdf"), d.join("Documents"))];
    let job = ops::spawn(1, OpKind::Sort { moves, label: "Tidying x".into() }, egui::Context::default());
    let t0 = Instant::now();
    while !job.progress.lock().unwrap().finished {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let p = std::mem::take(&mut *job.progress.lock().unwrap());
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(std::fs::read_to_string(d.join("Music/song (1).mp3")).unwrap(), "song.mp3");
    assert_eq!(std::fs::read_to_string(d.join("Music/song.mp3")).unwrap(), "old");
    assert!(d.join("2024/2024-06/a.png").is_file() && d.join("Documents/b.pdf").is_file());
    assert_eq!(p.made, [d.join("2024"), d.join("2024/2024-06"), d.join("Documents")]);
    assert_eq!(p.produced.len(), 3);
}

/// A small EPUB made with zip as the format asks (the mimetype first, stored): a package
/// in a subfolder with a title, an author and a cover, two chapters in spine order
/// (the manifest lists them the other way round), headings, paragraphs, a list, a quote,
/// entities, a script and Markdown's own characters in the text. And a CBZ of ten
/// pages named so only a natural sort orders them, with a hidden macOS copy and a text
/// file beside them. None without zip and ImageMagick.
pub(crate) fn book_fixture(d: &Path) -> Option<()> {
    if !crate::media::have("zip") || !crate::media::have("magick") {
        return None;
    }
    let src = d.join("book-src");
    let w = |p: &str, s: &str| touch(&src.join(p), s);
    w("mimetype", "application/epub+zip");
    w("META-INF/container.xml", r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#);
    w("OEBPS/content.opf", r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>The Lighthouse &amp; the Sea</dc:title>
    <dc:creator>Ada Keeper</dc:creator>
    <meta name="cover" content="cover-img"/>
  </metadata>
  <manifest>
    <item id="c2" href="text/chapter%202.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="text/chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover-img" href="images/cover.png" media-type="image/png"/>
    <item id="css" href="style.css" media-type="text/css"/>
  </manifest>
  <spine><itemref idref="c1"/><itemref idref="c2"/></spine>
</package>"#);
    w("OEBPS/style.css", "p { margin: 0 }");
    w("OEBPS/text/chapter1.xhtml", r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title><style>p{}</style></head><body><section><h1>Chapter One</h1><p>The keeper climbed   the
stairs at dusk &hellip; every night, for <em>forty</em> years.</p><script>alert(1)</script><p>It cost $5 and *never* more.</p><ul><li>Oil</li><li>Wicks</li></ul><blockquote><p>Light the lamp.</p></blockquote><p>1. Not a list.</p></section></body></html>"#);
    w("OEBPS/text/chapter 2.xhtml", r#"<html><body><h2>Chapter Two</h2><p>A ship&#8217;s bell rang<br/>twice.</p><!-- a comment --></body></html>"#);
    std::fs::create_dir_all(src.join("OEBPS/images")).unwrap();
    assert!(std::process::Command::new("magick").args(["-size", "60x90", "gradient:#123-#9ab"]).arg(src.join("OEBPS/images/cover.png")).status().unwrap().success());
    let book = d.join("Lighthouse.epub");
    let _ = std::fs::remove_file(&book);
    let zip = |args: &[&str]| assert!(std::process::Command::new("zip").args(args).current_dir(&src).stdout(std::process::Stdio::null()).status().unwrap().success());
    zip(&["-X0", book.to_str().unwrap(), "mimetype"]);
    zip(&["-rX9", book.to_str().unwrap(), "META-INF", "OEBPS"]);
    let pages = d.join("comic-src");
    std::fs::create_dir_all(pages.join("__MACOSX")).unwrap();
    for k in 1..=10 {
        let colour = format!("rgb({},{},{})", k * 20, 255 - k * 20, 120);
        assert!(std::process::Command::new("magick").args(["-size", &format!("{}x{}", 100 + k, 150), &format!("xc:{colour}")]).arg(pages.join(format!("page{k}.png"))).status().unwrap().success());
    }
    std::fs::write(pages.join("__MACOSX/._page1.png"), "junk").unwrap();
    std::fs::write(pages.join("credits.txt"), "drawn by hand").unwrap();
    let comic = d.join("Harbour.cbz");
    let _ = std::fs::remove_file(&comic);
    assert!(std::process::Command::new("zip").args(["-rqX"]).arg(&comic).arg(".").current_dir(&pages).status().unwrap().success());
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&pages);
    Some(())
}

/// A real font to look at: DejaVu Sans when installed, else Maia's own fallback.
pub(crate) fn sample_font() -> PathBuf {
    let dejavu = PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf");
    if dejavu.is_file() { dejavu } else { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fallback-sans.ttf") }
}

/// An EPUB read through the archive code: title, author, cover and chapters in spine
/// order as Markdown that keeps headings, paragraphs, lists and quotes, leaves scripts
/// and comments out and shows Markdown's own characters as typed; a zip that isn't a
/// book says so. A comic's pages in natural order without the junk beside them, each
/// decoded at the size asked.
#[test]
fn books_and_comics_read_through_the_archive_code() {
    let d = tmpdir("books");
    if !crate::archive::available() || book_fixture(&d).is_none() {
        return;
    }
    let md = crate::books::read_book(&d.join("Lighthouse.epub")).unwrap();
    let lines: Vec<String> = md.lines().map(String::from).collect();
    let doc = crate::markdown::parse(&lines, None);
    use crate::markdown::Block;
    let text = |b: &Block| match b {
        Block::Heading(_, r) | Block::Para(r) => crate::markdown::plain(r),
        _ => String::new(),
    };
    let heads: Vec<(u8, String)> = doc.blocks.iter().filter_map(|b| if let Block::Heading(n, r) = b { Some((*n, crate::markdown::plain(r))) } else { None }).collect();
    assert_eq!(heads, [(1, "The Lighthouse & the Sea".to_string()), (1, "Chapter One".into()), (2, "Chapter Two".into())]);
    let paras: Vec<String> = doc.blocks.iter().filter(|b| matches!(b, Block::Para(_))).map(text).collect();
    assert!(paras.contains(&"Ada Keeper".to_string()), "{paras:?}");
    assert!(paras.contains(&"The keeper climbed the stairs at dusk … every night, for forty years.".to_string()), "{paras:?}");
    assert!(paras.contains(&"It cost $5 and *never* more.".to_string()), "as typed, no formula, no emphasis: {paras:?}");
    assert!(paras.contains(&"1. Not a list.".to_string()), "{paras:?}");
    assert!(paras.contains(&"A ship’s bell rang twice.".to_string()), "{paras:?}");
    assert!(doc.blocks.iter().any(|b| matches!(b, Block::List { items, .. } if items.len() == 2)));
    assert!(doc.blocks.iter().any(|b| matches!(b, Block::Quote(_))));
    assert!(!md.contains("alert") && !md.contains("comment") && !md.contains("p{}"));
    assert!(md.find("Chapter One").unwrap() < md.find("Chapter Two").unwrap(), "spine order");
    assert_eq!(doc.images.len(), 1, "the cover");
    let cover = PathBuf::from(&doc.images[0].src);
    assert!(cover.is_absolute() && cover.is_file(), "the cover extracted: {cover:?}");
    assert_eq!(crate::imgdec::load(&cover, 128).unwrap().1, [60, 90]);
    // Not a book.
    let zip = d.join("plain.epub");
    assert!(std::process::Command::new("zip").args(["-qj"]).arg(&zip).arg(&cover).status().unwrap().success());
    assert_eq!(crate::books::read_book(&zip), Err("Not an EPUB book".into()));
    // The comic.
    let comic = d.join("Harbour.cbz");
    let pages = crate::books::comic_pages(&comic).unwrap();
    let names: Vec<String> = pages.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    assert_eq!(names, (1..=10).map(|k| format!("page{k}.png")).collect::<Vec<_>>());
    let p10 = crate::books::comic_page(&comic, &pages[9], 4096).unwrap();
    assert_eq!((p10.w, p10.h), (110, 150));
    let small = crate::books::comic_page(&comic, &pages[0], 75).unwrap();
    assert_eq!((small.w, small.h), (51, 75), "101 × 150 fitted to the size asked");
    // bsdtar reads by content: the same pages under a CBR's name.
    std::fs::copy(&comic, d.join("Harbour.cbr")).unwrap();
    assert_eq!(crate::books::comic_pages(&d.join("Harbour.cbr")).unwrap().len(), 10);
    assert!(crate::books::comic_page(&comic, Path::new("missing.png"), 100).is_none());
}

/// XML just well enough read: tags with namespaces, attributes in either quotes,
/// comments, CDATA and declarations.
#[test]
fn xml_tokens_and_attributes() {
    use crate::books::{Tok, attr, tokens};
    let t = tokens(r#"<?xml version="1.0"?><!-- c --><opf:item id='a' href="x y.html"/><p>Hi<![CDATA[ <b> ]]></p>"#);
    assert_eq!(t.len(), 5, "{t:?}");
    assert!(matches!(&t[0], Tok::Tag { name, empty: true, close: false, .. } if name == "item"));
    if let Tok::Tag { attrs, .. } = &t[0] {
        assert_eq!(attr(attrs, "id").as_deref(), Some("a"));
        assert_eq!(attr(attrs, "href").as_deref(), Some("x y.html"));
        assert_eq!(attr(attrs, "media-type"), None);
    }
    assert_eq!(t[2], Tok::Text("Hi"));
    assert_eq!(t[3], Tok::Text(" <b> "));
    assert!(matches!(&t[4], Tok::Tag { name, close: true, .. } if name == "p"));
    assert_eq!(attr(r#" xlink:href="&amp;x""#, "href").as_deref(), Some("&x"));
    assert_eq!(crate::books::package_path("<container><rootfiles><rootfile full-path=\"a/b.opf\"/></rootfiles></container>"), Some(PathBuf::from("a/b.opf")));
    assert!(tokens("<p unclosed").is_empty());
}

/// A real font's names as fontconfig reads them, sample lines drawn with it, the same
/// font packed as WOFF read back, WOFF2 and a non-font refused; Install copying it into
/// a fonts folder and calling fc-cache, a second Install finding it there.
#[test]
fn fonts_previewed_and_installed() {
    use crate::fontview::{info, install, read, samples, sfnt};
    let font = sample_font();
    let raw = std::fs::read(&font).unwrap();
    let i = info(&raw);
    if crate::media::have("fc-scan") {
        let o = std::process::Command::new("fc-scan").args(["--format", "%{family[0]}|%{style[0]}"]).arg(&font).output().unwrap();
        let fc = String::from_utf8_lossy(&o.stdout).into_owned();
        assert_eq!(fc, format!("{}|{}", i.family, i.style), "the names fontconfig gives");
    }
    assert!(i.glyphs > 10 && !i.version.is_empty() && !i.version.starts_with("Version"), "{i:?}");
    let d = tmpdir("fonts");
    let p = read(&font, 1.6, Some(&d.join("fonts"))).unwrap();
    assert_eq!(p.info, i);
    assert_eq!(p.lines.len(), samples().len());
    assert!(!p.installed);
    let (pt, img) = &p.lines[7];
    assert_eq!(*pt, 54.0);
    assert!(img.size[1] as f32 >= 54.0 * 1.6 * 0.9 && img.size[0] > img.size[1] * 10, "{:?}", img.size);
    assert!(img.pixels.iter().any(|c| c.a() == 255) && img.pixels.iter().any(|c| c.a() == 0), "ink and paper");
    // The same font as a WOFF: tables deflated behind WOFF's own directory.
    let n = u16::from_be_bytes([raw[4], raw[5]]) as usize;
    let mut tables = vec![];
    for k in 0..n {
        let e = 12 + k * 16;
        let be = |i: usize| u32::from_be_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
        let (tag, sum, off, len) = (be(e), be(e + 4), be(e + 8) as usize, be(e + 12) as usize);
        tables.push((tag, sum, raw[off..off + len].to_vec()));
    }
    let mut woff = b"wOFF".to_vec();
    woff.extend_from_slice(&raw[0..4]);
    woff.extend_from_slice(&[0; 4]);
    woff.extend_from_slice(&(n as u16).to_be_bytes());
    woff.extend_from_slice(&[0; 30]);
    let mut body = vec![];
    let start = 44 + n * 20;
    for (tag, sum, data) in &tables {
        // Compressed where that is smaller, as WOFF asks; stored as it is otherwise.
        let z = fdeflate::compress_to_vec(data);
        let stored = if z.len() < data.len() { z } else { data.clone() };
        let off = start + body.len();
        for v in [*tag, off as u32, stored.len() as u32, data.len() as u32, *sum] {
            woff.extend_from_slice(&v.to_be_bytes());
        }
        body.extend_from_slice(&stored);
        body.resize((body.len() + 3) & !3, 0);
    }
    woff.extend_from_slice(&body);
    let total = woff.len() as u32;
    woff[8..12].copy_from_slice(&total.to_be_bytes());
    let unpacked = sfnt(&woff).unwrap();
    assert_eq!(info(&unpacked), i, "the WOFF's names");
    std::fs::write(d.join("packed.woff"), &woff).unwrap();
    assert_eq!(read(&d.join("packed.woff"), 1.0, None).unwrap().lines.len(), samples().len());
    if crate::media::have("woff2_compress") {
        std::fs::copy(&font, d.join("w2.ttf")).unwrap();
        let _ = std::process::Command::new("woff2_compress").arg(d.join("w2.ttf")).stdout(std::process::Stdio::null()).status();
        if d.join("w2.woff2").exists() {
            assert_eq!(read(&d.join("w2.woff2"), 1.0, None).err().as_deref(), Some("WOFF2 fonts can't be shown here"));
        }
    }
    std::fs::write(d.join("fake.ttf"), "not a font at all").unwrap();
    assert_eq!(read(&d.join("fake.ttf"), 1.0, None).err().as_deref(), Some("Not a font"));
    let mut cut = raw.clone();
    cut.truncate(200);
    std::fs::write(d.join("cut.ttf"), &cut).unwrap();
    assert!(read(&d.join("cut.ttf"), 1.0, None).is_err(), "a damaged font is refused, not a crash");
    // Install, with a stand-in fc-cache.
    let tools = d.join("tools");
    fake_tool(&tools, "fc-cache", "exit 0");
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(tools.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let fonts = d.join("fonts");
    let name = font.file_name().unwrap();
    let got = install(&font, &fonts, Some(&t)).unwrap();
    assert_eq!(got, Some(fonts.join(name)));
    assert_eq!(std::fs::read(fonts.join(name)).unwrap(), raw);
    assert_eq!(fake_calls(&tools), [vec!["fc-cache".to_string(), fonts.display().to_string()]]);
    assert_eq!(install(&font, &fonts, Some(&t)).unwrap(), None, "already there");
    assert!(read(&font, 1.0, Some(&fonts)).unwrap().installed);
    // Another font under the same name keeps both.
    std::fs::create_dir_all(d.join("x")).unwrap();
    std::fs::write(d.join("x").join(name), &woff).unwrap();
    let other = install(&d.join("x").join(name), &fonts, None).unwrap().unwrap();
    assert_ne!(other, fonts.join(name));
}

/// Where a photo was taken, written for people and as a map's address.
#[test]
fn photo_places_and_map_links() {
    use crate::exif::{map_url, place};
    assert_eq!(place(-33.8598, 151.2112), "33.85980° S, 151.21120° E");
    assert_eq!(place(48.85837, -2.294481), "48.85837° N, 2.29448° W");
    assert_eq!(map_url(48.85837, 2.294481), "https://www.openstreetmap.org/?mlat=48.858370&mlon=2.294481#map=16/48.858370/2.294481");
}

/// A home folder for the Timeline: files changed today, three days ago, three weeks
/// ago and two months ago, and recent ones where it never looks (hidden folders and
/// files, a cache, a git folder, installed packages, a Rust build).
pub(crate) fn timeline_fixture(h: &Path) {
    let now = std::time::SystemTime::now();
    let ago = |days: u64| now - Duration::from_secs(days * 86_400 + 60);
    let at = |p: &str, days: u64| {
        touch(&h.join(p), p);
        std::fs::File::options().write(true).open(h.join(p)).unwrap().set_modified(if days == 0 { now - Duration::from_secs(60) } else { ago(days) }).unwrap();
    };
    at("Documents/letter.odt", 0);
    at("Downloads/ticket.pdf", 3);
    at("Pictures/2026/beach.png", 21);
    at("Documents/old report.pdf", 60);
    at(".cache/thumb.png", 0);
    at(".notes.txt", 0);
    at("Code/app/.git/index", 0);
    at("Code/app/web/package.json", 40);
    at("Code/app/web/node_modules/left-pad/index.js", 0);
    at("Code/app/Cargo.toml", 40);
    at("Code/app/target/debug/app", 0);
    at("Code/app/src/main.rs", 1);
}

/// The Timeline: the last month's files under home, the ones where it never looks left
/// out; opened by its id, it shows newest first in the date groups.
#[test]
fn timeline_lists_the_last_month() {
    use crate::smart::{TIMELINE, by_id, search, timeline, timeline_view};
    let d = tmpdir("timeline");
    timeline_fixture(&d);
    let mut sf = timeline();
    assert_eq!((sf.id, sf.name.as_str(), sf.days), (TIMELINE, "Timeline", Some(31)));
    assert_eq!(by_id(&[], TIMELINE), Some(timeline()));
    assert_eq!(by_id(&[], 7), None);
    sf.root = d.clone();
    let (cancel, cut) = (std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicBool::new(false));
    let mut found: Vec<String> = vec![];
    search(&sf, &mut |b| {
        found.extend(b.iter().map(|e| e.path.strip_prefix(&d).unwrap().to_string_lossy().into_owned()));
        true
    }, &cancel, &cut);
    found.sort();
    assert_eq!(found, ["Code/app/src/main.rs", "Documents/letter.odt", "Downloads/ticket.pdf", "Pictures/2026/beach.png"]);
    let v = timeline_view(crate::config::ViewSettings::default());
    assert_eq!((v.sort, v.desc, v.group), (crate::config::SortKey::Modified, true, crate::config::GroupBy::Modified));
}

/// Compress With Password through the real bsdtar and 7z: a zip locked with AES-256 and
/// a 7z with its names locked too, the password never on any command line while they
/// are made (every process's argv watched); opened by Maia's own extraction only with
/// the password, a wrong one refused as such.
#[test]
fn locked_archives_open_only_with_their_password() {
    use crate::archive::{NEEDS_PASSWORD, WRONG_PASSWORD, create_locked, extract, forget_password, list, set_password};
    if !crate::archive::available() {
        return;
    }
    let d = tmpdir("locked");
    let src = d.join("src");
    touch(&src.join("plans/notes – é.txt"), "the secret plans");
    // Big and incompressible, so the tools run long enough to be watched.
    let mut noise = vec![0u8; 24 << 20];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for b in noise.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    std::fs::write(src.join("noise.bin"), &noise).unwrap();
    let pw = "correct horse 9 battery";
    let names = vec![std::ffi::OsString::from("plans"), std::ffi::OsString::from("noise.bin")];
    let mut kinds = vec!["zip"];
    if crate::archive::can_lock_7z() {
        kinds.push("7z");
    }
    for ext in kinds {
        let out = d.join(format!("locked.{ext}"));
        let (o2, s2, n2) = (out.clone(), src.clone(), names.clone());
        let job = std::thread::spawn(move || create_locked(&o2, &s2, &n2, pw));
        // The archiver's command line while it runs (the process writing the part
        // file): the password is not in it.
        let (mut seen_tool, mut leaked) = (false, false);
        while !job.is_finished() {
            for e in std::fs::read_dir("/proc").unwrap().flatten() {
                let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
                if !matches!(comm.trim(), "bsdtar" | "7z" | "7za" | "7zz") {
                    continue;
                }
                let Ok(c) = std::fs::read(e.path().join("cmdline")) else { continue };
                let c = String::from_utf8_lossy(&c).replace('\0', " ");
                if c.contains(&format!(".locked.{ext}.part")) {
                    seen_tool = true;
                    leaked |= c.contains(pw);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        job.join().unwrap().unwrap();
        assert!(!leaked, "the password on the {ext} command line");
        assert!(seen_tool, "the {ext} tool was watched while it ran");
        // The real 7z says how it is locked.
        if crate::media::have("7z") {
            let o = std::process::Command::new("7z").args(["l", "-slt", "-ba", &format!("-p{pw}")]).arg(&out).output().unwrap();
            let t = String::from_utf8_lossy(&o.stdout);
            assert!(t.contains("Encrypted = +"), "{t}");
            if ext == "zip" {
                assert!(t.contains("AES-256"), "{t}");
            }
        }
        // Without the password: the zip lists its names but won't give its contents;
        // the 7z won't even list.
        forget_password(&out);
        let stage = d.join(format!("x-{ext}"));
        if ext == "zip" {
            assert!(list(&out).unwrap().iter().any(|i| i.path == Path::new("plans/notes – é.txt")));
        } else {
            assert_eq!(list(&out).err().as_deref(), Some(NEEDS_PASSWORD));
        }
        let e = extract(&out, &[PathBuf::from("plans")], &stage);
        assert!(e.as_ref().is_err_and(|e| crate::archive::wants_password(e)), "{e:?}");
        set_password(&out, "wrong".into());
        let e = extract(&out, &[PathBuf::from("plans")], &d.join(format!("w-{ext}")));
        assert_eq!(e.err().as_deref(), Some(WRONG_PASSWORD));
        set_password(&out, pw.into());
        let _ = std::fs::remove_dir_all(&stage);
        extract(&out, &[PathBuf::from("plans"), PathBuf::from("noise.bin")], &stage).unwrap();
        assert_eq!(std::fs::read_to_string(stage.join("plans/notes – é.txt")).unwrap(), "the secret plans");
        assert!(std::fs::read(stage.join("noise.bin")).unwrap() == noise, "every byte back");
        assert!(!d.join(format!(".locked.{ext}.part")).exists());
    }
    assert!(create_locked(&d.join("bad.zip"), &src, &names, "two\nlines").is_err());
    assert!(create_locked(&d.join("bad.zip"), &src, &names, "").is_err());
    assert!(format!("{:?}", crate::archive::Secret(pw.into())) == "Secret(…)", "never printed");
    let _ = std::fs::remove_dir_all(&d);
}

/// ffprobe's own words for the tags of a FLAC (in the file) and an Opus (with its
/// stream), as captured from it; spellings and cases files use read as one.
#[test]
fn music_tags_parsed_from_ffprobe() {
    use crate::musictags::{parse, track_name};
    let flac = "format.tags.title=\"T\"\nformat.tags.artist=\"A\"\nformat.tags.album_artist=\"AA\"\nformat.tags.track=\"3/12\"\nformat.tags.date=\"2020\"\nformat.tags.genre=\"Jazz\"\nformat.tags.encoder=\"Lavf63.1.102\"\n";
    let t = parse(flac);
    assert_eq!(t.values, ["T", "A", "", "AA", "3/12", "2020", "Jazz"].map(String::from));
    assert!(!t.cover);
    let opus = "streams.stream.0.tags.encoder=\"Lavc63.1.102 libopus\"\nstreams.stream.0.tags.TITLE=\"Say \\\"hi\\\"\"\nstreams.stream.0.tags.ALBUMARTIST=\"Band\"\nstreams.stream.0.tags.TRACKNUMBER=\"7\"\nstreams.stream.1.disposition.attached_pic=1\n";
    let t = parse(opus);
    assert_eq!((t.get("title"), t.get("album_artist"), t.get("track")), ("Say \"hi\"", "Band", "7"));
    assert!(t.cover);
    assert_eq!(track_name("3/12"), "03");
    assert_eq!(track_name("12"), "12");
    assert_eq!(track_name("A1"), "A1");
}

/// Tags written through the real ffmpeg into MP3, FLAC, M4A and Opus: the fields
/// changed, the others and the sound kept as they were, a cover put in from a picture
/// and taken out again (Opus has nowhere to keep one); the original in the trash and
/// back by Undo's own way, byte for byte.
#[test]
fn music_tags_written_through_ffmpeg() {
    use crate::musictags::{Cover, read, write};
    let d = tmpdir("musictags");
    let Some(tools) = convert_tools(&d.join("tools")) else { return };
    let w = d.join("work");
    std::fs::create_dir_all(&w).unwrap();
    convert_inputs(&w, &tools);
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(tools.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let ff = |args: &[&str]| assert!(std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).current_dir(&w).status().unwrap().success());
    ff(&["-i", "tone.mp3", "-map", "0:a", "-c:a", "flac", "-metadata", "genre=Drone", "tone.flac"]);
    ff(&["-i", "tone.mp3", "-map", "0:a", "-c:a", "aac", "tone.m4a"]);
    ff(&["-i", "tone.mp3", "-map", "0:a", "-c:a", "libopus", "tone.opus"]);
    ff(&["-f", "lavfi", "-i", "testsrc=size=80x80", "-frames:v", "1", "art.png"]);
    let sound = |p: &Path| {
        let o = std::process::Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(p).args(["-map", "0:a", "-f", "md5", "-"]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    for f in ["tone.mp3", "tone.flac", "tone.m4a", "tone.opus"] {
        let p = w.join(f);
        let before = read(&t, &p).unwrap();
        let (bytes, audio) = (std::fs::read(&p).unwrap(), sound(&p));
        write(&t, &p, &[(0, "Evening Tone".into()), (2, "Sine Waves".into()), (4, "4/9".into())], &Cover::Set(w.join("art.png"))).unwrap();
        let after = read(&t, &p).unwrap();
        assert_eq!((after.get("title"), after.get("album"), after.get("track")), ("Evening Tone", "Sine Waves", "4/9"), "{f}");
        assert_eq!(after.get("artist"), before.get("artist"), "{f}: untouched fields kept");
        assert_eq!(after.get("genre"), before.get("genre"), "{f}");
        assert_eq!(after.cover, f != "tone.opus", "{f}: the cover");
        assert_eq!(sound(&p), audio, "{f}: the sound itself untouched");
        let trashed: Vec<_> = crate::trashcan::list().into_iter().filter(|it| it.original == p).collect();
        assert_eq!(trashed.len(), 1, "{f}: the original in the trash");
        assert_eq!(std::fs::read(&trashed[0].file).unwrap(), bytes);
        if f != "tone.opus" {
            write(&t, &p, &[], &Cover::Remove).unwrap();
            assert!(!read(&t, &p).unwrap().cover, "{f}: cover taken out");
            assert_eq!(read(&t, &p).unwrap().get("title"), "Evening Tone");
        }
        // Undo: the newest original back.
        assert_eq!(crate::picfix::undo_replaced(std::slice::from_ref(&p)), 0);
        if f == "tone.opus" {
            assert_eq!(std::fs::read(&p).unwrap(), bytes, "{f}: back as it was");
        }
    }
    assert!(std::fs::read_dir(&w).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".part")));
    std::fs::write(w.join("fake.mp3"), "ID3 not really").unwrap();
    assert!(write(&t, &w.join("fake.mp3"), &[(0, "x".into())], &Cover::Keep).is_err());
    assert_eq!(std::fs::read_to_string(w.join("fake.mp3")).unwrap(), "ID3 not really", "a failed write leaves the file");
    assert!(write(&t, &w.join("clip.avi"), &[(0, "x".into())], &Cover::Keep).is_err());
}

/// Batch Rename's tag words: {track} padded, {artist} and {title} filled in, a slash in a
/// tag never making a folder, a file without tags leaving the words empty.
#[test]
fn batch_rename_names_from_music_tags() {
    use crate::batchname::{Rules, names_with, uses_tags};
    use crate::musictags::Tags;
    assert!(uses_tags("{track} {title}") && !uses_tags("{name} ###"));
    let tag = |title: &str, artist: &str, track: &str| {
        let mut t = Tags::default();
        t.values[0] = title.into();
        t.values[1] = artist.into();
        t.values[4] = track.into();
        Some(t)
    };
    let items: Vec<(String, bool)> = ["a.mp3", "b.flac", "c.mp3"].iter().map(|n| (n.to_string(), false)).collect();
    let r = Rules { name: "{track} {artist} - {title}".into(), start: "1".into(), ..Default::default() };
    let got = names_with(&items, &r, &[tag("Intro", "Band", "1/9"), tag("AC/DC #1", "Them", "10"), None]).unwrap();
    assert_eq!(got, ["01 Band - Intro.mp3", "10 Them - AC-DC #1.flac", "-.mp3"], "no tags: no stray spaces");
}

/// Formatting through stand-ins for lsblk, udisksctl and gdbus (never the real ones):
/// a removable stick unmounted first, then udisks asked over D-Bus for a new partition
/// table and one partition formatted with the label; the system disk, a drive lsblk no
/// longer calls removable and a bad label refused before anything runs.
#[test]
fn format_a_removable_drive_through_udisks() {
    use crate::drives::{Fs, format, format_calls, label_for, list, object_path};
    let d = tmpdir("format");
    let rows = [
        r#"PATH="/dev/sda" PKNAME="" LABEL="" FSTYPE="" SIZE="500107862016" RM="0" HOTPLUG="0" TYPE="disk" MOUNTPOINTS="""#,
        r#"PATH="/dev/sda2" PKNAME="sda" LABEL="" FSTYPE="btrfs" SIZE="499570991104" RM="0" HOTPLUG="0" TYPE="part" MOUNTPOINTS="/""#,
        r#"PATH="/dev/sdz" PKNAME="" LABEL="" FSTYPE="" SIZE="16008609792" RM="1" HOTPLUG="1" TYPE="disk" MOUNTPOINTS="""#,
        r#"PATH="/dev/sdz1" PKNAME="sdz" LABEL="OLD" FSTYPE="vfat" SIZE="16007561216" RM="1" HOTPLUG="1" TYPE="part" MOUNTPOINTS="/run/media/me/OLD""#,
    ];
    std::fs::write(d.join("rows"), rows.join("\n") + "\n").unwrap();
    fake_tool(&d, "lsblk", "cat \"$D/rows\"");
    fake_tool(&d, "udisksctl", "exit 0");
    fake_tool(&d, "gdbus", "echo '()'");
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.clone()));
    let t = crate::drives::Tools::current().unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
    let all = list(&t);
    let stick = all.iter().find(|x| x.dev == "/dev/sdz1").unwrap().clone();
    let system = all.iter().find(|x| x.dev == "/dev/sda2").unwrap().clone();
    assert!(stick.removable && !system.removable);
    let _ = std::fs::remove_file(d.join("calls.log"));
    format(&t, &stick, Fs::Exfat, "Holiday 26").unwrap();
    let calls = fake_calls(&d);
    let progs: Vec<&str> = calls.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(progs, ["lsblk", "udisksctl", "gdbus", "gdbus"], "unmounted, then the two udisks calls");
    assert_eq!(calls[1][1..], ["unmount", "-b", "/dev/sdz1"]);
    let want = format_calls("/dev/sdz", Fs::Exfat, "Holiday 26");
    assert_eq!(calls[2][1..], want[0][..]);
    assert_eq!(calls[3][1..], want[1][..]);
    assert!(want[0].contains(&"/org/freedesktop/UDisks2/block_devices/sdz".to_string()) && want[0].contains(&"org.freedesktop.UDisks2.Block.Format".to_string()) && want[0].contains(&"'dos'".to_string()));
    assert_eq!(want[1][want[1].len() - 2..], ["'exfat'".to_string(), "{'label': <'Holiday 26'>}".to_string()]);
    assert!(format_calls("/dev/sdz", Fs::Ext4, "x")[1].last().unwrap().contains("'take-ownership': <true>"));
    assert_eq!(format_calls("/dev/sdz", Fs::Fat32, "X")[1][format_calls("/dev/sdz", Fs::Fat32, "X")[1].len() - 5], "'0x0c'");
    // Refused before anything runs.
    let _ = std::fs::remove_file(d.join("calls.log"));
    assert!(format(&t, &system, Fs::Ext4, "x").is_err(), "the system disk");
    let mut lied = system.clone();
    lied.removable = true;
    assert!(format(&t, &lied, Fs::Ext4, "x").is_err(), "lsblk is asked again");
    assert!(format(&t, &stick, Fs::Fat32, "far too long a label").is_err());
    assert!(format(&t, &stick, Fs::Exfat, "it's").is_err(), "a quote would end the label");
    assert!(fake_calls(&d).iter().all(|c| c[0] != "gdbus" && c[0] != "udisksctl"), "{:?}", fake_calls(&d));
    let mut gone = stick.clone();
    gone.disk = "/dev/sdq".into();
    assert_eq!(format(&t, &gone, Fs::Exfat, "x").err().as_deref(), Some("The drive is no longer connected"));
    // Labels and object paths.
    assert_eq!(label_for(Fs::Fat32, " photos 26 ").unwrap(), "PHOTOS 26");
    assert_eq!(label_for(Fs::Exfat, "Photos").unwrap(), "Photos");
    assert!(label_for(Fs::Fat32, "TWELVE CHARS").is_err() && label_for(Fs::Exfat, "TWELVE CHARS").is_ok());
    assert!(label_for(Fs::Ext4, "a/b").is_err());
    assert_eq!(object_path("/dev/mmcblk0"), "/org/freedesktop/UDisks2/block_devices/mmcblk0");
    assert_eq!(object_path("/dev/dm-0"), "/org/freedesktop/UDisks2/block_devices/dm_2d0");
}

/// Quick Open's files: in a repository, git's (tracked and new, not what .gitignore
/// leaves out), in one order; outside one, a walk past hidden folders and build output.
/// A file's own name outranks its folders, and a query a newer one replaced gives up.
#[test]
fn quick_open_lists_and_ranks_a_projects_files() {
    use crate::quickopen::{list, rank};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let d = tmpdir("quickopen");
    let stop = AtomicBool::new(false);
    for (f, body) in [("src/main.rs", "fn main() {}\n"), ("src/view/list.rs", "\n"), ("docs/main notes.md", "\n"), ("README.md", "\n"), ("node_modules/pkg/index.js", "\n"), (".cache/x", "\n"), ("package.json", "{}\n")] {
        touch(&d.join(f), body);
    }
    // Not a repository: the walk, without node_modules (beside package.json) or .cache.
    let walked = list(&d, &stop);
    assert_eq!(walked, ["README.md", "docs/main notes.md", "package.json", "src/main.rs", "src/view/list.rs"]);
    if crate::media::have("git") {
        git(&d, &["init", "-q"]);
        touch(&d.join(".gitignore"), "node_modules/\nbuild.log\n");
        touch(&d.join("build.log"), "noise\n");
        git(&d, &["add", "src", "README.md", ".gitignore"]);
        git(&d, &["-c", "user.name=T", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "first"]);
        touch(&d.join("src/new.rs"), "\n");
        let files = list(&d, &stop);
        assert_eq!(files, [".cache/x", ".gitignore", "README.md", "docs/main notes.md", "package.json", "src/main.rs", "src/new.rs", "src/view/list.rs"], "tracked and new (hidden too, as git sees them), in one order; ignored left out");
        let g = AtomicU64::new(1);
        let hits: Vec<&str> = rank(&files, "main", &g, 1).unwrap().iter().map(|&i| files[i as usize].as_str()).collect();
        assert_eq!(hits.first(), Some(&"src/main.rs"), "the name, ahead of a folder or a longer name");
        assert!(hits.contains(&"docs/main notes.md"));
        let hits: Vec<&str> = rank(&files, "vl", &g, 1).unwrap().iter().map(|&i| files[i as usize].as_str()).collect();
        assert_eq!(hits, ["src/view/list.rs"], "letters across folders");
        assert!(rank(&files, "zzz", &g, 1).unwrap().is_empty());
        assert_eq!(rank(&files, "main", &g, 0), None, "a newer query took over");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A filter that hides a selected file takes it out of the selection: after renaming
/// one file (which stays selected) and typing another's name, F2 or Delete act on the
/// file found, never on the one out of sight.
#[test]
fn a_filter_drops_hidden_files_from_the_selection() {
    let d = tmpdir("filter-selection");
    for f in ["a.txt", "report.md", "notes.txt"] {
        touch(&d.join(f), "x\n");
    }
    let stats = HashMap::new();
    let mut t = Tab::new(1, d.clone(), ViewSettings::default(), egui::Context::default());
    pump(&mut t, &stats);
    let at = |t: &Tab, n: &str| t.entries.iter().position(|e| e.name == n).unwrap() as u32;
    let (a, report) = (at(&t, "a.txt"), at(&t, "report.md"));
    t.select_only(a);
    t.filter = "report".into();
    t.dirty = true;
    pump(&mut t, &stats);
    assert_eq!(t.cursor, Some(report), "the cursor on the match");
    assert!(t.selected.is_empty(), "a.txt hidden, no longer selected");
    assert_eq!(t.target_entries().iter().map(|e| e.name.clone()).collect::<Vec<_>>(), ["report.md"], "what F2 and Delete act on");
    // A selection the filter keeps in view stays.
    t.select_only(report);
    t.filter = "rep".into();
    t.dirty = true;
    pump(&mut t, &stats);
    assert!(t.selected.contains(&report));
    let _ = std::fs::remove_dir_all(&d);
}

/// Nothing a copy or move replaces is lost: a replacement is written beside what it
/// replaces and swapped in once whole, the old one going to the trash; one that fails
/// halfway (a file it can't read) leaves the old one as it was and nothing half made;
/// merging replaces a file in one step; a link to a folder deleted is only the link;
/// names swapped in one rename both survive, and a rename never lands on a name taken.
#[test]
fn file_operations_never_lose_what_they_replace() {
    use std::os::unix::fs::PermissionsExt;
    let ctx = egui::Context::default();
    let d = tmpdir("safety");
    let (src, dst) = (d.join("src"), d.join("dst"));
    let run = |kind: OpKind| {
        let j = ops::spawn(1, kind, ctx.clone());
        wait_job(&j);
        let p = j.progress.lock().unwrap();
        (p.errors.clone(), p.produced.clone())
    };
    let over = |p: &Path| HashMap::from([(p.to_path_buf(), Conflict::Overwrite)]);
    let parts = |dir: &Path| std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("maia-part")).count();
    let trashed = |orig: &Path| crate::trashcan::list().into_iter().filter(|i| i.original == orig).count();
    // Replaced, with the old one in the trash.
    touch(&src.join("a.txt"), "new");
    touch(&dst.join("a.txt"), "old");
    let (errs, made) = run(OpKind::Copy { srcs: vec![src.join("a.txt")], dest: dst.clone(), decide: over(&src.join("a.txt")) });
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "new");
    assert_eq!(made, [(src.join("a.txt"), dst.join("a.txt"))], "recorded where it landed");
    assert_eq!(trashed(&dst.join("a.txt")), 1, "the old one can be had back");
    assert_eq!(parts(&dst), 0);
    // A replacement that fails: the old file stays, nothing half made is left.
    touch(&src.join("locked.txt"), "can't read me");
    std::fs::set_permissions(src.join("locked.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
    touch(&dst.join("locked.txt"), "precious");
    let unreadable = std::fs::File::open(src.join("locked.txt")).is_err();
    if unreadable {
        let (errs, _) = run(OpKind::Copy { srcs: vec![src.join("locked.txt")], dest: dst.clone(), decide: over(&src.join("locked.txt")) });
        assert_eq!(errs.len(), 1);
        assert_eq!(std::fs::read_to_string(dst.join("locked.txt")).unwrap(), "precious", "kept when the new one couldn't be read");
        assert_eq!(parts(&dst), 0, "no part left behind");
        // Merging a folder whose file can't be read: the file there is kept too.
        std::fs::create_dir_all(src.join("box")).unwrap();
        std::fs::rename(src.join("locked.txt"), src.join("box/locked.txt")).unwrap();
        touch(&dst.join("box/locked.txt"), "precious too");
        let (errs, _) = run(OpKind::Copy { srcs: vec![src.join("box")], dest: dst.clone(), decide: HashMap::from([(src.join("box"), Conflict::Merge)]) });
        assert_eq!(errs.len(), 1);
        assert_eq!(std::fs::read_to_string(dst.join("box/locked.txt")).unwrap(), "precious too");
        assert_eq!(parts(&dst.join("box")), 0);
        // A folder copy that fails halfway leaves no half folder.
        std::fs::create_dir_all(src.join("half")).unwrap();
        touch(&src.join("half/fine.txt"), "fine");
        std::fs::rename(src.join("box/locked.txt"), src.join("half/zz-locked.txt")).unwrap();
        let (errs, _) = run(OpKind::Copy { srcs: vec![src.join("half")], dest: dst.clone(), decide: HashMap::new() });
        assert_eq!(errs.len(), 1);
        assert!(!dst.join("half").exists(), "half a copy is no copy");
        std::fs::set_permissions(src.join("half/zz-locked.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    // A move that replaces: the old one to the trash, the moved one in its place.
    touch(&src.join("m.txt"), "moved");
    touch(&dst.join("m.txt"), "was here");
    let (errs, _) = run(OpKind::Move { srcs: vec![src.join("m.txt")], dest: dst.clone(), decide: over(&src.join("m.txt")) });
    assert!(errs.is_empty(), "{errs:?}");
    assert!(!src.join("m.txt").exists() && std::fs::read_to_string(dst.join("m.txt")).unwrap() == "moved");
    assert_eq!(trashed(&dst.join("m.txt")), 1);
    // A link to a folder, deleted: the folder and its files stay.
    touch(&d.join("real/keep.txt"), "keep");
    std::os::unix::fs::symlink(d.join("real"), d.join("link")).unwrap();
    let (errs, _) = run(OpKind::Delete { paths: vec![d.join("link")] });
    assert!(errs.is_empty());
    assert!(std::fs::symlink_metadata(d.join("link")).is_err() && d.join("real/keep.txt").exists());
    // Two names swapped in one go; a rename onto a name taken is refused.
    touch(&d.join("x.txt"), "x");
    touch(&d.join("y.txt"), "y");
    let errs = crate::rename::apply(&[(d.join("x.txt"), "y.txt".into()), (d.join("y.txt"), "x.txt".into())]);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!((std::fs::read_to_string(d.join("x.txt")).unwrap(), std::fs::read_to_string(d.join("y.txt")).unwrap()), ("y".into(), "x".into()));
    let errs = crate::rename::apply(&[(d.join("x.txt"), "y.txt".into())]);
    assert_eq!(errs.len(), 1, "taken");
    assert_eq!((std::fs::read_to_string(d.join("x.txt")).unwrap(), std::fs::read_to_string(d.join("y.txt")).unwrap()), ("y".into(), "x".into()), "both untouched");
    assert!(crate::fsx::rename_new(&d.join("x.txt"), &d.join("y.txt")).is_err());
    // Clean the test's trash entries.
    for it in crate::trashcan::list().into_iter().filter(|i| i.original.starts_with(&d)) {
        let _ = crate::trashcan::purge(&it);
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Converting picks the encoder an ffmpeg build has: one without x264 (as some
/// distributions ship it) still makes an MP4, through OpenH264 or MPEG-4; one with no
/// MP3 encoder says so instead of failing halfway. The listing is ffmpeg's own; the
/// MP4 below is made by the real ffmpeg with what a build without x264 would get.
#[test]
fn convert_uses_the_encoders_ffmpeg_has() {
    use crate::convert::{Target, encoder_names, media_args};
    let listing = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n V....D libx264              libx264 H.264\n V....D mpeg4                MPEG-4 part 2\n A....D aac                  AAC (Advanced Audio Coding)\n A....D libopus              libopus Opus\n";
    assert_eq!(encoder_names(listing), ["libx264", "mpeg4", "aac", "libopus"], "the legend's lines are not encoders");
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let full = media_args(Target::Mp4, false, &s(&["libx264", "aac"])).unwrap();
    assert!(full.windows(2).any(|w| w == ["-c:v", "libx264"]));
    let fedora = media_args(Target::Mp4, false, &s(&["libopenh264", "aac", "libopus"])).unwrap();
    assert!(fedora.windows(2).any(|w| w == ["-c:v", "libopenh264"]));
    let bare = media_args(Target::Mp4, false, &s(&["mpeg4", "aac"])).unwrap();
    assert!(bare.windows(2).any(|w| w == ["-c:v", "mpeg4"]));
    assert!(media_args(Target::Mp3, false, &s(&["aac", "flac"])).is_err(), "no LAME, said before starting");
    let own_opus = media_args(Target::Opus, false, &s(&["opus"])).unwrap();
    assert!(own_opus.windows(2).any(|w| w == ["-strict", "-2"]), "ffmpeg's own Opus is experimental");
    if !crate::media::have("ffmpeg") || !crate::media::have("ffprobe") {
        return;
    }
    let d = tmpdir("encoders");
    let src = d.join("clip.mkv");
    let ok = std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=97x61:rate=10:duration=1", "-f", "lavfi", "-i", "sine=duration=1", "-shortest", "-c:v", "ffv1", "-c:a", "flac"]).arg(&src).status().unwrap().success();
    assert!(ok);
    let out = d.join("clip.mp4");
    let st = std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]).arg(&src).args(&bare).arg(&out).status().unwrap();
    assert!(st.success(), "an MP4 from MPEG-4 alone");
    let probe = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=codec_name,width", "-of", "csv=p=0"]).arg(&out).output().unwrap();
    let said = String::from_utf8_lossy(&probe.stdout);
    assert!(said.contains("mpeg4,96") && said.contains("aac"), "even sides and both streams: {said}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn tour_first_start_and_updates() {
    use crate::tour::{Kind, at_start_kind, version};
    assert_eq!(version("0.3.0"), [0, 3, 0]);
    assert_eq!(version("v0.10.2-rc1"), [0, 10, 2]);
    assert_eq!(version("1"), [1, 0, 0]);
    assert!(version("0.10.0") > version("0.9.9"), "numbers, not text");
    // Nothing on disk: the tour, whatever else.
    assert_eq!(at_start_kind(true, "", "0.3.0"), Some(Kind::Welcome));
    // A 0.2 home: settings, no version recorded.
    assert_eq!(at_start_kind(false, "", "0.3.0"), Some(Kind::News));
    assert_eq!(at_start_kind(false, "0.2.2", "0.3.0"), Some(Kind::News));
    // Seen already, or newer.
    assert_eq!(at_start_kind(false, "0.3.0", "0.3.0"), None);
    assert_eq!(at_start_kind(false, "0.3.1", "0.3.0"), None);
    // A userdata.json from 0.2.2 reads with no version seen; this one keeps it.
    let old: crate::config::UserData = serde_json::from_str(r#"{"bookmarks":["/tmp"],"recents":[],"quick":["new_tab"]}"#).unwrap();
    assert!(old.seen_version.is_empty() && old.bookmarks.len() == 1);
    let mut ud = crate::config::UserData::with_defaults();
    ud.seen_version = "0.3.0".into();
    let back: crate::config::UserData = serde_json::from_str(&serde_json::to_string(&ud).unwrap()).unwrap();
    assert_eq!(back.seen_version, "0.3.0");
    assert_eq!(env!("CARGO_PKG_VERSION"), crate::tour::NEWS_VERSION, "What's New describes this version");
}

/// Every film decodes as made: an animation of a few states of the same window, each
/// held a second or more, ending on a state other than its first; played through, the
/// player hands over every frame PLAYS times, the last held for good, and with
/// animations off only the last. Their bytes stay inside the binary's room for them.
#[test]
fn tour_films_decode_and_play() {
    use crate::tour::{Body, NEWS, PLAYS, WELCOME, play};
    let mut total = 0;
    let mut size = None;
    for p in WELCOME.iter().chain(NEWS) {
        let Body::Film(bytes) = p.body else { continue };
        total += bytes.len();
        let mut d = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        assert!(d.is_animated(), "{} is a film", p.title);
        let n = d.num_frames();
        assert!((2..=4).contains(&n), "{}: {n} states", p.title);
        let dims = d.dimensions();
        assert_eq!(*size.get_or_insert(dims), dims, "{}: every film the same size", p.title);
        let mut buf = vec![0u8; d.output_buffer_size().unwrap()];
        let mut frames = vec![];
        let mut ms = 0;
        for _ in 0..n {
            let t = d.read_frame(&mut buf).unwrap();
            assert!(t >= 900, "{}: a state held {t} ms", p.title);
            ms += t;
            frames.push(buf.clone());
        }
        assert!((3000..=6000).contains(&ms), "{}: {ms} ms in all", p.title);
        assert!(frames[0] != frames[n as usize - 1], "{}: something happens", p.title);
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let w = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let t = std::thread::spawn(move || play(bytes, false, tx, w, ctx));
        let got: Vec<crate::tour::Frame> = rx.iter().collect();
        t.join().unwrap();
        assert_eq!(got.len(), (n * PLAYS) as usize, "{}: every frame, {PLAYS} times", p.title);
        assert_eq!(got.last().unwrap().ms, 0, "the last state stays");
        assert!(got[..got.len() - 1].iter().all(|f| f.ms >= 900));
        assert_eq!(got[0].img.size, [dims.0 as usize, dims.1 as usize]);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let w = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = egui::Context::default();
        std::thread::spawn(move || play(bytes, true, tx, w, ctx));
        let still: Vec<crate::tour::Frame> = rx.iter().collect();
        assert_eq!(still.len(), 1, "{}: animations off, one picture", p.title);
        let last = &got[got.len() - 1].img;
        assert!(still[0].img.pixels == last.pixels, "{}: the last state", p.title);
    }
    assert!(total < 370_000, "the films take {total} bytes");
    // Drawn one pixel for one at 1.6: 540 points wide.
    assert_eq!(size.map(|s| s.0), Some(864));
}

/// The pages name commands as the palette does, show keys that exist, and are written as
/// the README is: no dashes for commas, no words of praise.
#[test]
fn tour_words_match_the_app() {
    use crate::tour::{ALSO, Body, NEWS, WELCOME};
    let keymap = Keymap::build(&Default::default());
    let titles: Vec<&str> = commands::ALL.iter().map(|c| c.info().title.trim_end_matches('…')).collect();
    for p in WELCOME.iter().chain(NEWS) {
        assert!(!p.title.is_empty());
        for &c in p.keys {
            // Shown by its keys, or by its name where it has none.
            assert!(keymap.label(c).is_some() || titles.contains(&c.info().title.trim_end_matches('…')));
        }
        if let Body::Film(_) = p.body {
            assert!(!p.keys.is_empty(), "{}: how to reach it", p.title);
        }
    }
    // Commands the pages name by name are real ones.
    for name in ["Go To", "View Pictures", "Show Space Used", "Batch Rename", "Quick Open", "Recent Projects", "Find Duplicates", "Tidy Folder", "Undo History", "Timeline", "Previous Versions", "What's Using This", "Clean Build Folders In Home", "Choose Theme"] {
        assert!(titles.contains(&name), "{name:?} is a command");
    }
    let mut all: Vec<&str> = WELCOME.iter().chain(NEWS).flat_map(|p| [p.title, p.text]).collect();
    for (g, items) in ALSO {
        all.push(g);
        for (a, b) in *items {
            all.push(a);
            all.push(b);
        }
    }
    for t in all {
        assert!(!t.contains('—') && !t.contains('–'), "{t:?}: a dash");
        for w in ["seamless", "powerful", "robust", "leverage", "utilize", "delve", "elevate", "streamline", "comprehensive", "!"] {
            assert!(!t.to_lowercase().contains(w), "{t:?}: {w:?}");
        }
    }
    // The choices on the first page are Options' own rows.
    assert!(crate::popups::OPTS.iter().any(|o| o.0 == crate::popups::Opt::Tour && o.1 == crate::popups::Section::System), "found again in Startup & system");
}

/// "Show in folder" over D-Bus, against a private bus (never the user's): Maia takes
/// org.freedesktop.FileManager1 (a second connection can't), and calls made with the
/// real gdbus reach the window as requests: items to show selected, folders to open,
/// properties; Introspect describes it; an unknown method is refused. A message
/// written and read back is the same message.
#[test]
fn file_manager1_answers_show_in_folder() {
    use crate::fm1::{Bus, NAME, answer, build, read, strings};
    use crate::single::Request;
    // The wire format alone first: a call with an `as` body, round trip.
    let mut body = vec![];
    // `as`: the array's byte length, then each string's length, bytes and nul.
    body.extend_from_slice(&13u32.to_le_bytes());
    body.extend_from_slice(&8u32.to_le_bytes());
    body.extend_from_slice(b"file:///\0");
    let m = build(1, 9, &[(1, 'o', "/org/freedesktop/FileManager1"), (2, 's', NAME), (3, 's', "ShowItems")], None, "as", &body);
    let back = read(&mut std::io::Cursor::new(m)).unwrap();
    assert_eq!((back.serial, back.path.as_str(), back.member.as_str(), back.signature.as_str()), (9, "/org/freedesktop/FileManager1", "ShowItems", "as"));
    assert_eq!(strings(&back.body), ["file:///"]);
    if !crate::media::have("dbus-daemon") || !crate::media::have("gdbus") {
        return;
    }
    let d = tmpdir("fm1");
    let sock = d.join("bus");
    let conf = d.join("bus.conf");
    std::fs::write(&conf, format!("<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>", sock.display())).unwrap();
    let mut daemon = std::process::Command::new("dbus-daemon").arg(format!("--config-file={}", conf.display())).arg("--nofork").stdout(std::process::Stdio::null()).spawn().unwrap();
    let start = Instant::now();
    while !sock.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let addr = format!("unix:path={}", sock.display());
    let mut bus = Bus::connect(&addr).expect("connected and authenticated");
    assert!(bus.own(NAME), "the name is Maia's");
    assert!(!Bus::connect(&addr).unwrap().own(NAME), "and no one else's while it holds it");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || answer(bus, tx, || {}));
    touch(&d.join("Some folder/a b.txt"), "x");
    let call = |method: &str, uris: &str| {
        std::process::Command::new("gdbus").args(["call", "--address", &addr, "--dest", NAME, "--object-path", "/org/freedesktop/FileManager1", "--method"]).arg(format!("org.freedesktop.FileManager1.{method}")).arg(uris).arg("").output().unwrap()
    };
    let file = d.join("Some folder/a b.txt");
    let uri = format!("['file://{}']", file.display().to_string().replace(' ', "%20"));
    let out = call("ShowItems", &uri);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "()");
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Request::Show(p)) if p == [file.clone()]), "the file, to show selected");
    assert!(call("ShowFolders", &format!("['file://{}']", d.display())).status.success());
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Request::Open(p)) if p == [d.clone()]));
    assert!(call("ShowItemProperties", &uri).status.success());
    assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Request::Properties(p)) if p == [file.clone()]));
    let intro = std::process::Command::new("gdbus").args(["introspect", "--address", &addr, "--dest", NAME, "--object-path", "/org/freedesktop/FileManager1"]).output().unwrap();
    assert!(String::from_utf8_lossy(&intro.stdout).contains("ShowItems"));
    let bad = call("Explode", "['file:///']");
    assert!(!bad.status.success() && String::from_utf8_lossy(&bad.stderr).contains("UnknownMethod"));
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&d);
}

/// A big file is counted and read a window at a time: lines anywhere in it, the last one
/// without a line break, long lines cut, and every match found, across chunk seams too.
#[test]
fn big_files_page_and_search() {
    use crate::bigfile::Pager;
    let d = tmpdir("bigfile");
    let p = d.join("big.log");
    let mut text = String::new();
    for i in 0..300_000 {
        text.push_str(&format!("line {i} {}\n", if i % 100_000 == 7 { "Needle" } else { "hay" }));
    }
    text.push_str(&"x".repeat(9000));
    std::fs::write(&p, &text).unwrap();
    let ctx = egui::Context::default();
    let mut v = Pager::open(&p, text.len() as u64, false, &ctx).unwrap();
    let start = Instant::now();
    while !v.rows().1 {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(v.rows().0, 300_001, "the unterminated last line counts");
    for (a, b) in [(0u64, 5u64), (123_456, 123_460), (299_998, 300_001)] {
        v.want(a, b);
        let start = Instant::now();
        while v.line(a).is_none() || v.line(b - 1).is_none() {
            assert!(start.elapsed() < Duration::from_secs(5), "lines {a}..{b}");
            std::thread::sleep(Duration::from_millis(2));
            v.want(a, b);
        }
        if a == 123_456 {
            assert_eq!(v.line(123_456), Some("line 123456 hay"));
        }
    }
    assert!(v.line(300_000).unwrap().ends_with('…') && v.line(300_000).unwrap().len() < 4100);
    v.search("needle", &ctx);
    let f = v.find.as_ref().unwrap();
    let start = Instant::now();
    while !f.done() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*f.hits.lock().unwrap(), vec![7, 100_007, 200_007], "lines, ignoring case");
    // Hex: offsets of the bytes, given as text or as pairs.
    let bin = d.join("blob.bin");
    let mut bytes = vec![0u8; 3 << 20];
    bytes[(1 << 20) - 2..(1 << 20) + 2].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    std::fs::write(&bin, &bytes).unwrap();
    assert_eq!(crate::bigfile::hex_query("de ad be ef"), vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(crate::bigfile::hex_query("0xDEAD"), vec![0xde, 0xad]);
    assert_eq!(crate::bigfile::hex_query("cafe"), b"cafe".to_vec());
    let mut h = Pager::open(&bin, bytes.len() as u64, true, &ctx).unwrap();
    assert_eq!(h.rows(), (3 << 16, true));
    h.search("de ad be ef", &ctx);
    let f = h.find.as_ref().unwrap();
    while !f.done() {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*f.hits.lock().unwrap(), vec![(1 << 20) - 2], "found across the seam of two chunks");
    let row = ((1 << 20) - 2) / 16;
    h.want(row, row + 1);
    let start = Instant::now();
    while h.row_bytes(row).is_none() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2));
        h.want(row, row + 1);
    }
    assert_eq!(&h.row_bytes(row).unwrap()[14..], &[0xde, 0xad]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Find in a preview: case ignored unless the query has capitals, matches stepped through
/// in a ring, and the place kept when more text arrives.
#[test]
fn find_in_preview_matches() {
    use crate::findbar::{Finder, find_in};
    let mut at = vec![];
    find_in("aXa xa XA", "xa", false, &mut at);
    assert_eq!(at, vec![(1, 3), (4, 6), (7, 9)]);
    at.clear();
    find_in("aaaa", "aa", true, &mut at);
    assert_eq!(at, vec![(0, 2), (2, 4)], "matches don't overlap");
    let mut lines: Vec<String> = ["fn main() {", "    let Main = 1;", "}"].iter().map(|s| s.to_string()).collect();
    let mut f = Finder::new("main".into());
    f.update(&lines, 1);
    assert_eq!(f.hits, vec![(0, 3, 7), (1, 8, 12)]);
    assert!(f.reveal && f.count() == "1 of 2");
    f.step(1);
    f.step(1);
    assert_eq!(f.cur, 0, "past the last, the first");
    f.step(-1);
    assert_eq!(f.count(), "2 of 2");
    f.query = "Main".into();
    f.update(&lines, 1);
    assert_eq!(f.hits, vec![(1, 8, 12)], "a capital matches case");
    lines.push("main again, Main".into());
    f.update(&lines, 1);
    assert_eq!((f.hits.len(), f.cur), (2, 0), "new lines found in, the place kept");
    f.query = "nothing".into();
    f.update(&lines, 1);
    assert_eq!(f.count(), "No matches");
}

/// A unified diff side by side: context on both sides with each side's own numbers,
/// removals beside the additions that replaced them, an addition alone beside a gap,
/// and the changed part of a changed line.
#[test]
fn diffs_side_by_side() {
    use crate::sidediff::{Kind, Row, changed_span, rows};
    let lines: Vec<String> = ["@@ -10,4 +10,5 @@ fn f", " same", "-old a", "-old b", "+new a", " tail", "+added", "\\ No newline at end of file"].iter().map(|s| s.to_string()).collect();
    let r = rows(&lines);
    let row = |old, new, kind| Row { old, new, kind };
    assert_eq!(r, vec![row(Some((0, 0)), None, Kind::Head), row(Some((10, 1)), Some((10, 1)), Kind::Same), row(Some((11, 2)), Some((11, 4)), Kind::Changed), row(Some((12, 3)), None, Kind::Changed), row(Some((13, 5)), Some((12, 5)), Kind::Same), row(None, Some((13, 6)), Kind::Changed),]);
    assert_eq!(changed_span("let x = score(a);", "let x = rank(a);"), ((8, 13), (8, 12)));
    assert_eq!(changed_span("same", "same"), ((4, 4), (4, 4)));
    assert_eq!(changed_span("é1", "é2"), ((2, 3), (2, 3)), "cut at character boundaries");
}

/// Sync all the way down: what the other side lacks copied, newer files over older ones
/// (the older to the trash), mirroring sends what only it has to the trash, folders that
/// differ inside are found, and Undo puts it all back as one step.
#[test]
fn sync_plans_copies_and_undoes() {
    use crate::compare::{differs, plan};
    let ctx = egui::Context::default();
    let d = tmpdir("sync");
    let (a, b) = (d.join("a"), d.join("b"));
    touch(&a.join("same.txt"), "same");
    touch(&b.join("same.txt"), "same");
    let t = std::fs::metadata(a.join("same.txt")).unwrap().modified().unwrap();
    std::fs::File::options().write(true).open(b.join("same.txt")).unwrap().set_modified(t).unwrap();
    touch(&a.join("sub/new.txt"), "new");
    touch(&a.join("sub/deep/edit.txt"), "newer");
    touch(&b.join("sub/deep/edit.txt"), "old");
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(b.join("sub/deep/edit.txt")).unwrap().set_modified(old).unwrap();
    touch(&a.join("fresh/inner.txt"), "x");
    touch(&b.join("sub/only-there.txt"), "extra");
    let stop = std::sync::atomic::AtomicBool::new(false);
    assert!(differs(&a.join("sub"), &b.join("sub"), &stop));
    assert!(!differs(&a.join("sub/deep"), &a.join("sub/deep"), &stop));
    let p = plan(&a, &b, &stop);
    assert_eq!(p.copy, vec![(a.join("fresh"), b.join("fresh")), (a.join("sub/new.txt"), b.join("sub/new.txt"))]);
    assert_eq!(p.update, vec![(a.join("sub/deep/edit.txt"), b.join("sub/deep/edit.txt"))]);
    assert_eq!(p.extra, vec![b.join("sub/only-there.txt")]);
    let copies = p.copy.iter().chain(&p.update).cloned().collect();
    let j = ops::spawn(1, OpKind::Sync { copies, remove: p.extra.clone() }, ctx);
    wait_job(&j);
    let g = j.progress.lock().unwrap();
    assert!(g.errors.is_empty(), "{:?}", g.errors);
    assert_eq!(std::fs::read_to_string(b.join("sub/deep/edit.txt")).unwrap(), "newer");
    assert!(b.join("fresh/inner.txt").exists() && b.join("sub/new.txt").exists() && !b.join("sub/only-there.txt").exists());
    assert!(!differs(&a, &b, &stop), "the same now");
    let kind = crate::app::UndoKind::Synced(g.produced.iter().map(|x| x.1.clone()).collect(), g.replaced.clone(), g.gone.clone());
    drop(g);
    let mut app = crate::app::App::new(&egui::Context::default(), vec![d.clone()]);
    app.undo.push(crate::app::UndoEntry { label: "Sync".into(), kind });
    assert!(app.undo_last());
    assert_eq!(std::fs::read_to_string(b.join("sub/deep/edit.txt")).unwrap(), "old");
    assert!(!b.join("fresh").exists() && !b.join("sub/new.txt").exists() && b.join("sub/only-there.txt").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// A terminal `cols` × `rows` after a program wrote `bytes`.
fn vt(cols: usize, rows: usize, bytes: &[u8]) -> crate::vt::Term {
    let mut t = crate::vt::Term::new(cols, rows);
    t.feed(bytes);
    t
}

fn vt_rows(t: &crate::vt::Term) -> Vec<String> {
    (0..t.screen.rows).map(|y| t.screen.row_text(y)).collect()
}

fn vt_cell(t: &crate::vt::Term, x: usize, y: usize) -> crate::vt::Cell {
    t.screen.line(t.screen.view_line(y)).unwrap().cells[x]
}

#[test]
fn terminal_writes_wraps_and_moves() {
    let t = vt(10, 3, b"hello world, wrap me");
    assert_eq!(vt_rows(&t), ["hello worl", "d, wrap me", ""]);
    assert!(t.screen.line(t.screen.view_line(0)).unwrap().wrapped);
    assert_eq!(t.screen.text((0, 0), (1, 9)), "hello world, wrap me", "a wrapped row copies as one line");
    // The last column holds the cursor until the next character wraps.
    assert_eq!(t.screen.cursor(), (9, 1));
    assert_eq!(vt_rows(&vt(20, 3, b"ab\x08c\r\nx\ty\x07")), ["ac", "x       y", ""]);
    assert_eq!(vt_rows(&vt(10, 3, b"\x1b[2;3Hx\x1b[Hy\x1b[3;9H\x1b[2Dz\x1b[A\x1b[Cw")), ["y", "  x     w", "      z"]);
    // Rows scrolled off the top are kept above.
    let t = vt(10, 2, b"1\r\n2\r\n3\r\n4");
    assert_eq!(vt_rows(&t), ["3", "4"]);
    assert_eq!((t.screen.history(), t.screen.text((0, 0), (1, 9))), (2, "1\n2".to_string()));
}

#[test]
fn terminal_erases_inserts_and_deletes() {
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[3D\x1b[K")), ["abc"]);
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[3D\x1b[1K")), ["    ef"]);
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[2K")), [""]);
    assert_eq!(vt_rows(&vt(10, 3, b"aa\r\nbbbb\r\ncc\x1b[2;3H\x1b[J")), ["aa", "bb", ""]);
    assert_eq!(vt_rows(&vt(10, 3, b"aa\r\nbbbb\r\ncc\x1b[2;3H\x1b[1J")), ["", "   b", "cc"]);
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[1;3H\x1b[2@")), ["ab  cdef"]);
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[1;2H\x1b[2P")), ["adef"]);
    assert_eq!(vt_rows(&vt(10, 1, b"abcdef\x1b[1;2H\x1b[2X")), ["a  def"]);
    let mut t = vt(10, 3, b"1\r\n2\r\n3\x1b[2;1H\x1b[L");
    assert_eq!(vt_rows(&t), ["1", "", "2"]);
    t.feed(b"\x1b[M");
    assert_eq!(vt_rows(&t), ["1", "2", ""]);
    // Erased cells keep the background colour set (xterm's bce).
    let t = vt(10, 1, b"\x1b[44m\x1b[K");
    assert_eq!(vt_cell(&t, 5, 0).bg, 4);
}

#[test]
fn terminal_scroll_region() {
    let mut t = vt(10, 5, b"1\r\n2\r\n3\r\n4\r\n5\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(vt_rows(&t), ["1", "3", "4", "", "5"]);
    assert_eq!(t.screen.history(), 0, "a region's rows don't go to the scrollback");
    t.feed(b"\x1b[2;1H\x1bM");
    assert_eq!(vt_rows(&t), ["1", "", "3", "4", "5"]);
    t.feed(b"\x1b[r\x1b[5;1H\n");
    assert_eq!(vt_rows(&t), ["", "3", "4", "5", ""]);
}

#[test]
fn terminal_alternate_screen() {
    let mut t = vt(10, 2, b"main text");
    t.feed(b"\x1b[?1049h");
    assert!(t.screen.alt());
    assert_eq!(vt_rows(&t), ["", ""]);
    t.feed(b"vim\r\n\n\n\n");
    assert_eq!(t.screen.history(), 0);
    t.feed(b"\x1b[1;1Hx\x1b[?1049l");
    assert!(!t.screen.alt());
    assert_eq!(vt_rows(&t), ["main text", ""]);
    assert_eq!(t.screen.cursor(), (9, 0));
}

#[test]
fn terminal_colors_and_attributes() {
    use crate::vt::{BOLD, DEFAULT, INVERSE, ITALIC, UNDERLINE, rgb};
    let t = vt(20, 1, b"\x1b[1;31mA\x1b[0m\x1b[38;5;123mB\x1b[48;2;255;0;0mC\x1b[38:2::0:255:0mD\x1b[7;3;4mE\x1b[0;95;102mF\x1b[mG");
    let c = |x| vt_cell(&t, x, 0);
    assert_eq!((c(0).ch(), c(0).fg, c(0).flags() & BOLD), ('A', 1, BOLD));
    assert_eq!((c(1).fg, c(1).bg, c(1).flags()), (123, DEFAULT, 0));
    assert_eq!((c(2).fg, c(2).bg), (123, rgb(255, 0, 0)));
    assert_eq!(c(3).fg, rgb(0, 255, 0));
    assert_eq!(c(4).flags(), INVERSE | ITALIC | UNDERLINE);
    assert_eq!((c(5).fg, c(5).bg), (13, 10));
    assert_eq!(c(6), crate::vt::Cell { c: 'G' as u32, ..crate::vt::Cell::BLANK });
}

#[test]
fn terminal_wide_characters() {
    use crate::vt::{SPACER, WIDE};
    let mut t = vt(10, 2, "a漢b".as_bytes());
    assert_eq!(vt_rows(&t)[0], "a漢b");
    assert_eq!((vt_cell(&t, 1, 0).flags() & WIDE, vt_cell(&t, 2, 0).flags() & SPACER), (WIDE, SPACER));
    assert_eq!(t.screen.cursor(), (4, 0));
    // Writing over one half blanks the other.
    t.feed(b"\x1b[1;3Hx");
    assert_eq!(vt_rows(&t)[0], "a xb");
    // One that doesn't fit at the end of a row starts the next.
    let t = vt(5, 2, "abcd漢".as_bytes());
    assert_eq!(vt_rows(&t), ["abcd", "漢"]);
    assert_eq!(t.screen.cursor(), (2, 1));
    // Combining marks don't take a cell.
    assert_eq!(vt_rows(&vt(5, 1, "e\u{301}x".as_bytes())), ["ex"]);
}

#[test]
fn terminal_resizes() {
    let mut t = vt(10, 4, b"1\r\n2\r\n3\r\n4");
    t.screen.resize(10, 2);
    assert_eq!((vt_rows(&t), t.screen.history(), t.screen.cursor()), (vec!["3".to_string(), "4".into()], 2, (1, 1)));
    t.screen.resize(10, 4);
    assert_eq!((vt_rows(&t), t.screen.cursor()), (vec!["1".to_string(), "2".into(), "3".into(), "4".into()], (1, 3)));
    t.screen.resize(3, 4);
    t.feed(b"\r\x1b[Kabcdef");
    assert_eq!(vt_rows(&t)[3], "def");
    // Rows below the cursor go before any above it do.
    let mut t = vt(10, 4, b"x");
    t.screen.resize(10, 2);
    assert_eq!((vt_rows(&t), t.screen.history()), (vec!["x".to_string(), String::new()], 0));
    // The alternate screen and the main one under it both fit the new size.
    let mut t = vt(10, 3, b"a\r\nb\r\nc\x1b[?1049h");
    t.screen.resize(4, 2);
    t.feed(b"\x1b[?1049l");
    assert_eq!(vt_rows(&t), ["b", "c"]);
}

#[test]
fn terminal_answers_and_modes() {
    let mut t = vt(10, 5, b"\x1b[3;5H\x1b[6n\x1b[c");
    assert_eq!(std::mem::take(&mut t.screen.replies), b"\x1b[3;5R\x1b[?62;22c");
    t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?25l\x1b[5 q");
    let s = &t.screen;
    assert!(s.app_cursor && s.bracketed_paste && !s.cursor_shown && s.shape == crate::vt::Shape::Bar);
    t.feed(b"\x1b]0;my title\x07\x1b]7;file://host/tmp/a%20b\x1b\\");
    assert_eq!((t.screen.title.as_str(), t.screen.cwd.as_deref()), ("my title", Some("/tmp/a b")));
    // Line drawing characters, as ncurses draws boxes.
    assert_eq!(vt_rows(&vt(10, 1, b"\x1b(0lqk\x1b(Bq")), ["┌─┐q"]);
}

#[test]
fn terminal_scrollback_is_bounded() {
    let mut t = vt(10, 2, b"");
    for i in 0..6000 {
        t.feed(format!("{i}\r\n").as_bytes());
    }
    let s = &t.screen;
    assert_eq!(s.history(), crate::vt::SCROLLBACK);
    assert_eq!(s.dropped, 6000 - crate::vt::SCROLLBACK as u64 - 1);
    let first = s.line(s.dropped).unwrap();
    assert_eq!((s.text((s.dropped, 0), (s.dropped, 9)), first.cells.len()), ("999".to_string(), 3), "kept rows hold only what shows");
    let mut t = vt(10, 2, b"1\r\n2\r\n3\x1b[3J");
    assert_eq!(t.screen.history(), 0);
    t.screen.scroll_view(5);
    assert_eq!(t.screen.scroll, 0);
}

#[test]
fn terminal_pty_round_trip() {
    use crate::terminal::Pty;
    use std::process::Command;
    let wait = |p: &Pty, ok: &dyn Fn(&crate::vt::Screen) -> bool| {
        let t0 = Instant::now();
        while !ok(&p.shared.term().screen) {
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out: {:?}", vt_rows(&p.shared.term()));
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let ctx = egui::Context::default();
    let mut sh = Command::new("/bin/sh");
    sh.args(["-c", "printf hi"]);
    let p = Pty::spawn(sh, 40, 5, ctx.clone()).unwrap();
    wait(&p, &|s| s.row_text(0) == "hi");
    let t0 = Instant::now();
    while !p.shared.ended() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the reader sees the end");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The terminal echoes what is typed, then cat writes it back.
    let mut p = Pty::spawn(Command::new("cat"), 40, 5, ctx.clone()).unwrap();
    p.write(b"abc\r");
    wait(&p, &|s| s.row_text(0) == "abc" && s.row_text(1) == "abc");
    let pid = p.pid().unwrap();
    drop(p);
    let t0 = Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(t0.elapsed() < Duration::from_secs(5), "hung up and reaped");
        std::thread::sleep(Duration::from_millis(10));
    }
    // A new size reaches the program.
    let mut sh = Command::new("/bin/sh");
    sh.args(["-c", "sleep 0.3; stty size"]);
    let p = Pty::spawn(sh, 40, 5, ctx).unwrap();
    p.resize(33, 7);
    wait(&p, &|s| s.row_text(0) == "7 33");
}

/// Conflicts settled as "if newer" (replaced only by a newer file, else skipped) and by
/// a name typed for the one coming.
#[test]
fn conflicts_if_newer_and_renamed() {
    let ctx = egui::Context::default();
    let d = tmpdir("conflict-more");
    let (src, dst) = (d.join("src"), d.join("dst"));
    for (name, body) in [("new.txt", "newer"), ("old.txt", "older"), ("named.txt", "mine")] {
        touch(&src.join(name), body);
        touch(&dst.join(name), "there");
    }
    let past = std::time::SystemTime::now() - Duration::from_secs(3600);
    for p in [dst.join("new.txt"), src.join("old.txt")] {
        std::fs::File::options().write(true).open(p).unwrap().set_modified(past).unwrap();
    }
    let decide = HashMap::from([(src.join("new.txt"), Conflict::Newer), (src.join("old.txt"), Conflict::Newer), (src.join("named.txt"), Conflict::Rename("kept.txt".into()))]);
    let j = ops::spawn(1, OpKind::Copy { srcs: ["new.txt", "old.txt", "named.txt"].iter().map(|n| src.join(n)).collect(), dest: dst.clone(), decide }, ctx);
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.is_empty());
    assert_eq!(std::fs::read_to_string(dst.join("new.txt")).unwrap(), "newer", "newer: replaced");
    assert_eq!(std::fs::read_to_string(dst.join("old.txt")).unwrap(), "there", "older: skipped");
    assert_eq!(std::fs::read_to_string(dst.join("kept.txt")).unwrap(), "mine", "renamed");
    assert_eq!(std::fs::read_to_string(dst.join("named.txt")).unwrap(), "there");
    let _ = std::fs::remove_dir_all(&d);
}

/// An animated GIF over `pal`: frames of (left, top, width, height, palette indices,
/// delay in hundredths, disposal), index 0 transparent.
fn gif_bytes(w: u16, h: u16, pal: &[u8], repeat: gif::Repeat, frames: &[(u16, u16, u16, u16, Vec<u8>, u16, gif::DisposalMethod)]) -> Vec<u8> {
    let mut out = vec![];
    {
        let mut e = gif::Encoder::new(&mut out, w, h, pal).unwrap();
        e.set_repeat(repeat).unwrap();
        for (x, y, fw, fh, px, delay, dispose) in frames {
            let mut f = gif::Frame::from_indexed_pixels(*fw, *fh, px.clone(), Some(0));
            (f.left, f.top, f.delay, f.dispose) = (*x, *y, *delay, *dispose);
            e.write_frame(&f).unwrap();
        }
    }
    out
}

/// `n` whole-picture frames, frame k all of colour k + 1 of a palette where colour i
/// is (i, 255 - i, 0), each `delay` hundredths.
pub(crate) fn gif_run(w: u16, h: u16, n: u8, delay: u16, repeat: gif::Repeat) -> Vec<u8> {
    let pal: Vec<u8> = (0..=255u8).flat_map(|i| [i, 255 - i, 0]).collect();
    let frames: Vec<_> = (0..n).map(|k| (0, 0, w, h, vec![k + 1; w as usize * h as usize], delay, gif::DisposalMethod::Keep)).collect();
    gif_bytes(w, h, &pal, repeat, &frames)
}

/// The colour frame k of `gif_run` shows.
fn run_colour(k: u8) -> egui::Color32 {
    egui::Color32::from_rgb(k + 1, 254 - k, 0)
}

/// Animated GIF frames composited as browsers do: each drawn over the last where it
/// says, its transparent pixels showing what lies under them, an area cleared or put
/// back as the frame before asks; delays as written (the tiny ones shown for a tenth of
/// a second) and the loop count as times played. One frame is one frame; a still PNG
/// is no animation.
#[test]
fn animated_gif_frames() {
    use crate::anim::{Frames, delay_ms};
    use gif::DisposalMethod::{Background, Keep, Previous};
    let d = tmpdir("anim-gif");
    let path = d.join("a.gif");
    let pal = [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
    let frames = [(0, 0, 8, 6, vec![1; 48], 5, Keep), (2, 2, 4, 3, vec![2; 12], 0, Background), (0, 0, 2, 2, vec![3, 3, 3, 0], 1, Previous), (7, 5, 1, 1, vec![2], 30, Keep)];
    std::fs::write(&path, gif_bytes(8, 6, &pal, gif::Repeat::Finite(2), &frames)).unwrap();
    let mut f = Frames::open(&path).unwrap();
    let px = |f: &Frames, x: usize, y: usize| <[u8; 4]>::try_from(&f.canvas.px[(y * 8 + x) * 4..][..4]).unwrap();
    let (red, green, blue, clear) = ([255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [0; 4]);
    let mut delays = vec![f.next().unwrap()];
    assert_eq!(px(&f, 0, 0), red);
    delays.push(f.next().unwrap());
    assert_eq!((px(&f, 3, 3), px(&f, 0, 0)), (green, red));
    delays.push(f.next().unwrap());
    assert_eq!(px(&f, 3, 3), clear, "the frame before cleared its area");
    assert_eq!((px(&f, 0, 0), px(&f, 1, 1)), (blue, red), "a transparent pixel shows what is under it");
    delays.push(f.next().unwrap());
    assert_eq!((px(&f, 0, 0), px(&f, 7, 5)), (red, green), "the frame before put back what it covered");
    assert!(f.next().is_none());
    assert_eq!(delays, [50, 0, 10, 300]);
    assert_eq!(delays.iter().map(|&d| delay_ms(d)).collect::<Vec<_>>(), [50, 100, 100, 300]);
    assert_eq!(f.plays, Some(3), "twice more after the first time");
    std::fs::write(&path, gif_run(4, 4, 1, 10, gif::Repeat::Infinite)).unwrap();
    let mut f = Frames::open(&path).unwrap();
    assert!(f.next().is_some() && f.next().is_none() && f.plays.is_none(), "one frame, looping forever");
    std::fs::write(d.join("still.png"), png_bytes(4, 4, 1)).unwrap();
    assert!(Frames::open(&d.join("still.png")).is_none());
    assert!(Frames::open(&d.join("missing.gif")).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// APNG frames: a frame blended over what is there or replacing it, at its offset; an
/// area cleared after its frame; delays from their fractions; plays as written. A default
/// image outside the animation is not one of its frames.
#[test]
fn apng_frames() {
    use crate::anim::Frames;
    use png::{BlendOp, DisposeOp};
    let apng = |separate: bool| {
        let mut out = vec![];
        let mut e = png::Encoder::new(&mut out, 4, 4);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_animated(3, 2).unwrap();
        e.set_sep_def_img(separate).unwrap();
        let mut w = e.write_header().unwrap();
        if separate {
            w.write_image_data(&[255; 64]).unwrap();
        }
        let red: Vec<u8> = [255, 0, 0, 255].repeat(16);
        let half_blue: Vec<u8> = [0, 0, 255, 128].repeat(4);
        let green: Vec<u8> = [0, 255, 0, 255].repeat(4);
        for (x, y, fw, fh, px, (n, den), blend, dispose) in [(0, 0, 4, 4, red, (1, 10), BlendOp::Source, DisposeOp::None), (1, 1, 2, 2, half_blue, (3, 0), BlendOp::Over, DisposeOp::Background), (2, 2, 2, 2, green, (25, 1000), BlendOp::Source, DisposeOp::None)] {
            w.reset_frame_position().unwrap();
            w.set_frame_dimension(fw, fh).unwrap();
            w.set_frame_position(x, y).unwrap();
            w.set_frame_delay(n, den).unwrap();
            w.set_blend_op(blend).unwrap();
            w.set_dispose_op(dispose).unwrap();
            w.write_image_data(&px).unwrap();
        }
        w.finish().unwrap();
        out
    };
    let d = tmpdir("apng");
    for separate in [false, true] {
        let path = d.join("a.png");
        std::fs::write(&path, apng(separate)).unwrap();
        let mut f = Frames::open(&path).unwrap_or_else(|| panic!("animated (default image apart: {separate})"));
        let px = |f: &Frames, x: usize, y: usize| <[u8; 4]>::try_from(&f.canvas.px[(y * 4 + x) * 4..][..4]).unwrap();
        assert_eq!(f.next(), Some(100));
        assert_eq!(px(&f, 1, 1), [255, 0, 0, 255], "the first frame, not the default image");
        assert_eq!(f.next(), Some(30), "a zero denominator means hundredths");
        let [r, g, b, a] = px(&f, 1, 1);
        assert!(a == 255 && (126..=128).contains(&r) && g == 0 && (127..=129).contains(&b), "half blue over red: {:?}", [r, g, b, a]);
        assert_eq!(px(&f, 0, 0), [255, 0, 0, 255], "outside the frame untouched");
        assert_eq!(f.next(), Some(25));
        assert_eq!(px(&f, 1, 1), [0; 4], "cleared after its frame");
        assert_eq!(px(&f, 3, 3), [0, 255, 0, 255]);
        assert!(f.next().is_none());
        assert_eq!(f.plays, Some(2));
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The real ImageMagick (`magick`, else the older `convert`) on the PATH, as a stand-in
/// folder holding only it, and its name.
pub(crate) fn any_imagemagick_tools(dir: &Path) -> Option<(PathBuf, &'static str)> {
    let path = std::env::var_os("PATH")?;
    let (real, name) = ["magick", "convert"].into_iter().find_map(|n| std::env::split_paths(&path).map(|d| d.join(n)).find(|m| m.is_file()).map(|m| (m, n)))?;
    std::fs::create_dir_all(dir).unwrap();
    let _ = std::os::unix::fs::symlink(real, dir.join(name));
    Some((dir.to_path_buf(), name))
}

/// An animated WebP made by ImageMagick, when it is here: three lossless frames, the
/// decoder's composited canvas each time, their delays, looping forever.
#[test]
fn animated_webp_frames() {
    let d = tmpdir("anim-webp");
    let Some((tools, im)) = any_imagemagick_tools(&d.join("tools")) else {
        eprintln!("animated WebP: skipped (no ImageMagick)");
        return;
    };
    let path = d.join("a.webp");
    let made = std::process::Command::new(tools.join(im)).args(["-size", "40x30", "xc:red", "xc:lime", "xc:blue", "-set", "delay", "5", "-loop", "0", "-define", "webp:lossless=true"]).arg(&path).status().is_ok_and(|s| s.success());
    let Some(mut f) = made.then(|| crate::anim::Frames::open(&path)).flatten() else {
        eprintln!("animated WebP: skipped (ImageMagick wrote no animated WebP)");
        return;
    };
    let mut seen = vec![];
    while let Some(ms) = f.next() {
        seen.push((ms, <[u8; 4]>::try_from(&f.canvas.px[..4]).unwrap()));
    }
    // Lossless, but ImageMagick may round a level by one.
    let near = |a: [u8; 4], b: [u8; 4]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 1);
    assert!(seen.len() == 3 && seen.iter().zip([[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]).all(|(s, c)| s.0 == 50 && near(s.1, c)), "{seen:?}");
    assert_eq!(f.plays, None);
    let _ = std::fs::remove_dir_all(&d);
}

/// Drive `p` through `secs` of simulated time a frame (1/60 s) at a time, its worker
/// given a moment each frame; the colours shown go to `seen`, each once as it comes,
/// and the most frames it held to `held`.
fn play_for(p: &mut crate::anim::Player, ctx: &egui::Context, t: &mut f64, secs: f64, held: &mut usize, seen: &mut Vec<egui::Color32>) {
    let end = *t + secs;
    while *t < end {
        *t += 1.0 / 60.0;
        let _ = ctx.run(egui::RawInput { time: Some(*t), ..Default::default() }, |_| {
            p.frame(true);
        });
        let (_, n, _, _, c) = p.state();
        *held = (*held).max(n);
        if let Some(c) = c.filter(|c| seen.last() != Some(c)) {
            seen.push(c);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Playing: frames on time, kept and looped from memory when they fit, as many times as
/// the GIF says, then still on the last; a pause holds the frame. One too long to keep
/// is decoded again each time round, a couple of frames held at once. Big frames are
/// scaled to the view; a still GIF never shows as moving.
#[test]
fn animations_play_within_their_memory() {
    use crate::anim::Player;
    let d = tmpdir("anim-play");
    let ctx = egui::Context::default();
    let mut t = 0.0;
    let path = d.join("loop.gif");
    std::fs::write(&path, gif_run(32, 32, 10, 5, gif::Repeat::Finite(1))).unwrap();
    let mut p = Player::start(&path, None, 200.0, 256 << 20, &ctx);
    let (mut held, mut seen) = (0, vec![]);
    play_for(&mut p, &ctx, &mut t, 2.0, &mut held, &mut seen);
    let want: Vec<_> = (0..10).chain(0..10).map(run_colour).collect();
    assert_eq!(seen, want, "twice through, in order");
    assert!(!p.playing && p.moving(), "then still on its last frame");
    assert_eq!((held, p.state().3), (10, false), "kept whole");
    p.toggle();
    assert!(p.playing, "played again from the top");
    play_for(&mut p, &ctx, &mut t, 0.12, &mut held, &mut seen);
    p.toggle();
    let at = p.state().0;
    assert!(at > 0, "moved on");
    play_for(&mut p, &ctx, &mut t, 0.5, &mut held, &mut seen);
    assert_eq!(p.state().0, at, "paused");
    // Too long to keep within 100 kB: forty 16 kB frames.
    std::fs::write(&path, gif_run(64, 64, 40, 5, gif::Repeat::Infinite)).unwrap();
    let mut p = Player::start(&path, None, 64.0, 400 << 10, &ctx);
    let (mut warm, mut held, mut seen) = (0, 0, vec![]);
    play_for(&mut p, &ctx, &mut t, 1.5, &mut warm, &mut seen);
    play_for(&mut p, &ctx, &mut t, 3.0, &mut held, &mut seen);
    assert!(p.state().3, "decoded again each time round");
    assert!(held <= 3, "the frame shown and two ahead, not {held}");
    let want: Vec<_> = (0..120u32).map(|k| run_colour((k % 40) as u8)).collect();
    assert!(seen.len() >= 80 && seen == want[..seen.len()], "round and round, in order: {} frames", seen.len());
    // Scaled to the view.
    let img = crate::anim::shown(&crate::imgdec::Rgba { w: 3000, h: 1000, px: vec![9; 12_000_000] }, 512);
    assert_eq!(img.size, [512, 171]);
    assert!(!p.is_for(&d.join("other.gif"), None, 64.0) && p.is_for(&path, None, 64.0));
    // A still GIF.
    std::fs::write(&path, gif_run(8, 8, 1, 5, gif::Repeat::Infinite)).unwrap();
    let mut p = Player::start(&path, None, 64.0, 256 << 20, &ctx);
    play_for(&mut p, &ctx, &mut t, 0.3, &mut held, &mut seen);
    assert!(!p.moving() && p.state().1 == 0, "nothing kept for a still");
    let _ = std::fs::remove_dir_all(&d);
}

/// HEIC, AVIF, TIFF and the like through the real ImageMagick: a TIFF stored turned
/// (orientation 6) comes upright, at its upright size, scaled to what was asked; an AVIF
/// (made by ffmpeg, when it is here) the same. Nothing here writes HEIC, so it is
/// skipped. PNM read with comments and alpha; what to install when nothing reads a
/// format.
#[test]
fn photos_through_external_tools() {
    use crate::extpic::{can, install_note, load, pnm};
    let (img, rest) = pnm(b"P6\n# by hand\n2 1\n255\n\xff\x00\x00\x00\xff\x00 400 300\n").unwrap();
    assert_eq!((img.w, img.h, &img.px[..]), (2, 1, &[255, 0, 0, 255, 0, 255, 0, 255][..]));
    assert_eq!(rest, b" 400 300\n");
    let (img, _) = pnm(b"P7\nWIDTH 1\nHEIGHT 1\nDEPTH 2\nMAXVAL 255\nTUPLTYPE GRAYSCALE_ALPHA\nENDHDR\n\x80\x40").unwrap();
    assert_eq!(img.px, [128, 128, 128, 64]);
    assert!(pnm(b"P6\n2 1\n65535\n\x00\x00").is_none(), "16 bits");
    assert!(pnm(b"P6\n2 1\n255\n\x00").is_none(), "cut short");
    assert_eq!(install_note("heic"), Some("Install ImageMagick to see HEIC photos"));
    assert_eq!(install_note("png"), None);
    let d = tmpdir("extpic");
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    std::fs::create_dir_all(d.join("none")).unwrap();
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.join("none")));
    assert!(!can(&crate::drives::Tools::current().unwrap(), "heic"), "no tools, no HEIC");
    let Some((tools, im)) = any_imagemagick_tools(&d.join("tools")) else {
        eprintln!("photos: skipped (no ImageMagick)");
        return;
    };
    crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(tools.clone()));
    let t = crate::drives::Tools::current().unwrap();
    assert!(["heic", "avif", "tiff", "tif", "nef", "dng"].iter().all(|e| can(&t, e)) && !can(&t, "png"));
    // Red above blue, stored turned: upright, blue is on the left and red on the right.
    let tiff = d.join("turned.tiff");
    let made = std::process::Command::new(tools.join(im)).args(["-size", "300x100", "xc:red", "-size", "300x100", "xc:blue", "-append", "-orient", "RightTop"]).arg(&tiff).status().unwrap();
    assert!(made.success());
    let (img, dims) = load(&t, &tiff, 64).expect("a TIFF");
    assert_eq!((dims, img.w, img.h), ([200, 300], 43, 64));
    let at = |x: u32, y: u32| &img.px[((y * img.w + x) * 4) as usize..][..3];
    assert!(at(2, 32)[2] > 200 && at(40, 32)[0] > 200, "upright: {:?} {:?}", at(2, 32), at(40, 32));
    let avif = d.join("red.avif");
    let ffmpeg = std::process::Command::new("ffmpeg").args(["-loglevel", "error", "-f", "lavfi", "-i", "color=c=red:s=320x240", "-frames:v", "1"]).arg(&avif).status().is_ok_and(|s| s.success());
    if ffmpeg {
        let (img, dims) = load(&t, &avif, 64).expect("an AVIF");
        assert_eq!((dims, img.w, img.h), ([320, 240], 64, 48));
        assert!(img.px[0] > 200 && img.px[1] < 60, "red: {:?}", &img.px[..4]);
    } else {
        eprintln!("photos: AVIF skipped (ffmpeg can't write one)");
    }
    eprintln!("photos: HEIC skipped (nothing here writes one)");
    assert!(load(&t, &d.join("missing.heic"), 64).is_none());
    std::fs::write(d.join("junk.tiff"), "not a picture").unwrap();
    assert!(load(&t, &d.join("junk.tiff"), 64).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// Permissions changed for several items and all the way down: bits set and cleared,
/// files inside given Execute only where they had it, the owner changed, and the same
/// written for chmod and chown when it has to run as administrator.
#[test]
fn permissions_set_cleared_and_deep() {
    use crate::perms::Change;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let c = Change { set: 0o111 | 0o4000, clear: 0o022, owner: None, group: None };
    assert_eq!(c.mode(0o664, false), 0o4755);
    assert_eq!(c.mode(0o664, true), 0o4644, "a document inside stays a document");
    assert_eq!(c.mode(0o700, true), 0o4711, "a program inside stays one");
    assert_eq!(c.symbolic(false).as_deref(), Some("u+xs,g+x,g-w,o+x,o-w"));
    assert_eq!(c.symbolic(true).as_deref(), Some("u+Xs,g+X,g-w,o+X,o-w"));
    let d = tmpdir("perms");
    touch(&d.join("top/doc.txt"), "x");
    touch(&d.join("top/sub/run.sh"), "x");
    let set = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    set(&d.join("top/doc.txt"), 0o666);
    set(&d.join("top/sub/run.sh"), 0o744);
    let gid = std::fs::metadata(&d).unwrap().gid();
    let c = Change { set: 0o011, clear: 0o002, owner: None, group: Some(gid) };
    let j = ops::spawn(1, OpKind::Perms { paths: vec![d.join("top")], change: c, deep: true }, egui::Context::default());
    wait_job(&j);
    assert!(j.progress.lock().unwrap().errors.is_empty(), "{:?}", j.progress.lock().unwrap().errors);
    let mode = |p: &str| std::fs::metadata(d.join(p)).unwrap().mode() & 0o7777;
    assert_eq!((mode("top"), mode("top/sub"), mode("top/doc.txt"), mode("top/sub/run.sh")), (0o755, 0o755, 0o664, 0o755));
    let t = Change { set: 0o100, clear: 0, owner: Some(0), group: None }.admin(vec![d.join("top")], true).unwrap();
    let argv: Vec<String> = t.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert!(argv[0].ends_with("chown") && argv[1..4] == ["-R", "--", "root"], "{argv:?}");
    assert!(matches!(t, crate::admin::Task::Perms { mode: Some(ref m), .. } if m == "u+X"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn theme_follows_the_desktop_and_comes_back() {
    use crate::theme::ThemeName;
    let mut app = crate::app::App::new(&egui::Context::default(), vec![]);
    app.cfg.theme = ThemeName::Nord;
    app.follow_system(true);
    assert_eq!(app.cfg.theme, ThemeName::Nord, "already dark");
    app.follow_system(false);
    assert_eq!(app.cfg.theme, ThemeName::Paper);
    app.cfg.theme = ThemeName::Latte;
    app.follow_system(true);
    assert_eq!(app.cfg.theme, ThemeName::Nord, "back to the dark theme left");
    app.follow_system(false);
    assert_eq!(app.cfg.theme, ThemeName::Latte, "and to the light one");
}

#[test]
fn a_dropped_watch_lets_go_of_its_descriptor() {
    let fds = || std::fs::read_dir("/proc/self/fd").unwrap().filter_map(|e| std::fs::read_link(e.ok()?.path()).ok()).filter(|l| l.to_string_lossy().contains("inotify")).count();
    let before = fds();
    let d = std::env::temp_dir().join(format!("maia-watch-drop-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    for watched in [false, true] {
        let mut w = crate::watch::Watch::new(egui::Context::default());
        if watched {
            w.sync(&[d.clone()].into_iter().collect());
        }
        drop(w);
    }
    let t0 = std::time::Instant::now();
    while fds() > before && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(fds(), before, "both closed, with or without a folder watched");
    let _ = std::fs::remove_dir_all(&d);
}
