# dex GUI design spec

Source of truth: the Paper file **Helpful canyon** (Arpit's Team), page `Page 1`:
https://app.paper.design/file/01M6RQZFHQK3WA0X5ZK25QGK0Y/p-1-0

The GUI replicates these artboards. Change the design in Paper first, then update this file and the code.

| Artboard | Size | Paper node | Notes |
| --- | --- | --- | --- |
| dex / Thread | 1100×760 | 1-0 | sidebar, header, transcript, composer, footer |
| dex / Home | 1100×760 | 1X-0 | greeting, stat cards, Today, Start |
| dex / Viewer · Files | 460×760 | 5Y-0 | tabs, path, code preview |
| dex / Viewer · Changes | 460×760 | 52-0 | tabs, file header, diff hunks |
| dex / Theme picker | 420×fit | 7J-0 | dialog with search and scheme list |

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

The app derives the rest of the palette from three seeds (background, text, accent) with `rune-kit`. The Paper file uses the same seeds, so the values above are the seeds' outputs and should match.

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
- Project header: 32px tall, 6px colour dot, name Inter 11px weight 600 uppercase, muted, chevron right.
- Thread rows: 32px tall, left padding 24px so the title lines up with the project name, Inter 13px. Selected: weight 500, text colour, bg #433b3b, radius 8px. Unselected: weight 400, text-3.
- Age: IBM Plex Mono 11px, faint, right-aligned in a 24px column. Titles truncate with an ellipsis.
- "Show N more": Inter 12px, faint, 28px tall, same left padding as thread rows.
- Gap of 12px between project groups.
- Home nav row is selected on the Home page; a thread row is selected on the Thread page.

## Known gaps

- The sidebar in the Viewer and Theme artboards is not drawn; they are shown without it.
