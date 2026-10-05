# Implementation tasks: Pane interactions and Settings consistency

Planning complete; all implementation tasks remain unchecked. See `tasks/plan.md` for decisions, evidence, risks and scope.

- [ ] Human reviews and approves the plan before implementation.

**Standing verification:** each task includes a focused behavior test, `cargo build -p ubra-app --bin ubra-gui` (plus `cargo build -p ubra-ui` when touched), and a real changed-path smoke/visual check. Commands below are planned, not executed. Update existing behavior documentation as each permanent change lands. All tasks have at most five likely files; if investigation exposes more independent work, split it explicitly rather than silently widening the task.

## Task 1: Repair Settings scroll ownership

- [ ] Implement and verify this task.

**Description:** Diagnose the reported double bounce in the production centered Settings dialog, then correct competing scroll ownership and viewport constraints. Audit every tab; do not suppress overscroll globally as a workaround.

**Acceptance criteria:**
- [ ] Each gesture moves/bounces only its intended page, list, detail, rail or editor region; a fill-pane list never also scrolls the outer Settings page.
- [ ] Trackpad finger/momentum and wheel input remain continuous at top/bottom boundaries, after tab switches and during resize; reduced motion settles without animation.
- [ ] Record before/after production-window evidence and deterministic regressions for the isolated ownership/gesture failure.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui settings`.
- [ ] Focused tests pass: `cargo test -p ubra-ui scroller`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui` and `cargo build -p ubra-ui`.
- [ ] Smoke/manual check: Native Settings: inspect all tabs with long content; sample scroll offsets/event ownership and observe one edge response per gesture.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/surface_shell.rs`
- `crates/ubra-app/src/settings_dialog.rs`
- `crates/ubra-app/src/settings.rs`
- `crates/ubra-ui/src/scroller.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 2: Make Settings list content fully reachable

- [ ] Implement and verify this task.

**Description:** Repair content sizing, expansion and scroll extents in the real dialog. Prioritize Shortcuts, Skills list/detail and Schedules; audit the remaining ordinary pages and Worktrees pagination, splitting any additional independent renderer repair into another small task before editing.

**Acceptance criteria:**
- [ ] Every seeded item and bottom action is reachable at default and small window sizes; expanding content reveals its full content without clipping or overlapping adjacent rows.
- [ ] Filtering, expansion, refresh and tab switching update/clamp content extents correctly; virtualized rows obey actual height contracts and Worktrees page controls expose entries beyond row 40.
- [ ] Production-dialog fixtures cover last-item reachability, long details, expansion and resize rather than only a whole-page bounding-box assertion.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui shortcut`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui skills`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui schedules`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui worktree`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native Settings: reach the last item/detail action on each tab; expand long content, filter, refresh, resize, and navigate a >40-entry Worktrees inventory.

**Dependencies:** 1

