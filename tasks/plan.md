# Implementation Plan: Pane interactions and Settings consistency

## Status and scope
Planning only; no application code changed, no runtime fixes claimed. Tasks are recorded in `tasks/todo.md`. Human review/approval is required before implementation.

Interpret “Remote the links” as **remove the Links and Session Details icon button from pane headers**. The current implementation uses one combined trigger. Preserve session data and unrelated inspector functionality.

Includes the additional requests: status-bar Worktree activation opens Settings → Worktrees, and substantial repair of Settings scrolling and inaccessible/incorrectly sized list content.

## Observed implementation
- `workspace_workbench.rs` owns pane controls. Split click currently toggles `split_menu`; hover sets it only on entry. A full-workbench menu scrim can intercept later clicks. `request_agent_split` already launches a new session with source host/cwd and typed split placement through spawn receipts.
- `agent_catalog.rs::resolved_target_agent` may replace an unavailable saved default with a different installed agent. Do not use that fallback unchanged for this request: splits should use the configured launchable agent or Terminal, not silently another agent.
- `WorkspaceGeometry::settled` projects zoom instantly and removes hidden panes/dividers. Terminal layout/controller ownership is separate from presentation; motion must preserve it.
- `terminal_pane.rs` renders the combined Links/session-details trigger in the pane header and hosted horizontal header actions. Pane drag currently uses `IconName::Hand`.
- `Prefs::default` sets `terminal_copy_on_select` to false. Container-level Serde defaults allow missing fields to inherit the revised default while explicit false remains false.
- Sidebar right-click producers already capture `event.position`. Sidebar popup rendering uses window coordinates; terminal context menus convert into pane/grid coordinates. The shared project menu contains the Expand/Collapse row. Removing it must not remove header disclosure or sidebar visibility controls.
- `SettingsDialogView` is the production modal. Its backdrop intentionally swallows clicks without dismissal. Escape uses `UtilitySurfaces::close_surface`, which preserves failed include-editor saves; direct Close events bypass that path today.
- `RootView` handles `StatusBarEvent::OpenWorktrees` through the standalone utility surface. `open_settings_dialog` already supports selecting Worktrees and reusing an existing dialog.
- `UtilitySurfaces::render_settings` always mounts `settings_scroll`/`overflow_y_scroll` and a `scroll_area`. Shortcuts and Skills also own inner scrollable lists; Skills details own another ScrollHandle. This is evidence of competing scroll regions, not proof of the reported double-bounce cause. `scroll_area` supplies custom overscroll physics; do not mistake it for a second content scroll container without inspecting event routing.
- Existing Settings page sizing tests use an older full-workbench harness; they do not prove the same content fits the centered production dialog. Worktrees inventory uses explicit 40-row pagination, which must stay reachable rather than be mistaken for truncation.
- `SemanticColors`, `Glass`, and `FloatingSurface` already define the visual language. Several Settings layers add work/background fills. GPUI has no per-element backdrop blur; the native window backdrop blurs desktop content, not live terminal content inside the window. Native macOS popup panels are a separate mechanism.

