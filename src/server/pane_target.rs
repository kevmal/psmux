//! Complete, read-only pane lookup. Return identity, never a focus fallback.
use crate::types::{AppState, FocusDir};

fn pane_ids(app: &AppState, wi: usize) -> Vec<usize> {
    let win = &app.windows[wi];
    let mut ids: Vec<_> = crate::tree::pane_paths(&win.root).iter()
        .filter_map(|path| crate::tree::get_active_pane_id_at_path(&win.root, path)).collect();
    ids.extend(win.floating.iter().map(|pane| pane.id));
    ids
}

fn current_pane(app: &AppState, wi: usize) -> Option<usize> {
    let win = app.windows.get(wi)?;
    win.floating_focus.and_then(|index| win.floating.get(index)).map(|pane| pane.id)
        .or_else(|| crate::tree::get_active_pane_id_at_path(&win.root, &win.active_path))
}

fn owner(app: &AppState, id: usize) -> Option<usize> {
    crate::tree::find_pane_by_id_global(app, id).or_else(|| crate::tree::find_floating_by_id(app, id)).map(|(wi, _)| wi)
}

fn select_in_window(app: &AppState, wi: usize, raw: &str) -> Result<usize, String> {
    let missing = || format!("can't find pane: {}", raw);
    let win = &app.windows[wi];
    let current = current_pane(app, wi).ok_or_else(missing)?;
    let ids = pane_ids(app, wi);
    let token = match raw {
        "{next}" => "+", "{previous}" => "-", "{last}" => "!", other => other,
    };
    if token.is_empty() { return Ok(current); }
    if let Some(id) = token.strip_prefix('%') {
        let id = id.parse::<usize>().map_err(|_| missing())?;
        return if owner(app, id) == Some(wi) { Ok(id) } else { Err(missing()) };
    }
    if let Some(n) = token.strip_prefix(['+', '-']) {
        let n = if n.is_empty() { 1 } else { n.parse::<usize>().map_err(|_| missing())? };
        if n == 0 || n > i32::MAX as usize { return Err(missing()); }
        let at = ids.iter().position(|id| *id == current).ok_or_else(missing)?;
        let step = n % ids.len();
        return Ok(ids[if token.starts_with('+') { (at + step) % ids.len() }
            else { (at + ids.len() - step) % ids.len() }]);
    }
    if token == "!" {
        if let Some(id) = win.pane_mru.iter().find(|id| **id != current && ids.contains(id)) { return Ok(*id); }
        if wi == app.active_idx && app.last_pane_path != win.active_path {
            if let Some(id) = crate::tree::get_active_pane_id_at_path(&win.root, &app.last_pane_path).filter(|id| *id != current) {
                return Ok(id);
            }
        }
        if ids.len() == 2 { return ids.into_iter().find(|id| *id != current).ok_or_else(missing); }
        return Err("no last pane".into());
    }
    if let Ok(index) = token.parse::<usize>() {
        return index.checked_sub(app.pane_base_index_of(wi)).and_then(|index| ids.get(index).copied()).ok_or_else(missing);
    }
    let area = if win.area.width > 0 && win.area.height > 0 { win.area } else { app.last_window_area };
    let mut rects = Vec::new();
    crate::tree::compute_rects(&win.root, area, &mut rects);
    let direction = match token {
        "{up-of}" => Some(FocusDir::Up), "{down-of}" => Some(FocusDir::Down),
        "{left-of}" => Some(FocusDir::Left), "{right-of}" => Some(FocusDir::Right), _ => None,
    };
    let path = if let Some(direction) = direction {
        let at = rects.iter().position(|(path, _)| *path == win.active_path).ok_or_else(missing)?;
        let pane_ids: Vec<_> = rects.iter().map(|(path, _)| crate::tree::get_active_pane_id_at_path(&win.root, path).unwrap_or(usize::MAX)).collect();
        crate::input::find_best_pane_in_direction(&rects, at, &rects[at].1, direction, &pane_ids, &win.pane_mru)
            .or_else(|| crate::input::find_wrap_target(&rects, at, &rects[at].1, direction, &pane_ids, &win.pane_mru))
            .map(|index| rects[index].0.clone())
    } else {
        let description = token.trim_matches(['{', '}']).to_ascii_lowercase();
        crate::window_ops::resolve_position_token(&format!("{{{}}}", description), area, &rects)
    };
    path.and_then(|path| crate::tree::get_active_pane_id_at_path(&win.root, &path)).ok_or_else(missing)
}

/// Resolve tmux's session:window.pane grammar on one immutable state snapshot.
pub(crate) fn resolve_pane(app: &AppState, raw: &str) -> Result<usize, String> {
    if app.windows.is_empty() { return Err("no current pane".into()); }
    let raw = raw.trim();
    if matches!(raw, "=" | "{mouse}") { return Err("no mouse target for this command".into()); }
    let raw = crate::cli::strip_exact_match_prefix(raw);
    if matches!(raw, "~" | "{marked}") {
        return app.marked_pane.map(|(_, id)| id).filter(|id| owner(app, *id).is_some()).ok_or_else(|| "no marked pane".into());
    }
    if matches!(raw, "{active}" | "{current}") { return current_pane(app, app.active_idx).ok_or_else(|| "no current pane".into()); }
    let (session, body) = raw.split_once(':').map(|(session, body)| (Some(session), body)).unwrap_or((None, raw));
    if let Some(session) = session.filter(|session| !session.is_empty()) {
        if !super::temp_target::session_names_this_server(app, session) { return Err(format!("can't find session: {}", session)); }
    }
    if let Some(id) = body.strip_prefix('%') {
        let id = id.parse::<usize>().map_err(|_| format!("can't find pane: {}", body))?;
        return owner(app, id).map(|_| id).ok_or_else(|| format!("can't find pane: {}", body));
    }
    if let Some((window, pane)) = body.split_once('.') {
        // A missing window is not an ownership assertion: .%id names that
        // pane's own window, including when another window is active.
        if window.is_empty() && pane.starts_with('%') {
            let id = pane[1..].parse::<usize>().map_err(|_| format!("can't find pane: {}", pane))?;
            return owner(app, id).map(|_| id).ok_or_else(|| format!("can't find pane: {}", pane));
        }
        let wi = if window.is_empty() { app.active_idx }
            else { app.resolve_window_spec(window, false)?.pos().ok_or_else(|| format!("can't find window: {}", window))? };
        return select_in_window(app, wi, pane);
    }
    if body.is_empty() { return current_pane(app, app.active_idx).ok_or_else(|| "no current pane".into()); }
    // With ':' the remaining component is a window, without it a bare token
    // is first a pane selector, then a window/session name.
    if session.is_none() && !body.starts_with('@') {
        if let Ok(id) = select_in_window(app, app.active_idx, body) { return Ok(id); }
    }
    match app.resolve_window_spec(body, false) {
        Ok(target) => {
            let wi = target.pos().ok_or_else(|| format!("can't find window: {}", body))?;
            current_pane(app, wi).ok_or_else(|| format!("can't find pane: {}", raw))
        }
        Err(_) if session.is_none() && super::temp_target::session_names_this_server(app, body) =>
            current_pane(app, app.active_idx).ok_or_else(|| "no current pane".into()),
        Err(_) if session.is_none() => Err(format!("can't find pane: {}", body)),
        Err(error) => Err(error),
    }
}

pub(crate) fn resolve_all(app: &AppState, specs: &[String]) -> Result<Vec<String>, String> {
    specs.iter().map(|spec| resolve_pane(app, spec).map(|id| format!("%{}", id))).collect()
}
