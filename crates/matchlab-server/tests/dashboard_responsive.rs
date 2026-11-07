//! Lightweight source-level UI regression checks. CSS layout is rendered by
//! browsers; these assertions preserve the mobile contracts and accessibility
//! affordances without adding browser tooling to the production Rust image.
const DASHBOARD:&str=include_str!("../src/dashboard.html");

#[test]
fn narrow_viewports_keep_navigation_and_controls_on_single_lines(){
    assert!(DASHBOARD.contains(r#"content="width=device-width,initial-scale=1""#));
    assert!(DASHBOARD.contains("@media(max-width:600px)"));
    assert!(DASHBOARD.contains(".top .nav{grid-row:2;grid-column:1/-1"));
    assert!(DASHBOARD.contains("overflow-x:auto"));
    assert!(DASHBOARD.contains(".field.options{display:grid"));
    assert!(DASHBOARD.contains(".field.options .btn{width:100%;grid-column:1/-1}"));
    assert!(DASHBOARD.contains(".paging{display:grid"));
    assert!(DASHBOARD.contains(".rank-top{flex-wrap:nowrap}"));
    assert!(DASHBOARD.contains(".stat-label-short"));
}

#[test]
fn history_scroll_keeps_unwrapped_titles_retrievable(){
    assert!(DASHBOARD.contains(r#"role="region" aria-label="Recent matching requests"#));
    assert!(DASHBOARD.contains(r#"tabindex="0"><table>"#));
    assert!(DASHBOARD.contains(".table-wrap td.title{max-width:260px;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}"));
    assert!(DASHBOARD.contains(r#"class="title" title="'+esc(r.title)+'""#));
    assert!(DASHBOARD.contains(r#"class="history-title copy-title copy-title-detail""#));
    assert!(DASHBOARD.contains(r#"e.key==='Enter'||e.key===' '"#));
}

#[test]
fn responsive_labels_preserve_information_and_health_status(){
    assert!(DASHBOARD.contains("Matching statistics · last 14 days (UTC)"));
    assert!(DASHBOARD.contains("Last 14 days · UTC"));
    assert!(DASHBOARD.contains("Latest 1,000 requests"));
    assert!(DASHBOARD.contains("Tap t_title to copy; tap other cells for details."));
    assert!(DASHBOARD.contains("node.querySelector('.health-short').textContent=short"));
    assert!(DASHBOARD.contains("node.setAttribute('aria-label',full)"));
}

#[test]
fn history_t_title_is_a_separate_accessible_copy_action(){
    // Clicking other cells still opens diagnostics. The t_title is an
    // independent keyboard-accessible copy button, also in detail view.
    assert!(DASHBOARD.contains(r#"<button type="button" class="copy-title" aria-label="Copy t_title:"#));
    assert!(DASHBOARD.contains("bindCopyTitle(tr.querySelector('.copy-title'),row.title)"));
    assert!(DASHBOARD.contains(r#"class="history-title copy-title copy-title-detail""#));
    assert!(DASHBOARD.contains("event.stopPropagation()"));
    assert!(DASHBOARD.contains("if(event.target!==tr)return"));
    assert!(DASHBOARD.contains("navigator.clipboard.writeText(title)"));
    assert!(DASHBOARD.contains("document.execCommand('copy')"));
    assert!(DASHBOARD.contains(r#"id="copy-feedback" class="copy-feedback" role="status" aria-live="polite""#));
}