## Architecture decisions
1. **Rust UI changes only.** Reuse Engine spawn receipts and workspace mutations. No remote transport/protocol change, session cloning, or second terminal parser. Local and remote splits keep source host/cwd. Read `REMOTE_PORT.md` before any implementation that unexpectedly requires remote behavior changes.
2. **Click executes; hover offers a choice.** A split click dispatches the configured launchable default immediately, otherwise a shell. No chooser and no selection from the source session's agent. Hovering creates no session; selecting a row launches exactly that agent. “Right away” means no extra user step: durable pane insertion follows the real Engine spawn/placement receipt, not a fabricated live SessionId. Preserve visible pending/failure feedback; do not hide asynchronous launch latency.
3. **Separate hover-picker lifetime from click dispatch.** Keep source pane/tab/edge identity; allow trigger-to-menu travel, dismiss when crossing unrelated controls, and switch right/bottom targets without stale callbacks. Do not allow the scrim to consume a split click.
4. **Pane zoom is presentation motion, not font zoom.** Use existing non-overshooting motion primitives, approximately 160–190 ms, reduced-motion immediate settlement, reversible interruption from the current pose. Preserve PTY/controller ownership and resize cadence; avoid rebuilding/duplicating terminal grids solely for animation.
5. **Drag affordance:** use a compact six-dot Lucide grip in the existing vendored icon family, retaining grab cursor, accessible name, tooltip, and move/swap behavior. No new icon dependency.
6. **Context menus versus dropdowns:** right-click menus use a small consistent pointer gap, flip left/up when necessary, then clamp to their host bounds. Button dropdowns, split hover pickers, caret menus, and row previews retain their logical anchors. Model trigger intent explicitly because `ProjectActions.position: Some` currently also serves button-origin menus. Keep native and in-window geometry equivalent and terminal coordinate conversion correct.
7. **One scroll owner per content region/gesture.** Ordinary Settings pages use the outer page scroll. Fill-pane list/detail pages own their list/detail scrolling and must not simultaneously move/bounce the outer page. Rail scrolling and multiline editors are intentional independent regions. Fix ownership, constraints, content measurement and event propagation before tuning physics; do not disable all bouncing as a symptom workaround.
8. **All list content remains reachable.** Derive scroll extents from rendered content, permit expandable content to change measured height, keep virtualized fixed-height rows fixed only where the content contract actually is fixed, and preserve intentional Worktrees pagination. Audit every Settings tab in the actual dialog at representative sizes.
9. **Shared modal styling, feature-owned dismissal.** Reuse existing semantic glass/floating tokens and one scoped modal treatment; do not globally restyle all `FloatingSurface` callers (menus, toasts and in-flow cards). Match sidebar hue, hairline/rim, geometry and typography while keeping live-content overlays dense enough to read. Do not claim a translucent tint is true backdrop blur. Native NSAlert remains platform-native; no new native modal-window architecture is planned.
10. **Settings backdrop dismissal uses owner-safe close.** Preserve edits/save errors, topmost nested-overlay precedence, no click-through, and return focus to the previously active terminal pane. No new unsaved-changes confirmation is required. Existing explicit close routes should share the same persistence contract.
11. **Defaults are not forced migrations.** Enable copy-on-selection for fresh/missing-field preferences; preserve an existing explicit opt-out.
12. **Worktree navigation scope:** change the status-bar entry point to Settings → Worktrees. Preserve standalone worktree workflows used by other commands unless they become genuinely obsolete; do not silently redirect every worktree command.

## Dependency graph and task index
See detailed criteria, verification and file sets in `tasks/todo.md`.

### Phase 1: Settings correctness (high-risk first)
- Task 1: Repair Settings scroll ownership.
- Task 2: Make Settings list and expanded content fully reachable (depends on 1).
- Task 3: Route status-bar Worktree activation to Settings → Worktrees.
- Checkpoint A: production-dialog scroll and Worktrees route review.

### Phase 2: Pane interaction
- Task 4: Split immediately with default agent/Terminal.
- Task 5: Correct split hover-picker lifetime (depends on 4).
- Task 6: Simplify pane header and improve drag grip.
- Checkpoint B: split/hover/drag flow review.
- Task 7: Animate pane zoom without changing session identity (depends on 4–6).
- Task 8: Remove project context-menu disclosure command.
- Task 9: Enable copy-on-selection by default.
- Checkpoint C: motion, sidebar and clipboard review.

### Phase 3: Context-menu geometry
- Task 10: Apply pointer placement to sidebar right-click menus.
- Task 11: Apply equivalent pointer policy to terminal context menus (depends on 10).
- Checkpoint D: native/in-window pointer geometry and focus review.

### Phase 4: Modal consistency
- Task 12: Establish shared modal treatment in production Settings (depends on 1–2).
- Task 13: Dismiss Settings safely on backdrop activation (depends on 12).
- Checkpoint E: production Settings styling and safe backdrop dismissal review.
- Task 14: Migrate utility/nested modal styling (depends on 12–13).
- Task 15: Migrate remaining custom confirmation and What's New modal styling (depends on 12–14).
- Checkpoint F: complete native visual and interaction review; full workspace gates.