**Files likely touched:**
- `crates/ubra-app/src/surface_shell.rs`
- `crates/ubra-app/src/skills_page.rs`
- `crates/ubra-app/src/schedules_page.rs`
- `crates/ubra-app/src/root.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 3: Open Settings Worktrees from the status bar

- [ ] Implement and verify this task.

**Description:** Change only the status-bar Worktree activation route to the existing Settings dialog Worktrees tab. Preserve other standalone Worktree workflows.

**Acceptance criteria:**
- [ ] Pointer and keyboard activation of status-bar-worktree opens Settings with Worktrees selected, not a second Worktrees dialog.
- [ ] An existing Settings dialog is reused and switches to Worktrees; no session selection or process lifecycle changes.
- [ ] Worktrees inventory loads through its existing Settings path and closing Settings restores the prior pane focus.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui status_bar`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui settings_dialog`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native app: activate status-bar Worktree with Settings closed and already open on another tab; verify one dialog and correct inventory.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/root.rs`
- `crates/ubra-app/src/status_bar.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint A — Settings correctness (after Task 3)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Real centered Settings dialog has one scroll owner per region, all content is reachable, and status-bar Worktree opens its Worktrees section.
- [ ] Native visual/interaction evidence is recorded; human reviews the checkpoint before continuing.

## Task 4: Split immediately using the configured default

- [ ] Implement and verify this task.

**Description:** Replace split-button menu toggles with immediate default-agent-or-shell dispatch through request_agent_split and existing receipts. Use the source host catalog and preserve typed host/cwd/edge placement; do not alter global shortcut fallback policy.

**Acceptance criteria:**
- [ ] Right and bottom buttons launch exactly one new configured launchable agent on click; Terminal/unavailable/missing-readiness defaults launch a shell rather than another installed agent.
- [ ] Click requires no chooser even after hover opened a picker, inherits source host/cwd, and never clones the source SessionId.
- [ ] Success inserts the real new session in the correct edge; failure/pending placement retains existing truthful feedback and does not spawn twice.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_workbench`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui agent_catalog`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Disposable live local sessions: click both directions with configured agent and Terminal; verify new process/session, directory, focus and failure feedback. Use fixture host catalogs for remote targeting.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/workspace_workbench.rs`
- `crates/ubra-app/src/agent_catalog.rs`
- `crates/ubra-app/src/store/workspace_spawn.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 5: Correct split hover-picker lifetime

- [ ] Implement and verify this task.

**Description:** Give the hover picker explicit trigger/menu lifetime and source identity without blocking normal toolbar clicks. Reuse agent_menu choices and existing Escape handling.

**Acceptance criteria:**
- [ ] Hover alone spawns nothing; selecting a listed agent launches that exact kind and edge once, then closes the picker.
- [ ] Trigger-to-menu travel stays usable; moving to zoom, close, drag or another unrelated control dismisses; switching right/bottom retargets without stale callbacks.
- [ ] Outside click, Escape, tab/pane removal and direct split activation clear stale state; direct split clicks are not swallowed by the scrim.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_workbench`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native toolbar: hover→menu pick, hover→direct split click, right→bottom, split→zoom/close, leave/re-enter and Escape; observe menu state and spawn count.

**Dependencies:** 4

**Files likely touched:**
- `crates/ubra-app/src/workspace_workbench.rs`
- `crates/ubra-app/src/agent_menu.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 6: Simplify pane headers and use a drag grip

- [ ] Implement and verify this task.

**Description:** Remove the combined Links/session-details trigger from pane and hosted horizontal header controls. Replace the hand glyph with a vendored six-dot Lucide grip; retain existing drag mechanics and unrelated information access.

**Acceptance criteria:**
- [ ] No Links/session-details icon remains in pane/header variants; remove code/state made genuinely unreachable without deleting engine metadata or inspector features.
- [ ] Drag grip has the existing accessible name/tooltip and grab cursor; edge-move and center-swap preserve original sessions.
- [ ] Header actions remain aligned and usable in single/split panes, narrow windows and horizontal/vertical tab layouts.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui terminal_pane`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_workbench`.
- [ ] Focused tests pass: `cargo test -p ubra-ui icon`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui` and `cargo build -p ubra-ui`.
- [ ] Smoke/manual check: Native window: inspect all header variants and drag to move/swap; check icon readability and unchanged identities.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/terminal_pane.rs`
- `crates/ubra-app/src/workspace_workbench.rs`
- `crates/ubra-ui/src/icon.rs`
- `crates/ubra-ui/assets/icons/grip.svg`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint B — Pane interaction (after Task 6)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Direct split click starts the default/Terminal; hover choice, cross-control dismissal, simplified header and move/swap grip work end-to-end.
- [ ] Native visual/interaction evidence is recorded; human reviews the checkpoint before continuing.

## Task 7: Animate pane zoom without restarting terminals

- [ ] Implement and verify this task.

**Description:** Add short, subtle reversible presentation motion for pane maximize/restore, reusing existing motion curves while keeping settled layout and terminal ownership authoritative.

**Acceptance criteria:**
- [ ] Zoom in/out animates approximately 160–190 ms without overshoot; rapid reversal starts from the current pose and reduced motion settles immediately.
- [ ] Pane/session/controller identity and input focus survive transitions; final geometry matches the saved layout for toolbar and keyboard zoom.
- [ ] Resize, tab change, removal and externally updated layout interrupt safely; no animation-only grid duplication, persistent frame loop or uncapped PTY resize burst.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_workbench`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_geometry`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui keyboard_workspace_operations_commit_through_engine_without_restarting_ptys -- --ignored`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui` and `cargo build -p ubra-ui`.
- [ ] Smoke/manual check: Disposable real PTYs: type before/during/after zoom, reverse quickly, resize and switch tabs; record transition and unchanged process identity.

**Dependencies:** 4, 5, 6

**Files likely touched:**
- `crates/ubra-app/src/workspace_workbench.rs`
- `crates/ubra-app/src/workspace_geometry.rs`
- `crates/ubra-app/src/workspace_workbench/commands.rs`
- `crates/ubra-ui/src/motion.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 8: Remove the sidebar context-menu disclosure row

