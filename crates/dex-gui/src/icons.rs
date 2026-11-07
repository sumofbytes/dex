//! The icon set from the Paper design (`DESIGN.md`): 24-unit stroke icons, stroke 1.6, round
//! caps and joins. Each element carries its own stroke attributes (rune does not inherit them
//! from a group), and `currentColor` takes the colour set with `.color(..)`.

use rune::Icon;

pub static HOME: Icon = Icon {
    name: "home",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M4 11l8-6 8 6v8a1 1 0 0 1-1 1h-4v-6h-6v6H5a1 1 0 0 1-1-1z"/>"#,
};
pub static PLUS: Icon = Icon {
    name: "plus",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M12 5v14M5 12h14"/>"#,
};
pub static SEARCH: Icon = Icon {
    name: "search",
    view: 24.0,
    body: r#"<circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="11" cy="11" r="6"/><path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M20 20l-4.3-4.3"/>"#,
};
pub static LEFT: Icon = Icon {
    name: "left",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M15 6l-6 6 6 6"/>"#,
};
pub static X: Icon = Icon {
    name: "x",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M6 6l12 12M18 6L6 18"/>"#,
};
pub static FOLDER: Icon = Icon {
    name: "folder",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>"#,
};
pub static TERMINAL: Icon = Icon {
    name: "terminal",
    view: 24.0,
    body: r#"<rect fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" x="3" y="4" width="18" height="16" rx="2"/><path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M7 9l3 3-3 3M13 15h4"/>"#,
};
pub static FILE: Icon = Icon {
    name: "file",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M14 3v5h5"/>"#,
};
pub static BRANCH: Icon = Icon {
    name: "branch",
    view: 24.0,
    body: r#"<circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="6" cy="5" r="2"/><circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="6" cy="19" r="2"/><circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="18" cy="7" r="2"/><path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M6 7v10M18 9c0 5-12 4-12 8"/>"#,
};
pub static COMPARE: Icon = Icon {
    name: "compare",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M7 4v12M3 8l4-4 4 4M17 20V8M13 16l4 4 4-4"/>"#,
};
pub static UP: Icon = Icon {
    name: "up",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M12 19V5M5 12l7-7 7 7"/>"#,
};
pub static CHECK: Icon = Icon {
    name: "check",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M5 12l4 4 9-9"/>"#,
};
pub static PANEL: Icon = Icon {
    name: "panel",
    view: 24.0,
    body: r#"<rect fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" x="3" y="4" width="18" height="16" rx="2"/><path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M9 4v16"/>"#,
};
pub static SLIDERS: Icon = Icon {
    name: "sliders",
    view: 24.0,
    body: r#"<path fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" d="M4 7h10M18 7h2M4 17h2M10 17h10"/><circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="16" cy="7" r="2"/><circle fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" cx="8" cy="17" r="2"/>"#,
};

pub static DOWN: Icon = Icon {
    name: "down",
    view: 24.0,
    body: r#"<path d="M6 9l6 6 6-6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

pub static RIGHT: Icon = Icon {
    name: "right",
    view: 24.0,
    body: r#"<path d="M9 6l6 6-6 6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