## Parallelization opportunities
After approval, independent slices can proceed: Task 3, Tasks 4–5, Task 8, Task 9. Settings scroll Tasks 1–2 and modal Tasks 12–14 share `surface_shell.rs`/`settings_dialog.rs`; execute sequentially or use a single integration owner. Pane Tasks 4–7 share `workspace_workbench.rs`; no concurrent edits there. Sidebar Tasks 8 and 10 share `sidebar/view.rs`; serialize. Root-owned routing, Settings tests and confirmations share `root.rs`; coordinate one integration owner. Establish pointer placement contract in Task 10 before terminal adoption. Build/lint/test once per integrated checkpoint, not in parallel mid-flight.

## Verification strategy
Implementation must start from the user's observed scrolling failures, not rerun a check just to dispute them. Record affected tabs, event/offset ownership, visible truncation and actual rendered bounds while diagnosing; then prove the changed paths after repair.

Use existing GPUI test contexts for deterministic gesture transitions, focus, placement, save failure and preference boundaries. Replace obsolete split-click menu tests with consumer-visible click/hover behavior tests; do not re-pin incidental wording. Add tests only for plausible behavioral regressions, not copied wiring or source text.

Run the actual native GUI via `scripts/dev.sh` and open the production Settings dialog. Inspect light/dark and Glass/Opaque, large/small windows, trackpad finger/momentum and wheel input, top/bottom edges, long and expanding lists, popups beyond card bounds, native floating panels, and reduced motion. Compare native menus with `UBRA_FLOATING_PANELS=0`. Capture evidence from the production dialog; older screenshots are page-content references only until fixtures are aligned. Native WindowServer blur and AppKit prompts cannot be proved by headless screenshots alone.

Use disposable local Engine/PTYS for split and zoom smoke proof; no real remote host required by default. The existing ignored `keyboard_workspace_operations_commit_through_engine_without_restarting_ptys` scenario checks identity preservation. Never substitute inert spawn receipts alone for proof that a new terminal/agent actually starts.

Final commands from workspace root:
```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```
Update existing README user-facing behavior documentation as relevant tasks land. No new implementation documentation convention is needed.

## Risks and mitigations
| Risk | Impact | Mitigation |
| --- | --- | --- |
| Double bounce cause not isolated | High | Inspect production gesture routing and custom overscroll alongside nested viewport offsets; do not just remove animation. |
| List clipping differs between old harness and modal | High | Seed long/expanded content in `RootView`'s actual SettingsDialogView and verify last items/actions after resize. |
| Zoom remounts terminal or thrashes PTY geometry | High | Separate settled ownership from visual transition; retain entities and existing resize pacing; verify real process identity and input. |
| Hover scrim consumes direct split click | High | Exercise hover→click and cross-control transitions, not only direct listener calls. |
| Unavailable default silently launches another agent | Medium | Split uses explicit launchability/default-or-shell policy; preserve global shortcuts' existing resolver unless explicitly changed. |
| Outside close loses drafts or bypasses save failure | High | Reuse feature-owner close path and test failed persistence plus nested popup precedence. |
| Thin modal fill exposes sharp terminal text | High | Dense themed overlay surface; actual native contrast review; no unsupported blur promise. |
| Pointer menu moves twice between coordinate spaces | High | Preserve window versus pane/grid conversion and native global conversion; test split origins and multi-monitor placement. |
| Project menu removal affects all reused entrypoints | Low | Remove shared Expand/Collapse row consistently; retain disclosure controls and remaining commands. |
| Shared modal styling changes menus/toasts | Medium | Scope styling explicitly; keep domain behavior outside presentation recipe. |

## Open questions / review notes
- No blocking product question. Review the choices above before implementation: grip icon, default-or-shell policy, scoped status-bar redirect, dense glass-style overlays rather than a new native modal architecture.
- Exact Settings tabs exhibiting the reported scrolling issues are not yet isolated. This is an implementation investigation, not a request for the user to locate repo-provided information; audit all tabs and long-content states.
- A Usage share overlay mounting gap was observed during code research: production Settings renders its page directly while the older utility renderer mounts that nested overlay. Verify and repair the affected composition in Task 14 as necessary to make the requested modal family work end-to-end.
- The planning skill's optional `../../references/definition-of-done.md` asset is absent in this installation; repository build/verification rules and the explicit acceptance criteria below govern delivery.

---

# Implementation Plan: Empty-workspace launch wizard

## Status and scope
Planning only. This section adds the requested onboarding flow without replacing the earlier pane/Settings plan. Detailed tasks are appended to `tasks/todo.md` as Tasks 16–21. Implementation starts only after human review and approval; no application code, tests, or runtime behavior changed during planning.