- [ ] Implement and verify this task.

**Description:** Remove Expand/Collapse from the shared project context menu and its now-unused collapsed lookup. Keep actual header/workspace disclosure and sidebar visibility controls.

**Acceptance criteria:**
- [ ] Project menus contain no Expand/Collapse command across right-click and shared ellipsis/project-picker entrypoints.
- [ ] Pin/Unpin and Close All Sessions continue to work with existing safety behavior.
- [ ] Project chevrons, workspace disclosure, sidebar visibility and their keyboard paths remain unchanged.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui sidebar`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native sidebar: compare project menus when expanded/collapsed; use chevrons and remaining commands.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/sidebar/view.rs`
- `README.md`

**Estimated scope:** Small: 1–2 files.

## Task 9: Enable copy-on-selection by default

- [ ] Implement and verify this task.

**Description:** Set the preference default to true without overriding an existing explicit false. Verify actual terminal selection completion and clipboard contents.

**Acceptance criteria:**
- [ ] Fresh preferences and files missing terminalCopyOnSelect enable copying; explicit false survives deserialize/save/reload.
- [ ] Drag, double-click and triple-click local selections copy their exact selected text on completion; empty selection does not replace the clipboard.
- [ ] Opt-out and terminal-owned mouse reporting preserve existing behavior; modifier-based local selection still works.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui prefs`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui selection`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Disposable terminal: select known multiline text, inspect clipboard, then disable setting and repeat; check an empty click leaves clipboard unchanged.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/store/prefs.rs`
- `crates/ubra-app/src/terminal_pane.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint C — Motion and defaults (after Task 9)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Real PTY zoom preserves process identity and input, reduced motion works, project menu disclosure is removed, and clipboard opt-out survives.
- [ ] Native visual/interaction evidence is recorded; human reviews the checkpoint before continuing.

## Task 10: Place sidebar right-click menus near the pointer

- [ ] Implement and verify this task.

**Description:** Define the pointer-gap/flip/clamp placement contract using existing floating geometry. Explicitly distinguish right-click from button-origin project menus; migrate every sidebar right-click producer without changing logical dropdown/hover anchors.

