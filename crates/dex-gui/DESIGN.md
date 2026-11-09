# dex GUI design spec

Source of truth: the Paper file **Helpful canyon** (Arpit's Team), page `Page 1`:
https://app.paper.design/file/01M6RQZFHQK3WA0X5ZK25QGK0Y/p-1-0

The GUI replicates these artboards. Change the design in Paper first, then update this file and the code.

| Artboard | Size | Paper node | Notes |
| --- | --- | --- | --- |
| dex / Home v2 | 1100×760 | DH-0 | large greeting, Recent list, This week numbers, actions |
| dex / Thread v2 | 1100×760 | G5-0 | header, transcript hierarchy, composer, footer |
| dex / Viewer v2 · Files | 460×760 | JE-0 | segmented tabs, breadcrumb, numbered code |
| dex / Viewer v2 · Changes | 460×760 | LX-0 | file list with status, diff with two gutters |
| dex / Theme picker v2 | 440×400 | OC-0 | dialog with swatches and check mark |

The v1 artboards were removed from the Paper file.

## Colour tokens (dark, dex default)

| Token | Hex | Use |
| --- | --- | --- |
| page | #342d2d | main pane, viewer |
| surface | #2c2626 | sidebar, cards, code blocks, dialogs |
| composer | #3d3636 | composer box |
| selected | #433b3b | selected row, user bubble, active tab |
| border | #5a5151 | outlined pills and inputs |
| text | #f4eeee | titles, primary text |
| text-2 | #e4dcdc | body text |
| text-3 | #c9c0c0 | sidebar rows, tool names |
| muted | #a89f9f | subtitles, secondary labels |
| faint | #8e8484 | project labels, metadata |
| placeholder | #7d7474 | input placeholders, footer |
| accent | #f38d70 | primary button, project dot, Today highlight |
| on-accent | #2a1f1c | text on accent |
| ok | #8fd694 | done ticks, Projects count, diff additions |
| waiting | #f2c87a | In progress count |
| error | #f08f8f | diff deletions |
| diff add bg | #2b3a2e | added lines |
| diff del bg | #3d2a2a | removed lines |

Paper variables are not set through the MCP (a CSS variable on a parent does not reach its children), so the values are literal hex in the artboards. Set these as Paper variables in the Paper UI if you want them linked. The app derives the rest of the palette from three seeds (background, text, accent) with `rune-kit`. The Paper file uses the same seeds, so the values above are the seeds' outputs and should match.

## Type

- UI: Inter. Code and numbers: IBM Plex Mono.
- Display: 44px, weight 300, −0.02em (Home greeting).
- Title: 18px, weight 600 (transcript headings).
- Header title: 14px, weight 500.
- Whole, even sizes only. Reading text (reply, user message, thought text, composer): 14px / 22. UI (sidebar rows, labels, quiet transcript rows such as "Ran N commands" and "Thought · 4s", buttons): 12px / 18. Caption: 12px / 16-18 (hints, descriptions, status). Caps labels: 10px, weight 600, tracked 1.0 (project and section labels). Time and shortcut hints: mono 12 / 16. Reply headings 18 / 16 / 14.
- In code these are named roles in `src/style.rs::typo`, and `theme.rs::even_type` sets Rune's own scale (ui 12/18, small 12/16, body 14/22) so its components follow. Never use odd sizes or fractional line heights.
- Mono: 12px / 18px (tool output, diffs, inline code).

## Radius and spacing

- Radius: rows 8px, code blocks 10px, cards 12px, bubbles 12px, composer and dialogs 14px, pills 999px.
- Sidebar: 280px wide, padding 14px 8px, rows 8px 10px. Every row is on one grid; hierarchy comes from type, not indent.
- Spacing scale (`src/style.rs::space`): XXS 2, XS 4, SM 6, MD 8, LG 10, XL 12, XXL 16. Gaps and paddings use these names; layout one-offs (14, 18, 20, 48 …) stay literal.
- Header: 48px tall, padding 0 16px.
- Transcript column: 760px max inside a 16px gutter, gap 16px, centred. One content line runs 12px inside the composer card's edge (card padding 4 + 8 in the elements): reply, thought, tool rows, the user bubble's right edge, the input text, the option chips' icon and text, the send button's right edge and the footer all start and end on it (left x = card + 12, right x = card right - 12). Work-block head rows and their Stop control keep 8px inside their band instead — they are chrome rows with a hover/header fill, not reading text — and what opens under a head sits on the head's text column (26 in), not on the content line. The queued-message boxes span the full card width.
- Home: padding 40px 48px, gap 28px. Stat cards: four equal columns, gap 12px.
- Viewer: 460px wide. Tabs row 48px, padding 0 12px.

## Screens

**Thread.** Sidebar, then the main pane: header (title, token count and cost), transcript, composer with mode and model selects and a send button, and a footer with the path, branch and status. The viewer pane (460px) opens on the right.

**Home.** Date, greeting, subtitle, four stat cards (Today, Last 2 days, In progress, Projects), a Today card, and a Start card with "New thread" (accent) and "Continue last" (outline).

**Viewer.** Tabs: Files, Changes (count), Terminal, and a close button. Files shows the path and a syntax-coloured preview. Changes shows each file's header (path and status) then diff hunks with unchanged-line ranges.

**Theme picker.** A dialog with a title, a close button, a search box and a list of schemes. The selected scheme has the selected background.

## Sidebar

One grid (`src/sidebar.rs::tok`, board "dex / Sidebar v3 · grid"). Width 280, margin 12, row padding 10, so content spans x 22..258. Every row is 32 tall and built from the same chrome.

- Lead column x 22..38: nav icon (16), a project's folder icon (16) and a thread's status dot (6)
  or spinner (16, like the folder icon) start flush at x 22 — the same edge the NEEDS YOU and PROJECTS
  labels start on.
- Text column starts x 46 (lead 16 + gap 8): nav label, project name, thread title, "Show more". Hierarchy is by type (small bold caps for projects, regular text for threads), not indent.
- Trailing: a 24 time column, right edge x 230 (one 8 gap left of the slot); a 20 slot centred x 248 holding + on a project header and × on a hovered thread; the "Ctrl N" hint right-aligned to x 258.
- Brand "dex" at x 22, 14px weight 600. Nav labels 12px weight 500; selected row fill #433b3b, radius 8. Thread titles 12px, selected weight 500. Project names 10px caps. Times and hint mono 12.
- The whole project header toggles collapse (no chevron); × (hide) shows on hover only. The 16px lead holds a static folder icon in the project colour — it never shows a spinner; a running thread's spinner lives on its own row. Titles truncate with an ellipsis.
- A double-click on a thread row edits its title in place: the title becomes a single-line field with the text selected, Return sends the rename to the daemon (`POST /api/sessions/{id}/name`), Escape or a press elsewhere keeps the old title.

## Composer options

Artboard **dex / Composer options** (Paper node 18N-0). Replaces the flat mode and model selects.

- Mode menu: a titled list (MODE) of Read-only, Ask and Trusted, each with a one-line description. The current mode has a fill and a check mark. Menu width 240px.
- Model menu: 280px. A search field, then models grouped by provider (OPENCODE). Each row shows the model name in mono, the provider in faint text, and a check on the current model. A "12 more · type to filter" row appears when the list is long.
- Chips in the composer: mode with a sliders icon, model with the provider in faint text. Both open their menus upward from the composer.
- Menus share a surface (#2c2626), 1px border (#4a4141), radius 12, 6px padding; rows radius 8.

## Footer under the composer

- It sits in the same centred column as the composer (680px in Paper, 760px max in the app), so the two edges line up.
- Height 36px with 8px bottom padding, so it doesn't touch the window edge.
- Left: the project path in IBM Plex Mono 12px, a "·" separator, the branch icon and the branch name in mono 12px. Faint colour.
- Right: a 6px status dot (green when ready, amber when busy) and the provider name in Inter 12px, faint. No status text — thread activity shows in the sidebar's dot/spinner.

## Transcript (text first, v5)

Designed in Penpot as **dex / Transcript v5 · text first** (760×640), replicated in `src/view.rs`:

- A run of tool calls in a turn folds into one summary line, "Ran N commands", with a chevron. Opening it shows the compact tool rows (`bash: command`, output dimmed under `↳`). Ctrl+O opens or closes all groups.
- Items sit 14 points apart; a user message gets 8 points more above it.
- Known gap: assistant text uses the Markdown component's own size, not the 16px in Penpot, and it has no orange dot on the first line.

## Terminal and transcript

The terminal was designed in Penpot as **dex / Terminal v3** (460×760) and replicated in `src/viewer.rs`. The Penpot file is "Helpful canyon" in Arpit's Team's Penpot instance (local, self-hosted). Its board id is `11917636-b751-8001-8008-e7b0ec2a1141`.

The transcript changes below were made in code at your request, because the Paper file was rate-limited. They are not in Paper yet.

- Terminal: each command is a block: `$ command` with its status on the right (running dot and seconds, a check with seconds, or `exit N` in the danger colour), then its output under a 2px rule. A "Clear" button appears once there is history. The prompt is a bordered row with a `›` and an Enter hint. The last 300 lines of each command are shown.
- Transcript: 22 points between items, 24 points at the top and bottom, and 10 extra points before each user message. The user bubble has 16×12 padding and an extra-large radius.

## Replication status

Replicated in `src/` (v2): the sidebar, Home, the thread header, tool rows, composer and footer, the viewer tabs and numbered file preview, and the theme picker rows. Icons are in `src/icons.rs`.

Composer options (`src/composer_menu.rs`) replicate the mode and model menus.

Differences that remain:
- The code blocks come from rune-kit's Markdown, so they don't have the "RUST" label from the design.
- The greeting uses medium weight. The design uses light (300); rune's type scale has no light weight yet.
- The viewer's code scrolls vertically only, so long lines run past the pane edge.
- The Changes view uses rune-kit's diff component, so its file list and hunk headers differ from the Paper design.
- Colours come from the theme picker (the three seeds), so the default is the design's dex palette, but other schemes change them.

## Known gaps

- The sidebar in the Viewer and Theme artboards is not drawn; they are shown without it.

## Icons

Stroke icons on a 24px grid, stroke 1.6, round caps and joins, drawn as inline SVG in Paper. Sizes: 12px (chevrons, meta), 14px (tool and tab icons), 16px (nav and header). Set: home, plus, search, chevrons, x, folder, terminal, file, branch, compare, sliders, up (send), check, theme, panel, clock. Source paths are in `ui.py` in the design scratchpad.

## Hierarchy rules (v2)

- One strong focal point per screen: the greeting on Home, the title in the thread header, the code on the viewer.
- Use scale contrast: 56px display against 12px caps labels. Use light weight (300) for display and medium (500) for labels.
- Separate with hairlines (#433b3b), not card backgrounds. Cards are only for code blocks and dialogs.
- Tool output sits under a 2px left rule. Code gets a label and a surface. Bubbles are used only for the user's message.
- The primary action is the accent button. Everything else is outline or ghost.

## Work block

Every run of thoughts and tool calls between two messages folds into one block (a run ends at an assistant message, a user message or an error).

- Done, collapsed (default): `› Worked for 14s  2 thoughts · 5 commands`, a 12px faint row (duration is the sum of thought and tool times, omitted under 1s; a zero count is omitted). The reply follows 20 below.
- Done, expanded: chevron down; the row takes the hover colour as a static header band (collapsed, it is hover-only); the rows nest 26 in, in order — on the head's text column, so tool names and a thought's chevron sit under the head's label. A thought is a `› Thought · 4s` row that opens by itself to 12px / 18 faint text (indented 18 under its own chevron, flush with its label); tool rows are unchanged (`bash: cmd`, time, dimmed `↳` output).
- Live: the spinner takes the chevron's place — `⟳ Working · 9s` (12px medium, muted) — so its label sits on the same column as the done row's; same collapsibility, collapsed by default; expansion survives into the done row. Stop sits at the right end.
- Ctrl+O opens or closes every block and tool row; Ctrl+T every thought. Rhythm: 16 between blocks, 32 around a user message, 20 from the block to the reply.

## Needs you

Penpot board "dex / Sidebar v4 · needs you". A NEEDS YOU caps label and rows sit above PROJECTS, on the same grid as thread rows: status dot in the lead column, title in the text column, time at the right.

- Amber dot: an approval or a question is waiting on you (the row shows the prompt). Green: a background turn finished while the thread was not open. Red: it failed.
- A row leaves the list when you open the thread. The thread stays under its project as well.
- A thread whose turn runs in the background keeps its spinner under its project, however many threads run at once. Starting a new thread or opening another one does not stop a running turn. The project header never shows a spinner.