Build a two-page, centered desktop modal for an empty work area. Page 1 combines workspace-folder and agent-CLI selection; both “Get Started” and “Start in home folder” advance to Page 2 without spawning anything. Page 2 previews a selected terminal layout in its left sidebar and launches exactly that many independent sessions only on final confirmation.

## Observed implementation
- `empty_workbench.rs` currently renders a pane-local, maximum-480-pixel card: workspace/home first, agent selection second. It is not a Root-owned modal.
- `terminal_pane.rs::render_empty_workbench` owns `empty_workspace`, opens a directory-only native picker, and dispatches one `spawn_kind` immediately when an agent is picked. Catalog detection is local; preserve that scope.
- `agent_catalog.rs` supplies installed-agent readiness, MRU ordering, command names, and sign-in facts. `default_agent_options` includes Terminal. Do not introduce a hard-coded agent list or optimistic detection.
- `AgentOption::binary` does not establish an absolute executable path. Do not reproduce the reference’s “Found at /opt/homebrew/…” unless actual Engine readiness data provides that fact.
- `RootView` owns the active workspace and production dialog composition. `root/workspace_launches.rs` exposes request-scoped launch feedback. Workspace panes are rendered by `WorkspaceWorkbench`.
- `store/workspace_spawn.rs` already distinguishes Creating, Placing, Placed, Unplaced and Unconfirmed, limits active requests to eight, and never repeats a spawn when retrying placement.
- `ubra-proto/src/workspace.rs::MAX_TAB_PANES` is eight. Engine workspace validation and app geometry consume it. A literal 4×4 therefore requires an intentional shared limit increase to 16, not just a new UI tile.
- Existing workspace mutations provide CreateWorkspace, CreateTab, SplitPane, ResizeSplit and FocusPane. Use these acknowledged operations and real session/pane identities rather than a second layout persistence model.