**Acceptance criteria:**
- [ ] Project/session rows, rename-row variant, horizontal tabs, Projects control and project-picker rows use captured click coordinates plus one consistent small gap; near edges flip/clamp within host bounds.
- [ ] Native glass panels and deferred in-window menus resolve equivalent geometry; owner focus, outside dismissal and hidden-sidebar tab menus work.
- [ ] Ellipsis/new-agent dropdowns remain button-relative and hover cards remain row-relative; no double subtraction of sidebar/pane origins.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui floating::`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui right_click`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui project_menu_opens_below_its_trigger`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Live Glass app and UBRA_FLOATING_PANELS=0: right-click center and window corners, move/resize host, inspect negative/nonzero monitor origins and restored focus.

**Dependencies:** 8

**Files likely touched:**
- `crates/ubra-app/src/floating.rs`
- `crates/ubra-app/src/sidebar/state.rs`
- `crates/ubra-app/src/sidebar/view.rs`
- `crates/ubra-app/src/sidebar/view/project_picker.rs`
- `crates/ubra-app/src/sidebar/view/tabs.rs`

**Estimated scope:** Medium: 3–5 files.

## Task 11: Align terminal context-menu pointer placement

- [ ] Implement and verify this task.

**Description:** Apply Task 10 pointer policy to the terminal menu while preserving pane/body containment, header offset conversion and terminal mouse ownership. Do not add native terminal-menu panels solely for this fix.

**Acceptance criteria:**
- [ ] Menu tracks the original right-click point with the shared gap/flip/clamp policy in single/split panes with shown/hidden headers.
- [ ] Every menu action remains reachable near viewport edges after resize, without coordinate drift or bottom clipping.
- [ ] Mouse-reporting applications receive right-click normally; Alt override, selection/link actions, Escape and outside dismissal retain their contracts.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui terminal_selection_drag_reaches_outside_and_context_menu_dismisses`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui pointer_owner`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Real terminal: menus in each split and all pane corners, sidebar shown/hidden, horizontal tabs, mouse-reporting program with/without Alt.

**Dependencies:** 10

**Files likely touched:**
- `crates/ubra-app/src/terminal_pane/qol.rs`
- `crates/ubra-app/src/terminal_pane.rs`
- `crates/ubra-app/src/floating.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint D — Pointer menus (after Task 11)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Right-click menus near all edges match native/in-window geometry; dropdown and hover anchors remain unchanged; terminal mouse reporting is intact.
- [ ] Native visual/interaction evidence is recorded; human reviews the checkpoint before continuing.

## Task 12: Apply shared glass-style treatment to Settings

- [ ] Implement and verify this task.

**Description:** Establish one scoped modal surface/scrim treatment using existing semantic tokens and FloatingSurface. Migrate Settings card/rail/page fill layers without globally changing menus/toasts or pretending alpha provides backdrop blur.

**Acceptance criteria:**
- [ ] Settings uses coherent themed fill, border/rim, radius, elevation, typography and backdrop; nested backgrounds do not accidentally make sections opaque or multiply tint.
- [ ] Dark/light and Glass/Opaque remain readable above live terminal content, including custom transparency and rounded corners; no unsupported blur API or new native-window architecture.
- [ ] Every Settings tab retains working scroll/resize/focus behavior and semantic control states after restyling.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-ui`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui settings_dialog`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui` and `cargo build -p ubra-ui`.
- [ ] Smoke/manual check: Native production Settings over live terminal output: all tabs in light/dark, Glass/Opaque, wallpaper/transparency variants, small/large window and reduced motion; capture evidence.

**Dependencies:** 1, 2

**Files likely touched:**
- `crates/ubra-ui/src/tokens.rs`
- `crates/ubra-ui/src/components.rs`
- `crates/ubra-app/src/settings_dialog.rs`
- `crates/ubra-app/src/surface_shell.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 13: Dismiss Settings safely on outside click

- [ ] Implement and verify this task.

**Description:** Route backdrop activation through the existing UtilitySurfaces owner-safe dismissal path and align explicit close routes with persistence semantics. Preserve innermost-overlay precedence and block click-through.

**Acceptance criteria:**
- [ ] Clicking the backdrop dismisses Settings once, while card/rail/control clicks do not; no underlying session receives the gesture.
- [ ] Nested dropdowns/prompts dismiss only the topmost applicable surface; native panel selection does not close Settings; Escape and window-close actions preserve their intended precedence.
- [ ] Dirty include edits persist through owner dismissal; a failed save retains edits and dialog with actionable error; successful close restores prior terminal-pane focus.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui settings_dialog`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui failed_include_save_retains_edits_and_keeps_settings_open`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui clicking_inside_settings_but_outside_a_dropdown_closes_only_the_dropdown`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Production Settings: backdrop/inside clicks, native dropdown extending outside card, nested prompt, failed persistence fixture, prior split-pane focus and no click-through.

**Dependencies:** 12

**Files likely touched:**
- `crates/ubra-app/src/settings_dialog.rs`
- `crates/ubra-app/src/surface_shell.rs`
- `crates/ubra-app/src/root.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint E — Settings modal (after Task 13)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Production Settings glass-style treatment, owner-safe outside dismissal, nested popup precedence and failed-save retention work in a native window.
- [ ] Human reviews the visual and interaction evidence before migrating the remaining modal families.

