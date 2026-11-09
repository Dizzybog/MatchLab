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
    assert!(DASHBOARD.contains(r#"event.key==='Enter'||event.key===' '"#));
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


#[test]
fn activity_days_are_keyboard_accessible_and_show_exact_utc_request_totals(){
    // Bars are buttons, not hover-only divs. Selection uses the already
    // aggregated day.total from the 14-day API (not the capped 1,000 rows).
    assert!(DASHBOARD.contains(r#"role="group" aria-label="Daily request activity (UTC)""#));
    assert!(DASHBOARD.contains(r#"id="day-summary" class="day-summary small" role="status" aria-live="polite""#));
    assert!(DASHBOARD.contains(r#"class="day-bar" data-day=""#));
    assert!(DASHBOARD.contains(r#"aria-controls="day-summary" aria-pressed="false""#));
    assert!(DASHBOARD.contains("button.addEventListener('click',()=>selectActivityDay(button.dataset.day,days))"));
    assert!(DASHBOARD.contains("const count=Math.max(0,Number(selected.total)||0);"));
    assert!(DASHBOARD.contains("summary.textContent=day+' (UTC) · '+count.toLocaleString()+' requests';"));
    assert!(DASHBOARD.contains("selectActivityDay(selectedActivityDay,days)"));
    assert!(DASHBOARD.contains("bar.setAttribute('aria-pressed',String(bar.dataset.day===selectedActivityDay))"));
    assert!(DASHBOARD.contains("focused?.focus({preventScroll:true})"));
    assert!(DASHBOARD.contains(".day-bar:focus-visible"));
    assert!(DASHBOARD.contains(".day-bar{min-width:24px}"));
}

#[test]
fn ml_regex_inventory_is_conditional_and_uses_live_status(){
    // The lifetime rule inventory is separate from 14-day matching statistics.
    // A disabled ML worker must not leave a stale dashboard card visible.
    assert!(DASHBOARD.contains(r#"id="ml-regex-stats" class="ml-regex-stats" aria-label="ML-generated regex statistics" hidden"#));
    assert!(DASHBOARD.contains(r#"id="ml-regex-count""#));
    assert!(DASHBOARD.contains(r#"id="ml-regex-archive""#));
    assert!(DASHBOARD.contains(".ml-regex-stats[hidden]{display:none}"));
    assert!(DASHBOARD.contains("const state=await json('/api/ml/status');"));
    assert!(DASHBOARD.contains("if(state.enabled!==true){section.hidden=true;return;}"));
    assert!(DASHBOARD.contains("inventory?.ready===true"));
    assert!(DASHBOARD.contains("Number(inventory?.active)"));
    assert!(DASHBOARD.contains("Number(inventory?.archived)"));
    assert!(DASHBOARD.contains("const total=ready&&valid(archived)?archived.toLocaleString():'—';"));
    assert!(DASHBOARD.contains("'Active patterns: '+"));
    assert!(DASHBOARD.contains("mlRegexStats();setInterval("));
}
