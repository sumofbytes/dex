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
- Header title: 15px, weight 500.
- Body: 14px / 22px line height; secondary 14px / 20px.
- Small: 13px. Caption: 12px, weight 500, 0.02em (project labels, uppercase in the sidebar).
- Mono: 12px / 18px (tool output, diffs), 13px (inline code).

## Radius and spacing

- Radius: rows 8px, code blocks 10px, cards 12px, bubbles 12px, composer and dialogs 14px, pills 999px.
- Sidebar: 280px wide, padding 14px 8px, rows 8px 10px. Thread rows under a project indent 14px.
- Header: 48px tall, padding 0 16px.
- Transcript column: 680px max, gap 14px, centred.
- Home: padding 40px 48px, gap 28px. Stat cards: four equal columns, gap 12px.
- Viewer: 460px wide. Tabs row 48px, padding 0 12px.

## Screens

**Thread.** Sidebar, then the main pane: header (title, token count and cost), transcript, composer with mode and model selects and a send button, and a footer with the path, branch and status. The viewer pane (460px) opens on the right.

**Home.** Date, greeting, subtitle, four stat cards (Today, Last 2 days, In progress, Projects), a Today card, and a Start card with "New thread" (accent) and "Continue last" (outline).

**Viewer.** Tabs: Files, Changes (count), Terminal, and a close button. Files shows the path and a syntax-coloured preview. Changes shows each file's header (path and status) then diff hunks with unchanged-line ranges.

**Theme picker.** A dialog with a title, a close button, a search box and a list of schemes. The selected scheme has the selected background.

## Sidebar

- Width 280px, padding 0 12px, surface #2c2626. The brand row is 48px tall, the same as the header, so their text lines up.
- Brand: "dex", Inter 15px, weight 600.
- Nav rows (Home, New thread, Search): 32px tall, icon 16px in muted, label Inter 13px weight 500, selected row bg #433b3b. Shortcut hints in IBM Plex Mono 11px, faint (for example "Ctrl N").
- Section label "PROJECTS": Inter 11px, weight 600, uppercase, letter-spacing 0.06em, placeholder colour, 28px tall, 20px space above.
- Project header: 32px tall, `[chevron] [dot] NAME … [×] [+]`. The whole row toggles collapse; the chevron (left) points right when collapsed, down when open. The + (new thread) is always visible on the right; the × (hide) shows on hover only. 6px colour dot, name Inter 11px weight 600 uppercase, muted.
- Thread rows: 32px tall, left padding 24px so the title lines up with the project name, Inter 13px. Selected: weight 500, text colour, bg #433b3b, radius 8px. Unselected: weight 400, text-3.
- Age: faint, right-aligned in a fixed 24px column left of the shared 20px right slot (thread status / hover ×, project +), so + and the slot line up. Titles truncate with an ellipsis.
- Code: layout tokens (row height, inset, gap, dot, indent, time column) live in `src/sidebar.rs::tok`.
- "Show N more": Inter 12px, faint, 28px tall, same left padding as thread rows.
- Gap of 12px between project groups.
- Home nav row is selected on the Home page; a thread row is selected on the Thread page.

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
- Right: a 6px status dot (green when ready, amber when busy) and "provider · status" in Inter 12px, faint.

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
- Use scale contrast: 56px display against 11px caps labels. Use light weight (300) for display and medium (500) for labels.
- Separate with hairlines (#433b3b), not card backgrounds. Cards are only for code blocks and dialogs.
- Tool output sits under a 2px left rule. Code gets a label and a surface. Bubbles are used only for the user's message.
- The primary action is the accent button. Everything else is outline or ghost.