## Task 14: Unify utility and nested modal styling

- [ ] Implement and verify this task.

**Description:** Apply the same scoped modal treatment to Worktrees/Diagnostics and their nested decisions, Settings root-conflict prompt and Usage share. Verify production composition rather than relying on the old utility harness; repair the observed share-overlay mounting gap if reproduced.

**Acceptance criteria:**
- [ ] All affected custom modal families share Task 12 visual treatment while preserving intentionally nested or partial-window geometry and destructive semantics.
- [ ] Usage share opens visibly from the production Settings dialog, including its theme picker, and nested overlays close in order without closing Settings unexpectedly.
- [ ] Worktree cleanup/move/error and root-conflict decisions remain actionable and readable, with correct focus and unchanged domain operations.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui worktree`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui usage_settings`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui settings_dialog`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native Worktrees, Diagnostics, cleanup/move prompts, root conflict and Usage share in both themes/materials; exercise nested dismissal and safe fixture operations.

**Dependencies:** 12, 13

**Files likely touched:**
- `crates/ubra-app/src/surface_shell.rs`
- `crates/ubra-app/src/usage_page.rs`
- `crates/ubra-app/src/settings_dialog.rs`
- `crates/ubra-app/src/root.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 15: Unify remaining custom modal styling

- [ ] Implement and verify this task.

**Description:** Migrate What’s New and custom close/protected-paste confirmations to the same modal family, preserving native AppKit alerts. Correct focus restoration when a sheet sits above Settings.

**Acceptance criteria:**
- [ ] What’s New and custom confirmations match the shared modal presentation in both themes/materials without changing their action meaning.
- [ ] Dismissing What’s New over Settings returns focus to Settings; ordinary closes return to the correct terminal; destructive cancel and protected-paste decisions remain correct.
- [ ] Native NSAlert stays native and functional; related palettes, hover popovers, menus, toasts and in-flow launcher cards do not inherit modal-only behavior.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui whats_new`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui paste`.
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui close`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native app: What’s New over Settings, close-session/window prompts and protected paste; compare custom fixture surfaces and real AppKit prompts, verify cancel/focus paths.

**Dependencies:** 12, 13, 14

**Files likely touched:**
- `crates/ubra-app/src/whats_new.rs`
- `crates/ubra-app/src/root.rs`
- `crates/ubra-app/src/terminal_pane/qol.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint F — Complete (after Task 15)