## Architecture decisions
1. **One wizard per window, owned by RootView.** Keep the rendering/draft model in the existing empty-workbench feature. Mount one modal through the production root composition, not one modal per empty terminal pane. Remove obsolete pane-local two-step launch state/callbacks after cutover; preserve the lightweight “No session open” resting state for nonempty workspaces.
2. **Empty is scoped to the current work area, and the setup cannot be dismissed.** Auto-present when the window has no active work, or the selected workspace has no tabs/panes. Sessions elsewhere must not suppress an empty selected workspace. An existing workspace containing work, a temporarily unselected pane, or an in-flight launch is not a reason to interrupt with onboarding. Now that the wizard opens automatically there is no reopen affordance and no way out of it: no corner close, no Escape/⌘W dismissal, and the pane-local placeholder is inert text. An empty work area is finished by launching into it — the product decision the owner asked for. The workbench placeholder is therefore only a transition state, and nothing else may dismiss the wizard; the owner closes it when the empty entry stops being empty or a launch succeeds.
3. **Match the reference’s hierarchy and one shared modal material.** Center a two-column dialog with a quiet branded information rail (~40%) and a form region (~60%), common alignment spines, one hairline divider, comfortable controls and a bottom decision row. Both columns sit on the same card material — `FloatingSurface::modal(colors, card).fill(colors.sidebar_surface())` with a `sidebar_stroke()` rail hairline — which is exactly what the production Settings dialog paints, so the two modals read as one family over live terminal content. Earlier revisions tinted the form region separately and used the denser default floating fill; both are gone. Use `SemanticColors`, theme-aware radii/type and rem-based layout; no invented API, raw palette, fake blur or new component dependency.
4. **Page 1: Choose where to work.** Left: “Put your agent in its project.”, a concise explanation of independent sessions/activity, and an explicitly illustrative mock macOS window over a mock terminal (traffic lights, window title, prompt line) so a newcomer recognises what they are choosing. No Ubra wordmark: the illustration and copy carry the page. Right: step indicator, heading, labeled folder dropdown and labeled agent dropdown — the agent control shows the selected CLI's brand mark. Folder menu lists actual local project roots known to the store with full-path disambiguation, plus “Choose folder…” using the native directory picker; an empty list still exposes the picker. Do not add a new persisted recents system. Selecting a folder changes only the draft.
5. **Explicit agent selection.** Reuse installed/MRU catalog options and Terminal; seed the draft with the configured default only if launchable, otherwise the first detected installed agent, otherwise Terminal. Show the actual command, detection/sign-in facts when known, and a rescan action when no agents are detected. Sign-in occurs in the real CLI, not in the wizard. Never silently replace a selected agent if readiness changes: show the issue and let the user choose. No free-form command input, CLI installer or injected prompt.
6. **Page 1 actions preserve the requested wording.** Primary “Get Started” requires a chosen folder and a launchable agent; secondary Button “Start in home folder” uses home plus the same agent and requires no folder selection. Both advance to the layout page. Supporting text says “Next, choose your pane layout” so the labels do not imply that sessions have already started. Home uses the existing local Engine home/cwd semantics, not a guessed `$HOME` path.
7. **Page 2: See your workspace before opening it.** Left rail becomes a live schematic preview with numbered pane headers, selected agent, folder summary and explicit count. Right has selectable layout tiles, brief use-case text, Back, and a primary “Launch N agents” (or “Open N terminals” for Terminal). The schematic is illustrative, never fabricated terminal output or running status. Selection updates the same preview immediately; no preview PTYs or scanning animation.
8. **Preset notation is columns × rows.** Provide Single (1×1), Side by side (2×1), Stacked (1×2), Focus + two (one large pane at left, two stacked at right), Grid (2×2), and Six (3×2). More layouts exposes Eight (4×2) and the requested Sixteen (4×4). “Side by side” and “2×1” are the same option, not duplicate tiles; Stacked supplies the other useful two-pane orientation. Default to Single, never automatically start 16 agents. Larger presets state that each pane runs an independent process and may consume agent resources.
9. **One preset description drives preview and placement.** Keep a small app-local recursive topology using the existing horizontal/vertical split concepts, stable leaf order and fractions. Map it to acknowledged workspace mutations; no protocol schema change or persisted preset ID is required. Balanced subdivision plus ResizeSplit yields equal grids, including three columns. Focus + two uses a larger left region. Preview and committed geometry must agree.
10. **Raise the shared pane ceiling deliberately.** Task 16 proves 16-pane validation, persistence and geometry before exposing 4×4. Preserve depth bounds, visible-terminal ownership and existing rendering budgets. Keep the eight-active-spawn admission bound; launch dependent create/place steps sequentially, driven by receipts rather than polling or sleep. A capacity/performance failure blocks delivery of the requested preset; do not silently relabel 4×4 as four panes or cap it at eight.
11. **Launch exactly once into one tab.** Reuse the selected empty workspace when present; otherwise create a workspace via the existing mutation API and wait for its acknowledged identity. The first real session creates the tab; subsequent real sessions split the acknowledged target panes according to the selected topology, then settle fractions and focus the first pane. All sessions receive the same explicitly selected agent and local cwd. Resolve folder existence/access and readiness at confirmation without constructing shell commands.
12. **Partial launch is visible, not destructive.** Disable duplicate confirmation and layout changes after admission. Show actual progress, keep the draft on errors, stop further admission on failure, and retain every created session. Unplaced sessions remain reachable through existing launch/All sessions UI; placement recovery must use the existing session, never respawn it. An unconfirmed creation cannot be blindly retried. Escape/Close before launch cancels only the draft; after admission it closes the presentation while the admitted operation/receipts remain observable. Never kill successful agents to simulate atomic rollback.
13. **Aha moment is preview-to-reality continuity.** Launching opens the actual selected layout, focuses its first terminal and displays real agent activity in existing headers/Agents UI. No fake role assignment, autonomous teamwork promise, prefilled prompt, confetti, synthetic activity or success toast when the real layout is already visible.
14. **Native keyboard and resizing contracts.** Logical Tab order, labeled controls, visible focus, Enter advances/confirms only when valid, arrows select layouts, and Back retains folder/agent/layout. Escape dismisses the topmost dropdown first, then the wizard, restoring work-area focus. Maintain a comfortable dialog near the reference’s proportions; clamp to available bounds, reduce secondary rail content at narrow widths and scroll the form/list region without losing either CTA. Inspect default/small windows, enlarged UI scale and both themes.

## Dependency graph and task index
Tasks 16–21 form a separate feature stream; none depends on completing earlier Tasks 1–15.