- [ ] Integrated focused tests and affected package builds pass.
- [ ] Every requested behavior works in the native app, all custom modal families are reviewed, native alerts remain correct, and production Settings scrolling/list expansion is verified.
- [ ] Native visual/interaction evidence is recorded; human reviews the checkpoint before continuing.

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build --workspace --release`
- [ ] All requested acceptance criteria met; existing documentation updated; no unsupported blur claims or unverified runtime claims.

---

# Implementation tasks: Empty-workspace launch wizard

Implemented in this branch; owned by the wizard stream. See the appended onboarding plan in `tasks/plan.md`.

**Status:** Tasks 16–20 are implemented and covered by focused tests. Task 17–20 behavior is verified end-to-end by `empty_workbench_launches_the_selected_layout_from_the_ui` (real UI clicks through the real Engine) and by the ignored `workspace_launch` smoke tests, which launch every preset — including sixteen real shell PTYs — and assert counts, cwd, input/output and reload persistence. `cargo clippy --workspace --all-targets -- -D warnings` is clean.

**Still outstanding:** numeric 8-vs-16-pane CPU/memory measurements; `cargo test --workspace` and `cargo build --workspace --release`; and the native interaction review, which the user is driving themselves. `cargo fmt --all -- --check` currently also fails on unrelated in-flight files (`store/prefs.rs`, `engine/registry.rs`, `engine/tests/omp_real.rs`, `ui/brand.rs`); wizard-owned files are formatted.

- [x] Human approves the onboarding plan, including literal 4×4 = 16 panes and current-workspace empty-state scope.

**Standing verification:** Use consumer-visible behavior regressions, existing GPUI/Engine fixtures, affected package builds and native changed-path smoke proof. Delete obsolete presentation-only tests; do not replace them with exact wording or selector inventories. Each task includes at most five likely files. If implementation reveals an independent additional subsystem, split the task explicitly. Update existing README user behavior documentation as relevant changes land.

## Task 16: Support sixteen-pane workspace layouts

- [ ] Implement and verify this task.

**Description:** Raise the shared pane ceiling from eight to sixteen and prove Engine persistence/validation and GUI geometry support balanced 4×4 layouts. Keep layout-depth, terminal-ownership and spawn-admission bounds intact. This is the early risk gate for the requested largest preset, not permission to silently omit it.

**Acceptance criteria:**
- [ ] Sixteen distinct panes can be committed, loaded and rendered with valid split fractions; a seventeenth is rejected without changing the saved layout, and malformed/deep/duplicate-ID layouts remain rejected.
- [ ] Reload preserves session identities and focused/zoomed pane behavior; all sixteen unzoomed panes resolve to distinct usable bounds at a representative large window size.
- [ ] Native 8-versus-16-pane measurements record idle/active CPU, memory, resize/input behavior and first-usable-pane latency; no terminal ownership or input/output loss occurs.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-proto workspace`, `cargo test -p ubra-engine workspace`, and `cargo test -p ubra-app --bin ubra-gui workspace_geometry`.
- [ ] Build succeeds: `cargo build -p ubra-engine -p ubra-app`.
- [ ] Smoke/manual check: Disposable local Engine/GUI with sixteen real shell PTYs; resize, focus, type into each, zoom/unzoom and reload; record measurements against eight panes.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-proto/src/workspace.rs`
- `crates/ubra-engine/src/workspace.rs`
- `crates/ubra-app/src/workspace_geometry.rs`
- `crates/ubra-app/src/workspace_workbench.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 17: Choose folder and agent in the empty-workspace modal

- [ ] Implement and verify this task.

**Description:** Replace the pane-local folder-then-agent card with one Root-owned, two-column modal. Page 1 matches the reference hierarchy and combines the labeled folder/agent dropdowns. Both requested CTAs advance a retained draft to the layout step without spawning. Preserve a lightweight resting state for nonempty workspaces and remove obsolete pane-local launch state.

**Acceptance criteria:**
- [ ] A settled empty work area opens one wizard per empty-state entry; existing work elsewhere is preserved, a nonempty selected workspace does not trigger it, and dismissal stays dismissed until explicit reopen or a new empty-state entry.
- [ ] Folder dropdown exposes actual known local roots plus the native directory picker; picker cancellation preserves the draft. Installed/MRU agents and Terminal use real readiness facts, scan/sign-in states and no fake binary paths; readiness changes do not silently substitute the chosen agent.
- [ ] “Get Started” requires folder/agent readiness, “Start in home folder” needs no project selection, and both reach Page 2 with zero spawn requests. Back retains choices; keyboard/popup dismissal restores appropriate focus.

**Verification:**
- [ ] Focused behavior tests pass: `cargo test -p ubra-app --bin ubra-gui empty_` and `cargo test -p ubra-app --bin ubra-gui agent_catalog`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native GUI with disposable empty state, existing sessions elsewhere, an empty selected workspace, loading/zero/one/multiple detected agents, folder-picker cancellation and both CTAs; inspect the real Page 1 surface.

**Dependencies:** None

**Files likely touched:**
- `crates/ubra-app/src/empty_workbench.rs`
- `crates/ubra-app/src/terminal_pane.rs`
- `crates/ubra-app/src/root.rs`
- `crates/ubra-app/src/agent_catalog.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint G — Entry and capacity (after Tasks 16–17)

- [ ] Focused tests/builds pass; 16-pane capacity and native resource evidence are recorded.
- [ ] Native Page 1 matches the reference hierarchy and both CTAs advance without creating a session.
- [ ] Human reviews empty-state scope, actual dropdown contents, focus behavior and 16-agent cost before continuing.

## Task 18: Preview and select the terminal layout

- [ ] Implement and verify this task.

**Description:** Complete Page 2 with a left-side live schematic and right-side selectable presets backed by one topology description. Offer Single, Side by side (2×1), Stacked (1×2), Focus + two, Grid (2×2), Six (3×2), and advanced Eight (4×2)/Sixteen (4×4). Default to Single; show exact independent-process counts and retain Page 1 choices.

**Acceptance criteria:**
- [ ] Every preset displays the correct pane count and relative geometry in the stable left preview; 2×1 is not duplicated as a separate Side by side option, and 4×4 means sixteen panes.
- [ ] Keyboard arrows and pointer selection update persistent selected state and the preview immediately; Back preserves folder/agent/layout, and no selection/preview creates a session.
- [ ] Explicit-count launch labels distinguish agents from terminals; larger presets disclose resource implications. Numbered preview panes are clearly illustrative, with no fake live status or output.

**Verification:**
- [ ] Focused behavior tests pass: `cargo test -p ubra-app --bin ubra-gui empty_`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Native Page 2, select every preset by keyboard and pointer, open More layouts and return via Back; inspect preview bounds, counts, selected states, long folder names and enlarged UI scale.

**Dependencies:** 16, 17

**Files likely touched:**
- `crates/ubra-app/src/empty_workbench.rs`
- `crates/ubra-app/src/root.rs`
- `README.md`

**Estimated scope:** Medium: 3 files.

## Task 19: Launch the selected single-pane workspace

- [ ] Implement and verify this task.

**Description:** Connect final confirmation to real Engine-backed workspace creation/reuse and one request-scoped session launch. Bind the admitted operation to its destination and owner, preserve the selected cwd/agent, expose genuine progress/errors, and transition to a usable terminal only after acknowledged placement.

**Acceptance criteria:**
- [ ] Single launches exactly one selected agent/Terminal in the chosen project or Engine-resolved home; reuse the selected empty workspace or create one when absent, place it in one tab and focus the actual terminal.
- [ ] Repeated Enter/click admits no duplicate operation; invalid folder/readiness prevents admission with actionable inline feedback, and creation/placement failures retain draft and expose any created session without silently substituting an agent.
- [ ] Unconfirmed creation is not blindly replayed, placement recovery never respawns, and dismissal/navigation during launch preserves admitted work without stealing a newly selected destination.

**Verification:**
- [ ] Focused behavior tests pass: `cargo test -p ubra-app --bin ubra-gui empty_` and `cargo test -p ubra-app --bin ubra-gui workspace_spawn`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: Real project/home launches with Terminal and an installed CLI in disposable state; verify PID/session count, cwd, interactive output/input and placement. Exercise failed placement, unconfirmed creation, repeated confirmation and navigation.

**Dependencies:** 17, 18

**Files likely touched:**
- `crates/ubra-app/src/empty_workbench.rs`
- `crates/ubra-app/src/root.rs`
- `crates/ubra-app/src/root/workspace_launches.rs`
- `crates/ubra-app/src/store/workspace_spawn.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Checkpoint H — Preview to real terminal (after Tasks 18–19)

- [ ] Integrated focused tests and app build pass.
- [ ] Both Page 1 CTAs lead through Page 2 to one real interactive session; cancellation and selection alone spawn nothing.
- [ ] Human reviews both native pages, keyboard flow, real progress/errors and draft preservation before multi-pane integration.

## Task 20: Launch multi-pane presets through acknowledged placement

- [ ] Implement and verify this task.

**Description:** Extend the admitted launch to create and place all preset leaves in one tab using the existing receipt/mutation pipeline. Resolve actual tab/pane/session identities between dependent steps, settle split fractions from the shared preview topology, and keep the existing eight-active-launch admission bound.