- Task 16: Support sixteen-pane workspace layouts (high-risk capacity gate).
- Task 17: Choose folder and agent in the empty-workspace modal.
- Checkpoint G: empty-state/folder/agent behavior and capacity review.
- Task 18: Preview and select the terminal layout (depends on 16, 17).
- Task 19: Launch the selected single-pane workspace (depends on 17, 18).
- Checkpoint H: real single-pane launch and selection review.
- Task 20: Launch multi-pane presets through acknowledged placement (depends on 16, 18, 19).
- Task 21: Complete native interaction, failure and visual verification (depends on 17–20).
- Checkpoint I: complete onboarding flow and workspace release gates.

## Parallelization opportunities
Task 16 and Task 17 are independent after approval. Tasks 18–20 depend on the settled draft/topology and launch contracts. Root composition, `empty_workbench.rs`, and launch orchestration need one integration owner; serialize overlapping edits. No parallel agent execution is required during planning. At implementation checkpoints run build/lint/tests once after integration, not in competing mid-flight jobs.

## Verification strategy
Use existing GPUI contexts and deterministic Engine fixtures for empty-entry transitions, picker cancellation, draft retention, catalog changes, duplicate submission, real layout topology, partial/unconfirmed outcomes and placement-only recovery. Replace obsolete two-step empty-pane tests; delete wording/source/selector-only assertions rather than re-pin the old presentation.

Launch the actual GUI with `scripts/dev.sh` against disposable state and directories, not the user’s saved workspace. Exercise project and home paths with one installed agent, and Terminal with no installed agents. Prove exact independent session/process counts, cwd, topology and working input/output for every preset, including 16. Reload the workspace and verify persisted identities/layout. Fixture receipts alone are not launch proof.

Capture both wizard pages in a native window at default/small sizes, light/dark, enlarged UI scale, keyboard-only and reduced-motion settings. Compare preview proportions with actual pane bounds. Measure idle/active CPU and memory plus time to first usable pane for 1, 4, 8 and 16 panes; record the existing 8-pane baseline and the 16-pane cost, without inventing a numeric product budget in this plan.

Final repository gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo build --workspace --release`. These are future verification commands, not results from planning. Update README behavior documentation as implementation lands.

## Risks and mitigations
| Risk | Impact | Mitigation |
| --- | --- | --- |
| Sixteen agents exceed useful visible size or resource budget | High | Advanced explicit-count option, Single default, early capacity gate, native readability and resource measurements. |
| App shows one wizard per empty pane or repeatedly reopens it | High | Root ownership, settled-load trigger, empty-entry identity and explicit reopen action. |
| Sequential splits produce the wrong grid | High | Shared topology, receipt-resolved pane IDs, explicit split fractions, geometry assertions and preview/actual comparison. |
| Partial launch creates duplicate or hidden agents | High | Existing request receipts, stop-on-failure, retain successes, placement-only recovery, no replay after unconfirmed creation. |
| User navigates away or removes the destination mid-launch | High | Bind launch to destination/owner identity; do not steal subsequent selection; expose unplaced results. |
| Reference absolute binary path becomes a false detection claim | Medium | Display only known readiness facts; command name is sufficient. |
| Large modal clips controls at small size or large font | Medium | Bounds-clamped composition, secondary-content reduction, region-owned scrolling and native focus-ring inspection. |
| Capacity change invalidates older local workspace readers | Medium | Existing schema remains structurally unchanged; document revised bound and test load/validation of 16-pane snapshots; do not bypass explicit Engine identity checks. |
| Concurrent pane/Settings implementation conflicts | Medium | Preserve earlier plan and assign one owner to shared Root/workbench files during integration. |

## Review notes
- Interpret 4×4 literally as 16 panes, and 2×1 as Side by side. These are explicit product decisions for approval, not hidden scope changes.
- “Nothing to work on” means no work in the current window/selected workspace, not globally no sessions. Preserve existing sessions elsewhere and avoid interrupting nonempty workspaces.
- The reference is the visual direction; the additional home Button and Page 2 replace its single-step launch/Close footer behavior.
- No remote transport, PTY holder, parser or packaging change is planned. If implementation needs any such change, read `REMOTE_PORT.md` and resolve that scope/design dependency explicitly before editing it.