**Acceptance criteria:**
- [ ] Each multi-pane preset yields exactly its advertised number of distinct real sessions in one tab, all using the same selected agent/cwd, with final pane bounds matching preview proportions and the first pane focused.
- [ ] Dependent create/place operations are acknowledgment-driven and bounded; final confirmation remains single-admission, does not overwrite unrelated work, and workspace reload preserves the actual layout/session identities.
- [ ] Failure at any leaf stops further admission and retains successful sessions; unplaced/unconfirmed results are reachable through existing launch feedback, and placement recovery cannot duplicate a process.

**Verification:**
- [ ] Focused behavior tests pass: `cargo test -p ubra-app --bin ubra-gui workspace_spawn`, `cargo test -p ubra-app --bin ubra-gui empty_`, and `cargo test -p ubra-engine workspace`.
- [ ] Build succeeds: `cargo build -p ubra-engine -p ubra-app`.
- [ ] Smoke/manual check: Launch every preset with real shell PTYs, and representative 2/4-pane layouts with an installed agent CLI; compare preview/actual geometry, cwd, PIDs, per-pane input/output and reload. Inject a middle-leaf failure and destination removal; verify no duplicate or lost sessions.

**Dependencies:** 16, 18, 19

**Files likely touched:**
- `crates/ubra-app/src/empty_workbench.rs`
- `crates/ubra-app/src/root/workspace_launches.rs`
- `crates/ubra-app/src/store/workspace_spawn.rs`
- `crates/ubra-app/src/store/mod.rs`
- `README.md`

**Estimated scope:** Medium: 3–5 files.

## Task 21: Complete native onboarding interaction verification

- [ ] Implement and verify this task.

**Description:** Exercise the finished production wizard across window sizes, themes, zoom and error states; fix feature-owned interaction/layout defects and update user documentation. This is real-window completion of the flow, not a screenshot-only styling pass or a substitute for the prior launch tasks.

**Acceptance criteria:**
- [ ] Both pages preserve clear left-information/right-decision hierarchy, shared alignment spines and readable preview/control states in light/dark, default/small windows and enlarged UI scale; all CTAs remain reachable without clipping focus rings.
- [ ] Every action is keyboard-operable; Tab order, Enter, layout arrows, Back and topmost Escape work predictably, dropdown/picker/wizard closure restores focus, and reduced motion never hides state or delays selection.
- [ ] Native project/home/Terminal/installed-agent flows, every preset, dismissal/reopen, rescanning, partial launch and navigation are verified; real pane activity supplies the final visible outcome and README documents the new two-step flow and 16-pane bound.

**Verification:**
- [ ] Focused tests pass: `cargo test -p ubra-app --bin ubra-gui empty_` and `cargo test -p ubra-app --bin ubra-gui workspace_spawn`.
- [ ] Build succeeds: `cargo build -p ubra-app --bin ubra-gui`.
- [ ] Smoke/manual check: `scripts/dev.sh` with disposable state; capture actual Page 1/Page 2/final-workspace evidence, compare intended equal bounds/gaps, and complete the design-guide accessibility/review checklist.

**Dependencies:** 17, 18, 19, 20

**Files likely touched:**
- `crates/ubra-app/src/empty_workbench.rs`
- `crates/ubra-app/src/root.rs`
- `crates/ubra-app/src/root/workspace_launches.rs`
- `README.md`

**Estimated scope:** Medium: 3–4 files.

## Checkpoint I — Onboarding complete (after Tasks 20–21)

- [ ] All preset counts/topologies, real launch paths, partial-failure invariants and reload persistence pass.
- [ ] Native visual/interaction proof and 1/4/8/16-pane measurements are recorded; every requested acceptance criterion is met.
- [ ] No obsolete pane-local wizard state, fake status, preview PTYs, duplicate options, temporary smoke scaffolds or unverified binary-path claims remain.
- [ ] Human reviews the final native flow and resource/readability tradeoffs.
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build --workspace --release`
